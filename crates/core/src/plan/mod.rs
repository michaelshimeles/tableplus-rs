//! Query plans: one tree model for every engine's EXPLAIN output, the statements that ask a
//! server for a plan ([`request()`]), and the parsers that read its output ([`parse`]).

mod access;
mod facts;
mod mariadb_json;
mod mysql_json;
mod mysql_tree;
mod postgres_json;
mod request;
mod sqlite_eqp;
mod sqlite_scanstatus;

pub use access::{Access, Extra};
pub use facts::{FactKind, FactValue, PlanFact, Trigger};
pub use request::{Explain, ExplainMode, ExplainStatements, Unsupported, asks_for_plan, request};
pub(crate) use sqlite_scanstatus::{COLUMNS as SQLITE_SCAN_STATUS_COLUMNS, STATEMENT_ID as SQLITE_STATEMENT_ID};

use crate::model::ResultSet;
use crate::server::ServerInfo;

/// A node's position in its [`Plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(usize);

impl NodeId {
    pub const ROOT: NodeId = NodeId(0);

    pub fn index(self) -> usize {
        self.0
    }
}

/// The output format a plan was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanFormat {
    /// MySQL 8.0.18+ `EXPLAIN FORMAT=TREE` and `EXPLAIN ANALYZE` text (the 9.x default).
    MySqlTree,
    /// MySQL `EXPLAIN FORMAT=JSON`, version 1.
    MySqlJson,
    /// MySQL `EXPLAIN [ANALYZE] FORMAT=JSON` with `explain_json_format_version = 2`, the 9.x
    /// default.
    MySqlJsonV2,
    MariaDbJson,
    PostgresJson,
    /// SQLite `EXPLAIN QUERY PLAN` rows.
    SqliteQueryPlan,
    /// What SQLite measured per plan step while the statement ran (`sqlite3_stmt_scanstatus_v2`).
    /// Step times are shares of counted CPU cycles, not timed.
    SqliteScanStatus,
}

impl PlanFormat {
    pub fn label(self) -> &'static str {
        match self {
            PlanFormat::MySqlTree => "MySQL tree",
            PlanFormat::MySqlJson => "MySQL JSON",
            PlanFormat::MySqlJsonV2 => "MySQL JSON version 2",
            PlanFormat::MariaDbJson => "MariaDB JSON",
            PlanFormat::PostgresJson => "PostgreSQL JSON",
            PlanFormat::SqliteQueryPlan => "SQLite query plan",
            PlanFormat::SqliteScanStatus => "SQLite scan status (times from CPU cycles)",
        }
    }
}

/// One key and value exactly as the engine reported it, for the detail panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub key: String,
    pub value: String,
}

impl Attribute {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self { key: key.into(), value: value.into() }
    }
}

/// The planner's cost, in the engine's own units. Costs include the node's children.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cost {
    /// Cost before the first row, where the engine reports one (`cost=2.6..2.6`).
    pub startup: Option<f64>,
    pub total: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// Rows per loop.
    pub rows: f64,
    pub cost: Option<Cost>,
}

/// Measured time per loop, including the node's children.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActualTime {
    /// Milliseconds to the first row; `None` when the engine reports only a duration (MariaDB).
    pub first_row_ms: Option<f64>,
    pub last_row_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Actual {
    /// Rows per loop, averaged over the loops, so it can be fractional. `None` for a step the
    /// engine times without counting its rows, such as a MariaDB query block.
    pub rows: Option<f64>,
    pub loops: u64,
    /// `None` when the engine counted rows without timing them.
    pub time: Option<ActualTime>,
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    /// What the step does: `Nested loop inner join`, `Seq Scan`, `SCAN`…
    pub operation: String,
    /// How the step feeds its parent when it is not one of its inputs, such as PostgreSQL's
    /// `SubPlan 1` or `CTE chain`: a subquery the parent runs for an expression.
    pub role: Option<String>,
    /// The table, CTE or temporary result it reads.
    pub object: Option<String>,
    pub index: Option<String>,
    /// How the engine reaches the rows, in its own words: `ALL`, `ref` or `eq_ref` in MySQL's
    /// JSON version 1, `table`, `index` or `join` in version 2.
    pub access_type: Option<String>,
    /// Join, filter, sort or lookup condition, as the engine prints it.
    pub condition: Option<String>,
    pub estimate: Option<Estimate>,
    pub actual: Option<Actual>,
    /// How many processes ran the loops side by side: PostgreSQL's parallel workers and leader
    /// below a Gather, else 1.
    pub processes: u32,
    /// Everything the engine reported for this node, in its order and wording.
    pub attributes: Vec<Attribute>,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    depth: usize,
}

