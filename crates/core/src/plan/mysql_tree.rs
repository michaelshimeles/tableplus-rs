//! MySQL's TREE format, from `EXPLAIN FORMAT=TREE`, `EXPLAIN ANALYZE` and (on 9.x) a bare
//! `EXPLAIN`: one text cell with a line per iterator, nested by indentation.
//!
//! ```text
//! -> Inner hash join (c.customer_id = o.customer_id)  (cost=594 rows=1.95) (actual time=0.578..0.61 rows=287 loops=1)
//!     -> Table scan on c  (cost=0.525 rows=91) (actual time=0.00275..0.0141 rows=91 loops=1)
//!     -> Hash
//!         -> Covering index lookup on od2 using idx_product (product_id=p2.product_id)  (cost=0.546 rows=28)
//! ```

use super::{
    Actual, ActualTime, Attribute, Cost, Estimate, NodeId, ParseError, Plan, PlanFormat, PlanNode, strip_outer_parens,
};
use crate::model::ResultSet;
use crate::server::{Flavor, ServerInfo};
use crate::value::Value;

pub(super) fn parse(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    if server.flavor != Flavor::MySql || result.columns.len() != 1 || result.rows.len() != 1 {
        return None;
    }
    let Value::Text(text) = &result.rows[0][0] else { return None };
    text.trim_start().starts_with("->").then(|| Ok(parse_text(text)))
}

/// One `->` line, plus any continuation lines (a string literal with a newline in a condition).
struct Entry {
    indent: usize,
    text: String,
}

fn entries(text: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for line in text.lines() {
        let body = line.trim_start();
        if let Some(rest) = body.strip_prefix("->") {
            out.push(Entry { indent: line.len() - body.len(), text: rest.trim().to_string() });
        } else if let Some(last) = out.last_mut().filter(|_| !line.trim().is_empty()) {
            last.text.push('\n');
            last.text.push_str(line);
        }
    }
    out
}

fn parse_text(text: &str) -> Plan {
    let entries = entries(text);
    let top = entries.iter().map(|e| e.indent).min().unwrap_or(0);
    let mut lines = entries.iter();
    // Several top-level iterators (rare) share a synthetic root.
    let (mut plan, mut stack) = if entries.iter().filter(|e| e.indent == top).count() > 1 {
        (Plan::new(PlanFormat::MySqlTree, PlanNode::new("Query"), text), vec![(None, NodeId::ROOT)])
    } else {
        let first = lines.next().map_or_else(|| PlanNode::new("Query"), |e| node(&e.text));
        (Plan::new(PlanFormat::MySqlTree, first, text), vec![(Some(top), NodeId::ROOT)])
    };
    for entry in lines {
        while stack.len() > 1 && stack.last().is_some_and(|(indent, _)| indent.is_some_and(|i| i >= entry.indent)) {
            stack.pop();
        }
        let parent = stack.last().map_or(NodeId::ROOT, |(_, id)| *id);
        let id = plan.add_child(parent, node(&entry.text));
        stack.push((Some(entry.indent), id));
    }
    plan
}

