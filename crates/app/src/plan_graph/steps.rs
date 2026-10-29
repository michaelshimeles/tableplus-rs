//! What each step of the graph shows, worked out once per plan and collapse state.

use tableplus_core::plan::{Access, Extra, NodeId, Plan, SPAN_SLACK};

use super::layout::Layout;
use super::paint::legend_class;
use crate::widgets::{fmt_count, fmt_ms, fmt_rows};

pub(super) struct Step {
    pub(super) access: Option<Access>,
    pub(super) join: bool,
    /// Rows the step passes up over all its loops, or the estimate: its edge's width and label.
    pub(super) flow: Option<f64>,
    /// `287 rows`, `≈ 1,616 rows`, `never ran`.
    pub(super) rows: String,
    /// `× 830` when the step ran more than once.
    pub(super) loops: Option<String>,
    /// Time including the steps below, or the estimated cost.
    pub(super) value: Option<String>,
    /// Share of the statement's time with and without the steps below, or of the plan's cost.
    pub(super) bar: Option<(f32, f32)>,
    /// `Estimated 1,616 rows, cost 757`, for the tooltip.
    pub(super) estimate: Option<String>,
    /// `temp table`, `filesort`: side work the colour doesn't show.
    pub(super) extras: Vec<Extra>,
    pub(super) misestimate: Option<f64>,
    pub(super) descendants: usize,
}

pub(super) struct Prepared {
    /// Every node's id, by index, so per-node vectors map back to the plan.
    pub(super) ids: Vec<NodeId>,
    pub(super) layout: Layout,
    pub(super) steps: Vec<Step>,
    pub(super) analyzed: bool,
    /// `20 rows · 9.38 ms`, on the cap above the root.
    pub(super) result: String,
    pub(super) legend: Vec<Access>,
    pub(super) has_joins: bool,
}

pub(super) fn prepare(plan: &Plan, collapsed: &[bool]) -> Prepared {
    let analyzed = plan.analyzed();
    let cost_spans = plan.cost_spans();
    let scale = if analyzed { plan.total_ms().unwrap_or(0.0) } else { cost_spans[NodeId::ROOT.index()] };
    let share = |v: f64| if scale > 0.0 { (v / scale).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let steps: Vec<Step> = plan
        .nodes()
        .map(|(id, node)| {
            let (rows, loops, value, bar) = if analyzed {
                let inclusive = plan.inclusive_ms(id);
                let rows = match node.actual {
                    Some(a) if a.loops == 0 => "never ran".to_string(),
                    Some(a) => a.rows.map(rows_text).unwrap_or_default(),
                    None => String::new(),
                };
                let loops = node.actual.filter(|a| a.loops > 1).map(|a| format!("× {}", fmt_count(a.loops)));
                let bar = inclusive.map(|t| (share(t), share(plan.self_ms(id).unwrap_or(0.0))));
                (rows, loops, inclusive.map(fmt_ms), bar)
            } else {
                let rows = plan.estimated_rows(id).map(|rows| format!("≈ {}", rows_text(rows)));
                let cost = plan.estimated_cost(id);
                let own = cost.map_or(0.0, |c| c.total);
                let span = cost_spans[id.index()];
                let bar = (span > 0.0).then(|| (share(if span > own * SPAN_SLACK { span } else { own }), share(own)));
                (rows.unwrap_or_default(), None, cost.map(|c| format!("cost {}", fmt_rows(c.total))), bar)
            };
            let estimate_parts: Vec<String> = plan
                .estimated_rows(id)
                .map(rows_text)
                .into_iter()
                .chain(plan.estimated_cost(id).map(|c| format!("cost {}", fmt_rows(c.total))))
                .collect();
            Step {
                estimate: (!estimate_parts.is_empty()).then(|| format!("Estimated {}", estimate_parts.join(", "))),
                access: plan.access(id),
                extras: plan.extras(id),
                join: node.is_join(),
                flow: flow(plan, id, analyzed),
                rows,
                loops,
                value,
                bar,
                misestimate: plan.row_estimate_ratio(id).filter(|_| plan.is_misestimated(id)),
                descendants: plan.descendant_count(id),
            }
        })
        .collect();
    let root = &steps[NodeId::ROOT.index()];
    let result = match (root.flow, plan.total_ms()) {
        (Some(rows), Some(ms)) => format!("{} · {}", rows_text(rows), fmt_ms(ms)),
        (Some(rows), None) => format!("≈ {}", rows_text(rows)),
        (None, Some(ms)) => fmt_ms(ms),
        (None, None) => String::new(),
    };
    let order =
        [Access::FullScan, Access::IndexScan, Access::Range, Access::Lookup, Access::Constant, Access::InternalResult];
    let legend = order.into_iter().filter(|a| steps.iter().any(|s| s.access.map(legend_class) == Some(*a))).collect();
    Prepared {
        ids: plan.nodes().map(|(id, _)| id).collect(),
        layout: Layout::new(plan, collapsed),
        has_joins: steps.iter().any(|s| s.join),
        steps,
        analyzed,
        result,
        legend,
    }
}

pub(super) fn rows_text(rows: f64) -> String {
    format!("{} {}", fmt_rows(rows), if rows == 1.0 { "row" } else { "rows" })
}

/// Rows a step passes up over all its loops, or its estimate. A step without numbers, such as
/// `Hash`, passes on its children's; a step MySQL left unestimated, such as a limit, has none.
pub(super) fn flow(plan: &Plan, id: NodeId, analyzed: bool) -> Option<f64> {
    let node = plan.node(id);
    let own =
        if analyzed { node.actual.and_then(|a| a.rows.map(|r| r * a.loops as f64)) } else { plan.estimated_rows(id) };
    match own {
        Some(rows) => Some(rows),
        None if !analyzed && node.estimate.is_some() => None,
        None => {
            let below: Vec<f64> = plan.children(id).iter().filter_map(|c| flow(plan, *c, analyzed)).collect();
            (!below.is_empty()).then(|| below.iter().sum())
        }
    }
}
