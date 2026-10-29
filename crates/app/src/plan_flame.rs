//! The Plan pane's flame chart. Every step is a bar as wide as its time, with the steps it runs
//! beneath it; the right-hand part with nothing beneath it is the step's own time. Double-click
//! zooms into a step, and the breadcrumb, Esc and Home zoom back out.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, CornerRadius, EventFilter, Id, Key, Pos2, Rect, Sense, Ui, pos2, scroll_area::ScrollSource, vec2,
};
use tableplus_core::plan::{NodeId, Plan, SPAN_SLACK};

use crate::plan_view::PlanEvent;
use crate::theme::palette;
use crate::widgets::ease_out;
use paint::{Look, header_strip, needs_times, paint_bar, tooltip};

mod paint;

const HEADER_H: f32 = 28.0;
const ROW_H: f32 = 24.0;
const PAD: f32 = 10.0;
const ZOOM_SECONDS: f32 = 0.38;
const REVEAL_SECONDS: f32 = 0.3;
const MIN_WINDOW: f64 = 1e-6;

/// A horizontal extent, in fractions of the root bar's width.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    left: f64,
    width: f64,
}

impl Span {
    const ALL: Span = Span { left: 0.0, width: 1.0 };

    fn right(self) -> f64 {
        self.left + self.width
    }

    fn contains(self, other: Span) -> bool {
        let slack = 1e-9 * self.width.max(1e-6);
        other.left >= self.left - slack && other.right() <= self.right() + slack
    }

    fn touches(self, other: Span) -> bool {
        other.left <= self.right() && other.right() >= self.left
    }
}

struct Bar {
    span: Span,
    own: f64,
    /// The step's own time as a share of the statement's; `None` when the server didn't time it.
    heat: Option<f64>,
    /// What the steps below report in all, when it is more than this step reports; the bar spans
    /// them. Steps are timed separately and MySQL averages over loops, so nested times don't always
    /// add up: MySQL 8.0.18 times a temporary table scan without the materialization below it.
    stretched: Option<f64>,
}

pub struct Flame {
    bars: Vec<Bar>,
    /// Steps by depth, left to right.
    rows: Vec<Vec<NodeId>>,
    /// The statement's time, which shares are of; the root bar can span more.
    total_ms: f64,
    camera: Camera,
    shown_at: Option<Instant>,
    /// The selection the chart last brought into view.
    seen: Option<NodeId>,
    scroll_to_row: Option<usize>,
    scroll_id: Option<Id>,
    /// The bar the last click landed on. A double-click zooms into it, because the first click
    /// opens the detail panel, which narrows the chart under the second.
    clicked: Option<NodeId>,
}

struct Camera {
    /// Where the camera is going, or is.
    window: Span,
    /// The step zoomed into: the last breadcrumb, and where Esc zooms out from.
    focus: NodeId,
    glide: Option<(Span, Instant)>,
}

impl Camera {
    fn current(&self) -> Span {
        match self.glide {
            Some((from, started)) => {
                let t = (started.elapsed().as_secs_f32() / ZOOM_SECONDS).min(1.0);
                glide(from, self.window, f64::from(ease_out(t)))
            }
            None => self.window,
        }
    }

    fn gliding(&self) -> bool {
        self.glide.is_some_and(|(_, started)| started.elapsed().as_secs_f32() < ZOOM_SECONDS)
    }

    fn glide_to(&mut self, window: Span, focus: NodeId) {
        self.glide = Some((self.current(), Instant::now()));
        self.window = window;
        self.focus = focus;
    }
}

/// The window `t` of the way from `from` to `to`. The narrower span grows linearly on screen, so
/// a deep zoom keeps its step in view; interpolating the edges would leave a sliver until the end.
fn glide(from: Span, to: Span, t: f64) -> Span {
    if t <= 0.0 || t >= 1.0 {
        return if t <= 0.0 { from } else { to };
    }
    let anchor = if to.width <= from.width { to } else { from };
    let on_screen = |w: Span| ((anchor.left - w.left) / w.width, (anchor.right() - w.left) / w.width);
    let (a0, b0) = on_screen(from);
    let (a1, b1) = on_screen(to);
    let (a, b) = (a0 + (a1 - a0) * t, b0 + (b1 - b0) * t);
    let width = anchor.width / (b - a);
    Span { left: anchor.left - a * width, width }
}

