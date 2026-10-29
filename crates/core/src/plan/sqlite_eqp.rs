//! SQLite's `EXPLAIN QUERY PLAN`: one row per step, with columns `id`, `parent`, `notused` and
//! `detail`. A row's `parent` is the `id` of an earlier row, or 0 at the top.
//!
//! ```text
//! id  parent  notused  detail
//! 4   0       0        CO-ROUTINE t
//! 6   4       0        SCAN od USING INDEX sqlite_autoindex_order_details_1
//! 22  0       0        SEARCH o USING INTEGER PRIMARY KEY (rowid=?)
//! ```
//!
//! SQLite reports no estimates and never measures, so nodes carry neither.

use std::collections::HashMap;

use super::{Attribute, NodeId, ParseError, Plan, PlanFormat, PlanNode, raw_text};
use crate::model::ResultSet;
use crate::server::{Flavor, ServerInfo};
use crate::value::Value;

const COLUMNS: [&str; 4] = ["id", "parent", "notused", "detail"];

pub(super) fn parse(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    let names = result.columns.iter().map(|c| c.name.as_str());
    if server.flavor != Flavor::Sqlite || !names.eq(COLUMNS) {
        return None;
    }
    Some(parse_rows(result))
}

struct Row {
    id: i64,
    parent: i64,
    detail: String,
}

fn parse_rows(result: &ResultSet) -> Result<Plan, ParseError> {
    let rows = result
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let int = |col: usize| match &row[col] {
                Value::Int(n) => Some(*n),
                other => other.to_text()?.trim().parse().ok(),
            };
            let (Some(id), Some(parent)) = (int(0), int(1)) else {
                return Err(ParseError {
                    format: PlanFormat::SqliteQueryPlan,
                    message: format!("row {} has no numeric id and parent", i + 1),
                });
            };
            Ok(Row { id, parent, detail: row[3].to_text().unwrap_or_default() })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let raw = raw_text(result);
    let top = rows.iter().filter(|r| r.parent == 0).count();
    let mut rows = rows.iter();
    let mut ids: HashMap<i64, NodeId> = HashMap::new();
    let mut plan = match rows.as_slice() {
        [first, ..] if top == 1 && first.parent == 0 => {
            rows.next();
            ids.insert(first.id, NodeId::ROOT);
            Plan::new(PlanFormat::SqliteQueryPlan, node(&first.detail), raw)
        }
        _ => Plan::new(PlanFormat::SqliteQueryPlan, PlanNode::new("Query"), raw),
    };
    for row in rows {
        // SQLite lists a parent before its children; an unknown parent hangs from the root.
        let parent = ids.get(&row.parent).copied().unwrap_or(NodeId::ROOT);
        let id = plan.add_child(parent, node(&row.detail));
        ids.insert(row.id, id);
    }
    Ok(plan)
}

/// How the text after a step's keywords reads.
#[derive(Clone, Copy)]
enum Tail {
    /// Nothing follows: `COMPOUND QUERY`.
    None,
    /// A table or subquery, then perhaps an index and a condition: `SEARCH o USING INDEX i (a=?)`.
    Access,
    /// A table, CTE or subquery, then perhaps a condition: `BLOOM FILTER ON e (reports_to=?)`.
    Object,
    /// The rest is the condition: `USE TEMP B-TREE FOR ORDER BY`, `MERGE (UNION ALL)`.
    Condition,
    /// The rest names this step: `CORRELATED SCALAR SUBQUERY 3`.
    Name,
}

