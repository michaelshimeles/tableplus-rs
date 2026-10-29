//! The Plan pane's graph, in the style of MySQL Workbench's Visual Explain: one box per step,
//! coloured by how it reads its table, joins as diamonds, and edges as wide as the rows flowing up
//! them. A frame paints only the steps inside the view.

mod layout;
mod paint;
mod steps;

use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, EventFilter, Id, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};
use tableplus_core::plan::{Access, NodeId, Plan};

use crate::icons::{self, Icon};
use crate::plan_view::PlanEvent;
use crate::theme::{self, palette};
use crate::widgets::ease_out;
use layout::{BOX_W, Layout};
use paint::{
    Edge, Frame, Look, TextScale, cap, dots, fit_icon, fold_pill, fold_pill_rect, fold_stack, legend, paint_box,
    paint_join, tooltip,
};
use steps::{Prepared, prepare};

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 2.5;
/// Below this zoom the boxes drop their text and the graph reads as a map of colours.
const TEXT_ZOOM: f32 = 0.36;
/// The first view fits the plan, but no smaller than this; a larger plan opens at its top.
const OPEN_ZOOM: f32 = 0.5;
const MOVE_SECONDS: f32 = 0.3;
const MARGIN: f32 = 28.0;
const OVERLAY_H: f32 = 40.0;
/// The join diamond's centre below the top of its slot, and its half-diagonal.
const DIAMOND_Y: f32 = 21.0;
const DIAMOND_R: f32 = 20.0;
const CAP_GAP: f32 = 40.0;
const CAP_H: f32 = 26.0;

#[derive(Clone, Copy)]
struct Camera {
    center: Pos2,
    zoom: f32,
}

impl Camera {
    fn to_screen(self, canvas: Rect, p: Pos2) -> Pos2 {
        canvas.center() + (p - self.center) * self.zoom
    }

    fn to_graph(self, canvas: Rect, s: Pos2) -> Pos2 {
        self.center + (s - canvas.center()) / self.zoom
    }

    fn rect(self, canvas: Rect, r: Rect) -> Rect {
        Rect::from_min_max(self.to_screen(canvas, r.min), self.to_screen(canvas, r.max))
    }
}

/// A camera move that zooms at a constant rate.
#[derive(Clone, Copy)]
struct Flight {
    from: Camera,
    to: Camera,
    /// Where a zoom happens, from the canvas centre; it stays over the same graph point.
    pivot: Option<Vec2>,
    start: f64,
    seconds: f32,
}

impl Flight {
    fn at(&self, now: f64) -> (Camera, bool) {
        let t = ((now - self.start) as f32 / self.seconds).clamp(0.0, 1.0);
        let e = ease_out(t);
        let zoom = (self.from.zoom.ln() + (self.to.zoom.ln() - self.from.zoom.ln()) * e).exp();
        let center = match self.pivot {
            Some(off) => self.from.center + off / self.from.zoom - off / zoom,
            None => self.from.center + (self.to.center - self.from.center) * e,
        };
        (Camera { center, zoom }, t >= 1.0)
    }

    fn shift(&mut self, d: Vec2) {
        self.from.center += d;
        self.to.center += d;
    }
}

/// Boxes gliding from where they were before a step was folded or unfolded.
struct Motion {
    from: Vec<Option<Rect>>,
    /// The step that was folded; boxes come out of it and go back into it.
    origin: NodeId,
    start: f64,
}

pub struct GraphView {
    prepared: Option<Prepared>,
    collapsed: Vec<bool>,
    camera: Camera,
    flight: Option<Flight>,
    motion: Option<Motion>,
    /// The frame the graph was last drawn in; a gap means it was hidden in between.
    drawn_frame: Option<u64>,
    opened_at: f64,
    /// When a wheel or pinch last zoomed, so text is laid out afresh once the zoom settles.
    zoomed_at: f64,
    /// Frames left in which to bring the selection into view, long enough for a detail panel
    /// that opens with the selection to take its room.
    reveal: u8,
    last_selected: Option<NodeId>,
    canvas_size: Option<Vec2>,
}