impl Flame {
    pub fn new(plan: &Plan) -> Option<Self> {
        let ids: Vec<NodeId> = plan.nodes().map(|(id, _)| id).collect();
        // Each bar is as wide as the step's time or the steps below it, whichever is more, and a
        // step the server didn't time, such as MySQL's `-> Hash`, is as wide as the steps below.
        let time = plan.spans(|id| plan.inclusive_ms(id));
        let total_ms = plan.total_ms().unwrap_or(0.0);
        if !plan.analyzed() || time[NodeId::ROOT.index()] <= 0.0 {
            return None;
        }
        let mut bars: Vec<Bar> =
            ids.iter().map(|_| Bar { span: Span::ALL, own: 0.0, heat: None, stretched: None }).collect();
        let mut rows: Vec<Vec<NodeId>> = Vec::new();
        for &id in &ids {
            let span = bars[id.index()].span;
            let below: f64 = plan.children(id).iter().map(|c| time[c.index()]).sum();
            let scale = span.width / time[id.index()].max(f64::MIN_POSITIVE);
            let mut left = span.left;
            for c in plan.children(id) {
                let width = time[c.index()] * scale;
                bars[c.index()].span = Span { left, width };
                left += width;
            }
            let own_ms = (time[id.index()] - below).max(0.0);
            let bar = &mut bars[id.index()];
            bar.own = own_ms * scale;
            bar.heat = plan.inclusive_ms(id).map(|_| own_ms / total_ms);
            bar.stretched = plan.inclusive_ms(id).filter(|ms| below > ms * SPAN_SLACK).map(|_| below);
            let depth = plan.depth(id);
            if rows.len() <= depth {
                rows.resize_with(depth + 1, Vec::new);
            }
            rows[depth].push(id);
        }
        for row in &mut rows {
            row.sort_by(|a, b| bars[a.index()].span.left.total_cmp(&bars[b.index()].span.left));
        }
        Some(Self {
            bars,
            rows,
            total_ms,
            camera: Camera { window: Span::ALL, focus: NodeId::ROOT, glide: None },
            shown_at: None,
            seen: None,
            scroll_to_row: None,
            scroll_id: None,
            clicked: None,
        })
    }

    fn span(&self, id: NodeId) -> Span {
        self.bars[id.index()].span
    }

    fn zoom_to(&mut self, id: NodeId) {
        let span = self.span(id);
        if span.width > 0.0 {
            self.camera.glide_to(span, id);
        }
    }

    /// Zooms out to the nearest step wider than the window; false when the whole plan shows.
    fn zoom_out(&mut self, plan: &Plan) -> bool {
        let window = self.camera.window;
        let mut at = Some(self.camera.focus);
        while let Some(id) = at {
            if self.span(id).width > window.width * (1.0 + 1e-6) {
                self.zoom_to(id);
                return true;
            }
            at = plan.parent(id);
        }
        false
    }

    fn set_window(&mut self, plan: &Plan, width: f64, left: f64) {
        let width = width.clamp(MIN_WINDOW, 1.0);
        let window = Span { left: left.clamp(0.0, 1.0 - width), width };
        self.camera.window = window;
        self.camera.glide = None;
        self.camera.focus = self.deepest_containing(plan, window);
    }

    fn deepest_containing(&self, plan: &Plan, window: Span) -> NodeId {
        let mut at = NodeId::ROOT;
        while let Some(child) = plan.children(at).iter().find(|c| self.span(**c).contains(window)) {
            at = *child;
        }
        at
    }

    /// The step at `x` (fraction of the plan) in a row, or the nearest within `slack`.
    fn hit(&self, depth: usize, x: f64, slack: f64) -> Option<NodeId> {
        let row = self.rows.get(depth)?;
        let start = row.partition_point(|id| self.span(*id).right() < x - slack);
        let distance = |id: &NodeId| {
            let s = self.span(*id);
            if x < s.left { s.left - x } else { (x - s.right()).max(0.0) }
        };
        row[start..]
            .iter()
            .take_while(|id| self.span(**id).left <= x + slack)
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
            .copied()
    }