/// The `detail` wordings SQLite prints. A `Tail::None` entry matches the whole text, the others
/// match a prefix; the first match wins.
const STEPS: &[(&str, &str, Tail)] = &[
    ("SCAN CONSTANT ROW", "Scan constant row", Tail::None),
    ("SCAN ", "Scan", Tail::Access),
    ("SEARCH ", "Search", Tail::Access),
    ("USE TEMP B-TREE FOR ", "Temp B-tree", Tail::Condition),
    ("CORRELATED SCALAR SUBQUERY", "Correlated scalar subquery", Tail::Name),
    ("SCALAR SUBQUERY", "Scalar subquery", Tail::Name),
    ("CORRELATED LIST SUBQUERY", "Correlated list subquery", Tail::Name),
    ("LIST SUBQUERY", "List subquery", Tail::Name),
    ("MATERIALIZE ", "Materialize", Tail::Object),
    ("CO-ROUTINE ", "Co-routine", Tail::Object),
    ("BLOOM FILTER ON ", "Bloom filter", Tail::Object),
    ("CREATE AUTOMATIC INDEX ON ", "Create automatic index", Tail::Object),
    ("CREATE BLOOM FILTER", "Create bloom filter", Tail::None),
    ("COMPOUND QUERY", "Compound query", Tail::None),
    ("LEFT-MOST SUBQUERY", "Left-most subquery", Tail::None),
    ("UNION ALL", "Union all", Tail::None),
    ("UNION USING TEMP B-TREE", "Union using temp B-tree", Tail::None),
    ("INTERSECT USING TEMP B-TREE", "Intersect using temp B-tree", Tail::None),
    ("EXCEPT USING TEMP B-TREE", "Except using temp B-tree", Tail::None),
    ("MERGE ", "Merge", Tail::Condition),
    ("LEFT", "Left", Tail::None),
    ("RIGHT", "Right", Tail::None),
    ("MULTI-INDEX OR", "Multi-index OR", Tail::None),
    ("INDEX ", "Index", Tail::Name),
    ("SETUP", "Setup", Tail::None),
    ("RECURSIVE STEP", "Recursive step", Tail::None),
];

pub(super) fn node(detail: &str) -> PlanNode {
    let mut node = describe(detail.trim());
    node.attributes.insert(0, Attribute::new("detail", detail));
    node
}

/// A `SCAN` or `SEARCH` of a table, index or subquery: a loop, the only step SQLite estimates
/// rows for.
pub(super) fn is_loop(detail: &str) -> bool {
    matches!(step(detail.trim()), Some((_, Tail::Access, _)))
}

/// The first entry of [`STEPS`] that `detail` matches, with the text after its keywords.
fn step(detail: &str) -> Option<(&'static str, Tail, &str)> {
    STEPS.iter().find_map(|(keywords, operation, tail)| {
        let rest = match tail {
            Tail::None if detail == *keywords => "",
            Tail::None => return None,
            _ => detail.strip_prefix(keywords)?.trim(),
        };
        Some((*operation, *tail, rest))
    })
}

/// Splits a `detail` text into operation, object, index and condition. Wording it doesn't know
/// becomes the operation as it stands.
fn describe(detail: &str) -> PlanNode {
    let Some((operation, tail, rest)) = step(detail) else { return PlanNode::new(detail) };
    match tail {
        Tail::None => PlanNode::new(operation),
        Tail::Access => access(operation, rest),
        Tail::Object => {
            let (object, rest) = take_name(rest);
            let mut node = PlanNode::new(operation);
            node.object = Some(object.to_string());
            node.condition = condition(rest);
            node
        }
        Tail::Condition => {
            let mut node = PlanNode::new(operation);
            node.condition = condition(rest);
            node
        }
        Tail::Name if rest.is_empty() => PlanNode::new(operation),
        Tail::Name => PlanNode::new(format!("{operation} {rest}")),
    }
}

/// `od2 USING COVERING INDEX sqlite_autoindex_order_details_1 (order_id=?) LEFT-JOIN`
fn access(operation: &str, text: &str) -> PlanNode {
    let mut node = PlanNode::new(operation);
    let (object, mut rest) = take_name(text);
    node.object = Some(object.to_string());
    for join in ["LEFT-JOIN", "RIGHT-JOIN"] {
        if let Some(r) = rest.strip_suffix(join) {
            node.attributes.push(Attribute::new("join", join));
            rest = r.trim_end();
        }
    }
    let (using, cond) = match rest.rfind(" (").filter(|_| rest.ends_with(')')) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    node.condition = condition(cond);
    // `INDEX i` names the index; `COVERING INDEX i`, `INTEGER PRIMARY KEY` and
    // `AUTOMATIC COVERING INDEX` keep SQLite's words, which say how the index is used.
    if let Some(using) = using.trim().strip_prefix("USING ") {
        let index = using.strip_prefix("INDEX ").unwrap_or(using);
        node.index = Some(index.to_string());
    } else if !using.trim().is_empty() {
        node.attributes.push(Attribute::new("access", using.trim()));
    }
    node
}

