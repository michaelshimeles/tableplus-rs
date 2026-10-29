//! MySQL's JSON plans, one text cell holding a JSON document. They come in two unrelated shapes.
//!
//! Version 1, from `EXPLAIN FORMAT=JSON` on 5.6 to 8.4, nests query blocks, the operations
//! around them and the tables they read. It has estimates only, and costs from 5.7 on:
//!
//! ```json
//! {"query_block": {"select_id": 1, "nested_loop": [{"table": {"table_name": "o", "access_type": "ALL"}}]}}
//! ```
//!
//! Version 2, with `explain_json_format_version = 2` (the 9.x default), nests the same iterators
//! as the TREE format through `inputs`, with actuals under `EXPLAIN ANALYZE`:
//!
//! ```json
//! {"operation": "Filter: (o.freight > 10)", "estimated_rows": 277.0, "inputs": [{"operation": "Table scan on o"}]}
//! ```

use serde_json::{Map, Value};

use super::mysql_tree::describe;
use super::{Actual, ActualTime, Attribute, Cost, Estimate, NodeId, ParseError, Plan, PlanFormat, PlanNode};
use crate::model::ResultSet;
use crate::server::{Flavor, ServerInfo};

type Object = Map<String, Value>;

/// The JSON object in a MySQL result's single text cell. Malformed JSON is reported as a
/// version 1 error, since neither shape can claim it.
fn document<'a>(server: &ServerInfo, result: &'a ResultSet) -> Option<(&'a str, Result<Object, ParseError>)> {
    if server.flavor != Flavor::MySql || result.columns.len() != 1 || result.rows.len() != 1 {
        return None;
    }
    let crate::value::Value::Text(text) = &result.rows[0][0] else { return None };
    if !text.trim_start().starts_with('{') {
        return None;
    }
    let parsed = serde_json::from_str::<Object>(text)
        .map_err(|e| ParseError { format: PlanFormat::MySqlJson, message: e.to_string() });
    Some((text, parsed))
}

pub(super) fn parse_v1(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    let (text, doc) = document(server, result)?;
    match doc {
        Ok(doc) => doc.get("query_block").and_then(Value::as_object).map(|block| Ok(v1::plan(text, block))),
        Err(e) => Some(Err(e)),
    }
}

pub(super) fn parse_v2(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    let (text, Ok(doc)) = document(server, result)? else { return None };
    let top = doc.get("query_plan").and_then(Value::as_object).unwrap_or(&doc);
    top.contains_key("operation").then(|| Ok(v2::plan(text, &doc)))
}