    fn neighbour(&self, plan: &Plan, id: NodeId, step: isize) -> Option<NodeId> {
        let row = &self.rows[plan.depth(id)];
        let at = row.iter().position(|n| *n == id)?.checked_add_signed(step)?;
        row.get(at).copied()
    }

    fn follow_selection(&mut self, plan: &Plan, selected: Option<NodeId>) {
        if selected == self.seen {
            return;
        }
        self.seen = selected;
        let Some(sel) = selected else { return };
        if !self.camera.window.touches(self.span(sel)) {
            let common = common_ancestor(plan, self.camera.focus, sel);
            self.zoom_to(common);
        }
        self.scroll_to_row = Some(plan.depth(sel));
    }
}

fn ancestry(plan: &Plan, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    std::iter::successors(Some(id), |n| plan.parent(*n))
}

fn common_ancestor(plan: &Plan, a: NodeId, b: NodeId) -> NodeId {
    ancestry(plan, b).find(|n| ancestry(plan, a).any(|m| m == *n)).unwrap_or(NodeId::ROOT)
}

/// Draws the chart, or why there is none, and returns what the user picked.
pub fn show(
    ui: &mut Ui,
    id: Id,
    plan: &Plan,
    flame: Option<&mut Flame>,
    selected: Option<NodeId>,
    detail_open: bool,
) -> Vec<PlanEvent> {
    let Some(flame) = flame else {
        needs_times(ui, plan);
        return Vec::new();
    };
    let p = palette(ui);
    flame.follow_selection(plan, selected);
    let mut pick = None;
    if ui.memory(|m| m.has_focus(id)) {
        pick = keyboard(ui, plan, flame, selected, detail_open);
    }
    let has_focus = ui.memory(|m| m.has_focus(id));

    let (header, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_H), Sense::hover());
    let crumb = header_strip(ui, id, header, plan, flame, p);
    if let Some(target) = crumb {
        flame.zoom_to(target);
    }

    let since = flame.shown_at.get_or_insert_with(Instant::now).elapsed().as_secs_f32();
    let stagger = (0.18 / flame.rows.len() as f32).min(0.02);
    let revealing = since < REVEAL_SECONDS + stagger * flame.rows.len() as f32;
    if revealing || flame.camera.gliding() {
        ui.ctx().request_repaint_after(Duration::from_millis(8));
    } else {
        flame.camera.glide = None;
    }

    let rows = flame.rows.len();
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::MOUSE_WHEEL);
    if let Some(row) = flame.scroll_to_row.take()
        && let Some(state) = flame.scroll_id.and_then(|sid| egui::scroll_area::State::load(ui.ctx(), sid))
    {
        let viewport_h = ui.available_height();
        let (top, mut off) = (row as f32 * ROW_H, state.offset.y);
        if top < off {
            off = top;
        } else if top + ROW_H > off + viewport_h - 8.0 {
            off = top + ROW_H - viewport_h + 8.0;
        }
        scroll = scroll.vertical_scroll_offset(off);
    }

    let mut press = None;
    let output = scroll.show_viewport(ui, |ui, viewport| {
        ui.set_min_size(vec2(ui.available_width(), (rows as f32 * ROW_H + 2.0 * PAD).max(viewport.height())));
        let origin = ui.max_rect().min;
        let clip = ui.clip_rect();
        let painter = ui.painter().clone();
        painter.rect_filled(clip, CornerRadius::ZERO, p.content_bg);
        let chart = Rect::from_min_max(pos2(clip.left() + PAD, clip.top()), pos2(clip.right() - PAD, clip.bottom()));

        let body = ui.interact(clip, id, Sense::click());
        if body.clicked() || body.double_clicked() {
            body.request_focus();
        }
        if body.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter { tab: false, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }
        if body.hovered()
            && let Some(pos) = body.hover_pos()
        {
            let window = flame.camera.current();
            let (zoom_by, scrolled) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            if zoom_by != 1.0 {
                let at = f64::from(((pos.x - chart.left()) / chart.width()).clamp(0.0, 1.0));
                let width = (window.width / f64::from(zoom_by)).clamp(MIN_WINDOW, 1.0);
                flame.set_window(plan, width, window.left + at * (window.width - width));
            } else if scrolled.x.abs() > scrolled.y.abs() && window.width < 1.0 {
                let left = window.left - f64::from(scrolled.x / chart.width()) * window.width;
                flame.set_window(plan, window.width, left);
            }
        }

        let window = flame.camera.current();
        let to_x = |f: f64| chart.left() + ((f - window.left) / window.width) as f32 * chart.width();
        let to_f = |x: f32| window.left + f64::from((x - chart.left()) / chart.width()) * window.width;
        let row_top = |depth: usize| origin.y + PAD + depth as f32 * ROW_H;
        let pointer_slack = f64::from(3.0 / chart.width()) * window.width;
        let hit = |pos: Pos2| -> Option<NodeId> {
            let r = ((pos.y - origin.y - PAD) / ROW_H).floor();
            (r >= 0.0).then(|| flame.hit(r as usize, to_f(pos.x), pointer_slack)).flatten()
        };
        if body.double_clicked() {
            press = Some(Press::Double);
        } else if body.clicked() {
            press = body.interact_pointer_pos().map(|pos| Press::Click(hit(pos)));
        }
        let hovered = body.hover_pos().and_then(hit);

        let first = ((viewport.min.y - PAD) / ROW_H).floor().max(0.0) as usize;
        let last = (((viewport.max.y - PAD) / ROW_H).ceil().max(0.0) as usize + 1).min(rows);
        let focus_depth = plan.depth(flame.camera.focus);
        let zoomed = flame.camera.focus != NodeId::ROOT;
        let bars_painter = painter.with_clip_rect(chart.intersect(clip));
        for depth in first..last {
            let unfold = ease_out(((since - depth as f32 * stagger) / REVEAL_SECONDS).clamp(0.0, 1.0));
            if unfold <= 0.0 {
                continue;
            }
            let fade = if zoomed && depth < focus_depth { 0.5 * unfold } else { unfold };
            let cy = row_top(depth) + ROW_H / 2.0 - (1.0 - unfold) * 6.0;
            let row = &flame.rows[depth];
            let start = row.partition_point(|n| flame.span(*n).right() < window.left);
            let mut last_sliver = f32::NEG_INFINITY;
            for &node in &row[start..] {
                let bar = &flame.bars[node.index()];
                if bar.span.left >= window.right() && bar.span.width > 0.0 {
                    break;
                }
                let (x0, x1) = (to_x(bar.span.left), to_x(bar.span.right()));
                if x1 - x0 < 1.5 {
                    if x0 < last_sliver + 1.5 {
                        continue;
                    }
                    last_sliver = x0;
                }
                let look =
                    Look { hovered: hovered == Some(node), selected: selected == Some(node), focused: has_focus, fade };
                paint_bar(&bars_painter, chart, plan, node, bar, (x0, x1), cy, to_x, look, p);
            }
        }

        if let Some(node) = hovered {
            body.on_hover_ui_at_pointer(|ui| tooltip(ui, plan, node, &flame.bars[node.index()], flame.total_ms));
        }
    });
    flame.scroll_id = Some(output.id);

    match press {
        Some(Press::Click(node)) => {
            flame.clicked = node;
            pick = node.map(PlanEvent::Select).or(pick);
        }
        Some(Press::Double) => {
            if let Some(node) = flame.clicked {
                let (window, span) = (flame.camera.window, flame.span(node));
                if span.contains(window) && span.width <= window.width * (1.0 + 1e-6) {
                    flame.zoom_out(plan);
                } else {
                    flame.zoom_to(node);
                }
            }
        }
        None => {}
    }
    if pick.is_some() {
        ui.ctx().request_repaint();
    }
    pick.into_iter().collect()
}