/// Actual rows this many times above or below the estimate count as a misestimate.
pub const MISESTIMATE_FACTOR: f64 = 10.0;

/// Engines print three significant digits, so a [`Plan::spans`] value only this much over the
/// step's own is rounding.
pub const SPAN_SLACK: f64 = 1.05;

impl PlanNode {
    pub fn new(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            role: None,
            object: None,
            index: None,
            access_type: None,
            condition: None,
            estimate: None,
            actual: None,
            processes: 1,
            attributes: Vec::new(),
            parent: None,
            children: Vec::new(),
            depth: 0,
        }
    }
}

/// A parsed plan: a tree of [`PlanNode`]s whose root is [`NodeId::ROOT`].
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub format: PlanFormat,
    /// Plan-wide values, such as planning time.
    pub attributes: Vec<Attribute>,
    /// The server's output as text, for the raw view.
    pub raw: String,
    nodes: Vec<PlanNode>,
}

impl Plan {
    pub fn new(format: PlanFormat, root: PlanNode, raw: impl Into<String>) -> Self {
        Self { format, attributes: Vec::new(), raw: raw.into(), nodes: vec![root] }
    }

    /// Appends `node` as the last child of `parent`.
    pub fn add_child(&mut self, parent: NodeId, mut node: PlanNode) -> NodeId {
        let id = NodeId(self.nodes.len());
        node.parent = Some(parent);
        node.depth = self.nodes[parent.0].depth + 1;
        node.children.clear();
        self.nodes[parent.0].children.push(id);
        self.nodes.push(node);
        id
    }

    pub fn node(&self, id: NodeId) -> &PlanNode {
        &self.nodes[id.0]
    }