impl Default for GraphView {
    fn default() -> Self {
        Self {
            prepared: None,
            collapsed: Vec::new(),
            camera: Camera { center: Pos2::ZERO, zoom: 1.0 },
            flight: None,
            motion: None,
            drawn_frame: None,
            opened_at: 0.0,
            zoomed_at: f64::NEG_INFINITY,
            reveal: 0,
            last_selected: None,
            canvas_size: None,
        }
    }
}

impl GraphView {
    fn fly(&mut self, to: Camera, now: f64, seconds: f32) {
        let to = Camera { zoom: to.zoom.clamp(MIN_ZOOM, MAX_ZOOM), ..to };
        self.flight = Some(Flight { from: self.camera, to, pivot: None, start: now, seconds });
    }

    fn zoom_to(&mut self, zoom: f32, pivot: Vec2, now: f64) {
        let from = self.camera;
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let to = Camera { center: from.center + pivot / from.zoom - pivot / zoom, zoom };
        self.flight = Some(Flight { from, to, pivot: Some(pivot), start: now, seconds: 0.3 });
    }

    /// Keeps the graph still under the canvas's top-left corner when the canvas changes size, so
    /// a detail panel opening beside it covers the graph instead of shifting it.
    fn anchor(&mut self, canvas: Rect) {
        if let Some(old) = self.canvas_size.replace(canvas.size())
            && old != canvas.size()
        {
            let d = (canvas.size() - old) / (2.0 * self.camera.zoom);
            self.camera.center += d;
            if let Some(f) = &mut self.flight {
                f.shift(d);
            }
        }
    }

    fn target(&self) -> Camera {
        self.flight.map_or(self.camera, |f| f.to)
    }

    fn fit(&self, canvas: Rect) -> Camera {
        let bounds = self.prepared.as_ref().map_or(Rect::ZERO, |p| graph_bounds(&p.layout));
        let room = canvas.shrink(MARGIN).size() - vec2(0.0, OVERLAY_H);
        let zoom = (room.x / bounds.width()).min(room.y / bounds.height()).clamp(MIN_ZOOM, 1.0);
        Camera { center: bounds.center() + vec2(0.0, OVERLAY_H / 2.0 / zoom), zoom }
    }

    /// The whole plan when it is readable that way, else the selected step or the top of the plan.
    fn opening(&self, canvas: Rect, selected: Option<NodeId>) -> Camera {
        let fit = self.fit(canvas);
        let Some(prepared) = &self.prepared else { return fit };
        if fit.zoom >= OPEN_ZOOM {
            return fit;
        }
        if let Some(rect) = selected.and_then(|s| prepared.layout.rect(s)) {
            return Camera { center: rect.center(), zoom: 0.85 };
        }
        let bounds = graph_bounds(&prepared.layout);
        let root = prepared.layout.rect(NodeId::ROOT).unwrap_or(bounds);
        let width_fit = (canvas.width() - 2.0 * MARGIN) / bounds.width();
        let zoom = width_fit.clamp(OPEN_ZOOM, 0.85);
        let x = if width_fit >= OPEN_ZOOM { bounds.center().x } else { root.center().x };
        Camera { center: pos2(x, bounds.top() + (canvas.height() / 2.0 - MARGIN) / zoom), zoom }
    }

    /// Flies the camera so `rect` (graph points) is inside the canvas, unless it already is.
    fn reveal_rect(&mut self, canvas: Rect, rect: Rect, now: f64) {
        let target = self.target();
        let safe = Rect::from_min_max(canvas.min + vec2(MARGIN, MARGIN), canvas.max - vec2(MARGIN, MARGIN + OVERLAY_H));
        let shown = target.rect(canvas, rect);
        let shift = |lo: f32, hi: f32, safe_lo: f32, safe_hi: f32| {
            if hi - lo > safe_hi - safe_lo {
                (lo + hi - safe_lo - safe_hi) / 2.0
            } else if lo < safe_lo {
                lo - safe_lo
            } else if hi > safe_hi {
                hi - safe_hi
            } else {
                0.0
            }
        };
        let d = vec2(
            shift(shown.left(), shown.right(), safe.left(), safe.right()),
            shift(shown.top(), shown.bottom(), safe.top(), safe.bottom()),
        );
        if d.length() > 0.5 {
            self.fly(Camera { center: target.center + d / target.zoom, zoom: target.zoom }, now, 0.28);
        }
    }

