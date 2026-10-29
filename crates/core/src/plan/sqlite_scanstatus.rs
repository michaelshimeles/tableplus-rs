//! What SQLite measured for each plan step while a statement ran, read by the SQLite driver
//! through `sqlite3_stmt_scanstatus_v2`: a row for the whole statement, whose `nvisit` is the
//! rows it returned, then one per `EXPLAIN QUERY PLAN` step.
//!
//! ```text
//! id  parent  detail          nloop  nvisit  est   ncycle   ms
//! 0   NULL    NULL            1      20      NULL  3177103  3.236
//! 8   6       CO-ROUTINE t    NULL   NULL    NULL  NULL     NULL
//! 15  8       SCAN od         1      2155    2048  483354   NULL
//! 66  6       SCAN t          1      830     96    653190   NULL
//! ```
//!
//! A step's rows per loop are `nvisit / nloop`, and its time is its share of the statement's
//! `ncycle`, scaled to the statement's measured `ms`. `est` is an estimate only for `SCAN` and
//! `SEARCH` steps. A co-routine reports nothing, so it takes the numbers of the `SCAN` that reads
//! it. `docs/query-plans.md` describes which instructions each step's counters cover.

use std::collections::HashMap;

use super::sqlite_eqp::{is_loop, node};
use super::{Actual, ActualTime, Attribute, Estimate, NodeId, ParseError, Plan, PlanFormat, PlanNode, raw_text};
use crate::model::ResultSet;
use crate::server::{Flavor, ServerInfo};
use crate::value::Value;

pub(crate) const COLUMNS: [&str; 8] = ["id", "parent", "detail", "nloop", "nvisit", "est", "ncycle", "ms"];

/// The id of the statement's row. `EXPLAIN QUERY PLAN` gives top-level steps this parent.
pub(crate) const STATEMENT_ID: i64 = 0;

pub(super) fn parse(server: &ServerInfo, result: &ResultSet) -> Option<Result<Plan, ParseError>> {
    let names = result.columns.iter().map(|c| c.name.as_str());
    if server.flavor != Flavor::Sqlite || !names.eq(COLUMNS) {
        return None;
    }
    Some(parse_rows(result))
}

/// `None` where SQLite reported nothing.
#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    loops: Option<u64>,
    visits: Option<u64>,
    cycles: Option<u64>,
}

struct Row {
    id: i64,
    parent: i64,
    detail: String,
    counts: Counts,
    estimate: Option<f64>,
    ms: Option<f64>,
}

fn read(row: &[Value]) -> Option<Row> {
    let count = |col: usize| match &row[col] {
        Value::Int(n) => u64::try_from(*n).ok(),
        other => other.to_text()?.trim().parse().ok(),
    };
    let float = |col: usize| match &row[col] {
        Value::Float(f) => Some(*f),
        Value::Int(n) => Some(*n as f64),
        other => other.to_text()?.trim().parse().ok(),
    };
    let int = |col: usize| match &row[col] {
        Value::Int(n) => Some(*n),
        other => other.to_text()?.trim().parse().ok(),
    };
    Some(Row {
        id: int(0)?,
        parent: int(1).unwrap_or(STATEMENT_ID),
        detail: row[2].to_text().unwrap_or_default(),
        counts: Counts { loops: count(3), visits: count(4), cycles: count(6) },
        estimate: float(5),
        ms: float(7),
    })
}