    /// Every node with its id, parents before their children.
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = (NodeId, &PlanNode)> {
        self.nodes.iter().enumerate().map(|(i, n)| (NodeId(i), n))
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id.0].children
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id.0].parent
    }

    pub fn depth(&self, id: NodeId) -> usize {
        self.nodes[id.0].depth
    }

    pub fn descendant_count(&self, id: NodeId) -> usize {
        self.children(id).iter().map(|c| 1 + self.descendant_count(*c)).sum()
    }

    /// True when the statement ran and the plan carries measured actuals.
    pub fn analyzed(&self) -> bool {
        self.nodes.iter().any(|n| n.actual.is_some())
    }

    /// MySQL prints `rows=0`, and often `cost=0`, on steps it doesn't estimate: temporary tables,
    /// and the sort and limit above them. Elsewhere zero rows is a real estimate, such as
    /// PostgreSQL's for an UPDATE without RETURNING.
    fn unestimated(&self, estimate: &Estimate) -> bool {
        estimate.rows == 0.0
            && matches!(self.format, PlanFormat::MySqlTree | PlanFormat::MySqlJson | PlanFormat::MySqlJsonV2)
    }

    /// Rows per loop the planner expected, without MySQL's `rows=0` placeholder.
    pub fn estimated_rows(&self, id: NodeId) -> Option<f64> {
        let estimate = self.node(id).estimate?;
        (!self.unestimated(&estimate)).then_some(estimate.rows)
    }

    /// The planner's cost, without the `cost=0` MySQL prints beside its `rows=0` placeholder.
    pub fn estimated_cost(&self, id: NodeId) -> Option<Cost> {
        let estimate = self.node(id).estimate?;
        estimate.cost.filter(|c| c.total != 0.0 || !self.unestimated(&estimate))
    }

    /// Actual rows divided by estimated rows, per loop. Both count as at least one row, so a
    /// guess of 0.3 rows that turns out to be 0.03 is not a misestimate. `None` without both
    /// numbers or when the step never ran.
    pub fn row_estimate_ratio(&self, id: NodeId) -> Option<f64> {
        let estimated = self.estimated_rows(id)?;
        let actual = self.node(id).actual.filter(|a| a.loops > 0)?.rows?;
        Some(actual.max(1.0) / estimated.max(1.0))
    }

    pub fn is_misestimated(&self, id: NodeId) -> bool {
        self.row_estimate_ratio(id).is_some_and(|r| r >= MISESTIMATE_FACTOR || r <= 1.0 / MISESTIMATE_FACTOR)
    }

    /// Milliseconds spent in the node and its children over all loops: the time to the last row
    /// per loop, times the loop count, shared among the processes that ran them.
    pub fn inclusive_ms(&self, id: NodeId) -> Option<f64> {
        let node = self.node(id);
        let actual = node.actual?;
        Some(actual.time?.last_row_ms * actual.loops as f64 / node.processes as f64)
    }

    /// Milliseconds spent in the node itself: its inclusive time minus its children's, never
    /// below zero. A child without its own time, such as MySQL's `-> Hash`, passes on the time
    /// of its children.
    pub fn self_ms(&self, id: NodeId) -> Option<f64> {
        Some((self.inclusive_ms(id)? - self.children_ms(id)).max(0.0))
    }

    /// The longest time any step reports, counting an untimed step as the steps below it. Usually
    /// the root's, but MySQL 8.0.18 reports 70 µs for a limit above a union that takes
    /// milliseconds to materialize.
    pub fn total_ms(&self) -> Option<f64> {
        let longest = self.nodes().map(|(id, _)| self.inclusive_ms(id).unwrap_or_else(|| self.children_ms(id)));
        let timed = self.nodes.iter().any(|n| n.actual.and_then(|a| a.time).is_some());
        timed.then(|| longest.fold(0.0, f64::max))
    }

    /// Each step's `value`, or the sum of the steps below it when that is more, by node index.
    /// MySQL leaves a temporary table's cost out of the steps that read it, so nested numbers
    /// don't always add up.
    pub fn spans(&self, value: impl Fn(NodeId) -> Option<f64>) -> Vec<f64> {
        let mut spans = vec![0.0; self.nodes.len()];
        // Children come after their parent, so walking backwards sees them first.
        for i in (0..self.nodes.len()).rev() {
            let below: f64 = self.nodes[i].children.iter().map(|c| spans[c.0]).sum();
            spans[i] = value(NodeId(i)).map_or(below, |v| v.max(below));
        }
        spans
    }

    /// [`Plan::spans`] of the estimated cost. The root's is what the whole plan costs, where a
    /// MySQL plan's own numbers leave out every temporary table below a limit or sort.
    pub fn cost_spans(&self) -> Vec<f64> {
        self.spans(|id| self.estimated_cost(id).map(|c| c.total))
    }

    fn children_ms(&self, id: NodeId) -> f64 {
        self.children(id).iter().map(|c| self.inclusive_ms(*c).unwrap_or_else(|| self.children_ms(*c))).sum()
    }
}

/// Why a result that looks like a plan could not be read.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("could not read the {} plan: {message}", format.label())]
pub struct ParseError {
    pub format: PlanFormat,
    pub message: String,
}