fn text(obj: &Object, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

/// A number, or a number in a string (version 1 prints costs as `"6438.75"`).
fn number(obj: &Object, key: &str) -> Option<f64> {
    match obj.get(key)? {
        Value::String(s) => s.parse().ok(),
        v => v.as_f64(),
    }
}

/// Appends `obj`'s values in the server's order, except the keys `skip` claims. A nested
/// object (`cost_info`) adds its own values, and a list of values becomes one attribute.
fn add_attributes(node: &mut PlanNode, obj: &Object, skip: &dyn Fn(&str, &Value) -> bool) {
    for (key, value) in obj {
        if skip(key, value) {
            continue;
        }
        match value {
            Value::Object(inner) => add_attributes(node, inner, skip),
            Value::Array(items) if items.iter().all(|v| !v.is_object() && !v.is_array()) => {
                let items: Vec<String> = items.iter().map(scalar).collect();
                node.attributes.push(Attribute::new(key, items.join(", ")));
            }
            Value::Array(_) => node.attributes.push(Attribute::new(key, value.to_string())),
            _ => node.attributes.push(Attribute::new(key, scalar(value))),
        }
    }
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

mod v1 {
    use super::*;

    /// What a key nested in a query block, operation or table adds to the tree.
    #[derive(Clone, Copy)]
    enum Child {
        /// `query_block`: one query block.
        Block,
        /// An object or list of objects that each wrap a `query_block`, with a note on the
        /// subquery's role.
        Subqueries(Option<&'static str>),
        /// An operation around its input, such as sorting.
        Operation(&'static str),
        /// A list of `{"table": …}` joined in order.
        NestedLoop,
        Table,
    }

    const CHILDREN: &[(&str, Child)] = &[
        ("query_block", Child::Block),
        ("query_specifications", Child::Subqueries(None)),
        ("materialized_from_subquery", Child::Subqueries(Some("materialized"))),
        ("attached_subqueries", Child::Subqueries(Some("subquery in condition"))),
        ("select_list_subqueries", Child::Subqueries(Some("subquery in projection"))),
        ("having_subqueries", Child::Subqueries(Some("subquery in HAVING"))),
        ("order_by_subqueries", Child::Subqueries(Some("subquery in ORDER BY"))),
        ("group_by_subqueries", Child::Subqueries(Some("subquery in GROUP BY"))),
        ("update_value_subqueries", Child::Subqueries(Some("subquery in SET"))),
        ("optimized_away_subqueries", Child::Subqueries(Some("optimized away"))),
        ("union_result", Child::Operation("Union result")),
        ("ordering_operation", Child::Operation("Ordering")),
        ("grouping_operation", Child::Operation("Grouping")),
        ("duplicates_removal", Child::Operation("Duplicates removal")),
        ("windowing", Child::Operation("Windowing")),
        ("buffer_result", Child::Operation("Buffer result")),
        ("nested_loop", Child::NestedLoop),
        ("table", Child::Table),
    ];

    /// The operation for each `access_type`, worded like MySQL Workbench's visual explain.
    const ACCESS: &[(&str, &str)] = &[
        ("system", "Single row (system table)"),
        ("const", "Single row (constant)"),
        ("eq_ref", "Unique key lookup"),
        ("ref", "Non-unique key lookup"),
        ("fulltext", "Full-text index search"),
        ("ref_or_null", "Key lookup and NULL values"),
        ("index_merge", "Index merge"),
        ("unique_subquery", "Unique key lookup into a subquery's table"),
        ("index_subquery", "Non-unique key lookup into a subquery's table"),
        ("range", "Index range scan"),
        ("index", "Full index scan"),
        ("ALL", "Full table scan"),
    ];

    fn child(key: &str) -> Option<Child> {
        CHILDREN.iter().find(|(k, _)| *k == key).map(|(_, c)| *c)
    }

    fn is_child(key: &str, _: &Value) -> bool {
        child(key).is_some()
    }

    pub(super) fn plan(raw: &str, block: &Object) -> Plan {
        let mut plan = Plan::new(PlanFormat::MySqlJson, block_node(block, None, None), raw);
        add_children(&mut plan, NodeId::ROOT, block);
        plan
    }

    /// Adds a node for each key of `obj` that [`CHILDREN`] names, in the server's order.
    fn add_children(plan: &mut Plan, parent: NodeId, obj: &Object) {
        for (key, value) in obj {
            let Some(kind) = child(key) else { continue };
            match (kind, value) {
                (Child::Block, Value::Object(block)) => {
                    let id = plan.add_child(parent, block_node(block, None, None));
                    add_children(plan, id, block);
                }
                (Child::Subqueries(note), Value::Object(wrapper)) => add_subquery(plan, parent, wrapper, note),
                (Child::Subqueries(note), Value::Array(wrappers)) => {
                    for wrapper in wrappers.iter().filter_map(Value::as_object) {
                        add_subquery(plan, parent, wrapper, note);
                    }
                }
                (Child::Operation(label), Value::Object(op)) => {
                    let mut node = PlanNode::new(label);
                    node.object = text(op, "table_name");
                    node.access_type = text(op, "access_type");
                    add_attributes(&mut node, op, &is_child);
                    let id = plan.add_child(parent, node);
                    add_children(plan, id, op);
                }
                (Child::NestedLoop, Value::Array(steps)) => {
                    let steps: Vec<&Object> = steps.iter().filter_map(Value::as_object).collect();
                    let id = plan.add_child(parent, nested_loop_node(&steps));
                    for step in steps {
                        add_children(plan, id, step);
                    }
                }
                (Child::Table, Value::Object(table)) => {
                    let id = plan.add_child(parent, table_node(table));
                    add_children(plan, id, table);
                }
                _ => {}
            }
        }
    }

    fn add_subquery(plan: &mut Plan, parent: NodeId, wrapper: &Object, note: Option<&str>) {
        let Some(block) = wrapper.get("query_block").and_then(Value::as_object) else { return };
        let id = plan.add_child(parent, block_node(block, Some(wrapper), note));
        add_children(plan, id, block);
    }

    /// `Select #4`, noted with its role (`subquery in condition, dependent`) and any message
    /// (`No tables used`).
    fn block_node(block: &Object, wrapper: Option<&Object>, note: Option<&str>) -> PlanNode {
        let mut node = PlanNode::new(match block.get("select_id").and_then(Value::as_u64) {
            Some(id) => format!("Select #{id}"),
            None => "Query".to_string(),
        });
        let dependent = wrapper.and_then(|w| w.get("dependent")).and_then(Value::as_bool) == Some(true);
        let notes: Vec<String> =
            [note.map(str::to_string), dependent.then(|| "dependent".into()), text(block, "message")]
                .into_iter()
                .flatten()
                .collect();
        node.condition = (!notes.is_empty()).then(|| notes.join(", "));
        if let Some(wrapper) = wrapper {
            add_attributes(&mut node, wrapper, &is_child);
        }
        add_attributes(&mut node, block, &is_child);
        node
    }

    /// A join's output: the rows and cumulative cost after its last table.
    fn nested_loop_node(steps: &[&Object]) -> PlanNode {
        let mut node = PlanNode::new("Nested loop");
        let last = steps.last().and_then(|s| s.get("table")?.as_object());
        node.estimate = last.and_then(|t| {
            let cost = t.get("cost_info").and_then(Value::as_object).and_then(|c| number(c, "prefix_cost"));
            Some(Estimate {
                rows: number(t, "rows_produced_per_join")?,
                cost: cost.map(|total| Cost { startup: None, total }),
            })
        });
        node
    }

    /// A table read. Its estimate is the rows it examines per scan, at its own read and
    /// evaluation cost; a table's `prefix_cost` belongs to the join it ends.
    fn table_node(table: &Object) -> PlanNode {
        let access = text(table, "access_type");
        let operation = match &access {
            Some(a) => ACCESS.iter().find(|(k, _)| k == a).map_or("Table access", |(_, label)| label),
            None => "Table",
        };
        let mut node = PlanNode::new(operation);
        node.object = text(table, "table_name");
        node.index = text(table, "key");
        node.condition = text(table, "attached_condition");
        node.access_type = access;
        let rows = number(table, "rows_examined_per_scan").or_else(|| number(table, "rows"));
        let cost = table
            .get("cost_info")
            .and_then(Value::as_object)
            .and_then(|c| Some(Cost { startup: None, total: number(c, "read_cost")? + number(c, "eval_cost")? }));
        node.estimate = rows.map(|rows| Estimate { rows, cost });
        add_attributes(&mut node, table, &is_child);
        node
    }
}

mod v2 {
    use super::*;

    /// Keys of the whole statement rather than of its top step.
    const STATEMENT_KEYS: &[&str] = &["query", "query_type", "json_schema_version"];

    /// Children are every list of steps: `inputs`, `inputs_from_select_list` and the like.
    fn is_steps(value: &Value) -> bool {
        value.as_array().is_some_and(|items| items.iter().any(|v| v.get("operation").is_some()))
    }

    fn steps(obj: &Object) -> impl Iterator<Item = &Object> {
        obj.values()
            .filter(|v| is_steps(v))
            .flat_map(|v| v.as_array().into_iter().flatten())
            .filter_map(Value::as_object)
    }

    pub(super) fn plan(raw: &str, doc: &Object) -> Plan {
        let top = doc.get("query_plan").and_then(Value::as_object).unwrap_or(doc);
        let (mut plan, id) = match heading(top) {
            Some(heading) => {
                let mut plan = Plan::new(PlanFormat::MySqlJsonV2, heading, raw);
                let id = plan.add_child(NodeId::ROOT, node(top));
                (plan, id)
            }
            None => (Plan::new(PlanFormat::MySqlJsonV2, node(top), raw), NodeId::ROOT),
        };
        for key in STATEMENT_KEYS {
            if let Some(value) = doc.get(*key) {
                plan.attributes.push(Attribute::new(*key, scalar(value)));
            }
        }
        add_steps(&mut plan, id, top);
        plan
    }

    fn add_steps(plan: &mut Plan, parent: NodeId, obj: &Object) {
        for step in steps(obj) {
            let under = match heading(step) {
                Some(heading) => plan.add_child(parent, heading),
                None => parent,
            };
            let id = plan.add_child(under, node(step));
            add_steps(plan, id, step);
        }
    }

    /// TREE prints a step's `heading` (`Hash`, `Select #2 (subquery in projection)`) as a line
    /// of its own above it; it becomes a node the same way.
    fn heading(step: &Object) -> Option<PlanNode> {
        let heading = step.get("heading")?.as_str()?;
        let mut node = describe(heading);
        node.attributes.push(Attribute::new("heading", heading));
        Some(node)
    }

    /// The operation text is the TREE line, so it splits the same way; the structured keys
    /// then replace what the split found.
    fn node(step: &Object) -> PlanNode {
        let mut node = describe(step.get("operation").and_then(Value::as_str).unwrap_or(""));
        if let Some(object) = text(step, "alias").or_else(|| text(step, "table_name")) {
            node.object = Some(object);
        }
        if let Some(index) = text(step, "index_name") {
            node.index = Some(index);
        }
        if let Some(condition) = text(step, "condition").or_else(|| text(step, "lookup_condition")) {
            node.condition = Some(condition);
        }
        node.access_type = text(step, "access_type");
        node.estimate = number(step, "estimated_rows").map(|rows| Estimate {
            rows,
            cost: number(step, "estimated_total_cost")
                .map(|total| Cost { startup: number(step, "estimated_first_row_cost"), total }),
        });
        node.actual = step.get("actual_loops").and_then(Value::as_u64).map(|loops| Actual {
            rows: number(step, "actual_rows"),
            loops,
            time: number(step, "actual_last_row_ms")
                .map(|last_row_ms| ActualTime { first_row_ms: number(step, "actual_first_row_ms"), last_row_ms }),
        });
        add_attributes(&mut node, step, &|key, value| {
            key == "heading" || STATEMENT_KEYS.contains(&key) || is_steps(value)
        });
        node
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;
    use crate::server::Version;

    fn parse(server: ServerInfo, json: &str) -> Option<Result<Plan, ParseError>> {
        let result = ResultSet {
            columns: vec![ColumnMeta::new("EXPLAIN", "VARCHAR")],
            rows: vec![vec![crate::value::Value::text(json)].into()],
            truncated: false,
        };
        super::super::parse(&server, &result)
    }

    const MYSQL_57: ServerInfo = ServerInfo::new(Flavor::MySql, Version::new(5, 7, 44));
    const MYSQL_97: ServerInfo = ServerInfo::new(Flavor::MySql, Version::new(9, 7, 2));

    fn ops(plan: &Plan) -> Vec<(usize, &str)> {
        plan.nodes().map(|(id, n)| (plan.depth(id), n.operation.as_str())).collect()
    }

    #[test]
    fn version_1_nests_blocks_operations_and_tables() {
        let plan = parse(
            MYSQL_57,
            r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "12.5"},
                "ordering_operation": {"using_filesort": true,
                  "nested_loop": [
                    {"table": {"table_name": "o", "access_type": "ALL", "rows_examined_per_scan": 830,
                      "rows_produced_per_join": 277, "cost_info": {"read_cost": "2.5", "eval_cost": "1.5", "prefix_cost": "4.00"},
                      "attached_condition": "(o.freight > 10)",
                      "attached_subqueries": [{"dependent": true, "cacheable": false,
                        "query_block": {"select_id": 2, "table": {"table_name": "s", "access_type": "eq_ref", "key": "PRIMARY", "rows_examined_per_scan": 1}}}]}},
                    {"table": {"table_name": "c", "access_type": "const", "key": "PRIMARY", "rows_examined_per_scan": 1,
                      "rows_produced_per_join": 277, "cost_info": {"read_cost": "0.5", "eval_cost": "0.5", "prefix_cost": "5.00"}}}
                  ]}}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.format, PlanFormat::MySqlJson);
        assert_eq!(
            ops(&plan),
            [
                (0, "Select #1"),
                (1, "Ordering"),
                (2, "Nested loop"),
                (3, "Full table scan"),
                (4, "Select #2"),
                (5, "Unique key lookup"),
                (3, "Single row (constant)"),
            ]
        );
        let scan = plan.node(NodeId(3));
        assert_eq!((scan.object.as_deref(), scan.access_type.as_deref()), (Some("o"), Some("ALL")));
        assert_eq!(scan.condition.as_deref(), Some("(o.freight > 10)"));
        assert_eq!(scan.estimate, Some(Estimate { rows: 830.0, cost: Some(Cost { startup: None, total: 4.0 }) }));
        let keys: Vec<&str> = scan.attributes.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "table_name",
                "access_type",
                "rows_examined_per_scan",
                "rows_produced_per_join",
                "read_cost",
                "eval_cost",
                "prefix_cost",
                "attached_condition"
            ]
        );
        let join = plan.node(NodeId(2));
        assert_eq!(join.estimate, Some(Estimate { rows: 277.0, cost: Some(Cost { startup: None, total: 5.0 }) }));
        let sub = plan.node(NodeId(4));
        assert_eq!(sub.condition.as_deref(), Some("subquery in condition, dependent"));
        assert_eq!(plan.node(NodeId(5)).index.as_deref(), Some("PRIMARY"));
        assert!(!plan.analyzed());
    }

    #[test]
    fn version_1_without_costs_still_estimates_rows() {
        let plan = parse(
            MYSQL_57,
            r#"{"query_block": {"select_id": 1, "table": {"table_name": "t", "access_type": "range", "rows": 12}}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.node(NodeId(1)).estimate, Some(Estimate { rows: 12.0, cost: None }));
        assert_eq!(plan.node(NodeId(1)).operation, "Index range scan");
    }

    #[test]
    fn version_2_follows_inputs_and_headings() {
        let plan = parse(
            MYSQL_97,
            r#"{"query": "select …", "query_plan": {
                "operation": "Inner hash join (c.id = o.customer_id)", "access_type": "join",
                "estimated_rows": 10.0, "estimated_total_cost": 9.5, "actual_rows": 12.0, "actual_loops": 1,
                "actual_first_row_ms": 0.5, "actual_last_row_ms": 1.25,
                "inputs": [
                  {"operation": "Table scan on o", "alias": "o", "table_name": "orders", "access_type": "table",
                   "estimated_rows": 830.0, "estimated_total_cost": 4.0, "actual_rows": 830.0, "actual_loops": 1},
                  {"heading": "Hash", "operation": "Index lookup on c using PRIMARY (id=o.customer_id)", "alias": "c",
                   "index_name": "PRIMARY", "lookup_condition": "id=o.customer_id", "access_type": "index", "actual_loops": 0}
                ],
                "inputs_from_select_list": [
                  {"heading": "Select #2 (subquery in projection; dependent)", "operation": "Aggregate: max(o2.d)", "access_type": "aggregate"}
                ]}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.format, PlanFormat::MySqlJsonV2);
        assert_eq!(
            ops(&plan),
            [
                (0, "Inner hash join"),
                (1, "Table scan"),
                (1, "Hash"),
                (2, "Index lookup"),
                (1, "Select #2"),
                (2, "Aggregate"),
            ]
        );
        let root = plan.node(NodeId::ROOT);
        assert_eq!(root.condition.as_deref(), Some("c.id = o.customer_id"));
        assert_eq!(root.access_type.as_deref(), Some("join"));
        assert_eq!(
            root.actual,
            Some(Actual {
                rows: Some(12.0),
                loops: 1,
                time: Some(ActualTime { first_row_ms: Some(0.5), last_row_ms: 1.25 })
            })
        );
        assert_eq!(plan.inclusive_ms(NodeId::ROOT), Some(1.25));
        let lookup = plan.node(NodeId(3));
        assert_eq!(
            (lookup.object.as_deref(), lookup.index.as_deref(), lookup.condition.as_deref()),
            (Some("c"), Some("PRIMARY"), Some("id=o.customer_id"))
        );
        assert_eq!(lookup.actual, Some(Actual { rows: None, loops: 0, time: None }));
        assert_eq!(plan.node(NodeId(4)).condition.as_deref(), Some("subquery in projection; dependent"));
        assert!(plan.node(NodeId(1)).attributes.iter().any(|a| a.key == "table_name" && a.value == "orders"));
        assert!(!root.attributes.iter().any(|a| a.key == "inputs" || a.key == "query"));
        assert_eq!(plan.attributes, [Attribute::new("query", "select …")]);
    }

    #[test]
    fn only_mysql_json_plans_are_claimed() {
        assert!(parse(MYSQL_57, r#"{"query_block": {"select_id": 1}}"#).is_some_and(|r| r.is_ok()));
        assert!(parse(MYSQL_97, r#"{"operation": "Rows fetched before execution"}"#).is_some_and(|r| r.is_ok()));
        assert!(parse(MYSQL_97, r#"{"rows": 1}"#).is_none());
        assert!(parse(MYSQL_97, "[1, 2]").is_none());
        assert!(matches!(parse(MYSQL_57, r#"{"query_block": "#), Some(Err(_))));
        let mariadb = ServerInfo::new(Flavor::MariaDb, Version::new(11, 4, 0));
        assert!(
            parse(mariadb, r#"{"query_block": {"select_id": 1}}"#)
                .is_none_or(|r| r.is_ok_and(|p| p.format != PlanFormat::MySqlJson))
        );
    }
}