    fn zoom_at(&mut self, canvas: Rect, at: Pos2, factor: f32) {
        let g = self.camera.to_graph(canvas, at);
        let zoom = (self.camera.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.camera = Camera { center: g - (at - canvas.center()) / zoom, zoom };
    }

    /// Keeps `node` in place on screen while the others glide. True when the fold hides `selected`.
    fn toggle(&mut self, plan: &Plan, node: NodeId, now: f64, selected: Option<NodeId>) -> bool {
        if plan.children(node).is_empty() {
            return false;
        }
        self.collapsed[node.index()] = !self.collapsed[node.index()];
        self.relayout(plan, node, now);
        selected.is_some_and(|s| self.prepared.as_ref().is_some_and(|p| p.layout.rect(s).is_none()))
    }

    /// Unfolds the steps hiding `node`, as when it was picked in the tree. True when any were folded.
    fn unfold_to(&mut self, plan: &Plan, node: NodeId, now: f64) -> bool {
        let mut outermost = None;
        let mut at = plan.parent(node);
        while let Some(n) = at {
            if self.collapsed[n.index()] {
                self.collapsed[n.index()] = false;
                outermost = Some(n);
            }
            at = plan.parent(n);
        }
        let Some(origin) = outermost else { return false };
        self.relayout(plan, origin, now);
        true
    }

    /// Lays the graph out again after a fold changed, keeping `origin` in place on screen while the
    /// other boxes glide out of it or back into it.
    fn relayout(&mut self, plan: &Plan, origin: NodeId, now: f64) {
        let Some(prepared) = self.prepared.take() else { return };
        let before = prepared.layout.rects;
        let next = prepare(plan, &self.collapsed);
        if let (Some(a), Some(b)) = (before[origin.index()], next.layout.rect(origin)) {
            let d = b.center() - a.center();
            self.camera.center += d;
            if let Some(f) = &mut self.flight {
                f.shift(d);
            }
        }
        self.prepared = Some(next);
        self.motion = Some(Motion { from: before, origin, start: now });
    }

    /// Every step's box this frame, with its opacity: in place, or gliding after a fold.
    fn frame_rects(&mut self, now: f64) -> Vec<Option<(Rect, f32)>> {
        let Some(prepared) = &self.prepared else { return Vec::new() };
        let layout = &prepared.layout;
        let Some(motion) = &self.motion else {
            return layout.rects.iter().map(|r| r.map(|r| (r, 1.0))).collect();
        };
        let t = ((now - motion.start) as f32 / MOVE_SECONDS).min(1.0);
        let e = ease_out(t);
        let origin_from = motion.from[motion.origin.index()].unwrap_or(Rect::ZERO);
        let origin_to = layout.rect(motion.origin).unwrap_or(origin_from);
        let lerp = |a: Rect, b: Rect| Rect::from_min_max(a.min.lerp(b.min, e), a.max.lerp(b.max, e));
        let rects = layout
            .rects
            .iter()
            .zip(&motion.from)
            .map(|(to, from)| match (from, to) {
                (Some(a), Some(b)) => Some((lerp(*a, *b), 1.0)),
                (None, Some(b)) => Some((lerp(shrunk(origin_from, *b), *b), e)),
                (Some(a), None) => (t < 1.0).then(|| (lerp(*a, shrunk(origin_to, *a)), 1.0 - e)),
                (None, None) => None,
            })
            .collect();
        if t >= 1.0 {
            self.motion = None;
        }
        rects
    }
}

/// Where folded boxes come from and go: a smaller box centred on `at`.
fn shrunk(at: Rect, like: Rect) -> Rect {
    Rect::from_center_size(at.center(), like.size() * 0.3)
}

/// Workbench's colours, but reading back a temporary result is a muted violet instead of red: no
/// index can make that read cheaper, so red would point at a step the user can't change.
pub(crate) fn access_color(access: Access, dark: bool) -> Color32 {
    let (light, night) = match access {
        Access::FullScan => (0xE5484D, 0xFF6369),
        Access::IndexScan => (0xEC6A2C, 0xFF8A4D),
        Access::Range => (0xE59A00, 0xFFBA3B),
        Access::Lookup | Access::UniqueLookup => (0x2E9E5B, 0x3DD68C),
        Access::Constant => (0x1B9BD8, 0x55C3F7),
        Access::InternalResult => (0x8E86B0, 0xA29BC9),
    };
    let rgb: u32 = if dark { night } else { light };
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn cap_rect(root: Rect) -> Rect {
    Rect::from_center_size(pos2(root.center().x, root.top() - CAP_GAP - CAP_H / 2.0), vec2(BOX_W, CAP_H))
}

fn graph_bounds(layout: &Layout) -> Rect {
    layout.bounds.union(cap_rect(layout.rect(NodeId::ROOT).unwrap_or(Rect::ZERO)))
}

/// Draws the graph and returns what the user picked.
pub fn show(
    ui: &mut Ui,
    id: Id,
    plan: &Plan,
    view: &mut GraphView,
    selected: Option<NodeId>,
    detail_open: bool,
) -> Vec<PlanEvent> {
    let p = palette(ui);
    let dark = ui.visuals().dark_mode;
    let now = ui.input(|i| i.time);
    let frame_nr = ui.ctx().cumulative_frame_nr();
    let canvas = ui.max_rect();
    if view.prepared.is_none() {
        view.collapsed = vec![false; plan.node_count()];
        view.prepared = Some(prepare(plan, &view.collapsed));
    }

    match view.drawn_frame {
        None => {
            let open = view.opening(canvas, selected);
            view.camera = Camera { zoom: open.zoom * 0.94, ..open };
            view.fly(open, now, 0.45);
            view.opened_at = now;
        }
        Some(f) if f + 1 < frame_nr => view.reveal = 2,
        _ => {}
    }
    view.drawn_frame = Some(frame_nr);
    view.anchor(canvas);

    let response = ui.interact(canvas, id, Sense::click_and_drag());
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    let focused = response.has_focus();
    let mut events = Vec::new();
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                EventFilter { tab: false, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
        events = keyboard(ui, plan, view, selected, detail_open, now);
    }

    if response.dragged() {
        view.flight = None;
        view.camera.center -= response.drag_delta() / view.camera.zoom;
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    }
    if response.contains_pointer() {
        let (zoom, scroll, pointer) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
        if zoom != 1.0 {
            view.flight = None;
            view.zoomed_at = now;
            view.zoom_at(canvas, pointer.unwrap_or(canvas.center()), zoom);
        }
        if scroll != Vec2::ZERO {
            view.flight = None;
            view.camera.center -= scroll / view.camera.zoom;
        }
    }

    let rects = view.frame_rects(now);
    let ids = view.prepared.as_ref().map(|p| p.ids.clone()).unwrap_or_default();
    let camera = view.camera;
    let hit = |pos: Pos2| -> Option<NodeId> {
        rects.iter().zip(&ids).rev().find_map(|(r, id)| {
            let (rect, alpha) = (*r)?;
            (alpha > 0.5 && camera.rect(canvas, rect).contains(pos)).then_some(*id)
        })
    };
    let fold_pill_at = |pos: Pos2| -> Option<NodeId> {
        rects.iter().zip(&ids).find_map(|(r, id)| {
            let (rect, _) = (*r)?;
            let pill = fold_pill_rect(camera.rect(canvas, rect), camera.zoom).expand(3.0);
            (view.collapsed[id.index()] && pill.contains(pos)).then_some(*id)
        })
    };
    let hovered = response.hover_pos().filter(|_| !response.dragged()).and_then(hit);
    let mut toggle = None;
    if let Some(pos) = response.interact_pointer_pos() {
        if response.double_clicked() {
            match hit(pos) {
                Some(node) => toggle = Some(node),
                None => view.zoom_to(view.target().zoom * 2.0, pos - canvas.center(), now),
            }
        } else if response.clicked() {
            if let Some(node) = fold_pill_at(pos) {
                toggle = Some(node);
            } else if let Some(node) = hit(pos) {
                events.push(PlanEvent::Select(node));
            }
        }
    }
    let (mut selected, mut detail_open) = (selected, detail_open);
    for event in &events {
        event.apply(&mut selected, &mut detail_open);
    }
    if let Some(node) = toggle
        && view.toggle(plan, node, now, selected)
    {
        events.push(PlanEvent::Select(node));
        selected = Some(node);
    }

    // Keep the selection in view after it moves, or when the graph comes back into view. A step
    // picked in the tree or flame chart may sit inside a branch folded here; open it first.
    if selected != view.last_selected {
        view.last_selected = selected;
        view.reveal = 2;
    }
    if view.reveal > 0 {
        view.reveal -= 1;
        if let Some(s) = selected {
            view.unfold_to(plan, s, now);
        }
        if let Some(rect) = selected.and_then(|s| view.prepared.as_ref()?.layout.rect(s)) {
            view.reveal_rect(canvas, rect.expand(8.0), now);
        }
    }
    if let Some(flight) = view.flight {
        let (camera, done) = flight.at(now);
        view.camera = camera;
        if done {
            view.flight = None;
        }
    }

    let rects = view.frame_rects(now);
    let Some(prepared) = view.prepared.as_ref() else { return events };
    let opacity = ease_out(((now - view.opened_at) as f32 / 0.3).clamp(0.0, 1.0));
    let moving = view.flight.is_some() || view.motion.is_some() || now - view.zoomed_at < 0.2;
    if moving || opacity < 1.0 {
        ui.ctx().request_repaint();
    } else if now - view.zoomed_at < 0.4 {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    let mut painter = ui.painter_at(canvas);
    painter.rect_filled(canvas, CornerRadius::ZERO, p.plan_canvas);
    dots(&painter, canvas, view.camera, p.plan_dots);
    painter.set_opacity(opacity);

    let mut lit = vec![false; plan.node_count()];
    let mut stack: Vec<NodeId> = selected.iter().copied().collect();
    while let Some(n) = stack.pop() {
        lit[n.index()] = true;
        stack.extend(plan.children(n));
    }
    let f = Frame {
        painter: &painter,
        canvas,
        camera: view.camera,
        p,
        dark,
        analyzed: prepared.analyzed,
        text: (view.camera.zoom >= TEXT_ZOOM).then(|| TextScale::new(view.camera.zoom, moving)),
        focused,
        lit,
    };
    let visible =
        Rect::from_min_max(view.camera.to_graph(canvas, canvas.min), view.camera.to_graph(canvas, canvas.max));

    for (i, slot) in rects.iter().enumerate() {
        let Some((rect, alpha)) = slot else { continue };
        let child = prepared.ids[i];
        let Some(parent) = plan.parent(child) else { continue };
        let Some((parent_rect, parent_alpha)) = rects[parent.index()] else { continue };
        let inputs: Vec<NodeId> =
            plan.children(parent).iter().copied().filter(|c| rects[c.index()].is_some()).collect();
        let edge = Edge::new(&prepared.steps, child, *rect, parent, parent_rect, &inputs);
        if edge.bounds().expand(24.0).intersects(visible) {
            edge.paint(&f, f.lit[i], prepared.steps[i].flow, alpha.min(parent_alpha));
        }
    }
    if let Some((root, alpha)) = rects[NodeId::ROOT.index()] {
        cap(&f, root, prepared, alpha);
    }
    for (i, slot) in rects.iter().enumerate() {
        let Some((rect, alpha)) = *slot else { continue };
        if !rect.expand(20.0).intersects(visible) {
            continue;
        }
        let node_id = prepared.ids[i];
        let look = Look {
            selected: selected == Some(node_id),
            hovered: ui.ctx().animate_bool_with_time(id.with(("hover", i)), hovered == Some(node_id), 0.12),
            alpha,
        };
        let step = &prepared.steps[i];
        let screen = f.screen(rect);
        let folded = view.collapsed[i] && alpha >= 1.0;
        if folded && !step.join {
            fold_stack(&f, screen);
        }
        if step.join {
            paint_join(&f, screen, plan.node(node_id), step, look);
        } else {
            paint_box(&f, screen, plan.node(node_id), step, look);
        }
        if folded {
            fold_pill(&f, screen, step.descendants);
        }
    }

    if let Some(node) = hovered {
        let step = &prepared.steps[node.index()];
        response.on_hover_ui_at_pointer(|ui| tooltip(ui, plan.node(node), step, prepared.analyzed, dark));
    }
    legend(ui, canvas, prepared, p, dark);
    controls(ui, id, canvas, view, now);
    events
}

fn controls(ui: &mut Ui, id: Id, canvas: Rect, view: &mut GraphView, now: f64) {
    let p = palette(ui);
    let zoom = view.target().zoom;
    let label = format!("{:.0}%", zoom * 100.0);
    let (bw, lw, h) = (28.0, 50.0, 26.0);
    let rect =
        Rect::from_min_size(canvas.max - vec2(12.0 + 3.0 * bw + lw + 9.0, 12.0 + h), vec2(3.0 * bw + lw + 9.0, h));
    let painter = ui.painter_at(canvas);
    painter.rect(
        rect,
        CornerRadius::same(7),
        p.plan_card.gamma_multiply(0.94),
        Stroke::new(1.0, p.plan_card_border),
        StrokeKind::Inside,
    );
    let slot = |left: f32, w: f32| Rect::from_min_size(pos2(rect.left() + left, rect.top() + 2.0), vec2(w, h - 4.0));
    let (r_out, r_reset, r_in, r_fit) =
        (slot(2.0, bw), slot(2.0 + bw, lw), slot(2.0 + bw + lw, bw), slot(5.0 + 2.0 * bw + lw, bw));
    let button = |r: Rect, key: &str, tip: &str| {
        let resp = ui.interact(r, id.with(key), Sense::click()).on_hover_text(tip);
        if resp.hovered() {
            painter.rect_filled(r, CornerRadius::same(5), p.hover);
        }
        resp
    };
    let out = button(r_out, "zoom_out", "Zoom out (⌘-scroll or pinch)");
    let reset = button(r_reset, "zoom_reset", "Actual size");
    let zin = button(r_in, "zoom_in", "Zoom in (⌘-scroll or pinch)");
    let fit = button(r_fit, "fit", "Fit to view");
    icons::paint(&painter, Rect::from_center_size(r_out.center(), Vec2::splat(12.0)), Icon::Minus, p.text_muted);
    painter.text(r_reset.center(), Align2::CENTER_CENTER, &label, theme::ui_font(11.5), p.text_muted);
    icons::paint(&painter, Rect::from_center_size(r_in.center(), Vec2::splat(12.0)), Icon::Plus, p.text_muted);
    let sep = r_fit.left() - 1.5;
    painter.line_segment(
        [pos2(sep, rect.top() + 6.0), pos2(sep, rect.bottom() - 6.0)],
        Stroke::new(1.0, p.plan_card_border),
    );
    fit_icon(&painter, r_fit.center(), p.text_muted);

    let to = [(out, zoom / 1.25), (zin, zoom * 1.25), (reset, 1.0)].into_iter().find(|(r, _)| r.clicked());
    if let Some((_, zoom)) = to {
        view.zoom_to(zoom, Vec2::ZERO, now);
    } else if fit.clicked() {
        view.fly(view.fit(canvas), now, 0.4);
    }
}

/// Arrows walk the tree, Enter or Space folds, and Esc closes the details, then clears the selection.
fn keyboard(
    ui: &mut Ui,
    plan: &Plan,
    view: &mut GraphView,
    mut selected: Option<NodeId>,
    mut detail_open: bool,
    now: f64,
) -> Vec<PlanEvent> {
    let keys = [Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight, Key::Enter, Key::Space, Key::Escape];
    let pressed: Vec<Key> = ui.input(|i| keys.into_iter().filter(|k| i.key_pressed(*k)).collect());
    let copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
    let mut events = Vec::new();
    for key in pressed {
        let Some(layout) = view.prepared.as_ref().map(|p| &p.layout) else { return events };
        let neighbour = |sel: NodeId, step: isize| {
            let row = &layout.rows[layout.row_of(sel)?];
            let at = row.iter().position(|n| *n == sel)? as isize + step;
            row.get(usize::try_from(at).ok()?).copied()
        };
        let event = match (key, selected) {
            (Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight, None) => {
                Some(PlanEvent::Select(NodeId::ROOT))
            }
            (Key::ArrowUp, Some(sel)) => plan.parent(sel).map(PlanEvent::Select),
            (Key::ArrowDown, Some(sel)) => {
                plan.children(sel).first().filter(|c| layout.rect(**c).is_some()).map(|c| PlanEvent::Select(*c))
            }
            (Key::ArrowLeft, Some(sel)) => neighbour(sel, -1).map(PlanEvent::Select),
            (Key::ArrowRight, Some(sel)) => neighbour(sel, 1).map(PlanEvent::Select),
            (Key::Enter | Key::Space, Some(sel)) => {
                view.toggle(plan, sel, now, selected).then_some(PlanEvent::Select(sel))
            }
            (Key::Escape, Some(_)) if detail_open => Some(PlanEvent::CloseDetail),
            (Key::Escape, Some(_)) => Some(PlanEvent::Deselect),
            _ => None,
        };
        if let Some(event) = event {
            event.apply(&mut selected, &mut detail_open);
            events.push(event);
        }
    }
    if copy && let Some(sel) = selected {
        ui.ctx().copy_text(crate::plan_view::node_text(plan.node(sel)));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::steps::flow;
    use super::*;
    use tableplus_core::plan::{Actual, Estimate, PlanFormat, PlanNode};

    fn ran(op: &str, rows: f64, loops: u64) -> PlanNode {
        let mut n = PlanNode::new(op);
        n.actual = Some(Actual { rows: Some(rows), loops, time: None });
        n
    }

    fn estimated(op: &str, rows: f64) -> PlanNode {
        let mut n = PlanNode::new(op);
        n.estimate = Some(Estimate { rows, cost: None });
        n
    }

    /// join ─┬─ scan ×1
    ///       ├─ hash (no numbers) ── scan
    ///       └─ lookup × 830 loops
    fn analyzed() -> (Plan, [NodeId; 5]) {
        let mut plan = Plan::new(PlanFormat::MySqlTree, ran("Inner hash join", 287.0, 1), "");
        let scan = plan.add_child(NodeId::ROOT, ran("Table scan", 91.0, 1));
        let hash = plan.add_child(NodeId::ROOT, PlanNode::new("Hash"));
        let inner = plan.add_child(hash, ran("Table scan", 830.0, 1));
        let lookup = plan.add_child(NodeId::ROOT, ran("Single-row index lookup", 1.0, 830));
        (plan, [NodeId::ROOT, scan, hash, inner, lookup])
    }

    #[test]
    fn edges_carry_the_rows_of_every_loop() {
        let (plan, [_, scan, hash, inner, lookup]) = analyzed();
        assert_eq!(flow(&plan, scan, true), Some(91.0));
        assert_eq!(flow(&plan, lookup, true), Some(830.0));
        assert_eq!(flow(&plan, hash, true), flow(&plan, inner, true), "a step without numbers passes its input on");
        let mut never = plan.clone();
        never.add_child(NodeId::ROOT, ran("Index lookup", 0.0, 0));
        assert_eq!(flow(&never, never.nodes().last().unwrap().0, true), Some(0.0));
    }

    #[test]
    fn unestimated_steps_carry_no_row_count() {
        let mut plan = Plan::new(PlanFormat::MySqlTree, estimated("Limit", 0.0), "");
        let sort = plan.add_child(NodeId::ROOT, PlanNode::new("Sort"));
        plan.add_child(sort, estimated("Table scan", 1616.0));
        assert_eq!(flow(&plan, NodeId::ROOT, false), None, "MySQL's rows=0 means not estimated");
        assert_eq!(flow(&plan, sort, false), Some(1616.0));
    }

    fn view(plan: &Plan) -> GraphView {
        let mut view = GraphView::default();
        view.collapsed = vec![false; plan.node_count()];
        view.prepared = Some(prepare(plan, &view.collapsed));
        view
    }

    fn canvas() -> Rect {
        Rect::from_min_size(pos2(250.0, 310.0), vec2(1190.0, 590.0))
    }

    #[test]
    fn fit_shows_every_step() {
        let (plan, _) = analyzed();
        let mut view = view(&plan);
        view.camera = view.fit(canvas());
        let layout = &view.prepared.as_ref().unwrap().layout;
        for (id, _) in plan.nodes() {
            let on_screen = view.camera.rect(canvas(), layout.rect(id).unwrap());
            assert!(canvas().contains_rect(on_screen), "{id:?} is on screen");
        }
    }

    #[test]
    fn revealing_a_step_brings_it_on_screen() {
        let (plan, [.., lookup]) = analyzed();
        let mut view = view(&plan);
        let rect = view.prepared.as_ref().unwrap().layout.rect(lookup).unwrap();
        view.camera = Camera { center: pos2(-3000.0, -3000.0), zoom: 1.0 };
        view.reveal_rect(canvas(), rect, 0.0);
        let (camera, done) = view.flight.expect("flies to the step").at(10.0);
        assert!(done);
        assert!(canvas().contains_rect(camera.rect(canvas(), rect)));
    }

    #[test]
    fn folding_keeps_the_step_in_place_and_reports_a_hidden_selection() {
        let (plan, [_, _, hash, inner, _]) = analyzed();
        let mut view = view(&plan);
        let before = view.camera.rect(canvas(), view.prepared.as_ref().unwrap().layout.rect(hash).unwrap());
        assert!(view.toggle(&plan, hash, 0.0, Some(inner)), "folding the hash hides its inner scan");
        let layout = &view.prepared.as_ref().unwrap().layout;
        assert_eq!(layout.rect(inner), None, "the folded step's inputs are hidden");
        let after = view.camera.rect(canvas(), layout.rect(hash).unwrap());
        assert!((after.center() - before.center()).length() < 0.01, "the folded step stays where it was");
    }

    #[test]
    fn a_step_picked_inside_a_folded_branch_unfolds_it() {
        let (plan, [_, scan, hash, inner, _]) = analyzed();
        let mut view = view(&plan);
        view.toggle(&plan, hash, 0.0, None);
        let before = view.camera.rect(canvas(), view.prepared.as_ref().unwrap().layout.rect(hash).unwrap());
        assert!(!view.unfold_to(&plan, scan, 1.0), "nothing folded above the scan");
        assert!(view.unfold_to(&plan, inner, 1.0), "the hash is folded above its inner scan");
        let layout = &view.prepared.as_ref().unwrap().layout;
        assert!(layout.rect(inner).is_some(), "the picked step is shown again");
        let after = view.camera.rect(canvas(), layout.rect(hash).unwrap());
        assert!((after.center() - before.center()).length() < 0.01, "the unfolded step stays where it was");
    }

    // Each layout size rasterizes the font again, so a zoom gesture must pass through only a few.
    #[test]
    fn a_moving_zoom_lays_text_out_at_few_sizes() {
        let zooms = (0..200).map(|i| TEXT_ZOOM + i as f32 * (MAX_ZOOM - TEXT_ZOOM) / 199.0);
        let mut sizes: Vec<f32> = zooms.clone().map(|z| TextScale::new(z, true).layout).collect();
        sizes.dedup();
        assert!(sizes.len() <= 8, "{} sizes while zooming", sizes.len());
        assert!(zooms.clone().all(|z| TextScale::new(z, true).mesh <= 1.0), "glyphs only shrink, so text stays sharp");
        assert!(
            zooms.clone().all(|z| (TextScale::new(z, false).mesh - 1.0).abs() < 0.05),
            "at rest text is laid out at its size"
        );
    }
}