/// The measurements MySQL appends to a line, in parentheses.
enum Group<'a> {
    Estimate(&'a str),
    Actual(&'a str),
    NeverExecuted,
}

/// Splits the trailing `(cost=…)`, `(actual …)` and `(never executed)` groups off a line.
fn split_groups(text: &str) -> (&str, Vec<Group<'_>>) {
    let mut head = text.trim_end();
    let mut groups = Vec::new();
    while head.ends_with(')') {
        let mut depth = 0;
        let Some(open) = head.char_indices().rev().find_map(|(i, c)| {
            match c {
                ')' => depth += 1,
                '(' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(i)
        }) else {
            break;
        };
        let inner = &head[open + 1..head.len() - 1];
        let group = if inner == "never executed" {
            Group::NeverExecuted
        } else if let Some(rest) = inner.strip_prefix("actual ") {
            Group::Actual(rest)
        } else if inner.starts_with("cost=") || inner.starts_with("rows=") {
            Group::Estimate(inner)
        } else {
            break;
        };
        groups.push(group);
        head = head[..open].trim_end();
    }
    groups.reverse();
    (head, groups)
}

/// `key=value` pairs of a group, such as `time=0.578..0.61 rows=287 loops=1`.
fn pairs(group: &str) -> impl Iterator<Item = (&str, &str)> {
    group.split_whitespace().filter_map(|kv| kv.split_once('='))
}

/// `A..B` as `(Some(A), B)`, or `A` as `(None, A)`.
fn range(text: &str) -> Option<(Option<f64>, f64)> {
    match text.split_once("..") {
        Some((a, b)) => Some((Some(a.parse().ok()?), b.parse().ok()?)),
        None => Some((None, text.parse().ok()?)),
    }
}

fn node(text: &str) -> PlanNode {
    let (head, groups) = split_groups(text);
    let mut node = describe(head);
    node.attributes.push(Attribute::new("step", head));
    for group in groups {
        match group {
            Group::Estimate(g) => {
                let (mut rows, mut cost) = (None, None);
                for (k, v) in pairs(g) {
                    node.attributes.push(Attribute::new(k, v));
                    match k {
                        "rows" => rows = v.parse().ok(),
                        "cost" => cost = range(v).map(|(startup, total)| Cost { startup, total }),
                        _ => {}
                    }
                }
                node.estimate = rows.map(|rows| Estimate { rows, cost });
            }
            Group::Actual(g) => {
                let (mut time, mut rows, mut loops) = (None, None, None);
                for (k, v) in pairs(g) {
                    node.attributes.push(Attribute::new(format!("actual {k}"), v));
                    match k {
                        "time" => {
                            time = range(v).map(|(first_row_ms, last_row_ms)| ActualTime { first_row_ms, last_row_ms })
                        }
                        "rows" => rows = v.parse().ok(),
                        "loops" => loops = v.parse().ok(),
                        _ => {}
                    }
                }
                if let (Some(rows), Some(loops)) = (rows, loops) {
                    node.actual = Some(Actual { rows: Some(rows), loops, time });
                }
            }
            Group::NeverExecuted => {
                node.attributes.push(Attribute::new("actual", "never executed"));
                node.actual = Some(Actual { rows: Some(0.0), loops: 0, time: None });
            }
        }
    }
    node
}

/// Prefixes after which MySQL names the object the step works on.
const OBJECT_PREFIXES: &[&str] =
    &["Materialize recursive CTE ", "Materialize CTE ", "Update ", "Delete from ", "Insert into "];

/// Splits a line's text into operation, object, index and condition.
pub(super) fn describe(head: &str) -> PlanNode {
    // `Filter: (a = 1)`, `Sort: x DESC`, `Limit: 20 row(s)`, `Group aggregate: count(…)`
    if let Some((op, rest)) = head.split_once(": ")
        && is_label(op)
    {
        return with(op, None, None, rest);
    }
    // `Materialize CTE ranked`, `Update products (buffered)`
    if let Some(prefix) = OBJECT_PREFIXES.iter().find(|p| head.starts_with(**p)) {
        let (object, rest) = take_name(&head[prefix.len()..]);
        return with(prefix.trim_end(), Some(object), None, rest);
    }
    // `Table scan on <union temporary>`, `Index lookup on t using <auto_key0> (order_id=…)`
    if let Some((op, rest)) = head.split_once(" on ")
        && is_label(op)
    {
        let (object, rest) = take_name(rest);
        let (index, rest) = match rest.strip_prefix(" using ") {
            Some(r) => {
                let (index, r) = take_name(r);
                (Some(index), r)
            }
            None => (None, rest),
        };
        return with(op, Some(object), index, rest);
    }
    // `Inner hash join (c.customer_id = o.customer_id)`, `Select #2 (subquery in projection)`
    if let Some(i) = head.find(" (")
        && is_label(&head[..i])
    {
        return with(&head[..i], None, None, &head[i..]);
    }
    PlanNode::new(head)
}

fn with(op: &str, object: Option<&str>, index: Option<&str>, condition: &str) -> PlanNode {
    let mut node = PlanNode::new(op.trim());
    node.object = object.map(str::to_string);
    node.index = index.map(str::to_string);
    let condition = strip_outer_parens(condition.trim());
    node.condition = (!condition.is_empty()).then(|| condition.to_string());
    node
}

/// Words, digits, hyphens and `#`: `Nested loop anti-join`, `Select #2`.
fn is_label(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_alphabetic())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '#'))
}

