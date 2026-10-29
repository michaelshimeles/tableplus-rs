//! PostgreSQL's `EXPLAIN (FORMAT JSON)`, with or without ANALYZE: one `json` cell holding an
//! array with an object per statement. Each object has a `Plan` tree, whose children sit in
//! `Plans`, beside plan-wide values such as `Planning Time` and `Triggers`.
//!
//! ```json
//! [{"Plan": {"Node Type": "Hash Join", "Join Type": "Semi", "Hash Cond": "(a.id = b.id)",
//!            "Plan Rows": 12, "Actual Rows": 9, "Actual Loops": 1, "Plans": [ … ]},
//!   "Planning Time": 0.31, "Execution Time": 1.94}]
//! ```

use serde_json::{Map, Value as Json};

use super::{
    Actual, ActualTime, Attribute, Cost, Estimate, NodeId, ParseError, Plan, PlanFormat, PlanNode, strip_outer_parens,
};
use crate::model::ResultSet;
use crate::server::{Flavor, ServerInfo};
use crate::value::Value;

type Object = Map<String, Json>;

pub(super) fn parse(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    if server.flavor != Flavor::Postgres || result.columns.len() != 1 || result.rows.len() != 1 {
        return None;
    }
    let Value::Text(text) = &result.rows[0][0] else { return None };
    text.trim_start()
        .starts_with('[')
        .then(|| parse_text(text).map_err(|message| ParseError { format: PlanFormat::PostgresJson, message }))
}

/// JSON levels a plan may nest. serde_json stops at 128, which a join of about 90 tables passes;
/// a debug build overflows a 2 MiB worker stack near 500.
const MAX_DEPTH: usize = 256;

fn parse_text(text: &str) -> Result<Plan, String> {
    if json_depth(text) > MAX_DEPTH {
        return Err(format!("the plan nests deeper than {MAX_DEPTH} levels"));
    }
    let mut json = serde_json::Deserializer::from_str(text);
    json.disable_recursion_limit();
    let statements: Vec<Object> = serde::Deserialize::deserialize(&mut json).map_err(|e| e.to_string())?;
    json.end().map_err(|e| e.to_string())?;
    let mut plan = match statements.as_slice() {
        [] => return Err("the output holds no statement".into()),
        [one] => {
            let root = root_of(one)?;
            let mut plan = Plan::new(PlanFormat::PostgresJson, node(root)?, text);
            add_children(&mut plan, NodeId::ROOT, root, 1)?;
            plan
        }
        // A rule can turn one statement into several, each with its own plan.
        many => {
            let mut plan = Plan::new(PlanFormat::PostgresJson, PlanNode::new("Query"), text);
            for statement in many {
                let root = root_of(statement)?;
                let id = plan.add_child(NodeId::ROOT, node(root)?);
                add_children(&mut plan, id, root, 1)?;
            }
            plan
        }
    };
    let several = statements.len() > 1;
    for (i, statement) in statements.iter().enumerate() {
        for (key, value) in statement.iter().filter(|(k, _)| *k != "Plan") {
            let key = if several { format!("Statement {} · {key}", i + 1) } else { key.clone() };
            push_attributes(&mut plan.attributes, key, value);
        }
    }
    Ok(plan)
}

