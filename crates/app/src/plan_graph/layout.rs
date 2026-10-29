//! Where each step sits in the graph: a tidy tree with the result at the top, so rows flow upward.
//! A parent sits centred over its first and last child, and each subtree packs against its left
//! neighbour as closely as their row-by-row outlines allow (Reingold–Tilford).

use eframe::egui::{Rect, pos2, vec2};
use tableplus_core::plan::{NodeId, Plan};

pub const BOX_W: f32 = 208.0;
pub const JOIN_W: f32 = 176.0;
pub const NODE_H: f32 = 74.0;
pub const ROW_GAP: f32 = 66.0;
const SIBLING_GAP: f32 = 24.0;
/// Between subtrees of different parents, so separate branches read as separate.
const COUSIN_GAP: f32 = 48.0;

pub struct Layout {
    /// Each step's box in graph points, by node index; `None` below a collapsed step.
    pub rects: Vec<Option<Rect>>,
    /// The shown steps row by row, top to bottom, each row left to right.
    pub rows: Vec<Vec<NodeId>>,
    pub bounds: Rect,
}

impl Layout {
    pub fn new(plan: &Plan, collapsed: &[bool]) -> Self {
        let width = |id: NodeId| if plan.node(id).is_join() { JOIN_W } else { BOX_W };
        let mut offset = vec![0.0; plan.node_count()];
        place(plan, NodeId::ROOT, collapsed, &width, &mut offset);

        let mut rects = vec![None; plan.node_count()];
        let mut rows: Vec<Vec<NodeId>> = Vec::new();
        let mut stack = vec![(NodeId::ROOT, 0.0, 0)];
        while let Some((id, x, row)) = stack.pop() {
            let top = row as f32 * (NODE_H + ROW_GAP);
            rects[id.index()] = Some(Rect::from_min_size(pos2(x - width(id) / 2.0, top), vec2(width(id), NODE_H)));
            if rows.len() <= row {
                rows.push(Vec::new());
            }
            rows[row].push(id);
            if !collapsed[id.index()] {
                stack.extend(plan.children(id).iter().map(|c| (*c, x + offset[c.index()], row + 1)));
            }
        }
        for row in &mut rows {
            row.sort_by(|a, b| rects[a.index()].unwrap().left().total_cmp(&rects[b.index()].unwrap().left()));
        }
        let bounds = rects.iter().flatten().fold(Rect::NOTHING, |b, r| b.union(*r));
        Self { rects, rows, bounds }
    }

    pub fn rect(&self, id: NodeId) -> Option<Rect> {
        self.rects[id.index()]
    }

    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        let top = self.rect(id)?.top();
        Some((top / (NODE_H + ROW_GAP)).round() as usize)
    }
}