fn parse_rows(result: &ResultSet) -> Result<Plan, ParseError> {
    let error = |message: String| ParseError { format: PlanFormat::SqliteScanStatus, message };
    let rows = result
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| read(row).ok_or_else(|| error(format!("row {} has no numeric id", i + 1))))
        .collect::<Result<Vec<_>, _>>()?;
    let Some((statement, steps)) = rows.split_first().filter(|(s, _)| s.id == STATEMENT_ID) else {
        return Err(error("the first row is not the statement's".into()));
    };
    let (Some(returned), Some(ms)) = (statement.counts.visits, statement.ms) else {
        return Err(error("the statement has no row count or time".into()));
    };
    let total_cycles = statement.counts.cycles.unwrap_or(0);
    let ms_per_cycle = (total_cycles > 0).then(|| ms / total_cycles as f64);
    let timing = match ms_per_cycle {
        Some(_) => format!(
            "SQLite counted {total_cycles} CPU cycles in {ms:.3} ms. A step's time is its share of those cycles."
        ),
        None => "SQLite counted no CPU cycles on this platform, so steps have no times.".to_string(),
    };

    let mut nodes: Vec<PlanNode> = steps.iter().map(measured_node).collect();
    let mut counts: Vec<Counts> = steps.iter().map(|s| s.counts).collect();
    for (coroutine, reader) in coroutine_readers(steps, &nodes) {
        counts[coroutine] = counts[reader];
        counts[reader].cycles = None;
        nodes[coroutine].attributes.push(Attribute::new("numbers from", steps[reader].detail.as_str()));
    }

    let mut root = PlanNode::new("Query");
    root.actual = Some(Actual {
        rows: Some(returned as f64),
        loops: 1,
        time: Some(ActualTime { first_row_ms: None, last_row_ms: ms }),
    });
    root.attributes = vec![
        Attribute::new("rows returned", returned.to_string()),
        Attribute::new("ncycle", total_cycles.to_string()),
        Attribute::new("ms", format!("{ms:.3}")),
        Attribute::new("timing", timing.as_str()),
    ];
    let mut plan = Plan::new(PlanFormat::SqliteScanStatus, root, raw_text(result));
    plan.attributes.push(Attribute::new("timing", timing));

    let mut ids: HashMap<i64, NodeId> = HashMap::from([(STATEMENT_ID, NodeId::ROOT)]);
    for ((step, mut node), counts) in steps.iter().zip(nodes).zip(counts) {
        node.actual = actual(counts, ms_per_cycle);
        // SQLite lists a parent before its children; an unknown parent hangs from the root.
        let parent = ids.get(&step.parent).copied().unwrap_or(NodeId::ROOT);
        ids.insert(step.id, plan.add_child(parent, node));
    }
    Ok(plan)
}

/// The step as `EXPLAIN QUERY PLAN` describes it, with its estimate and SQLite's numbers as
/// reported.
fn measured_node(step: &Row) -> PlanNode {
    let mut node = node(&step.detail);
    if is_loop(&step.detail) {
        node.estimate = step.estimate.map(|rows| Estimate { rows, cost: None });
    }
    let Counts { loops, visits, cycles } = step.counts;
    for (key, value) in [
        ("nloop", loops.map(|n| n.to_string())),
        ("nvisit", visits.map(|n| n.to_string())),
        ("est", step.estimate.map(|e| e.to_string())),
        ("ncycle", cycles.map(|n| n.to_string())),
    ] {
        if let Some(value) = value {
            node.attributes.push(Attribute::new(key, value));
        }
    }
    node
}

/// Each `CO-ROUTINE x` step with the `SCAN x` beside it that reads the co-routine, as indexes
/// into `steps`.
fn coroutine_readers(steps: &[Row], nodes: &[PlanNode]) -> Vec<(usize, usize)> {
    let is_coroutine = |i: usize| steps[i].detail.trim_start().starts_with("CO-ROUTINE ");
    (0..steps.len())
        .filter(|&i| is_coroutine(i))
        .filter_map(|coroutine| {
            let reader = (0..steps.len()).find(|&j| {
                steps[j].parent == steps[coroutine].parent
                    && steps[j].detail.trim_start().starts_with("SCAN ")
                    && is_loop(&steps[j].detail)
                    && nodes[j].object == nodes[coroutine].object
            })?;
            Some((coroutine, reader))
        })
        .collect()
}