/// The deepest nesting of arrays and objects in `text`, outside strings.
fn json_depth(text: &str) -> usize {
    let (mut depth, mut deepest, mut in_string, mut escaped) = (0usize, 0, false, false);
    for b in text.bytes() {
        match b {
            _ if escaped => escaped = false,
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'[' | b'{' if !in_string => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b']' | b'}' if !in_string => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

fn root_of(statement: &Object) -> Result<&Object, String> {
    statement.get("Plan").and_then(Json::as_object).ok_or_else(|| "a statement has no \"Plan\" object".into())
}

/// Adds `object`'s children below `parent`. `runs` is how many times the Gather above ran, 1
/// outside one.
fn add_children(plan: &mut Plan, parent: NodeId, object: &Object, runs: u64) -> Result<(), String> {
    let gather = matches!(object.get("Node Type").and_then(Json::as_str), Some("Gather" | "Gather Merge"));
    let runs = if gather { plan.node(parent).actual.map_or(1, |a| a.loops.max(1)) } else { runs };
    for child in object.get("Plans").and_then(Json::as_array).into_iter().flatten() {
        let child = child.as_object().ok_or("an entry of \"Plans\" is not an object")?;
        let mut node = node(child)?;
        // Each time a Gather runs, its input runs once in every process, and the steps below
        // share those processes. A step with fewer loops per run ran in fewer of them, such as a
        // branch of a Parallel Append.
        let processes = match node.actual {
            Some(actual) if gather => actual.loops / runs,
            Some(actual) => u64::from(plan.node(parent).processes).min(actual.loops.div_ceil(runs)),
            None => plan.node(parent).processes.into(),
        };
        node.processes = processes.clamp(1, u32::MAX.into()) as u32;
        let id = plan.add_child(parent, node);
        add_children(plan, id, child, runs)?;
    }
    Ok(())
}

fn node(object: &Object) -> Result<PlanNode, String> {
    let text = |key: &str| object.get(key).and_then(Json::as_str);
    let number = |key: &str| object.get(key).and_then(Json::as_f64);
    let node_type = text("Node Type").ok_or("a plan node has no \"Node Type\"")?;
    let mut node = PlanNode::new(operation(node_type, object));
    let relationship = text("Parent Relationship");
    node.role = matches!(relationship, Some("SubPlan" | "InitPlan"))
        .then(|| text("Subplan Name").or(relationship).map(str::to_string))
        .flatten();
    node.object = object_name(object);
    node.index = text("Index Name").map(str::to_string);
    node.condition = condition(object);
    node.estimate = number("Plan Rows").map(|rows| Estimate {
        rows,
        cost: number("Total Cost").map(|total| Cost { startup: number("Startup Cost"), total }),
    });
    if let (Some(rows), Some(loops)) = (number("Actual Rows"), object.get("Actual Loops").and_then(Json::as_u64)) {
        // A node that never ran reports zero times; they are not measurements.
        let time = number("Actual Total Time")
            .filter(|_| loops > 0)
            .map(|last_row_ms| ActualTime { first_row_ms: number("Actual Startup Time"), last_row_ms });
        node.actual = Some(Actual { rows: Some(rows), loops, time });
    }
    for (key, value) in object.iter().filter(|(k, _)| *k != "Plans") {
        push_attributes(&mut node.attributes, key.clone(), value);
    }
    Ok(node)
}

/// The step as PostgreSQL's text format names it: `Hash Semi Join`, `GroupAggregate`,
/// `Parallel Seq Scan`, `Index Scan Backward`, `Update`.
fn operation(node_type: &str, object: &Object) -> String {
    let text = |key: &str| object.get(key).and_then(Json::as_str);
    let flag = |key: &str| object.get(key).and_then(Json::as_bool).unwrap_or(false);
    let mut op = match (node_type, text("Strategy"), text("Operation")) {
        ("Aggregate", Some("Sorted"), _) => "GroupAggregate".to_string(),
        ("Aggregate", Some("Hashed"), _) => "HashAggregate".to_string(),
        ("Aggregate", Some("Mixed"), _) => "MixedAggregate".to_string(),
        ("SetOp", Some("Hashed"), _) => "HashSetOp".to_string(),
        ("ModifyTable", _, Some(operation)) => operation.to_string(),
        ("Foreign Scan", _, Some(operation)) if operation != "Select" => format!("Foreign {operation}"),
        ("Custom Scan", _, _) => match text("Custom Plan Provider") {
            Some(provider) => format!("Custom Scan ({provider})"),
            None => node_type.to_string(),
        },
        _ => node_type.to_string(),
    };
    if let Some(join) = text("Join Type").filter(|j| *j != "Inner") {
        op = format!("{} {join} Join", op.strip_suffix(" Join").unwrap_or(&op));
    }
    if let Some(mode) = text("Partial Mode").filter(|m| *m != "Simple") {
        op = format!("{mode} {op}");
    }
    if flag("Async Capable") {
        op = format!("Async {op}");
    }
    if flag("Parallel Aware") {
        op = format!("Parallel {op}");
    }
    if text("Scan Direction") == Some("Backward") {
        op.push_str(" Backward");
    }
    op
}

/// `customers c`, the CTE or function a scan reads, or a subquery's alias.
fn object_name(object: &Object) -> Option<String> {
    let text = |key: &str| object.get(key).and_then(Json::as_str);
    let name = ["Relation Name", "CTE Name", "Function Name", "Table Function Name", "Tuplestore Name"]
        .into_iter()
        .find_map(text);
    match (name, text("Alias")) {
        (Some(name), Some(alias)) if alias != name => Some(format!("{name} {alias}")),
        (Some(name), _) => Some(name.to_string()),
        (None, alias) => alias.map(str::to_string),
    }
}

const CONDITION_KEYS: &[&str] =
    &["Hash Cond", "Merge Cond", "Index Cond", "Recheck Cond", "Join Filter", "Filter", "One-Time Filter"];

/// The node's one condition, or each of several on its own line under its key.
fn condition(object: &Object) -> Option<String> {
    let found: Vec<(&str, &str)> = object
        .iter()
        .filter(|(k, _)| CONDITION_KEYS.contains(&k.as_str()))
        .filter_map(|(k, v)| Some((k.as_str(), strip_outer_parens(v.as_str()?))))
        .collect();
    match found.as_slice() {
        [] => None,
        [(_, only)] => Some(only.to_string()),
        several => Some(several.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n")),
    }
}

/// One attribute per value, with objects and lists of objects flattened into
/// `Planning · Memory Used` and `Workers[0] · Actual Rows`.
fn push_attributes(out: &mut Vec<Attribute>, key: String, value: &Json) {
    match value {
        Json::Object(map) if !map.is_empty() => {
            for (k, v) in map {
                push_attributes(out, format!("{key} · {k}"), v);
            }
        }
        Json::Array(items) if items.iter().any(|v| v.is_object() || v.is_array()) => {
            for (i, v) in items.iter().enumerate() {
                push_attributes(out, format!("{key}[{i}]"), v);
            }
        }
        Json::Array(items) if !items.is_empty() => {
            out.push(Attribute::new(key, items.iter().map(scalar).collect::<Vec<_>>().join(", ")));
        }
        Json::Object(_) | Json::Array(_) => out.push(Attribute::new(key, "none")),
        scalar_value => out.push(Attribute::new(key, scalar(scalar_value))),
    }
}

fn scalar(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;
    use crate::server::Version;

    fn one(json: &str) -> PlanNode {
        node(serde_json::from_str::<Json>(json).unwrap().as_object().unwrap()).unwrap()
    }

    fn result(text: &str) -> ResultSet {
        ResultSet {
            columns: vec![ColumnMeta::new("QUERY PLAN", "json")],
            rows: vec![vec![Value::text(text)].into()],
            truncated: false,
        }
    }

    fn postgres() -> ServerInfo {
        ServerInfo::new(Flavor::Postgres, Version::new(17, 0, 0))
    }

    #[test]
    fn names_steps_like_the_text_format() {
        for (json, op) in [
            (r#"{"Node Type": "Hash Join", "Join Type": "Inner"}"#, "Hash Join"),
            (r#"{"Node Type": "Hash Join", "Join Type": "Right Anti"}"#, "Hash Right Anti Join"),
            (r#"{"Node Type": "Nested Loop", "Join Type": "Left"}"#, "Nested Loop Left Join"),
            (r#"{"Node Type": "Merge Join", "Join Type": "Semi"}"#, "Merge Semi Join"),
            (r#"{"Node Type": "Aggregate", "Strategy": "Plain", "Partial Mode": "Simple"}"#, "Aggregate"),
            (r#"{"Node Type": "Aggregate", "Strategy": "Hashed"}"#, "HashAggregate"),
            (
                r#"{"Node Type": "Aggregate", "Strategy": "Sorted", "Partial Mode": "Finalize"}"#,
                "Finalize GroupAggregate",
            ),
            (r#"{"Node Type": "SetOp", "Strategy": "Hashed"}"#, "HashSetOp"),
            (r#"{"Node Type": "Seq Scan", "Parallel Aware": true}"#, "Parallel Seq Scan"),
            (r#"{"Node Type": "Index Scan", "Scan Direction": "Backward"}"#, "Index Scan Backward"),
            (r#"{"Node Type": "Index Scan", "Scan Direction": "Forward"}"#, "Index Scan"),
            (r#"{"Node Type": "ModifyTable", "Operation": "Update"}"#, "Update"),
            (r#"{"Node Type": "Foreign Scan", "Operation": "Select", "Async Capable": true}"#, "Async Foreign Scan"),
            (r#"{"Node Type": "Foreign Scan", "Operation": "Delete"}"#, "Foreign Delete"),
            (r#"{"Node Type": "Custom Scan", "Custom Plan Provider": "ColumnarScan"}"#, "Custom Scan (ColumnarScan)"),
        ] {
            assert_eq!(one(json).operation, op, "{json}");
        }
    }

    #[test]
    fn reads_object_index_and_condition() {
        let scan =
            one(r#"{"Node Type": "Index Scan", "Index Name": "orders_pkey", "Relation Name": "orders", "Alias": "o",
                "Index Cond": "(order_id = od.order_id)"}"#);
        assert_eq!(
            (scan.object.as_deref(), scan.index.as_deref(), scan.condition.as_deref()),
            (Some("orders o"), Some("orders_pkey"), Some("order_id = od.order_id"))
        );
        let same_alias = one(r#"{"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "orders"}"#);
        assert_eq!(same_alias.object.as_deref(), Some("orders"));
        let cte = one(r#"{"Node Type": "WorkTable Scan", "CTE Name": "chain", "Alias": "chain_1"}"#);
        assert_eq!(cte.object.as_deref(), Some("chain chain_1"));
        let subquery = one(r#"{"Node Type": "Subquery Scan", "Alias": "t"}"#);
        assert_eq!(subquery.object.as_deref(), Some("t"));
        let join = one(r#"{"Node Type": "Hash Join", "Hash Cond": "(o.id = t.id)", "Join Filter": "(o.a > t.b)"}"#);
        assert_eq!(join.condition.as_deref(), Some("Hash Cond: o.id = t.id\nJoin Filter: o.a > t.b"));
    }

    #[test]
    fn reads_estimates_and_actuals_and_keeps_every_key() {
        let n = one(r#"{"Node Type": "Seq Scan", "Startup Cost": 0.00, "Total Cost": 23.22, "Plan Rows": 799,
                "Actual Startup Time": 0.001, "Actual Total Time": 0.037, "Actual Rows": 639, "Actual Loops": 3,
                "Output": ["a", "b"], "Workers": [{"Worker Number": 0, "Actual Rows": 213}], "Sort Key": [],
                "Plans": []}"#);
        assert_eq!(n.estimate, Some(Estimate { rows: 799.0, cost: Some(Cost { startup: Some(0.0), total: 23.22 }) }));
        // Leader plus two workers, as PostgreSQL counts them.
        assert_eq!(
            n.actual,
            Some(Actual {
                rows: Some(639.0),
                loops: 3,
                time: Some(ActualTime { first_row_ms: Some(0.001), last_row_ms: 0.037 })
            })
        );
        let attrs: Vec<(&str, &str)> = n.attributes.iter().map(|a| (a.key.as_str(), a.value.as_str())).collect();
        assert_eq!(&attrs[..2], [("Node Type", "Seq Scan"), ("Startup Cost", "0.0")]);
        assert!(attrs.contains(&("Output", "a, b")));
        assert!(attrs.contains(&("Workers[0] · Worker Number", "0")));
        assert!(attrs.contains(&("Workers[0] · Actual Rows", "213")));
        assert!(attrs.contains(&("Sort Key", "none")));
        assert!(!attrs.iter().any(|(k, _)| *k == "Plans"));

        let untimed = one(r#"{"Node Type": "Seq Scan", "Plan Rows": 5, "Actual Rows": 5, "Actual Loops": 1}"#);
        assert_eq!(untimed.actual, Some(Actual { rows: Some(5.0), loops: 1, time: None }));
        let fractional = one(r#"{"Node Type": "Index Scan", "Actual Rows": 0.50, "Actual Loops": 2}"#);
        assert_eq!(fractional.actual.unwrap().rows, Some(0.5));
    }

    /// Captured from PostgreSQL 17, keys trimmed: an InitPlan that never ran and a SubPlan.
    const SUBPLANS: &str = r#"[{
      "Plan": {"Node Type": "Seq Scan", "Relation Name": "customers", "Alias": "c", "Startup Cost": 4.34,
        "Total Cost": 7.70, "Plan Rows": 2, "Actual Startup Time": 0.013, "Actual Total Time": 0.013,
        "Actual Rows": 0, "Actual Loops": 1,
        "Filter": "((ANY ((customer_id)::text = ((hashed SubPlan 1).col1)::text)) AND ((country)::text = ((InitPlan 2).col1)::text))",
        "Plans": [
          {"Node Type": "Limit", "Parent Relationship": "InitPlan", "Subplan Name": "InitPlan 2",
           "Startup Cost": 0.00, "Total Cost": 0.04, "Plan Rows": 1, "Actual Startup Time": 0.000,
           "Actual Total Time": 0.000, "Actual Rows": 0, "Actual Loops": 0,
           "Plans": [{"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Relation Name": "suppliers",
             "Alias": "suppliers", "Startup Cost": 0.00, "Total Cost": 1.29, "Plan Rows": 29,
             "Actual Startup Time": 0.000, "Actual Total Time": 0.000, "Actual Rows": 0, "Actual Loops": 0}]},
          {"Node Type": "Index Scan", "Parent Relationship": "SubPlan", "Subplan Name": "SubPlan 1",
           "Index Name": "orders_pkey", "Relation Name": "orders", "Alias": "orders", "Startup Cost": 0.28,
           "Total Cost": 4.29, "Plan Rows": 1, "Actual Startup Time": 0.003, "Actual Total Time": 0.003,
           "Actual Rows": 0, "Actual Loops": 1, "Index Cond": "(order_id < 0)"}]},
      "Planning": {"Memory Used": 1187, "Memory Allocated": 2056},
      "Planning Time": 0.306, "Triggers": [], "Execution Time": 0.035}]"#;

    #[test]
    fn never_executed_steps_have_no_time() {
        let plan = parse_text(SUBPLANS).unwrap();
        let (id, never) = plan.nodes().nth(1).unwrap();
        assert_eq!(never.operation, "Limit");
        assert_eq!(never.actual, Some(Actual { rows: Some(0.0), loops: 0, time: None }));
        assert_eq!(plan.inclusive_ms(id), None);
        assert_eq!(plan.node(plan.children(id)[0]).actual.unwrap().time, None);
        assert!((plan.self_ms(NodeId::ROOT).unwrap() - (0.013 - 0.003)).abs() < 1e-9);
    }

    #[test]
    fn subplans_keep_their_role() {
        let plan = parse_text(SUBPLANS).unwrap();
        let roles: Vec<(&str, Option<&str>)> =
            plan.nodes().map(|(_, n)| (n.operation.as_str(), n.role.as_deref())).collect();
        assert_eq!(
            roles,
            [("Seq Scan", None), ("Limit", Some("InitPlan 2")), ("Seq Scan", None), ("Index Scan", Some("SubPlan 1"))]
        );
        let relationship = plan.nodes().nth(1).unwrap().1.attributes.iter().find(|a| a.key == "Parent Relationship");
        assert_eq!(relationship.map(|a| a.value.as_str()), Some("InitPlan"));
    }

    #[test]
    fn plan_wide_values_become_plan_attributes() {
        let plan = parse_text(SUBPLANS).unwrap();
        let attrs: Vec<(&str, &str)> = plan.attributes.iter().map(|a| (a.key.as_str(), a.value.as_str())).collect();
        assert_eq!(
            attrs,
            [
                ("Planning · Memory Used", "1187"),
                ("Planning · Memory Allocated", "2056"),
                ("Planning Time", "0.306"),
                ("Triggers", "none"),
                ("Execution Time", "0.035"),
            ]
        );
        assert!(plan.analyzed());
        assert_eq!(plan.raw, SUBPLANS);
    }

    #[test]
    fn parallel_workers_share_the_time_below_a_gather() {
        let plan = parse_text(
            r#"[{"Plan": {"Node Type": "Gather", "Workers Launched": 2, "Actual Total Time": 681.482,
                "Actual Rows": 3, "Actual Loops": 1, "Plans": [
                  {"Node Type": "Aggregate", "Partial Mode": "Partial", "Actual Total Time": 653.620,
                   "Actual Rows": 1, "Actual Loops": 3, "Plans": [
                     {"Node Type": "Seq Scan", "Parallel Aware": true, "Relation Name": "t",
                      "Actual Total Time": 1.658, "Actual Rows": 16667, "Actual Loops": 3}]}]},
               "Execution Time": 689.683}]"#,
        )
        .unwrap();
        let partial = NodeId(1);
        assert_eq!(plan.node(partial).processes, 3);
        assert_eq!(plan.node(NodeId(2)).processes, 3, "steps below the Gather's input share its processes");
        assert!((plan.inclusive_ms(partial).unwrap() - 653.62).abs() < 1e-9);
        assert!(plan.inclusive_ms(partial) < plan.inclusive_ms(NodeId::ROOT));
        assert_eq!(plan.total_ms(), Some(681.482));
    }

    #[test]
    fn processes_follow_each_run_and_branch() {
        let plan = parse_text(
            r#"[{"Plan": {"Node Type": "Nested Loop", "Actual Total Time": 30, "Actual Rows": 5, "Actual Loops": 1,
                "Plans": [
                  {"Node Type": "Seq Scan", "Relation Name": "a", "Actual Total Time": 1, "Actual Rows": 5,
                   "Actual Loops": 1},
                  {"Node Type": "Gather", "Workers Launched": 2, "Actual Total Time": 5, "Actual Rows": 1,
                   "Actual Loops": 5, "Plans": [
                     {"Node Type": "Append", "Parallel Aware": true, "Actual Total Time": 4, "Actual Rows": 1,
                      "Actual Loops": 15, "Plans": [
                        {"Node Type": "Seq Scan", "Relation Name": "b", "Actual Total Time": 3,
                         "Actual Rows": 1, "Actual Loops": 1},
                        {"Node Type": "Seq Scan", "Relation Name": "c", "Actual Total Time": 0.29,
                         "Actual Rows": 1, "Actual Loops": 5}]}]}]}}]"#,
        )
        .unwrap();
        let (gather, append, branch, each_run) = (NodeId(2), NodeId(3), NodeId(4), NodeId(5));
        assert_eq!(plan.node(gather).processes, 1);
        assert_eq!(plan.node(append).processes, 3, "15 loops over the Gather's 5 runs");
        assert!((plan.inclusive_ms(append).unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(plan.node(branch).processes, 1, "a branch one process ran keeps its own time");
        assert_eq!(plan.inclusive_ms(branch), Some(3.0));
        assert_eq!(plan.node(each_run).processes, 1, "one process ran it once per Gather run");
        assert!((plan.inclusive_ms(each_run).unwrap() - 1.45).abs() < 1e-9);
    }

    #[test]
    fn deep_plans_parse_up_to_a_limit() {
        let nested = |levels: usize| {
            let open = r#"{"Node Type": "Limit", "Plans": ["#.repeat(levels);
            let close = "]}".repeat(levels);
            format!(r#"[{{"Plan": {open}{{"Node Type": "Result"}}{close}}}]"#)
        };
        let deep = parse_text(&nested(120)).expect("a 120-step plan parses");
        assert_eq!(deep.node_count(), 121);
        let err = parse_text(&nested(130)).unwrap_err();
        assert!(err.contains("256 levels"), "{err}");
        assert_eq!(json_depth(r#"{"a": "[{\"[", "b": [1]}"#), 2, "brackets in strings don't count");
    }

    #[test]
    fn several_statements_share_a_root() {
        let plan = parse_text(
            r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "a"}, "Planning Time": 0.1},
                {"Plan": {"Node Type": "ModifyTable", "Operation": "Insert", "Relation Name": "log"}}]"#,
        )
        .unwrap();
        assert_eq!(plan.node(NodeId::ROOT).operation, "Query");
        assert_eq!(plan.children(NodeId::ROOT).len(), 2);
        assert_eq!(plan.attributes, [Attribute::new("Statement 1 · Planning Time", "0.1")]);
    }

    #[test]
    fn only_json_text_from_postgres_is_claimed() {
        assert!(matches!(parse(&postgres(), &result(SUBPLANS)), Some(Ok(_))));
        assert!(parse(&postgres(), &result("Seq Scan on t  (cost=0.00..1.00 rows=1 width=4)")).is_none());
        let mysql = ServerInfo::new(Flavor::MySql, Version::new(8, 4, 0));
        assert!(parse(&mysql, &result(SUBPLANS)).is_none());
        for broken in ["[{\"Plan\": ", "[]", "[{\"Planning Time\": 1}]", "[{\"Plan\": {\"Plans\": []}}]"] {
            let error = parse(&postgres(), &result(broken)).unwrap().unwrap_err();
            assert_eq!(error.format, PlanFormat::PostgresJson, "{broken}");
        }
    }
}