/// Lays out the subtree under `id`, writing each child's centre relative to its parent's into
/// `offset`. Returns the subtree's outline: each row's left and right edge, relative to `id`'s
/// centre, top row first.
fn place(
    plan: &Plan,
    id: NodeId,
    collapsed: &[bool],
    width: &impl Fn(NodeId) -> f32,
    offset: &mut [f32],
) -> Vec<(f32, f32)> {
    let half = width(id) / 2.0;
    let children = if collapsed[id.index()] { &[][..] } else { plan.children(id) };
    // The outline of the children placed so far, relative to the first child's centre.
    let mut below: Vec<(f32, f32)> = Vec::new();
    let mut xs = Vec::with_capacity(children.len());
    for &child in children {
        let outline = place(plan, child, collapsed, width, offset);
        let x = if below.is_empty() {
            0.0
        } else {
            below
                .iter()
                .zip(&outline)
                .enumerate()
                .map(|(row, (left, right))| left.1 + if row == 0 { SIBLING_GAP } else { COUSIN_GAP } - right.0)
                .fold(f32::MIN, f32::max)
        };
        for (row, (l, r)) in outline.into_iter().enumerate() {
            match below.get_mut(row) {
                Some(edge) => *edge = (edge.0.min(l + x), edge.1.max(r + x)),
                None => below.push((l + x, r + x)),
            }
        }
        xs.push(x);
    }
    let mid = match (xs.first(), xs.last()) {
        (Some(first), Some(last)) => (first + last) / 2.0,
        _ => 0.0,
    };
    for (child, x) in children.iter().zip(xs) {
        offset[child.index()] = x - mid;
    }
    std::iter::once((-half, half)).chain(below.into_iter().map(|(l, r)| (l - mid, r - mid))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tableplus_core::plan::{PlanFormat, PlanNode};

    fn join_chain(tables: usize) -> Plan {
        let mut plan = Plan::new(PlanFormat::MySqlTree, PlanNode::new("Aggregate"), "");
        let mut joins = Vec::new();
        let mut at = NodeId::ROOT;
        for _ in 1..tables {
            at = plan.add_child(at, PlanNode::new("Nested loop inner join"));
            joins.push(at);
        }
        plan.add_child(at, PlanNode::new("Table scan"));
        for join in joins {
            plan.add_child(join, PlanNode::new("Single-row index lookup"));
        }
        plan
    }

    /// limit ── join ─┬─ filter ── scan
    ///                ├─ hash ── join ─┬─ scan
    ///                │                └─ lookup
    ///                └─ lookup
    fn branching() -> Plan {
        let mut plan = Plan::new(PlanFormat::MySqlTree, PlanNode::new("Limit"), "");
        let join = plan.add_child(NodeId::ROOT, PlanNode::new("Inner hash join"));
        let filter = plan.add_child(join, PlanNode::new("Filter"));
        plan.add_child(filter, PlanNode::new("Table scan"));
        let hash = plan.add_child(join, PlanNode::new("Hash"));
        let inner = plan.add_child(hash, PlanNode::new("Nested loop inner join"));
        plan.add_child(inner, PlanNode::new("Table scan"));
        plan.add_child(inner, PlanNode::new("Index lookup"));
        plan.add_child(join, PlanNode::new("Single-row index lookup"));
        plan
    }

    fn shown(plan: &Plan, layout: &Layout) -> Vec<(NodeId, Rect)> {
        plan.nodes().filter_map(|(id, _)| Some((id, layout.rect(id)?))).collect()
    }

    fn assert_tidy(plan: &Plan, layout: &Layout) {
        let boxes = shown(plan, layout);
        for (i, (a, ra)) in boxes.iter().enumerate() {
            for (b, rb) in &boxes[i + 1..] {
                assert!(!ra.expand(SIBLING_GAP / 2.0 - 0.5).intersects(*rb), "{a:?} and {b:?} overlap");
            }
            if let Some(parent) = plan.parent(*a) {
                let rp = layout.rect(parent).unwrap();
                assert!(ra.top() > rp.bottom(), "{a:?} sits below its parent");
                let kids = plan.children(parent);
                let first = layout.rect(kids[0]).unwrap().center().x;
                let last = layout.rect(*kids.last().unwrap()).unwrap().center().x;
                assert!((rp.center().x - (first + last) / 2.0).abs() < 0.01, "{parent:?} is centred over its children");
            }
        }
    }

    #[test]
    fn steps_never_overlap_and_parents_centre_over_their_children() {
        for plan in [branching(), join_chain(60)] {
            let layout = Layout::new(&plan, &vec![false; plan.node_count()]);
            assert_tidy(&plan, &layout);
            assert_eq!(shown(&plan, &layout).len(), plan.node_count());
        }
    }

    #[test]
    fn children_keep_the_plan_order_left_to_right() {
        let plan = branching();
        let layout = Layout::new(&plan, &vec![false; plan.node_count()]);
        for (id, _) in plan.nodes() {
            let xs: Vec<f32> = plan.children(id).iter().map(|c| layout.rect(*c).unwrap().center().x).collect();
            assert!(xs.windows(2).all(|w| w[0] < w[1]), "children of {id:?} run left to right");
        }
        let root_row: Vec<NodeId> = layout.rows[0].clone();
        assert_eq!(root_row, [NodeId::ROOT]);
        assert_eq!(layout.rows.len(), 5);
    }

    #[test]
    fn same_plan_same_layout() {
        let plan = join_chain(60);
        let collapsed = vec![false; plan.node_count()];
        assert_eq!(Layout::new(&plan, &collapsed).rects, Layout::new(&plan, &collapsed).rects);
    }

    #[test]
    fn collapsed_steps_hide_their_subtree() {
        let plan = branching();
        let hash = plan.children(plan.children(NodeId::ROOT)[0])[1];
        let mut collapsed = vec![false; plan.node_count()];
        collapsed[hash.index()] = true;
        let layout = Layout::new(&plan, &collapsed);
        assert!(layout.rect(hash).is_some());
        assert_eq!(shown(&plan, &layout).len(), plan.node_count() - 3);
        assert_tidy(&plan, &layout);
    }
}