enum Press {
    Click(Option<NodeId>),
    Double,
}

/// Esc zooms out a step, then closes the details, then clears the selection.
fn keyboard(ui: &Ui, plan: &Plan, flame: &mut Flame, selected: Option<NodeId>, detail_open: bool) -> Option<PlanEvent> {
    let pressed: Vec<Key> = ui.input(|i| {
        [Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight, Key::Enter, Key::Space, Key::Escape, Key::Home]
            .into_iter()
            .filter(|k| i.key_pressed(*k))
            .collect()
    });
    let mut pick = None;
    for key in pressed {
        let sel = if let Some(PlanEvent::Select(n)) = pick { Some(n) } else { selected };
        let step = sel.and_then(|s| match key {
            Key::ArrowUp => plan.parent(s),
            Key::ArrowDown => {
                plan.children(s).iter().copied().max_by(|a, b| flame.span(*a).width.total_cmp(&flame.span(*b).width))
            }
            Key::ArrowLeft => flame.neighbour(plan, s, -1),
            Key::ArrowRight => flame.neighbour(plan, s, 1),
            _ => None,
        });
        let arrow = matches!(key, Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight);
        pick = match (key, sel) {
            _ if arrow => {
                sel.map_or(Some(PlanEvent::Select(flame.camera.focus)), |_| step.map(PlanEvent::Select).or(pick))
            }
            (Key::Enter | Key::Space, Some(s)) => {
                flame.zoom_to(s);
                pick
            }
            (Key::Home, _) => {
                flame.zoom_to(NodeId::ROOT);
                pick
            }
            (Key::Escape, _) if flame.zoom_out(plan) => pick,
            (Key::Escape, Some(_)) if detail_open => Some(PlanEvent::CloseDetail),
            (Key::Escape, Some(_)) => Some(PlanEvent::Deselect),
            _ => pick,
        };
    }
    pick
}

