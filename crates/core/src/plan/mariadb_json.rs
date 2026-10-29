//! MariaDB's JSON plans, from `EXPLAIN FORMAT=JSON` (estimates) and `ANALYZE FORMAT=JSON`
//! (estimates plus `r_` measurements): one text cell holding a `query_block` object.
//!
//! A join is a `nested_loop` array from 10.11 on, and in 10.6 a run of repeated `table` keys in
//! one object. Objects are therefore read with duplicate keys kept, in order.
//!
//! # Timing
//!
//! Every MariaDB time is a total over all of a step's loops: the [documentation][analyze] calls
//! `r_total_time_ms` the time spent "in total", and the [trackers][trackers] add up cycles without
//! dividing by the loop count. A query block's time covers everything below it. A table's
//! `r_table_time_ms` plus `r_other_time_ms` is its own work, and a sort's time leaves out a
//! temporary table the join filled before it. Each step's total, children included, is divided by
//! its `r_loops` to give the model's time per loop.
//!
//! [analyze]: https://mariadb.com/kb/en/analyze-format-json/
//! [trackers]: https://github.com/MariaDB/server/blob/main/sql/sql_analyze_stmt.h

use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use super::{Actual, ActualTime, Attribute, Cost, Estimate, NodeId, ParseError, Plan, PlanFormat, PlanNode};
use crate::model::ResultSet;
use crate::server::{Flavor, ServerInfo};
use crate::value::Value;

pub(super) fn parse(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    if server.flavor != Flavor::MariaDb || result.columns.len() != 1 || result.rows.len() != 1 {
        return None;
    }
    let Value::Text(text) = &result.rows[0][0] else { return None };
    text.trim_start().starts_with('{').then(|| parse_text(text))
}

fn parse_text(text: &str) -> Result<Plan, ParseError> {
    let error = |message: String| ParseError { format: PlanFormat::MariaDbJson, message };
    let json: Json = serde_json::from_str(text).map_err(|e| error(e.to_string()))?;
    let Json::Object(top) = json else { return Err(error("the plan is not a JSON object".into())) };
    let mut query = PlanNode::new("Query");
    let mut steps = read_members(&top, &mut query);
    let attributes = std::mem::take(&mut query.attributes);
    let root = match steps.len() {
        0 => return Err(error("the plan has no query_block".into())),
        1 => steps.remove(0),
        _ => Step::new(query, Timing::Untimed, steps),
    };
    let mut plan = Plan::new(PlanFormat::MariaDbJson, root.node, text);
    plan.attributes = attributes;
    insert(&mut plan, NodeId::ROOT, root.children);
    Ok(plan)
}

fn insert(plan: &mut Plan, parent: NodeId, steps: Vec<Step>) {
    for step in steps {
        let id = plan.add_child(parent, step.node);
        insert(plan, id, step.children);
    }
}

/// A plan node with its children, before it goes into the [`Plan`].
struct Step {
    node: PlanNode,
    children: Vec<Step>,
    /// Milliseconds over all loops, children included; for an untimed step, its children's.
    ms: f64,
}

/// What a step's reported time covers.
enum Timing {
    Untimed,
    /// The step and every child.
    Covers(f64),
    /// Only the step's own work; the children ran outside it.
    Own(f64),
}

impl Step {
    /// Sets the node's time per loop from its total and its children's.
    fn new(mut node: PlanNode, timing: Timing, children: Vec<Step>) -> Self {
        let below: f64 = children.iter().map(|c| c.ms).sum();
        let total = match timing {
            Timing::Untimed => None,
            Timing::Covers(ms) => Some(ms),
            Timing::Own(ms) => Some(ms + below),
        };
        if let (Some(total), Some(actual)) = (total, node.actual.as_mut())
            && actual.loops > 0
        {
            actual.time = Some(ActualTime { first_row_ms: None, last_row_ms: total / actual.loops as f64 });
        }
        Self { node, children, ms: total.unwrap_or(below) }
    }
}

type Members = [(String, Json)];

/// Keys whose value lists steps: an array of one-key objects, or in 10.6 one object with
/// repeated keys.
const LISTS: &[&str] = &["nested_loop", "subqueries", "query_specifications", "sorts"];

