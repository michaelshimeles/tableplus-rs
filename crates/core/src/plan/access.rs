//! How a step reaches a table's rows, which steps are joins, and the side work a step does.

use super::{NodeId, Plan, PlanFormat, PlanNode};

/// How a step reads its rows. The first six are MySQL's access types (`ALL`, `index`, `range`,
/// `ref`, `eq_ref`, `const`), which MySQL Workbench's Visual Explain colours its boxes by; the
/// other engines' steps map onto them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Access {
    /// Reads every row of a table (`ALL`, `Seq Scan`, `SCAN t`).
    FullScan,
    /// Reads every entry of an index (`index`).
    IndexScan,
    /// Reads the part of an index between bounds (`range`).
    Range,
    /// Looks up the rows matching a key value, possibly several (`ref`). PostgreSQL doesn't say
    /// whether its index is unique, so its lookups are always this.
    Lookup,
    /// Looks up at most one row through a unique key (`eq_ref`).
    UniqueLookup,
    /// Rows known before the statement runs, or computed without a table (`const`, `Values Scan`).
    Constant,
    /// Reads back a result the statement built itself: a temporary table, a union's result, a
    /// materialized derived table, subquery or CTE. Such a read is always a full scan, and no
    /// index can change that, so it isn't coloured as one.
    InternalResult,
}

impl Access {
    pub fn label(self) -> &'static str {
        match self {
            Access::FullScan => "Full table scan",
            Access::IndexScan => "Full index scan",
            Access::Range => "Index range scan",
            Access::Lookup => "Key lookup",
            Access::UniqueLookup => "Unique key lookup",
            Access::Constant => "Constant or computed rows",
            Access::InternalResult => "Internal result",
        }
    }

    /// MySQL's and MariaDB's `access_type` as their JSON formats print it.
    fn from_mysql_type(access_type: &str) -> Option<Self> {
        Some(match access_type {
            "ALL" | "hash_ALL" => Access::FullScan,
            "index" => Access::IndexScan,
            "range" | "index_merge" | "hash_range" | "hash_index_merge" => Access::Range,
            "ref" | "ref_or_null" | "fulltext" | "index_subquery" | "hash_index" => Access::Lookup,
            "eq_ref" | "unique_subquery" => Access::UniqueLookup,
            "const" | "system" => Access::Constant,
            _ => return None,
        })
    }

    fn is_scan(self) -> bool {
        matches!(self, Access::FullScan | Access::IndexScan)
    }
}

/// MySQL TREE iterator names, which JSON format version 2 and MariaDB's steps share.
const MYSQL_TREE: &[(&str, Access)] = &[
    ("Table scan", Access::FullScan),
    ("Sample scan", Access::FullScan),
    ("Scan new records", Access::InternalResult),
    ("Index scan", Access::IndexScan),
    ("Covering index scan", Access::IndexScan),
    ("Index range scan", Access::Range),
    ("Covering index range scan", Access::Range),
    ("Index skip scan", Access::Range),
    ("Covering index skip scan", Access::Range),
    ("Index skip scan for grouping", Access::Range),
    ("Covering index skip scan for grouping", Access::Range),
    ("Index skip scan for deduplication", Access::Range),
    ("Covering index skip scan for deduplication", Access::Range),
    ("Multi-range index lookup", Access::Range),
    ("Index lookup", Access::Lookup),
    ("Covering index lookup", Access::Lookup),
    ("Indexed full text search", Access::Lookup),
    ("Single-row index lookup", Access::UniqueLookup),
    ("Single-row covering index lookup", Access::UniqueLookup),
    ("Rows fetched before execution", Access::Constant),
];

/// MySQL TREE steps that write a temporary result, which a later step reads back.
const MYSQL_TEMP_BUILDERS: &[&str] = &[
    "Materialize",
    "Union materialize",
    "Union all materialize",
    "Intersect materialize",
    "Except materialize",
    "Temporary table",
    "Aggregate using temporary table",
];