fn actual(counts: Counts, ms_per_cycle: Option<f64>) -> Option<Actual> {
    let loops = counts.loops?;
    let per_loop = |total: f64| if loops == 0 { 0.0 } else { total / loops as f64 };
    let time = match (counts.cycles, ms_per_cycle) {
        (Some(cycles), Some(k)) if loops > 0 => {
            Some(ActualTime { first_row_ms: None, last_row_ms: per_loop(cycles as f64 * k) })
        }
        _ => None,
    };
    Some(Actual { rows: counts.visits.map(|v| per_loop(v as f64)), loops, time })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ColumnMeta;
    use crate::server::Version;

    /// `(id, parent, detail, nloop, nvisit, est, ncycle)`, with -1 where SQLite reports nothing.
    type Step<'a> = (i64, i64, &'a str, i64, i64, f64, i64);

    /// A statement that returned `returned` rows in `ms` over `cycles` cycles, then `steps`.
    fn result(returned: i64, cycles: i64, ms: f64, steps: &[Step]) -> ResultSet {
        let int = |n: i64| if n < 0 { Value::Null } else { Value::Int(n) };
        let mut rows = vec![
            vec![
                Value::Int(0),
                Value::Null,
                Value::Null,
                Value::Int(1),
                Value::Int(returned),
                Value::Null,
                Value::Int(cycles),
                Value::Float(ms),
            ]
            .into(),
        ];
        for (id, parent, detail, nloop, nvisit, est, ncycle) in steps {
            rows.push(
                vec![
                    Value::Int(*id),
                    Value::Int(*parent),
                    Value::text(*detail),
                    int(*nloop),
                    int(*nvisit),
                    if *est < 0.0 { Value::Null } else { Value::Float(*est) },
                    int(*ncycle),
                    Value::Null,
                ]
                .into(),
            );
        }
        ResultSet { columns: COLUMNS.iter().map(|c| ColumnMeta::new(*c, "")).collect(), rows, truncated: false }
    }

    fn sqlite() -> ServerInfo {
        ServerInfo::new(Flavor::Sqlite, Version::new(3, 53, 2))
    }

    fn parsed(rs: &ResultSet) -> Plan {
        parse(&sqlite(), rs).expect("claimed").expect("parsed")
    }

    fn find<'a>(plan: &'a Plan, detail: &str) -> (NodeId, &'a PlanNode) {
        plan.nodes()
            .find(|(_, n)| n.attributes.first().is_some_and(|a| a.key == "detail" && a.value == detail))
            .unwrap_or_else(|| panic!("no step {detail}"))
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    /// The customer half of the `portable` query, with SQLite's numbers rounded: 3 ms over
    /// 3,000,000 cycles, so 1,000 cycles are a microsecond.
    fn portable() -> ResultSet {
        result(
            20,
            3_000_000,
            3.0,
            &[
                (6, 0, "LEFT", -1, -1, 1.0, -1),
                (8, 6, "CO-ROUTINE t", -1, -1, 1.0, -1),
                (15, 8, "SCAN od USING INDEX sqlite_autoindex_order_details_1", 1, 2155, 2048.0, 480_000),
                (66, 6, "SCAN t", 1, 830, 96.0, 650_000),
                (69, 6, "SEARCH o USING INTEGER PRIMARY KEY (rowid=?)", 830, 830, 0.9, 860_000),
                (75, 6, "CORRELATED SCALAR SUBQUERY 3", 678, -1, 1.0, 510_000),
                (
                    82,
                    75,
                    "SEARCH od2 USING COVERING INDEX sqlite_autoindex_order_details_1 (order_id=?)",
                    678,
                    1424,
                    3.0,
                    170_000,
                ),
                (86, 75, "SEARCH p2 USING INTEGER PRIMARY KEY (rowid=?)", 1424, 1424, 1.0, 200_000),
                (111, 6, "USE TEMP B-TREE FOR GROUP BY", 1, 287, 1.0, 80_000),
                (191, 6, "USE TEMP B-TREE FOR count(DISTINCT)", -1, -1, 1.0, -1),
            ],
        )
    }

    #[test]
    fn loops_get_rows_per_loop_estimates_and_their_share_of_the_time() {
        let plan = parsed(&portable());
        assert_eq!(plan.format, PlanFormat::SqliteScanStatus);
        assert!(plan.analyzed());
        let (id, o) = find(&plan, "SEARCH o USING INTEGER PRIMARY KEY (rowid=?)");
        assert_eq!((o.operation.as_str(), o.object.as_deref()), ("Search", Some("o")));
        let actual = o.actual.unwrap();
        assert_eq!((actual.rows, actual.loops), (Some(1.0), 830));
        assert_eq!(o.estimate, Some(Estimate { rows: 0.9, cost: None }));
        assert!(close(plan.inclusive_ms(id), 0.86), "860,000 of 3,000,000 cycles in 3 ms");
        assert!(close(actual.time.map(|t| t.last_row_ms), 0.86 / 830.0));
        assert_eq!(actual.time.unwrap().first_row_ms, None);

        let (_, od) = find(&plan, "SCAN od USING INDEX sqlite_autoindex_order_details_1");
        assert_eq!(od.actual.map(|a| (a.rows, a.loops)), Some((Some(2155.0), 1)));
        assert_eq!(od.estimate.map(|e| e.rows), Some(2048.0));
        assert_eq!(od.attributes[0], Attribute::new("detail", "SCAN od USING INDEX sqlite_autoindex_order_details_1"));
        for (key, value) in [("nloop", "1"), ("nvisit", "2155"), ("est", "2048"), ("ncycle", "480000")] {
            assert!(od.attributes.contains(&Attribute::new(key, value)), "{key}");
        }
    }

    #[test]
    fn the_root_is_the_whole_statement() {
        let plan = parsed(&portable());
        let root = plan.node(NodeId::ROOT);
        assert_eq!(root.actual.map(|a| (a.rows, a.loops)), Some((Some(20.0), 1)));
        assert_eq!(plan.total_ms(), Some(3.0));
        // 3 ms minus the timed steps below LEFT: co-routine 0.65, o 0.86, subquery 0.51, group by 0.08.
        assert!(close(plan.self_ms(NodeId::ROOT), 0.9));
        assert_eq!(plan.children(NodeId::ROOT).len(), 1);
        assert!(plan.attributes.iter().any(|a| a.key == "timing" && a.value.contains("3000000 CPU cycles")));
    }

    #[test]
    fn a_coroutine_takes_the_numbers_of_the_scan_that_reads_it() {
        let plan = parsed(&portable());
        let (coroutine, co) = find(&plan, "CO-ROUTINE t");
        let (scan, t) = find(&plan, "SCAN t");
        assert_eq!(co.actual.map(|a| (a.rows, a.loops)), Some((Some(830.0), 1)));
        assert!(close(plan.inclusive_ms(coroutine), 0.65));
        assert!(close(plan.self_ms(coroutine), 0.17), "its own work: 650,000 minus the 480,000 of SCAN od");
        assert!(co.attributes.contains(&Attribute::new("numbers from", "SCAN t")));
        assert_eq!(t.actual.map(|a| (a.rows, a.loops, a.time)), Some((Some(830.0), 1, None)));
        assert_eq!(t.estimate.map(|e| e.rows), Some(96.0));
        assert_eq!(plan.inclusive_ms(scan), None);

        let (left, _) = find(&plan, "LEFT");
        let below: f64 = plan.children(left).iter().filter_map(|c| plan.inclusive_ms(*c)).sum();
        assert!((below - 2.1).abs() < 1e-9, "the co-routine's 0.65 ms counted once, not again on SCAN t: {below}");
    }

    #[test]
    fn a_subquery_time_includes_its_children() {
        let plan = parsed(&portable());
        let (id, subquery) = find(&plan, "CORRELATED SCALAR SUBQUERY 3");
        let actual = subquery.actual.unwrap();
        assert_eq!((actual.rows, actual.loops), (None, 678));
        assert!(close(plan.inclusive_ms(id), 0.51));
        assert!(close(plan.self_ms(id), 0.14), "510,000 minus od2's 170,000 and p2's 200,000");
        assert_eq!(subquery.estimate, None, "SQLite's 1 is a default, not an estimate");
    }

    #[test]
    fn steps_without_loop_counts_carry_no_actuals() {
        let plan = parsed(&portable());
        for detail in ["LEFT", "USE TEMP B-TREE FOR count(DISTINCT)"] {
            let (_, n) = find(&plan, detail);
            assert!(n.actual.is_none() && n.estimate.is_none(), "{detail}");
        }
        let (_, group_by) = find(&plan, "USE TEMP B-TREE FOR GROUP BY");
        assert_eq!(group_by.actual.map(|a| (a.rows, a.loops)), Some((Some(287.0), 1)));
        assert_eq!(group_by.estimate, None);
    }

    #[test]
    fn steps_that_never_ran_have_no_time() {
        let plan = parsed(&result(0, 1000, 0.01, &[(2, 0, "SCAN t", 0, 0, 10.0, 0)]));
        let (id, t) = find(&plan, "SCAN t");
        assert_eq!(t.actual.map(|a| (a.rows, a.loops, a.time)), Some((Some(0.0), 0, None)));
        assert_eq!(plan.row_estimate_ratio(id), None);
        assert_eq!(plan.inclusive_ms(id), None);
    }

    #[test]
    fn without_counted_cycles_steps_have_counts_but_no_times() {
        let plan = parsed(&result(3, 0, 0.5, &[(2, 0, "SCAN t", 1, 3, 3.0, 0)]));
        let (_, t) = find(&plan, "SCAN t");
        assert_eq!(t.actual.map(|a| (a.rows, a.loops, a.time)), Some((Some(3.0), 1, None)));
        assert_eq!(plan.total_ms(), Some(0.5), "the statement's own time is measured, not derived");
        assert!(plan.attributes.iter().any(|a| a.key == "timing" && a.value.contains("no CPU cycles")));
    }

    #[test]
    fn only_measured_runs_are_claimed() {
        let rs = portable();
        assert!(super::super::sqlite_eqp::parse(&sqlite(), &rs).is_none());
        let mysql = ServerInfo::new(Flavor::MySql, Version::new(8, 4, 0));
        assert!(parse(&mysql, &rs).is_none());
        assert!(matches!(super::super::parse(&sqlite(), &rs), Some(Ok(p)) if p.format == PlanFormat::SqliteScanStatus));
    }

    #[test]
    fn a_result_without_the_statement_row_is_an_error() {
        let mut rs = portable();
        rs.rows.remove(0);
        assert!(matches!(parse(&sqlite(), &rs), Some(Err(_))));
    }
}