/// Steps that take part in their parent's join, in join order.
const JOINED: &[&str] = &["nested_loop", "table", "read_sorted_file", "duplicates_removal", "block-nl-join"];

/// The steps a member holds, or `None` when it is an attribute of its parent.
fn read(key: &str, value: &Json) -> Option<Vec<Step>> {
    if LISTS.contains(&key) {
        return Some(listed(value));
    }
    if key == "duplicates_removal" {
        return Some(vec![Step::new(PlanNode::new("Remove duplicates"), Timing::Untimed, listed(value))]);
    }
    let Json::Object(members) = value else { return None };
    let step = match key {
        "query_block" => return Some(query_block(members)),
        "table" => table(members),
        "filesort" => filesort(members),
        "block-nl-join" => block_join(members),
        "union_result" => union("Union", members),
        "recursive_union" => union("Recursive union", members),
        "temporary_table" => group("Temporary table", members),
        "materialized" => group("Materialize", members),
        "read_sorted_file" => group("Read sorted file", members),
        "window_functions_computation" => group("Window functions", members),
        "materialization" => counted("Materialize subquery", members),
        "subquery_cache" => counted("Subquery cache", members),
        "expression_cache" => counted("Expression cache", members),
        _ => return None,
    };
    Some(vec![step])
}

fn listed(value: &Json) -> Vec<Step> {
    let entries: Box<dyn Iterator<Item = &(String, Json)>> = match value {
        Json::Array(items) => Box::new(items.iter().flat_map(Json::members)),
        other => Box::new(other.members().iter()),
    };
    entries.filter_map(|(k, v)| read(k, v)).flatten().collect()
}

/// Splits an object into the node's attributes and its child steps. The members of a join sit
/// under one `Nested loop` step, at the position of the first.
fn read_members(members: &Members, node: &mut PlanNode) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut join: Option<(usize, Vec<Step>)> = None;
    for (key, value) in members {
        match read(key, value) {
            None => attributes(&mut node.attributes, key, value),
            Some(found) if JOINED.contains(&key.as_str()) => {
                join.get_or_insert_with(|| (steps.len(), Vec::new())).1.extend(found)
            }
            Some(found) => steps.extend(found),
        }
    }
    if let Some((at, mut tables)) = join {
        let step = if tables.len() == 1 {
            tables.remove(0)
        } else {
            Step::new(PlanNode::new("Nested loop"), Timing::Untimed, tables)
        };
        steps.insert(at, step);
    }
    steps
}

/// A `key: value` attribute per scalar, with nested objects such as `r_engine_stats` flattened
/// into dotted keys.
fn attributes(out: &mut Vec<Attribute>, key: &str, value: &Json) {
    match value {
        Json::Object(members) => {
            for (k, v) in members {
                attributes(out, &format!("{key}.{k}"), v);
            }
        }
        _ => out.push(Attribute::new(key, value.to_string())),
    }
}

fn number(members: &Members, key: &str) -> Option<f64> {
    members.iter().find_map(|(k, v)| match v {
        Json::Number(n) if k == key => Some(*n),
        _ => None,
    })
}

fn text<'a>(members: &'a Members, key: &str) -> Option<&'a str> {
    members.iter().find_map(|(k, v)| match v {
        Json::Text(s) if k == key => Some(s.as_str()),
        _ => None,
    })
}

fn loops(members: &Members) -> Option<u64> {
    number(members, "r_loops").map(|n| n as u64)
}

/// A step that only groups its children.
fn group(operation: &str, members: &Members) -> Step {
    let mut node = PlanNode::new(operation);
    let children = read_members(members, &mut node);
    Step::new(node, Timing::Untimed, children)
}

/// A step that counts its loops but neither rows nor time: caches and subquery materialization.
fn counted(operation: &str, members: &Members) -> Step {
    let mut node = PlanNode::new(operation);
    let children = read_members(members, &mut node);
    node.actual = loops(members).map(|loops| Actual { rows: None, loops, time: None });
    Step::new(node, Timing::Untimed, children)
}