/// Work a step does beside reading or combining rows, which its colour doesn't show: what the
/// `Extra` column of MySQL's traditional EXPLAIN calls `Using temporary` and `Using filesort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Extra {
    /// Writes its rows to a temporary table for a later step to read back.
    TempTable,
    /// Sorts through MySQL's sort buffer, spilling to disk when it fills.
    Filesort,
}

impl Extra {
    pub fn label(self) -> &'static str {
        match self {
            Extra::TempTable => "temp table",
            Extra::Filesort => "filesort",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Extra::TempTable => "Writes its rows to a temporary table, which a later step reads back",
            Extra::Filesort => "Sorts in a sort buffer, on disk when the buffer fills",
        }
    }
}

impl PlanNode {
    /// True for a step that combines the rows of its inputs: a nested loop, hash or merge join,
    /// including semi- and antijoins.
    pub fn is_join(&self) -> bool {
        let op = self.operation.to_ascii_lowercase();
        op.contains("join") || op.starts_with("nested loop")
    }

    pub(super) fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.iter().find(|a| a.key == key).map(|a| a.value.as_str())
    }

    /// `<temporary>`, `<union1,2>`, `<derived3>`, `<subquery4>`: MySQL's and MariaDB's names for
    /// results the statement builds.
    fn reads_internal_name(&self) -> bool {
        self.object.as_deref().is_some_and(|o| o.starts_with('<'))
    }
}

impl Plan {
    /// How a step reads its rows; `None` for steps that work on other steps' rows, such as joins,
    /// filters and sorts.
    pub fn access(&self, id: NodeId) -> Option<Access> {
        let node = self.node(id);
        match self.format {
            PlanFormat::MySqlTree | PlanFormat::MySqlJsonV2 | PlanFormat::MySqlJson | PlanFormat::MariaDbJson => {
                let access = mysql_access(node)?;
                // A scan of `<temporary>`, or of a derived table or CTE that a step below it
                // materializes, reads back a result built below it.
                let built_below = || self.children(id).iter().any(|c| self.extras(*c).contains(&Extra::TempTable));
                Some(if access.is_scan() && (node.reads_internal_name() || built_below()) {
                    Access::InternalResult
                } else {
                    access
                })
            }
            PlanFormat::PostgresJson => postgres_access(node),
            PlanFormat::SqliteQueryPlan | PlanFormat::SqliteScanStatus => self.sqlite_access(node),
        }
    }

    /// The side work a step does, in the formats that report it.
    pub fn extras(&self, id: NodeId) -> Vec<Extra> {
        let node = self.node(id);
        let op = node.operation.as_str();
        let flag = |key: &str| node.attribute(key) == Some("true");
        let mut extras = Vec::new();
        let temp = match self.format {
            PlanFormat::MySqlTree | PlanFormat::MySqlJsonV2 => {
                MYSQL_TEMP_BUILDERS.iter().any(|b| op == *b || op.starts_with(&format!("{b} ")))
            }
            PlanFormat::MySqlJson => flag("using_temporary_table"),
            PlanFormat::MariaDbJson => {
                matches!(op, "Temporary table" | "Materialize" | "Materialize subquery")
                    || (matches!(op, "Union" | "Recursive union") && node.reads_internal_name())
            }
            PlanFormat::SqliteQueryPlan | PlanFormat::SqliteScanStatus => op == "Materialize",
            PlanFormat::PostgresJson => false,
        };
        if temp {
            extras.push(Extra::TempTable);
        }
        let filesort = match self.format {
            PlanFormat::MySqlJson => flag("using_filesort"),
            // MariaDB's `filesort` object; its step is named `Sort`, like MySQL TREE's.
            PlanFormat::MariaDbJson => op == "Sort",
            _ => false,
        };
        if filesort {
            extras.push(Extra::Filesort);
        }
        extras
    }