#[cfg(test)]
mod tests {
    use super::*;
    use tableplus_core::plan::{Actual, ActualTime, PlanFormat, PlanNode};

    fn timed(op: &str, last: f64, loops: u64) -> PlanNode {
        let mut n = PlanNode::new(op);
        n.actual = Some(Actual {
            rows: Some(1.0),
            loops,
            time: Some(ActualTime { first_row_ms: Some(0.0), last_row_ms: last }),
        });
        n
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// join (10 ms) ─┬─ scan (2 ms)
    ///               ├─ hash (untimed) ── scan (3 ms)
    ///               └─ lookup (0.01 ms × 100 loops)
    fn sample() -> (Plan, [NodeId; 5]) {
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("join", 10.0, 1), "");
        let scan = plan.add_child(NodeId::ROOT, timed("scan", 2.0, 1));
        let hash = plan.add_child(NodeId::ROOT, PlanNode::new("hash"));
        let inner = plan.add_child(hash, timed("scan", 3.0, 1));
        let lookup = plan.add_child(NodeId::ROOT, timed("lookup", 0.01, 100));
        (plan, [NodeId::ROOT, scan, hash, inner, lookup])
    }

    #[test]
    fn bars_are_as_wide_as_their_time_with_own_time_on_the_right() {
        let (plan, [root, scan, hash, inner, lookup]) = sample();
        let flame = Flame::new(&plan).unwrap();
        assert_eq!(flame.span(root), Span::ALL);
        assert!(close(flame.span(scan).width, 0.2) && close(flame.span(scan).left, 0.0));
        assert!(close(flame.span(hash).left, 0.2) && close(flame.span(hash).width, 0.3));
        assert_eq!(flame.span(inner), flame.span(hash));
        assert!(close(flame.span(lookup).left, 0.5) && close(flame.span(lookup).width, 0.1));
        assert!(close(flame.bars[root.index()].own, 0.4), "10 ms less the 6 ms below is the join's own");
        assert!(close(flame.bars[root.index()].heat.unwrap(), 0.4));
        assert_eq!(flame.rows, vec![vec![root], vec![scan, hash, lookup], vec![inner]]);
        let bar = &flame.bars[hash.index()];
        assert_eq!((bar.heat, bar.stretched), (None, None), "the untimed hash spans its scan, unmarked");
    }