fn query_block(members: &Members) -> Vec<Step> {
    let mut node = PlanNode::new("Query");
    if let Some(id) = number(members, "select_id") {
        node.operation = format!("Select #{id}");
    }
    let children = read_members(members, &mut node);
    // The top-level block of a union has no id and nothing but the union in it.
    if node.attributes.is_empty() {
        return children;
    }
    node.condition = text(members, "having_condition").map(str::to_string);
    node.actual = loops(members).map(|loops| Actual { rows: None, loops, time: None });
    let timing = number(members, "r_total_time_ms").map_or(Timing::Untimed, Timing::Covers);
    vec![Step::new(node, timing, children)]
}

fn table(members: &Members) -> Step {
    let flag = |key| number(members, key) == Some(1.0);
    let operation = if flag("update") {
        "Update"
    } else if flag("delete") {
        "Delete"
    } else {
        text(members, "access_type").map_or("Table", access_label)
    };
    let mut node = PlanNode::new(operation);
    node.object = text(members, "table_name").map(str::to_string);
    node.index = text(members, "key").map(str::to_string);
    node.condition = text(members, "attached_condition").map(str::to_string);
    node.estimate = number(members, "rows")
        .map(|rows| Estimate { rows, cost: number(members, "cost").map(|total| Cost { startup: None, total }) });
    node.actual = loops(members).map(|loops| Actual { rows: number(members, "r_rows"), loops, time: None });
    let own = number(members, "r_total_time_ms").or_else(|| {
        let engine = number(members, "r_table_time_ms")?;
        Some(engine + number(members, "r_other_time_ms").unwrap_or(0.0))
    });
    let children = read_members(members, &mut node);
    Step::new(node, own.map_or(Timing::Untimed, Timing::Own), children)
}

/// MariaDB's access types, worded like MySQL's TREE steps.
fn access_label(access: &str) -> &str {
    match access {
        "ALL" => "Table scan",
        "index" => "Index scan",
        "range" => "Index range scan",
        "ref" => "Index lookup",
        "eq_ref" => "Single-row index lookup",
        "ref_or_null" => "Index lookup or null",
        "const" | "system" => "Constant row",
        "fulltext" => "Full-text index lookup",
        "index_merge" => "Index merge",
        "unique_subquery" => "Unique subquery lookup",
        "index_subquery" => "Subquery index lookup",
        "hash_ALL" => "Hash join scan",
        "hash_index" | "hash_range" | "hash_index_merge" => "Hash join index lookup",
        other => other,
    }
}

fn filesort(members: &Members) -> Step {
    let mut node = PlanNode::new("Sort");
    node.condition = text(members, "sort_key").map(str::to_string);
    node.actual = loops(members).map(|loops| Actual { rows: number(members, "r_output_rows"), loops, time: None });
    let children = read_members(members, &mut node);
    let timing = match number(members, "r_total_time_ms") {
        None => Timing::Untimed,
        Some(ms) if members.iter().any(|(k, _)| k == "temporary_table") => Timing::Own(ms),
        Some(ms) => Timing::Covers(ms),
    };
    Step::new(node, timing, children)
}

/// A join buffer (10.x): its own unpacking and condition time, with the buffered table below.
fn block_join(members: &Members) -> Step {
    let operation = match text(members, "join_type") {
        Some("BNLH") => "Block nested loop hash join",
        Some("BKA") => "Batched key access",
        Some("BKAH") => "Batched key access hash join",
        _ => "Block nested loop",
    };
    let mut node = PlanNode::new(operation);
    node.condition = text(members, "attached_condition").map(str::to_string);
    node.actual = loops(members).map(|loops| Actual { rows: None, loops, time: None });
    let own = ["r_unpack_time_ms", "r_other_time_ms"].iter().filter_map(|k| number(members, k)).reduce(|a, b| a + b);
    let children = read_members(members, &mut node);
    Step::new(node, own.map_or(Timing::Untimed, Timing::Own), children)
}

fn union(operation: &str, members: &Members) -> Step {
    let mut node = PlanNode::new(operation);
    node.object = text(members, "table_name").map(str::to_string);
    // A recursive union reports `r_loops: 0` and `r_rows: null` although its parts ran, so only a
    // row count makes the loops trustworthy.
    node.actual = number(members, "r_rows").map(|rows| Actual {
        rows: Some(rows),
        loops: loops(members).unwrap_or(1),
        time: None,
    });
    let children = read_members(members, &mut node);
    Step::new(node, Timing::Untimed, children)
}