    /// `SCAN t` reads a table, unless `t` is a CTE or subquery that a `MATERIALIZE` or
    /// `CO-ROUTINE` step builds. A CTE read under an alias (`MATERIALIZE ranked`, `SCAN r`)
    /// can't be told from a table.
    fn sqlite_access(&self, node: &PlanNode) -> Option<Access> {
        let built = |name: &str| {
            name.starts_with('(')
                || self.nodes.iter().any(|n| {
                    matches!(n.operation.as_str(), "Materialize" | "Co-routine") && n.object.as_deref() == Some(name)
                })
        };
        let index = node.index.as_deref();
        match node.operation.as_str() {
            "Scan constant row" => Some(Access::Constant),
            "Scan" if node.object.as_deref().is_some_and(built) => Some(Access::InternalResult),
            "Scan" if index.is_some() => Some(Access::IndexScan),
            "Scan" => Some(Access::FullScan),
            "Search" => Some(match index {
                Some(i) if i.starts_with("AUTOMATIC ") => Access::Lookup,
                _ if node.condition.as_deref().is_some_and(is_range) => Access::Range,
                Some("INTEGER PRIMARY KEY") => Access::UniqueLookup,
                _ => Access::Lookup,
            }),
            _ => None,
        }
    }
}

/// MySQL TREE and JSON version 2 steps by name, JSON version 1 and MariaDB tables by their
/// `access_type`.
fn mysql_access(node: &PlanNode) -> Option<Access> {
    // MySQL prints `Constant row from <table>` with no `on`, so the table stays in the name.
    if node.operation.starts_with("Constant row from") {
        return Some(Access::Constant);
    }
    if let Some((_, access)) = MYSQL_TREE.iter().find(|(op, _)| *op == node.operation) {
        return Some(*access);
    }
    // Checked after the name: JSON format version 2 prints a generic `index` for every index
    // access, which would read as a full index scan in version 1's vocabulary.
    node.access_type.as_deref().or_else(|| node.attribute("access_type")).and_then(Access::from_mysql_type)
}

/// PostgreSQL's scan nodes. An index scan with an equality condition is a lookup, with a bound
/// a range, and without a condition, as for an ORDER BY, it reads the whole index.
fn postgres_access(node: &PlanNode) -> Option<Access> {
    let op = node.operation.as_str();
    let op = op.strip_prefix("Parallel ").unwrap_or(op);
    let op = op.strip_prefix("Async ").unwrap_or(op);
    let op = op.strip_suffix(" Backward").unwrap_or(op);
    let by_condition = |key: &str| match node.attribute(key) {
        Some(cond) if is_range(cond) => Access::Range,
        Some(_) => Access::Lookup,
        None => Access::IndexScan,
    };
    Some(match op {
        "Seq Scan" | "Sample Scan" => Access::FullScan,
        "Index Scan" | "Index Only Scan" | "Bitmap Index Scan" => by_condition("Index Cond"),
        "Bitmap Heap Scan" => by_condition("Recheck Cond"),
        "Tid Scan" => Access::UniqueLookup,
        "Tid Range Scan" => Access::Range,
        "Function Scan" | "Table Function Scan" | "Values Scan" => Access::Constant,
        "Result" if node.children.is_empty() => Access::Constant,
        "CTE Scan" | "WorkTable Scan" | "Subquery Scan" | "Named Tuplestore Scan" => Access::InternalResult,
        _ => return None,
    })
}