/// A name up to the first space or parenthesis, or a parenthesized one such as `(subquery-6)`.
fn take_name(text: &str) -> (&str, &str) {
    let end = if text.starts_with('(') { text.find(')').map(|i| i + 1) } else { None }
        .unwrap_or_else(|| text.find([' ', '(']).unwrap_or(text.len()));
    (&text[..end], text[end..].trim())
}

/// `(rowid=?)` → `rowid=?`; text without parentheses stays as it is.
fn condition(text: &str) -> Option<String> {
    let text = text.trim();
    let text = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')).unwrap_or(text);
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;
    use crate::server::Version;

    fn result(rows: &[(i64, i64, &str)]) -> ResultSet {
        ResultSet {
            columns: COLUMNS.iter().map(|c| ColumnMeta::new(*c, "")).collect(),
            rows: rows
                .iter()
                .map(|(id, parent, detail)| {
                    vec![Value::Int(*id), Value::Int(*parent), Value::Int(0), Value::text(*detail)].into()
                })
                .collect(),
            truncated: false,
        }
    }

    fn sqlite() -> ServerInfo {
        ServerInfo::new(Flavor::Sqlite, Version::new(3, 53, 2))
    }

    fn parts(n: &PlanNode) -> (&str, Option<&str>, Option<&str>, Option<&str>) {
        (n.operation.as_str(), n.object.as_deref(), n.index.as_deref(), n.condition.as_deref())
    }

    #[test]
    fn splits_details_into_parts() {
        for (detail, expected) in [
            ("SCAN o", ("Scan", Some("o"), None, None)),
            (
                "SEARCH o USING INDEX orders_customer_id_idx (customer_id=?)",
                ("Search", Some("o"), Some("orders_customer_id_idx"), Some("customer_id=?")),
            ),
            (
                "SEARCH od2 USING COVERING INDEX sqlite_autoindex_order_details_1 (order_id=?)",
                ("Search", Some("od2"), Some("COVERING INDEX sqlite_autoindex_order_details_1"), Some("order_id=?")),
            ),
            (
                "SCAN od USING INDEX sqlite_autoindex_order_details_1",
                ("Scan", Some("od"), Some("sqlite_autoindex_order_details_1"), None),
            ),
            (
                "SEARCH o USING INTEGER PRIMARY KEY (rowid=?)",
                ("Search", Some("o"), Some("INTEGER PRIMARY KEY"), Some("rowid=?")),
            ),
            (
                "SEARCH e USING AUTOMATIC COVERING INDEX (reports_to=?)",
                ("Search", Some("e"), Some("AUTOMATIC COVERING INDEX"), Some("reports_to=?")),
            ),
            ("SCAN (subquery-7)", ("Scan", Some("(subquery-7)"), None, None)),
            ("SCAN CONSTANT ROW", ("Scan constant row", None, None, None)),
            ("USE TEMP B-TREE FOR ORDER BY", ("Temp B-tree", None, None, Some("ORDER BY"))),
            ("USE TEMP B-TREE FOR count(DISTINCT)", ("Temp B-tree", None, None, Some("count(DISTINCT)"))),
            ("CORRELATED SCALAR SUBQUERY 3", ("Correlated scalar subquery 3", None, None, None)),
            ("MATERIALIZE t", ("Materialize", Some("t"), None, None)),
            ("CO-ROUTINE t", ("Co-routine", Some("t"), None, None)),
            ("CO-ROUTINE (subquery-6)", ("Co-routine", Some("(subquery-6)"), None, None)),
            ("COMPOUND QUERY", ("Compound query", None, None, None)),
            ("UNION USING TEMP B-TREE", ("Union using temp B-tree", None, None, None)),
            ("MERGE (UNION ALL)", ("Merge", None, None, Some("UNION ALL"))),
            ("LEFT", ("Left", None, None, None)),
            ("INDEX 1", ("Index 1", None, None, None)),
            ("BLOOM FILTER ON e (reports_to=?)", ("Bloom filter", Some("e"), None, Some("reports_to=?"))),
            (
                "CREATE AUTOMATIC INDEX ON tree(id, depth)",
                ("Create automatic index", Some("tree"), None, Some("id, depth")),
            ),
            ("SETUP", ("Setup", None, None, None)),
            ("SOMETHING NEWER", ("SOMETHING NEWER", None, None, None)),
            ("", ("", None, None, None)),
        ] {
            let n = node(detail);
            assert_eq!(parts(&n), expected, "{detail}");
            assert_eq!(n.attributes[0], Attribute::new("detail", detail), "{detail}");
            assert!(n.estimate.is_none() && n.actual.is_none(), "{detail}");
        }
    }

    #[test]
    fn outer_join_marks_stay_off_the_condition() {
        let n = node("SEARCH m USING INTEGER PRIMARY KEY (rowid=?) LEFT-JOIN");
        assert_eq!(parts(&n), ("Search", Some("m"), Some("INTEGER PRIMARY KEY"), Some("rowid=?")));
        assert!(n.attributes.contains(&Attribute::new("join", "LEFT-JOIN")));
        let n = node("SCAN t RIGHT-JOIN");
        assert_eq!(parts(&n), ("Scan", Some("t"), None, None));
        assert!(n.attributes.contains(&Attribute::new("join", "RIGHT-JOIN")));
    }

    #[test]
    fn builds_the_tree_from_parent_ids() {
        let rows = [
            (1, 0, "COMPOUND QUERY"),
            (2, 1, "LEFT-MOST SUBQUERY"),
            (5, 2, "SCAN a"),
            (9, 1, "UNION ALL"),
            (12, 9, "SCAN b"),
            (13, 9, "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"),
        ];
        let plan = parse(&sqlite(), &result(&rows)).unwrap().unwrap();
        let shape: Vec<(usize, &str)> = plan.nodes().map(|(id, n)| (plan.depth(id), n.operation.as_str())).collect();
        assert_eq!(
            shape,
            [
                (0, "Compound query"),
                (1, "Left-most subquery"),
                (2, "Scan"),
                (1, "Union all"),
                (2, "Scan"),
                (2, "Search")
            ]
        );
        assert!(!plan.analyzed());
        assert_eq!(plan.total_ms(), None);
    }

    #[test]
    fn unknown_parents_hang_from_the_root() {
        let plan =
            parse(&sqlite(), &result(&[(2, 0, "SCAN t"), (3, 0, "SCAN u"), (7, 99, "SCAN v")])).unwrap().unwrap();
        assert_eq!(plan.children(NodeId::ROOT).len(), 3);
    }

    #[test]
    fn only_query_plan_rows_are_claimed() {
        assert!(matches!(parse(&sqlite(), &result(&[(2, 0, "SCAN t")])), Some(Ok(_))));
        let mysql = ServerInfo::new(Flavor::MySql, Version::new(8, 4, 0));
        assert!(parse(&mysql, &result(&[(2, 0, "SCAN t")])).is_none());
        let bytecode = ResultSet {
            columns: ["addr", "opcode", "p1", "p2", "p3", "p4", "p5", "comment"]
                .iter()
                .map(|c| ColumnMeta::new(*c, ""))
                .collect(),
            rows: vec![],
            truncated: false,
        };
        assert!(parse(&sqlite(), &bytecode).is_none());
    }

    #[test]
    fn rows_without_ids_are_an_error() {
        let mut rs = result(&[(2, 0, "SCAN t")]);
        rs.rows[0] = vec![Value::text("x"), Value::Int(0), Value::Int(0), Value::text("SCAN t")].into();
        assert!(matches!(parse(&sqlite(), &rs), Some(Err(_))));
    }
}