    #[test]
    fn a_step_spans_steps_below_that_report_more_time_and_is_marked() {
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("scan", 0.03, 1), "");
        let a = plan.add_child(NodeId::ROOT, timed("materialize", 6.0, 1));
        let b = plan.add_child(NodeId::ROOT, timed("scan", 2.0, 1));
        let flame = Flame::new(&plan).unwrap();
        assert!(close(flame.span(a).width, 0.75) && close(flame.span(b).left, 0.75));
        assert!(close(flame.span(b).right(), 1.0));
        let root = &flame.bars[0];
        assert_eq!(root.stretched, Some(8.0));
        assert!(close(root.own, 0.0));
        assert!(close(flame.total_ms, 6.0), "shares are of the longest step, as in the tree");
        assert!(close(flame.bars[a.index()].heat.unwrap(), 1.0));
        assert!(close(flame.bars[b.index()].heat.unwrap(), 2.0 / 6.0));
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("join", 10.0, 1), "");
        let x = plan.add_child(NodeId::ROOT, timed("lookup", 1.0, 1));
        plan.add_child(x, timed("materialize", 4.0, 1));
        let z = plan.add_child(NodeId::ROOT, timed("scan", 5.0, 1));
        let flame = Flame::new(&plan).unwrap();
        assert!(close(flame.span(x).width, 0.4) && close(flame.span(z).left, 0.4));
        assert!(close(flame.bars[0].own, 0.1), "a stretched step widens its parent only into its own time");
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("join", 1.0, 1), "");
        plan.add_child(NodeId::ROOT, timed("scan", 1.02, 1));
        assert_eq!(Flame::new(&plan).unwrap().bars[0].stretched, None, "rounding is not worth a mark");
    }

    #[test]
    fn estimate_only_plans_have_no_chart() {
        let mut plan = Plan::new(PlanFormat::MySqlTree, PlanNode::new("limit"), "");
        plan.add_child(NodeId::ROOT, PlanNode::new("scan"));
        assert!(Flame::new(&plan).is_none());
    }

    #[test]
    fn glide_keeps_the_narrower_span_growing_on_screen() {
        let to = Span { left: 0.4, width: 0.001 };
        assert_eq!(glide(Span::ALL, to, 0.0), Span::ALL);
        assert_eq!(glide(Span::ALL, to, 1.0), to);
        let half = glide(Span::ALL, to, 0.5);
        let on_screen = to.width / half.width;
        assert!((on_screen - 0.5005).abs() < 1e-6, "halfway into a 1000× zoom the target fills {on_screen}");
        let back = glide(to, Span::ALL, 0.5);
        assert!((to.width / back.width - 0.5005).abs() < 1e-6);
    }

    #[test]
    fn zooming_out_steps_to_the_nearest_wider_step() {
        let (plan, [root, _, hash, inner, _]) = sample();
        let mut flame = Flame::new(&plan).unwrap();
        flame.zoom_to(inner);
        assert_eq!((flame.camera.focus, flame.camera.window), (inner, flame.span(inner)));
        assert!(flame.zoom_out(&plan));
        assert_eq!(flame.camera.focus, root, "Esc skips the hash, exactly as wide as its scan");
        assert!(!flame.zoom_out(&plan));
        assert_eq!(flame.deepest_containing(&plan, flame.span(hash)), inner);
    }

    #[test]
    fn a_pinch_keeps_the_window_on_the_plan_and_tracks_the_focus() {
        let (plan, [_, scan, _, _, _]) = sample();
        let mut flame = Flame::new(&plan).unwrap();
        flame.set_window(&plan, 0.1, -0.5);
        assert_eq!(flame.camera.window, Span { left: 0.0, width: 0.1 });
        assert_eq!(flame.camera.focus, scan);
        flame.set_window(&plan, 4.0, 0.3);
        assert_eq!(flame.camera.window, Span::ALL);
        assert_eq!(flame.camera.focus, NodeId::ROOT);
    }

    #[test]
    fn hits_find_the_bar_under_the_pointer_or_the_nearest_sliver() {
        let (plan, [_, scan, hash, _, lookup]) = sample();
        let flame = Flame::new(&plan).unwrap();
        assert_eq!(flame.hit(1, 0.1, 0.0), Some(scan));
        assert_eq!(flame.hit(1, 0.35, 0.0), Some(hash));
        assert_eq!(flame.hit(1, 0.55, 0.0), Some(lookup));
        assert_eq!(flame.hit(1, 0.9, 0.0), None);
        assert_eq!(flame.hit(1, 0.605, 0.01), Some(lookup));
        assert_eq!(flame.hit(7, 0.1, 0.0), None);
    }
}