type Parser = fn(&ServerInfo, &ResultSet) -> Option<Result<Plan, ParseError>>;

/// Every format reader. Each recognizes its own result shape and returns `None` for others.
const PARSERS: &[Parser] = &[
    mysql_tree::parse,
    mysql_json::parse_v1,
    mysql_json::parse_v2,
    mariadb_json::parse,
    postgres_json::parse,
    sqlite_eqp::parse,
    sqlite_scanstatus::parse,
];

/// Reads a statement's result as a query plan. `None` when the result isn't a plan in a format
/// with a parser; the caller then shows it as rows or as [`raw_text`].
pub fn parse(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    PARSERS.iter().find_map(|parser| parser(server, result))
}

/// `(a = b)` → `a = b`, but `(a = b), (c = d)` stays as it is.
fn strip_outer_parens(text: &str) -> &str {
    if !(text.starts_with('(') && text.ends_with(')')) {
        return text;
    }
    let mut depth = 0;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 && i < text.len() - 1 {
            return text;
        }
    }
    &text[1..text.len() - 1]
}

/// A result as plain text: the values of a one-column result line by line, otherwise aligned
/// columns under a header.
pub fn raw_text(result: &ResultSet) -> String {
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(|v| v.to_text().unwrap_or_else(|| "NULL".into())).collect())
        .collect();
    if result.columns.len() == 1 {
        return cells.into_iter().map(|mut row| row.remove(0)).collect::<Vec<_>>().join("\n");
    }
    let header: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|c| {
            std::iter::once(&header)
                .chain(&cells)
                .map(|row| row.get(c).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    std::iter::once(&header)
        .chain(&cells)
        .map(|row| {
            let line: Vec<String> = row.iter().zip(&widths).map(|(s, w)| format!("{s:<w$}")).collect();
            line.join("  ").trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;
    use crate::value::Value;

    fn timed(op: &str, first: f64, last: f64, rows: f64, loops: u64) -> PlanNode {
        let mut n = PlanNode::new(op);
        n.actual = Some(Actual {
            rows: Some(rows),
            loops,
            time: Some(ActualTime { first_row_ms: Some(first), last_row_ms: last }),
        });
        n
    }

    /// join (10 ms) ─┬─ scan (2 ms) ── hash (untimed) ── scan (3 ms)
    ///               └─ lookup (0.01 ms × 100 loops)
    fn sample() -> (Plan, [NodeId; 5]) {
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("join", 1.0, 10.0, 50.0, 1), "");
        let scan = plan.add_child(NodeId::ROOT, timed("scan", 0.1, 2.0, 100.0, 1));
        let hash = plan.add_child(NodeId::ROOT, PlanNode::new("hash"));
        let inner = plan.add_child(hash, timed("scan", 0.1, 3.0, 20.0, 1));
        let lookup = plan.add_child(NodeId::ROOT, timed("lookup", 0.005, 0.01, 1.0, 100));
        (plan, [NodeId::ROOT, scan, hash, inner, lookup])
    }

    #[test]
    fn tree_shape() {
        let (plan, [root, scan, hash, inner, lookup]) = sample();
        assert_eq!(plan.node_count(), 5);
        assert_eq!(plan.children(root), &[scan, hash, lookup]);
        assert_eq!(plan.children(hash), &[inner]);
        assert_eq!(plan.parent(inner), Some(hash));
        assert_eq!(plan.parent(root), None);
        assert_eq!((plan.depth(root), plan.depth(hash), plan.depth(inner)), (0, 1, 2));
        assert_eq!(plan.descendant_count(root), 4);
        assert_eq!(plan.descendant_count(hash), 1);
        assert!(plan.analyzed());
    }

    #[test]
    fn inclusive_time_multiplies_loops() {
        let (plan, [root, _, hash, _, lookup]) = sample();
        assert_eq!(plan.inclusive_ms(root), Some(10.0));
        assert!((plan.inclusive_ms(lookup).unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(plan.inclusive_ms(hash), None);
        assert_eq!(plan.total_ms(), Some(10.0));
    }

    #[test]
    fn total_time_is_the_longest_step_not_the_root() {
        // MySQL 8.0.18: `Limit` 72 µs over a union materialized in 14.6 ms, from branches of
        // 9.53 and 5.01 ms. A dependent subquery's 4 ms is already inside the first branch.
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("limit", 0.0, 0.072, 20.0, 1), "");
        let scan = plan.add_child(NodeId::ROOT, timed("scan union temporary", 0.0, 0.009, 67.0, 1));
        let union = plan.add_child(scan, timed("union materialize", 14.5, 14.6, 20.0, 1));
        plan.add_child(union, timed("filter", 4.0, 9.53, 49.0, 1));
        let subquery = plan.add_child(union, PlanNode::new("select #2"));
        plan.add_child(subquery, timed("aggregate", 0.0, 0.014, 1.0, 287));
        plan.add_child(union, timed("filter", 1.9, 5.01, 18.0, 1));
        assert_eq!(plan.total_ms(), Some(14.6));
        assert!(plan.nodes().all(|(id, _)| plan.inclusive_ms(id).is_none_or(|ms| ms <= 14.6)));
    }

    #[test]
    fn spans_cover_steps_below_that_report_more() {
        let mut plan = Plan::new(PlanFormat::MySqlTree, PlanNode::new("limit"), "");
        let union = plan.add_child(NodeId::ROOT, PlanNode::new("union"));
        let a = plan.add_child(union, PlanNode::new("a"));
        plan.add_child(a, PlanNode::new("a1"));
        plan.add_child(union, PlanNode::new("b"));
        // limit 2.85 · union none · a 757 over a1 909 · b 31,389
        let values = [Some(2.85), None, Some(757.0), Some(909.0), Some(31389.0)];
        assert_eq!(plan.spans(|id| values[id.index()]), [32298.0, 32298.0, 909.0, 909.0, 31389.0]);
        assert_eq!(plan.spans(|_| None), [0.0; 5]);
    }

    #[test]
    fn self_time_subtracts_children_through_untimed_nodes() {
        let (plan, [root, scan, hash, inner, _]) = sample();
        // 10 - (2 + 3 through the untimed hash + 100 × 0.01)
        assert!((plan.self_ms(root).unwrap() - 4.0).abs() < 1e-9);
        assert_eq!(plan.self_ms(scan), Some(2.0));
        assert_eq!(plan.self_ms(inner), Some(3.0));
        assert_eq!(plan.self_ms(hash), None);
    }

    #[test]
    fn self_time_never_goes_negative() {
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("limit", 0.0, 0.03, 20.0, 1), "");
        plan.add_child(NodeId::ROOT, timed("materialize", 6.0, 6.6, 20.0, 1));
        assert_eq!(plan.self_ms(NodeId::ROOT), Some(0.0));
    }

    #[test]
    fn total_time_counts_an_untimed_step_as_the_steps_below_it() {
        let mut plan = Plan::new(PlanFormat::MariaDbJson, PlanNode::new("union"), "");
        plan.add_child(NodeId::ROOT, timed("block", 0.0, 1.5, 10.0, 1));
        plan.add_child(NodeId::ROOT, timed("block", 0.0, 0.5, 10.0, 1));
        assert_eq!(plan.total_ms(), Some(2.0));
        let estimate_only = Plan::new(PlanFormat::MySqlTree, PlanNode::new("scan"), "");
        assert_eq!(estimate_only.total_ms(), None);
        assert!(!estimate_only.analyzed());
    }

    #[test]
    fn row_estimate_ratio() {
        let with = |estimated: f64, actual: f64, loops: u64| {
            let mut n = timed("scan", 0.0, 1.0, actual, loops);
            n.estimate = Some(Estimate { rows: estimated, cost: None });
            Plan::new(PlanFormat::MySqlTree, n, "")
        };
        let ratio = |plan: Plan| plan.row_estimate_ratio(NodeId::ROOT);
        let misestimated = |plan: Plan| plan.is_misestimated(NodeId::ROOT);
        assert_eq!(ratio(with(10.0, 100.0, 1)), Some(10.0));
        assert!(misestimated(with(10.0, 100.0, 1)));
        assert!(misestimated(with(1000.0, 99.0, 1)));
        assert!(!misestimated(with(1000.0, 101.0, 1)));
        // MySQL's `rows=0` on a union's temporary table means no estimate.
        assert_eq!(ratio(with(0.0, 67.0, 1)), None);
        assert_eq!(ratio(with(0.5, 67.0, 1)), Some(67.0));
        // Fractions of a row are not a misestimate.
        assert!(!misestimated(with(0.333, 0.01, 1083)));
        assert_eq!(ratio(with(5.0, 0.0, 0)), None);
        assert_eq!(ratio(Plan::new(PlanFormat::MySqlTree, timed("scan", 0.0, 1.0, 5.0, 1), "")), None);
        let mut uncounted = with(10.0, 0.0, 1);
        uncounted.nodes[0].actual.as_mut().unwrap().rows = None;
        assert_eq!(ratio(uncounted), None);
    }

    #[test]
    fn mysql_zero_rows_is_no_estimate_but_postgres_zero_is() {
        let step = |format: PlanFormat, rows: f64, cost: f64| {
            let mut n = PlanNode::new("step");
            n.estimate = Some(Estimate { rows, cost: Some(Cost { startup: None, total: cost }) });
            Plan::new(format, n, "")
        };
        let numbers =
            |plan: Plan| (plan.estimated_rows(NodeId::ROOT), plan.estimated_cost(NodeId::ROOT).map(|c| c.total));
        // `Union all materialize  (cost=0..0 rows=0)` and `Limit: 20 row(s)  (cost=2.85..2.85 rows=0)`.
        for format in [PlanFormat::MySqlTree, PlanFormat::MySqlJson, PlanFormat::MySqlJsonV2] {
            assert_eq!(numbers(step(format, 0.0, 0.0)), (None, None), "{format:?}");
            assert_eq!(numbers(step(format, 0.0, 2.85)), (None, Some(2.85)), "{format:?}");
            assert_eq!(numbers(step(format, 0.33, 0.66)), (Some(0.33), Some(0.66)), "{format:?}");
        }
        // PostgreSQL's `Update on products  (cost=0.00..44.18 rows=0)` returns nothing, by plan.
        assert_eq!(numbers(step(PlanFormat::PostgresJson, 0.0, 44.18)), (Some(0.0), Some(44.18)));
        assert_eq!(numbers(step(PlanFormat::PostgresJson, 0.0, 0.0)), (Some(0.0), Some(0.0)));
        assert_eq!(numbers(Plan::new(PlanFormat::MySqlTree, PlanNode::new("Hash"), "")), (None, None));
    }

    #[test]
    fn raw_text_lines_and_columns() {
        let one = ResultSet {
            columns: vec![ColumnMeta::new("QUERY PLAN", "text")],
            rows: vec![vec![Value::text("Seq Scan on t")].into(), vec![Value::text("  Filter: (a > 1)")].into()],
            truncated: false,
        };
        assert_eq!(raw_text(&one), "Seq Scan on t\n  Filter: (a > 1)");
        let table = ResultSet {
            columns: vec![ColumnMeta::new("id", ""), ColumnMeta::new("detail", "")],
            rows: vec![vec![Value::Int(4), Value::text("SCAN t")].into(), vec![Value::Int(15), Value::Null].into()],
            truncated: false,
        };
        assert_eq!(raw_text(&table), "id  detail\n4   SCAN t\n15  NULL");
    }
}