/// A JSON value whose objects keep every member in order, duplicate keys included.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn members(&self) -> &Members {
        match self {
            Json::Object(members) => members,
            _ => &[],
        }
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Number(n) => write!(f, "{n}"),
            Json::Text(s) => f.write_str(s),
            Json::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                Ok(())
            }
            Json::Object(members) => {
                f.write_str("{")?;
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str("}")
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Number(v as f64))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Number(v as f64))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Json, E> {
        Ok(Json::Number(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Text(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Json, E> {
        Ok(Json::Text(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut members = Vec::new();
        while let Some(member) = map.next_entry()? {
            members.push(member);
        }
        Ok(Json::Object(members))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;
    use crate::server::Version;

    fn plan(text: &str) -> Plan {
        parse_text(text).expect("parsed")
    }

    fn find(plan: &Plan, operation: &str, object: Option<&str>) -> NodeId {
        plan.nodes()
            .find(|(_, n)| n.operation == operation && n.object.as_deref() == object)
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("no {operation} {object:?}"))
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    /// Select #1 (10 ms) ── Sort (2 ms, over a temporary table) ── Temporary table ── Nested loop
    ///   ├─ a (1 + 0.5 ms)
    ///   └─ d (0.4 + 0.4 ms over 4 loops) ── Materialize ── Select #2 (3.2 ms over 4 loops)
    ///        └─ Read sorted file ── Sort (2 ms, reading b itself) ── b (1.2 + 0.4 ms)
    const TIMED: &str = r#"{
      "query_optimization": {"r_total_time_ms": 0.3},
      "query_block": {
        "select_id": 1, "r_loops": 1, "r_total_time_ms": 10,
        "filesort": {
          "sort_key": "a.x", "r_loops": 1, "r_total_time_ms": 2, "r_output_rows": 4,
          "temporary_table": {
            "nested_loop": [
              {"table": {"table_name": "a", "access_type": "ALL", "r_loops": 1, "rows": 4, "r_rows": 4,
                         "r_table_time_ms": 1, "r_other_time_ms": 0.5, "r_engine_stats": {"pages_accessed": 3}}},
              {"table": {"table_name": "d", "access_type": "ref", "key": "key0", "r_loops": 4, "rows": 1, "r_rows": 1,
                         "r_table_time_ms": 0.4, "r_other_time_ms": 0.4,
                         "materialized": {"query_block": {"select_id": 2, "r_loops": 4, "r_total_time_ms": 3.2,
                           "read_sorted_file": {"r_rows": 5, "filesort": {
                             "sort_key": "b.y", "r_loops": 4, "r_total_time_ms": 2, "r_output_rows": 5,
                             "table": {"table_name": "b", "access_type": "ALL", "r_loops": 4, "rows": 5, "r_rows": 5,
                                       "r_table_time_ms": 1.2, "r_other_time_ms": 0.4}}}}}}}
            ]
          }
        }
      }
    }"#;

    #[test]
    fn times_become_per_loop_and_include_children() {
        let plan = plan(TIMED);
        assert_eq!(plan.node_count(), 11);
        let sorts: Vec<NodeId> = plan.nodes().filter(|(_, n)| n.operation == "Sort").map(|(id, _)| id).collect();
        let [outer_sort, inner_sort] = sorts[..] else { panic!("two sorts, got {}", sorts.len()) };
        let [select1, a, d, select2, b] = [
            find(&plan, "Select #1", None),
            find(&plan, "Table scan", Some("a")),
            find(&plan, "Index lookup", Some("d")),
            find(&plan, "Select #2", None),
            find(&plan, "Table scan", Some("b")),
        ];
        assert_eq!(select1, NodeId::ROOT);
        for (id, inclusive, own, per_loop) in [
            (b, 1.6, 1.6, 0.4),
            // The sort reads b itself, so its 2 ms already hold b's 1.6.
            (inner_sort, 2.0, 0.4, 0.5),
            (select2, 3.2, 1.2, 0.8),
            // d's own 0.8 ms, plus the 3.2 ms of the derived table it materialized.
            (d, 4.0, 0.8, 1.0),
            (a, 1.5, 1.5, 1.5),
            // The join filled the temporary table before the 2 ms sort began.
            (outer_sort, 7.5, 2.0, 7.5),
            (select1, 10.0, 2.5, 10.0),
        ] {
            let at = &plan.node(id).operation;
            assert!(close(plan.inclusive_ms(id), inclusive), "{at}: inclusive {:?}", plan.inclusive_ms(id));
            assert!(close(plan.self_ms(id), own), "{at}: self {:?}", plan.self_ms(id));
            let time = plan.node(id).actual.unwrap().time.unwrap();
            assert_eq!(time.first_row_ms, None, "{at}");
            assert!(close(Some(time.last_row_ms), per_loop), "{at}: per loop {}", time.last_row_ms);
        }
        assert_eq!(plan.total_ms(), Some(10.0));
        for untimed in ["Temporary table", "Nested loop", "Materialize", "Read sorted file"] {
            let id = find(&plan, untimed, None);
            assert_eq!(plan.inclusive_ms(id), None, "{untimed}");
        }
    }

    #[test]
    fn reads_estimates_actuals_and_attributes() {
        let plan = plan(TIMED);
        let d = plan.node(find(&plan, "Index lookup", Some("d")));
        assert_eq!(d.index.as_deref(), Some("key0"));
        assert_eq!(d.estimate, Some(Estimate { rows: 1.0, cost: None }));
        assert_eq!(d.actual.map(|a| (a.rows, a.loops)), Some((Some(1.0), 4)));
        let sort = plan.node(plan.children(NodeId::ROOT)[0]);
        assert_eq!(sort.condition.as_deref(), Some("a.x"));
        assert_eq!(sort.actual.map(|a| (a.rows, a.loops)), Some((Some(4.0), 1)));
        let select = plan.node(NodeId::ROOT);
        assert_eq!(select.actual.map(|a| (a.rows, a.loops)), Some((None, 1)));
        let a = plan.node(find(&plan, "Table scan", Some("a")));
        let attr = |key: &str| a.attributes.iter().find(|x| x.key == key).map(|x| x.value.as_str());
        assert_eq!(attr("access_type"), Some("ALL"));
        assert_eq!(attr("r_engine_stats.pages_accessed"), Some("3"));
        assert_eq!(attr("r_other_time_ms"), Some("0.5"));
        assert_eq!(plan.attributes, [Attribute::new("query_optimization.r_total_time_ms", "0.3")]);
    }

    #[test]
    fn repeated_table_keys_form_one_join() {
        let plan = plan(
            r#"{"query_block": {"select_id": 1,
                "table": {"table_name": "a", "access_type": "ALL", "rows": 3},
                "table": {"table_name": "b", "access_type": "eq_ref", "key": "PRIMARY", "rows": 1, "cost": 0.5,
                          "attached_condition": "b.id = a.id"},
                "subqueries": [{"query_block": {"select_id": 2,
                  "table": {"table_name": "c", "access_type": "ALL", "possible_keys": ["PRIMARY", "k"], "rows": 9}}}]}}"#,
        );
        let ops: Vec<(usize, &str, Option<&str>)> =
            plan.nodes().map(|(id, n)| (plan.depth(id), n.operation.as_str(), n.object.as_deref())).collect();
        assert_eq!(
            ops,
            [
                (0, "Select #1", None),
                (1, "Nested loop", None),
                (2, "Table scan", Some("a")),
                (2, "Single-row index lookup", Some("b")),
                (1, "Select #2", None),
                (2, "Table scan", Some("c")),
            ]
        );
        let b = plan.node(find(&plan, "Single-row index lookup", Some("b")));
        assert_eq!(b.condition.as_deref(), Some("b.id = a.id"));
        assert_eq!(b.estimate, Some(Estimate { rows: 1.0, cost: Some(Cost { startup: None, total: 0.5 }) }));
        let c = plan.node(find(&plan, "Table scan", Some("c")));
        assert!(c.attributes.contains(&Attribute::new("possible_keys", "PRIMARY, k")));
        assert!(!plan.analyzed());
        assert_eq!(plan.total_ms(), None);
    }

    #[test]
    fn unions_and_steps_that_never_ran() {
        let plan = plan(
            r#"{"query_block": {"union_result": {"table_name": "<union1,2>", "access_type": "ALL", "r_loops": 1, "r_rows": 2,
                "query_specifications": [
                  {"query_block": {"select_id": 1, "r_loops": 1, "r_total_time_ms": 0.5,
                    "table": {"table_name": "a", "access_type": "ALL", "r_loops": 1, "rows": 1, "r_rows": 2,
                              "r_table_time_ms": 0.1, "r_other_time_ms": 0.1}}},
                  {"query_block": {"select_id": 2, "operation": "UNION", "r_loops": 1, "r_total_time_ms": 0.25,
                    "table": {"table_name": "<derived3>", "access_type": "ALL", "r_loops": 0, "rows": 2, "r_rows": null,
                      "materialized": {"query_block": {"recursive_union": {"table_name": "<union3,4>", "access_type": "ALL",
                        "r_loops": 0, "r_rows": null, "query_specifications": []}}}}}}]}}}"#,
        );
        let root = plan.node(NodeId::ROOT);
        assert_eq!((root.operation.as_str(), root.object.as_deref()), ("Union", Some("<union1,2>")));
        assert_eq!(root.actual, Some(Actual { rows: Some(2.0), loops: 1, time: None }));
        assert_eq!(plan.total_ms(), Some(0.75));
        let derived = plan.node(find(&plan, "Table scan", Some("<derived3>")));
        assert_eq!(derived.actual, Some(Actual { rows: None, loops: 0, time: None }));
        let recursive = plan.node(find(&plan, "Recursive union", Some("<union3,4>")));
        assert_eq!(recursive.actual, None, "a recursive union's zero loops don't mean it never ran");
    }

    #[test]
    fn a_join_buffer_adds_its_own_time() {
        let plan = plan(
            r#"{"query_block": {"select_id": 1, "r_loops": 1, "r_total_time_ms": 5,
                "nested_loop": [
                  {"table": {"table_name": "a", "access_type": "ALL", "r_loops": 1, "rows": 10, "r_rows": 10,
                             "r_table_time_ms": 1, "r_other_time_ms": 0}},
                  {"block-nl-join": {"table": {"table_name": "b", "access_type": "ALL", "r_loops": 1, "rows": 10,
                                               "r_rows": 10, "r_table_time_ms": 1, "r_other_time_ms": 0},
                    "buffer_type": "flat", "join_type": "BNL", "attached_condition": "b.x = a.x", "r_loops": 1,
                    "r_filtered": 10, "r_unpack_time_ms": 0.5, "r_other_time_ms": 1.5}}]}}"#,
        );
        let join = find(&plan, "Block nested loop", None);
        assert_eq!(plan.node(join).condition.as_deref(), Some("b.x = a.x"));
        assert!(close(plan.inclusive_ms(join), 3.0));
        assert!(close(plan.self_ms(join), 2.0));
        assert_eq!(plan.node(plan.children(join)[0]).object.as_deref(), Some("b"));
    }

    #[test]
    fn only_mariadb_json_is_claimed() {
        let result = |text: &str| ResultSet {
            columns: vec![ColumnMeta::new("EXPLAIN", "VARCHAR")],
            rows: vec![vec![Value::text(text)].into()],
            truncated: false,
        };
        let mariadb = ServerInfo::new(Flavor::MariaDb, Version::new(11, 8, 0));
        let json =
            r#"{"query_block": {"select_id": 1, "table": {"table_name": "t", "access_type": "ALL", "rows": 1}}}"#;
        assert!(matches!(parse(&mariadb, &result(json)), Some(Ok(_))));
        assert!(matches!(parse(&mariadb, &result("{\"query_block\": ")), Some(Err(_))));
        assert!(matches!(parse(&mariadb, &result("{\"other\": 1}")), Some(Err(_))));
        assert!(parse(&mariadb, &result("-> Table scan on t")).is_none());
        let mysql = ServerInfo::new(Flavor::MySql, Version::new(8, 4, 0));
        assert!(parse(&mysql, &result(json)).is_none());
    }
}