/// A condition with a bound: `<`, `>`, `<=`, `>=` or PostgreSQL's `~>=~` for a LIKE prefix. JSON
/// arrows, containment operators and quoted text are not bounds.
fn is_range(condition: &str) -> bool {
    let mut text = condition.to_string();
    for operator in ["->>", "->", "#>>", "#>", "<@", "@>", "<->"] {
        text = text.replace(operator, " ");
    }
    let mut quoted = false;
    text.chars().any(|c| {
        quoted ^= c == '\'';
        !quoted && (c == '<' || c == '>')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnMeta, ResultSet};
    use crate::server::{Flavor, ServerInfo, Version};
    use crate::value::Value;

    fn one_cell(flavor: Flavor, version: Version, text: &str) -> Plan {
        let result = ResultSet {
            columns: vec![ColumnMeta::new("EXPLAIN", "VARCHAR")],
            rows: vec![vec![Value::text(text)].into()],
            truncated: false,
        };
        crate::plan::parse(&ServerInfo::new(flavor, version), &result).unwrap().unwrap()
    }

    fn steps(plan: &Plan) -> Vec<(String, Option<Access>)> {
        plan.nodes().map(|(id, n)| (n.operation.clone(), plan.access(id))).collect()
    }

    fn mysql_tree(text: &str) -> Plan {
        one_cell(Flavor::MySql, Version::new(8, 4, 0), text)
    }

    #[test]
    fn mysql_tree_steps_map_to_workbench_access_types() {
        let plan = mysql_tree(
            "\
-> Nested loop inner join  (cost=1150 rows=830)
    -> Inner hash join (c.customer_id = o.customer_id)  (cost=594 rows=1.95)
        -> Table scan on c  (cost=0.525 rows=91)
        -> Hash
            -> Covering index scan on sh using PRIMARY  (cost=0.55 rows=3)
    -> Index range scan on o using idx_date over ('1997-01-01' <= order_date)  (cost=60 rows=270)
    -> Index lookup on t using <auto_key0> (order_id=o.order_id)  (cost=0.25 rows=2)
    -> Covering index lookup on od2 using idx_product (product_id=p2.product_id)  (cost=0.5 rows=28)
    -> Single-row index lookup on e using PRIMARY (employee_id=o.employee_id)  (cost=0.25 rows=1)
    -> Single-row covering index lookup on m using PRIMARY (employee_id=e.reports_to)  (cost=0.25 rows=1)
    -> Constant row from shippers
    -> Rows fetched before execution
    -> Index scan on od using PRIMARY (reverse)  (cost=217 rows=2155)
    -> Covering index skip scan for grouping on p using idx_supplier  (cost=3 rows=29)
    -> Filter: (r.order_date >= DATE'1997-01-01')
    -> Sort: c.customer_id
    -> Nested loop antijoin
    -> Hash semijoin (a.x = b.x)
",
        );
        use Access::*;
        let expected = [
            ("Nested loop inner join", None),
            ("Inner hash join", None),
            ("Table scan", Some(FullScan)),
            ("Hash", None),
            ("Covering index scan", Some(IndexScan)),
            ("Index range scan", Some(Range)),
            ("Index lookup", Some(Lookup)),
            ("Covering index lookup", Some(Lookup)),
            ("Single-row index lookup", Some(UniqueLookup)),
            ("Single-row covering index lookup", Some(UniqueLookup)),
            ("Constant row from shippers", Some(Constant)),
            ("Rows fetched before execution", Some(Constant)),
            ("Index scan", Some(IndexScan)),
            ("Covering index skip scan for grouping", Some(Range)),
            ("Filter", None),
            ("Sort", None),
            ("Nested loop antijoin", None),
            ("Hash semijoin", None),
        ];
        let got = steps(&plan);
        let got: Vec<(&str, Option<Access>)> = got.iter().map(|(o, a)| (o.as_str(), *a)).collect();
        assert_eq!(got, expected);
        let joins: Vec<bool> = plan.nodes().map(|(_, n)| n.is_join()).collect();
        assert_eq!(joins.iter().filter(|j| **j).count(), 4);
    }

    #[test]
    fn mysql_scans_of_internal_results_are_not_full_scans() {
        let plan = mysql_tree(
            "\
-> Limit: 20 row(s)  (cost=2.85..2.85 rows=0)
    -> Table scan on <union temporary>  (cost=2.5..2.5 rows=0)
        -> Union all materialize  (cost=0..0 rows=0)
            -> Table scan on t  (cost=2.5..2.5 rows=0)
                -> Materialize  (cost=0..0 rows=0)
                    -> Table scan on orders  (cost=84 rows=830)
            -> Table scan on <temporary>
                -> Temporary table
                    -> Index lookup on t2 using <auto_key0> (order_id=orders.order_id)  (cost=0.25 rows=2)
                        -> Materialize CTE ranked  (cost=0..0 rows=0)
                            -> Table scan on <subquery4>  (cost=93.7..99.6 rows=269)
                                -> Materialize with deduplication  (cost=93.7..93.7 rows=269)
                                    -> Table scan on cat2  (cost=1.05 rows=8)
            -> Table scan on chain  (cost=2.73 rows=2)
                -> Materialize recursive CTE chain  (cost=2.73 rows=2)
                    -> Scan new records on chain  (cost=2.73 rows=2)
",
        );
        use Access::*;
        let got: Vec<_> = plan
            .nodes()
            .map(|(id, n)| (n.operation.as_str(), n.object.as_deref(), plan.access(id), plan.extras(id)))
            .collect();
        let (temp, none) = (vec![Extra::TempTable], vec![]);
        assert_eq!(
            got,
            [
                ("Limit", None, None, none.clone()),
                ("Table scan", Some("<union temporary>"), Some(InternalResult), none.clone()),
                ("Union all materialize", None, None, temp.clone()),
                ("Table scan", Some("t"), Some(InternalResult), none.clone()),
                ("Materialize", None, None, temp.clone()),
                ("Table scan", Some("orders"), Some(FullScan), none.clone()),
                ("Table scan", Some("<temporary>"), Some(InternalResult), none.clone()),
                ("Temporary table", None, None, temp.clone()),
                ("Index lookup", Some("t2"), Some(Lookup), none.clone()),
                ("Materialize CTE", Some("ranked"), None, temp.clone()),
                ("Table scan", Some("<subquery4>"), Some(InternalResult), none.clone()),
                ("Materialize with deduplication", None, None, temp.clone()),
                ("Table scan", Some("cat2"), Some(FullScan), none.clone()),
                ("Table scan", Some("chain"), Some(InternalResult), none.clone()),
                ("Materialize recursive CTE", Some("chain"), None, temp),
                ("Scan new records", Some("chain"), Some(InternalResult), none),
            ]
        );
    }

    #[test]
    fn mysql_json_v1_tables_by_access_type_with_flags_on_their_operations() {
        let plan = one_cell(
            Flavor::MySql,
            Version::new(5, 7, 44),
            r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "41.2"},
                "ordering_operation": {"using_temporary_table": true, "using_filesort": true,
                  "nested_loop": [
                    {"table": {"table_name": "<derived2>", "access_type": "ALL", "rows_examined_per_scan": 830,
                       "materialized_from_subquery": {"using_temporary_table": true, "dependent": false,
                         "query_block": {"select_id": 2, "table": {"table_name": "od", "access_type": "index", "key": "PRIMARY"}}}}},
                    {"table": {"table_name": "o", "access_type": "eq_ref", "key": "PRIMARY"}},
                    {"table": {"table_name": "c", "access_type": "ref", "key": "idx_country"}},
                    {"table": {"table_name": "s", "access_type": "range", "key": "PRIMARY"}},
                    {"table": {"table_name": "sh", "access_type": "const", "key": "PRIMARY"}},
                    {"table": {"table_name": "e", "access_type": "ALL"}}]}}}"#,
        );
        let by_object = |name: &str| {
            let (id, _) = plan.nodes().find(|(_, n)| n.object.as_deref() == Some(name)).unwrap();
            plan.access(id)
        };
        assert_eq!(by_object("<derived2>"), Some(Access::InternalResult));
        assert_eq!(by_object("od"), Some(Access::IndexScan));
        assert_eq!(by_object("o"), Some(Access::UniqueLookup));
        assert_eq!(by_object("c"), Some(Access::Lookup));
        assert_eq!(by_object("s"), Some(Access::Range));
        assert_eq!(by_object("sh"), Some(Access::Constant));
        assert_eq!(by_object("e"), Some(Access::FullScan));
        let extras_of = |op: &str| {
            let (id, _) = plan.nodes().find(|(_, n)| n.operation == op).unwrap();
            plan.extras(id)
        };
        assert_eq!(extras_of("Ordering"), [Extra::TempTable, Extra::Filesort]);
        assert_eq!(extras_of("Select #2"), [Extra::TempTable], "the derived table's materialization");
        assert_eq!(extras_of("Select #1"), []);
    }

    #[test]
    fn mariadb_tables_by_access_type_and_internal_results() {
        let plan = one_cell(
            Flavor::MariaDb,
            Version::new(11, 8, 0),
            r#"{"query_block": {"union_result": {"table_name": "<union1,2>", "access_type": "ALL",
                 "query_specifications": [
                   {"query_block": {"select_id": 1, "filesort": {"sort_key": "a.x", "temporary_table": {
                      "nested_loop": [
                        {"table": {"table_name": "a", "access_type": "ALL", "rows": 4}},
                        {"table": {"table_name": "d", "access_type": "ref", "key": "key0", "rows": 1,
                           "materialized": {"query_block": {"select_id": 3,
                             "table": {"table_name": "b", "access_type": "index", "key": "PRIMARY", "rows": 5}}}}},
                        {"table": {"table_name": "e", "access_type": "eq_ref", "key": "PRIMARY", "rows": 1}},
                        {"table": {"table_name": "f", "access_type": "range", "key": "k", "rows": 3}},
                        {"table": {"table_name": "g", "access_type": "const", "key": "PRIMARY", "rows": 1}}]}}}},
                   {"query_block": {"select_id": 2,
                      "table": {"table_name": "<derived4>", "access_type": "ALL", "rows": 2,
                        "materialized": {"query_block": {"select_id": 4,
                          "table": {"table_name": "h", "access_type": "hash_ALL", "rows": 2}}}}}}]}}}"#,
        );
        let access_of = |object: &str| {
            let (id, _) = plan.nodes().find(|(_, n)| n.object.as_deref() == Some(object)).unwrap();
            plan.access(id)
        };
        assert_eq!(access_of("<union1,2>"), Some(Access::InternalResult));
        assert_eq!(access_of("a"), Some(Access::FullScan));
        assert_eq!(access_of("d"), Some(Access::Lookup));
        assert_eq!(access_of("b"), Some(Access::IndexScan));
        assert_eq!(access_of("e"), Some(Access::UniqueLookup));
        assert_eq!(access_of("f"), Some(Access::Range));
        assert_eq!(access_of("g"), Some(Access::Constant));
        assert_eq!(access_of("<derived4>"), Some(Access::InternalResult));
        assert_eq!(access_of("h"), Some(Access::FullScan));
        let extras: Vec<(String, Vec<Extra>)> = plan
            .nodes()
            .filter(|(id, _)| !plan.extras(*id).is_empty())
            .map(|(id, n)| (n.operation.clone(), plan.extras(id)))
            .collect();
        let extras: Vec<(&str, &[Extra])> = extras.iter().map(|(o, e)| (o.as_str(), e.as_slice())).collect();
        assert_eq!(
            extras,
            [
                ("Union", &[Extra::TempTable][..]),
                ("Sort", &[Extra::Filesort]),
                ("Temporary table", &[Extra::TempTable]),
                ("Materialize", &[Extra::TempTable]),
                ("Materialize", &[Extra::TempTable]),
            ]
        );
    }

    fn postgres(json: &str) -> Plan {
        one_cell(Flavor::Postgres, Version::new(17, 0, 0), json)
    }

    #[test]
    fn postgres_scans_map_to_access_types_by_their_conditions() {
        let plan = postgres(
            r#"[{"Plan": {"Node Type": "Append", "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "o", "Plan Rows": 830, "Total Cost": 22},
              {"Node Type": "Seq Scan", "Parallel Aware": true, "Relation Name": "orders", "Plan Rows": 830, "Total Cost": 22},
              {"Node Type": "Index Scan", "Index Name": "orders_pkey", "Index Cond": "(order_id = od.order_id)", "Plan Rows": 1, "Total Cost": 0.3},
              {"Node Type": "Index Only Scan", "Index Name": "idx_c", "Index Cond": "(customer_id = ANY ('{ALFKI,ANATR}'::bpchar[]))", "Plan Rows": 9, "Total Cost": 1},
              {"Node Type": "Index Scan", "Index Name": "idx_date", "Scan Direction": "Backward", "Index Cond": "((order_date >= '1997-01-01'::date) AND (order_date < '1998-01-01'::date))", "Plan Rows": 9, "Total Cost": 1},
              {"Node Type": "Index Scan", "Index Name": "idx_name", "Index Cond": "((name ~>=~ 'Al'::text) AND (name ~<~ 'Am'::text))", "Plan Rows": 1, "Total Cost": 1},
              {"Node Type": "Index Scan", "Index Name": "orders_pkey", "Plan Rows": 830, "Total Cost": 40},
              {"Node Type": "Index Scan", "Index Name": "idx_doc", "Index Cond": "(doc @> '{\"a\": 1}'::jsonb)", "Plan Rows": 1, "Total Cost": 4},
              {"Node Type": "Index Scan", "Index Name": "idx_tags", "Index Cond": "(tags <@ '{a}'::text[])", "Plan Rows": 1, "Total Cost": 4},
              {"Node Type": "Index Scan", "Index Name": "idx_d", "Index Cond": "((d ->> 'k'::text) = 'v'::text)", "Plan Rows": 1, "Total Cost": 4},
              {"Node Type": "Index Scan", "Index Name": "idx_s", "Index Cond": "(s = '<b>'::text)", "Plan Rows": 1, "Total Cost": 4},
              {"Node Type": "Bitmap Heap Scan", "Relation Name": "orders", "Recheck Cond": "(customer_id = 'ALFKI'::bpchar)", "Plan Rows": 6, "Total Cost": 12,
               "Plans": [{"Node Type": "Bitmap Index Scan", "Index Name": "idx_c", "Index Cond": "(customer_id = 'ALFKI'::bpchar)", "Plan Rows": 6, "Total Cost": 4}]},
              {"Node Type": "Bitmap Heap Scan", "Relation Name": "orders", "Recheck Cond": "(freight > '50'::numeric)", "Plan Rows": 6, "Total Cost": 12,
               "Plans": [{"Node Type": "Bitmap Index Scan", "Index Name": "idx_f", "Index Cond": "(freight > '50'::numeric)", "Plan Rows": 6, "Total Cost": 4}]},
              {"Node Type": "Function Scan", "Function Name": "generate_series", "Plan Rows": 23, "Total Cost": 0.2},
              {"Node Type": "Values Scan", "Plan Rows": 2, "Total Cost": 0.1},
              {"Node Type": "Result", "Plan Rows": 1, "Total Cost": 0.01},
              {"Node Type": "Result", "Plan Rows": 1, "Total Cost": 22, "Plans": [
                {"Node Type": "Seq Scan", "Relation Name": "products", "Plan Rows": 77, "Total Cost": 2}]},
              {"Node Type": "CTE Scan", "CTE Name": "ranked", "Plan Rows": 77, "Total Cost": 2},
              {"Node Type": "WorkTable Scan", "CTE Name": "chain", "Plan Rows": 10, "Total Cost": 1},
              {"Node Type": "Subquery Scan", "Alias": "*SELECT* 1", "Plan Rows": 30, "Total Cost": 698},
              {"Node Type": "Hash Join", "Join Type": "Inner", "Hash Cond": "(a.x = b.x)", "Plan Rows": 1, "Total Cost": 3},
              {"Node Type": "Sort", "Plan Rows": 1, "Total Cost": 3}]}}]"#,
        );
        use Access::*;
        let got: Vec<Option<Access>> = plan.nodes().map(|(id, _)| plan.access(id)).collect();
        assert_eq!(
            got,
            [
                None,
                Some(FullScan),
                Some(FullScan),
                Some(Lookup),
                Some(Lookup),
                Some(Range),
                Some(Range),
                Some(IndexScan),
                Some(Lookup),
                Some(Lookup),
                Some(Lookup),
                Some(Lookup),
                Some(Lookup),
                Some(Lookup),
                Some(Range),
                Some(Range),
                Some(Constant),
                Some(Constant),
                Some(Constant),
                None,
                Some(FullScan),
                Some(InternalResult),
                Some(InternalResult),
                Some(InternalResult),
                None,
                None,
            ]
        );
        assert!(plan.nodes().all(|(id, _)| plan.extras(id).is_empty()), "PostgreSQL reports no side work this way");
    }

    #[test]
    fn sqlite_scans_and_searches_map_to_access_types() {
        let rows = [
            (2, 0, "MATERIALIZE ranked"),
            (3, 2, "SCAN orders"),
            (5, 0, "CO-ROUTINE t"),
            (6, 5, "SCAN order_details USING INDEX sqlite_autoindex_order_details_1"),
            (9, 0, "SCAN t"),
            (10, 0, "SCAN (subquery-7)"),
            (11, 0, "SCAN c USING COVERING INDEX idx_country"),
            (12, 0, "SEARCH o USING INTEGER PRIMARY KEY (rowid=?)"),
            (13, 0, "SEARCH o2 USING INTEGER PRIMARY KEY (rowid>? AND rowid<?)"),
            (14, 0, "SEARCH od USING INDEX idx_product (product_id=?)"),
            (15, 0, "SEARCH od2 USING COVERING INDEX idx_order (order_id>?)"),
            (16, 0, "SEARCH t2 USING INDEX i (a=? AND b>? AND b<?)"),
            (17, 0, "SEARCH e USING AUTOMATIC COVERING INDEX (reports_to=?)"),
            (18, 0, "SEARCH w USING PRIMARY KEY (a=?)"),
            (19, 0, "SCAN CONSTANT ROW"),
            (20, 0, "USE TEMP B-TREE FOR ORDER BY"),
            (21, 0, "SCAN r"),
        ];
        let result = ResultSet {
            columns: ["id", "parent", "notused", "detail"].iter().map(|c| ColumnMeta::new(*c, "")).collect(),
            rows: rows
                .iter()
                .map(|(id, parent, detail)| {
                    vec![Value::Int(*id), Value::Int(*parent), Value::Int(0), Value::text(*detail)].into()
                })
                .collect(),
            truncated: false,
        };
        let plan =
            crate::plan::parse(&ServerInfo::new(Flavor::Sqlite, Version::new(3, 53, 2)), &result).unwrap().unwrap();
        use Access::*;
        let got: Vec<(&str, Option<Access>)> =
            plan.nodes().skip(1).map(|(id, n)| (n.attributes[0].value.as_str(), plan.access(id))).collect();
        assert_eq!(
            got,
            [
                ("MATERIALIZE ranked", None),
                ("SCAN orders", Some(FullScan)),
                ("CO-ROUTINE t", None),
                ("SCAN order_details USING INDEX sqlite_autoindex_order_details_1", Some(IndexScan)),
                ("SCAN t", Some(InternalResult)),
                ("SCAN (subquery-7)", Some(InternalResult)),
                ("SCAN c USING COVERING INDEX idx_country", Some(IndexScan)),
                ("SEARCH o USING INTEGER PRIMARY KEY (rowid=?)", Some(UniqueLookup)),
                ("SEARCH o2 USING INTEGER PRIMARY KEY (rowid>? AND rowid<?)", Some(Range)),
                ("SEARCH od USING INDEX idx_product (product_id=?)", Some(Lookup)),
                ("SEARCH od2 USING COVERING INDEX idx_order (order_id>?)", Some(Range)),
                ("SEARCH t2 USING INDEX i (a=? AND b>? AND b<?)", Some(Range)),
                ("SEARCH e USING AUTOMATIC COVERING INDEX (reports_to=?)", Some(Lookup)),
                ("SEARCH w USING PRIMARY KEY (a=?)", Some(Lookup)),
                ("SCAN CONSTANT ROW", Some(Constant)),
                ("USE TEMP B-TREE FOR ORDER BY", None),
                // A CTE read under an alias reads like a table.
                ("SCAN r", Some(FullScan)),
            ]
        );
        let materialize = plan.nodes().find(|(_, n)| n.operation == "Materialize").unwrap().0;
        assert_eq!(plan.extras(materialize), [Extra::TempTable]);
    }

    #[test]
    fn joins_are_never_table_access() {
        let plan = mysql_tree("-> Nested loop inner join\n    -> Table scan on c  (cost=0.5 rows=91)\n");
        assert!(plan.node(NodeId::ROOT).is_join());
        assert_eq!(plan.access(NodeId::ROOT), None);
        for op in ["Nested Loop", "Hash Join", "Merge Join", "Left hash join", "Nested loop semijoin"] {
            assert!(PlanNode::new(op).is_join(), "{op}");
        }
    }
}