/// A table, index or CTE name: `<union temporary>`, `` `name` `` or a bare word.
fn take_name(text: &str) -> (&str, &str) {
    let end = match text.chars().next() {
        Some('<') => text.find('>').map(|i| i + 1),
        Some('`') => text[1..].find('`').map(|i| i + 2),
        _ => None,
    }
    .unwrap_or_else(|| text.find([' ', '(']).unwrap_or(text.len()));
    (&text[..end], &text[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> PlanNode {
        node(line.trim_start_matches("-> "))
    }

    #[test]
    fn splits_steps_into_parts() {
        for (line, op, object, index, condition) in [
            (
                "Filter: ((o.order_date >= DATE'1997-01-01') and (o.employee_id is not null))",
                "Filter",
                None,
                None,
                Some("(o.order_date >= DATE'1997-01-01') and (o.employee_id is not null)"),
            ),
            (
                "Sort: revenue DESC, `name`, limit input to 20 row(s) per chunk",
                "Sort",
                None,
                None,
                Some("revenue DESC, `name`, limit input to 20 row(s) per chunk"),
            ),
            ("Table scan on <union temporary>", "Table scan", Some("<union temporary>"), None, None),
            (
                "Covering index lookup on p2 using idx_products_category_id (category_id=cat2.category_id)",
                "Covering index lookup",
                Some("p2"),
                Some("idx_products_category_id"),
                Some("category_id=cat2.category_id"),
            ),
            (
                "Single-row index lookup on <subquery6> using <auto_distinct_key> (supplier_id=s.supplier_id)",
                "Single-row index lookup",
                Some("<subquery6>"),
                Some("<auto_distinct_key>"),
                Some("supplier_id=s.supplier_id"),
            ),
            (
                "Inner hash join (c.customer_id = o.customer_id)",
                "Inner hash join",
                None,
                None,
                Some("c.customer_id = o.customer_id"),
            ),
            (
                "Inner hash join (o.ship_via = sh.shipper_id), (o.order_id = `<subquery4>`.order_id)",
                "Inner hash join",
                None,
                None,
                Some("(o.ship_via = sh.shipper_id), (o.order_id = `<subquery4>`.order_id)"),
            ),
            (
                "Select #2 (subquery in projection; dependent)",
                "Select #2",
                None,
                None,
                Some("subquery in projection; dependent"),
            ),
            ("Materialize recursive CTE chain", "Materialize recursive CTE", Some("chain"), None, None),
            ("Update products (buffered)", "Update", Some("products"), None, Some("buffered")),
            ("Nested loop anti-join", "Nested loop anti-join", None, None, None),
            ("Hash", "Hash", None, None, None),
        ] {
            let n = one(line);
            assert_eq!(
                (n.operation.as_str(), n.object.as_deref(), n.index.as_deref(), n.condition.as_deref()),
                (op, object, index, condition),
                "{line}"
            );
        }
    }

    #[test]
    fn reads_estimates_and_actuals() {
        let n = one(
            "-> Table scan on <subquery4>  (cost=58.6..64.5 rows=269) (actual time=244e-6..0.0983 rows=33.7 loops=12)",
        );
        assert_eq!(n.estimate, Some(Estimate { rows: 269.0, cost: Some(Cost { startup: Some(58.6), total: 64.5 }) }));
        assert_eq!(
            n.actual,
            Some(Actual {
                rows: Some(33.7),
                loops: 12,
                time: Some(ActualTime { first_row_ms: Some(244e-6), last_row_ms: 0.0983 })
            })
        );
        let keys: Vec<&str> = n.attributes.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["step", "cost", "rows", "actual time", "actual rows", "actual loops"]);

        let estimate_only = one("-> Stream results  (cost=757 rows=1616)");
        assert_eq!(estimate_only.estimate.unwrap().cost, Some(Cost { startup: None, total: 757.0 }));
        assert_eq!(estimate_only.actual, None);

        let actual_only = one("-> Filter: (count(distinct o.order_id) >= 3)  (actual time=2.85..2.92 rows=49 loops=1)");
        assert_eq!(actual_only.estimate, None);
        assert_eq!(actual_only.condition.as_deref(), Some("count(distinct o.order_id) >= 3"));
        assert_eq!(actual_only.actual.unwrap().rows, Some(49.0));

        let never = one("-> Index lookup on t using k (a=1)  (cost=0.35 rows=1) (never executed)");
        assert_eq!(never.actual, Some(Actual { rows: Some(0.0), loops: 0, time: None }));
        assert_eq!(never.condition.as_deref(), Some("a=1"));
    }

    #[test]
    fn nests_by_indentation() {
        let plan = parse_text(
            "-> Limit: 1 row(s)\n    -> Nested loop inner join\n        -> Table scan on a\n        -> Hash\n            -> Table scan on b\n    -> Select #2 (subquery in condition)\n",
        );
        let ops: Vec<(usize, &str)> = plan.nodes().map(|(id, n)| (plan.depth(id), n.operation.as_str())).collect();
        assert_eq!(
            ops,
            [
                (0, "Limit"),
                (1, "Nested loop inner join"),
                (2, "Table scan"),
                (2, "Hash"),
                (3, "Table scan"),
                (1, "Select #2")
            ]
        );
        assert_eq!(plan.children(NodeId::ROOT).len(), 2);
        assert!(!plan.analyzed());
    }

    #[test]
    fn several_top_level_steps_share_a_root() {
        let plan = parse_text("-> Table scan on a\n-> Table scan on b\n    -> Hash\n");
        assert_eq!(plan.node(NodeId::ROOT).operation, "Query");
        assert_eq!(plan.children(NodeId::ROOT).len(), 2);
        assert_eq!(plan.node_count(), 4);
    }

    #[test]
    fn continuation_lines_stay_with_their_step() {
        let plan =
            parse_text("-> Filter: (t.note = 'a\nb')  (cost=1 rows=2)\n    -> Table scan on t  (cost=1 rows=2)\n");
        assert_eq!(plan.node_count(), 2);
        assert_eq!(plan.node(NodeId::ROOT).condition.as_deref(), Some("t.note = 'a\nb'"));
        assert_eq!(plan.node(NodeId::ROOT).estimate.unwrap().rows, 2.0);
    }

    #[test]
    fn only_tree_text_is_claimed() {
        use crate::model::ColumnMeta;
        use crate::server::Version;
        let result = |text: &str| ResultSet {
            columns: vec![ColumnMeta::new("EXPLAIN", "VARCHAR")],
            rows: vec![vec![Value::text(text)].into()],
            truncated: false,
        };
        let mysql = ServerInfo::new(Flavor::MySql, Version::new(8, 4, 0));
        assert!(matches!(parse(&mysql, &result("-> Table scan on t")), Some(Ok(_))));
        assert!(parse(&mysql, &result("<not executable by iterator executor>")).is_none());
        assert!(parse(&mysql, &result("{\"query_block\": {}}")).is_none());
        let mariadb = ServerInfo::new(Flavor::MariaDb, Version::new(11, 4, 0));
        assert!(parse(&mariadb, &result("-> Table scan on t")).is_none());
    }
}
