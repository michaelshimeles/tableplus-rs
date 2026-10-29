//! The Plan pane's tree: one row per step with its estimated rows, actual rows, loops and a time
//! bar. Rows are custom-painted and only the visible ones are laid out, like the data grid, so
//! long and deeply nested plans stay smooth.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, EventFilter, FontId, Galley, Id, Key, Painter, Pos2, Rect, Sense, Shape,
    Stroke, Ui, pos2, scroll_area::ScrollSource, text::LayoutJob, vec2,
};
use tableplus_core::plan::{NodeId, Plan, PlanNode};

use super::{Metrics, PlanView, node_text};
use crate::plan_graph;
use crate::theme::{self, Palette, palette};
use crate::widgets::{ease_out, fmt_count, fmt_factor, fmt_ms, fmt_rows};

const ROW_H: f32 = 26.0;
const HEADER_H: f32 = 28.0;
const PAD: f32 = 10.0;
const CHEVRON: f32 = 16.0;
const BAR_H: f32 = 8.0;
pub(super) const BADGE_PAD: f32 = 10.0;
const GROW_SECONDS: f32 = 0.45;

/// The right-hand number and bar columns, which stay put while the tree scrolls sideways.
struct Columns {
    analyzed: bool,
    /// Off when no step has a number, as in SQLite's plans; the tree then takes the full width.
    shown: bool,
    /// The bar gives up width first when the pane is narrow, so step names stay readable.
    bar: f32,
}

impl Columns {
    const EST: f32 = 78.0;
    const ROWS: f32 = 116.0;
    const LOOPS: f32 = 58.0;
    const TIME: f32 = 74.0;
    const COST: f32 = 74.0;

    fn new(plan: &Plan, pane_width: f32) -> Self {
        let analyzed = plan.analyzed();
        let shown = analyzed || plan.nodes().any(|(_, n)| n.estimate.is_some());
        let mut cols = Self { analyzed, shown, bar: 0.0 };
        cols.bar = (pane_width - cols.numbers() - 2.0 * PAD - 420.0).clamp(96.0, 176.0);
        cols
    }

    fn numbers(&self) -> f32 {
        if self.analyzed { Self::EST + Self::ROWS + Self::LOOPS + Self::TIME } else { Self::EST + Self::COST }
    }

    fn width(&self) -> f32 {
        if !self.shown {
            return 0.0;
        }
        self.numbers() + self.bar + 2.0 * PAD
    }
}

pub(super) fn show(ui: &mut Ui, id: Id, view: &mut PlanView) {
    if ui.memory(|m| m.has_focus(id)) {
        keyboard(ui, view);
    }
    let has_focus = ui.memory(|m| m.has_focus(id));
    let p = palette(ui);
    let since = view.shown_at.get_or_insert_with(Instant::now).elapsed().as_secs_f32();
    let grow = ease_out((since / GROW_SECONDS).min(1.0));
    if grow < 1.0 {
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
    let Some(plan) = view.plan() else { return };
    let analyzed = plan.analyzed();
    let cols = Columns::new(plan, ui.available_width());
    let rows = view.visible.len();
    let max_depth = view.visible.iter().map(|n| plan.depth(*n)).max().unwrap_or(0) as f32;

    // Deep plans indent less, so their steps stay readable before the tree scrolls sideways.
    let tree_w = (ui.available_width() - cols.width()).max(240.0);
    let indent = ((tree_w - 260.0) / max_depth.max(1.0)).clamp(11.0, 18.0);
    let content_w = (PAD + max_depth * indent + CHEVRON + 260.0).max(tree_w) + cols.width();

    let mut scroll = egui::ScrollArea::both()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::MOUSE_WHEEL);
    if view.scroll_to_selected
        && let Some(row) = view.selected.and_then(|s| view.visible.iter().position(|v| *v == s))
        && let Some(state) = view.scroll_id.and_then(|sid| egui::scroll_area::State::load(ui.ctx(), sid))
    {
        let viewport_h = ui.available_height() - HEADER_H;
        let mut off = state.offset;
        let top = row as f32 * ROW_H;
        if top < off.y {
            off.y = top;
        } else if top + ROW_H > off.y + viewport_h - 8.0 {
            off.y = top + ROW_H - viewport_h + 8.0;
        }
        scroll = scroll.scroll_offset(off);
    }

    let mut toggle = None;
    let mut clicked = None;
    let output = scroll.show_viewport(ui, |ui, viewport| {
        ui.set_min_size(vec2(content_w, (HEADER_H + rows as f32 * ROW_H).max(viewport.height())));
        let origin = ui.max_rect().min;
        let clip = ui.clip_rect();
        let painter = ui.painter().clone();
        let body_top = clip.top() + HEADER_H;
        let metrics_left = clip.right() - cols.width();
        let tree_clip = Rect::from_min_max(pos2(clip.left(), body_top), pos2(metrics_left, clip.bottom()));
        let numbers_clip = Rect::from_min_max(pos2(metrics_left, body_top), clip.max);

        let body = ui.interact(Rect::from_min_max(pos2(clip.left(), body_top), clip.max), id, Sense::click());
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
        let row_at = |pos: Pos2| -> Option<usize> {
            let r = ((pos.y - origin.y - HEADER_H) / ROW_H).floor();
            (pos.y >= body_top && r >= 0.0 && (r as usize) < rows).then_some(r as usize)
        };
        let chevron_x = |depth: usize| origin.x + PAD + depth as f32 * indent;
        if let Some(pos) = body.interact_pointer_pos()
            && let Some(r) = row_at(pos)
        {
            let node = view.visible[r];
            let x = chevron_x(plan.depth(node));
            let on_chevron = !plan.children(node).is_empty()
                && pos.x < metrics_left
                && pos.x >= x - 3.0
                && pos.x < x + CHEVRON + 3.0;
            if body.clicked() {
                if on_chevron {
                    toggle = Some(node);
                } else {
                    clicked = Some(node);
                }
            }
            if body.double_clicked() && !on_chevron {
                toggle = Some(node);
            }
        }
        let hovered_row = body.hover_pos().and_then(row_at);
        let hover_x = body.hover_pos().map_or(f32::NEG_INFINITY, |h| h.x);

        let first = (viewport.min.y / ROW_H).floor().max(0.0) as usize;
        let last = ((viewport.max.y / ROW_H).ceil() as usize + 1).min(rows);
        let selected_depth = view.selected.map(|s| plan.depth(s));
        let tp = painter.with_clip_rect(tree_clip);
        let np = painter.with_clip_rect(numbers_clip);
        let num_font = theme::ui_font(12.5);

        for r in first..last {
            let node_id = view.visible[r];
            let node = plan.node(node_id);
            let m = &view.metrics[node_id.index()];
            let depth = plan.depth(node_id);
            let y = origin.y + HEADER_H + r as f32 * ROW_H;
            let row_rect = Rect::from_min_size(pos2(clip.left(), y), vec2(clip.width(), ROW_H));
            let selected = view.selected == Some(node_id);
            let on_accent = selected && has_focus;
            let bg = if selected {
                if has_focus { p.selection } else { p.selection_inactive }
            } else if hovered_row == Some(r) {
                p.hover
            } else if r % 2 == 1 {
                p.row_alt
            } else {
                p.content_bg
            };
            painter.rect_filled(row_rect, CornerRadius::ZERO, bg);
            let cy = row_rect.center().y;

            // Indent guides; the selected step's subtree is highlighted.
            let in_selection = view.selected.is_some_and(|s| s != node_id && is_within(plan, node_id, s));
            for level in 0..depth {
                let x = chevron_x(level) + CHEVRON / 2.0;
                let color = if on_accent {
                    p.accent_text.gamma_multiply(0.18)
                } else if in_selection && selected_depth == Some(level) {
                    p.plan_guide_active
                } else {
                    p.plan_guide
                };
                tp.line_segment([pos2(x, y), pos2(x, y + ROW_H)], Stroke::new(1.0, color));
            }

            let x = chevron_x(depth);
            let collapsed = view.collapsed[node_id.index()];
            if !plan.children(node_id).is_empty() {
                let open = ui.ctx().animate_bool_with_time(id.with(("open", node_id)), !collapsed, 0.12);
                chevron(&tp, pos2(x + CHEVRON / 2.0, cy), open, if on_accent { p.accent_text } else { p.text_muted });
            }

            // The graph's access colour: in the chevron's place on a leaf, before the text otherwise.
            let mut text_left = x + CHEVRON + 4.0;
            if let Some(access) = m.access {
                let center = if plan.children(node_id).is_empty() {
                    pos2(x + CHEVRON / 2.0, cy)
                } else {
                    text_left += 11.0;
                    pos2(text_left - 7.5, cy)
                };
                let swatch = Rect::from_center_size(center, vec2(7.0, 7.0));
                tp.rect_filled(swatch, CornerRadius::same(2), plan_graph::access_color(access, ui.visuals().dark_mode));
            }
            let hint = collapsed.then(|| format!("+{}", fmt_count(m.descendants as u64)));
            let badge_color = if on_accent { p.accent_text.gamma_multiply(0.85) } else { p.text_muted };
            let badges: Vec<Arc<Galley>> = m
                .extras
                .iter()
                .map(|e| ui.painter().layout_no_wrap(e.label().into(), theme::ui_font(10.5), badge_color))
                .collect();
            let badges_w: f32 = badges.iter().map(|g| g.size().x + BADGE_PAD + 6.0).sum();
            let room = metrics_left - text_left - 10.0 - badges_w - if hint.is_some() { 44.0 } else { 0.0 };
            let galley = label(ui, node, room.max(20.0), on_accent, p);
            let label_w = galley.size().x;
            if galley.elided && hovered_row == Some(r) && hover_x < metrics_left {
                body.clone().on_hover_ui_at_pointer(|ui| {
                    ui.label(full_label(node));
                });
            }
            tp.galley(pos2(text_left, cy - galley.size().y / 2.0), galley, p.text);
            let mut after = text_left + label_w + 8.0;
            let badge_border = if on_accent { p.accent_text.gamma_multiply(0.45) } else { p.border };
            for g in badges {
                after += extra_badge(&tp, pos2(after, cy), g, badge_color, badge_border) + 6.0;
            }
            if let Some(hint) = hint {
                let color = if on_accent { p.accent_text } else { p.text_muted };
                let g = ui.painter().layout_no_wrap(hint, theme::bold_font(10.5), color);
                let pill = Rect::from_min_size(pos2(after, cy - 8.0), vec2(g.size().x + 12.0, 16.0));
                tp.rect_filled(
                    pill,
                    CornerRadius::same(8),
                    if on_accent { p.accent_text.gamma_multiply(0.2) } else { p.plan_bar_track },
                );
                tp.galley(pill.center() - g.size() / 2.0, g, color);
            }

            if !cols.shown {
                continue;
            }
            // Number columns, painted over the row so long step text ends under them.
            np.rect_filled(
                Rect::from_min_max(pos2(metrics_left, y), pos2(clip.right(), y + ROW_H)),
                CornerRadius::ZERO,
                bg,
            );
            let (strong, weak, faint) = if on_accent {
                (p.accent_text, p.accent_text.gamma_multiply(0.8), p.accent_text.gamma_multiply(0.6))
            } else {
                (p.text, p.text_muted, p.text_faint)
            };
            let number = |left: f32, w: f32, text: &str, color: Color32| {
                np.text(pos2(left + w - 8.0, cy), Align2::RIGHT_CENTER, text, num_font.clone(), color).width()
            };
            // A dash wherever the server gave no estimate, whether it printed a placeholder or nothing.
            let estimated = |left: f32, w: f32, value: Option<f64>, color: Color32| match value {
                Some(v) => number(left, w, &fmt_rows(v), color),
                None => number(left, w, "–", faint),
            };
            let mut cx = metrics_left + PAD;
            estimated(cx, Columns::EST, plan.estimated_rows(node_id), weak);
            cx += Columns::EST;
            if analyzed {
                if let Some(actual) = node.actual {
                    let misestimated = plan.is_misestimated(node_id);
                    let text =
                        if actual.loops == 0 { Some("never ran".to_string()) } else { actual.rows.map(fmt_rows) };
                    let color = if misestimated && !on_accent { p.warning } else { strong };
                    let text_w = text.map_or(0.0, |text| number(cx, Columns::ROWS, &text, color));
                    if misestimated && let Some(ratio) = plan.row_estimate_ratio(node_id) {
                        let right = cx + Columns::ROWS - 8.0 - text_w - 5.0;
                        misestimate_badge(
                            &np,
                            pos2(right, cy),
                            ratio,
                            if on_accent { p.accent_text } else { p.warning },
                        );
                    }
                    let loops_color = if actual.loops == 1 { faint } else { weak };
                    number(cx + Columns::ROWS, Columns::LOOPS, &fmt_count(actual.loops), loops_color);
                }
                cx += Columns::ROWS + Columns::LOOPS;
                if let Some(ms) = m.inclusive_ms {
                    number(cx, Columns::TIME, &fmt_ms(ms), strong);
                }
                cx += Columns::TIME;
            } else {
                estimated(cx, Columns::COST, m.cost, strong);
                cx += Columns::COST;
            }
            let bar = Rect::from_min_size(pos2(cx + 4.0, cy - BAR_H / 2.0), vec2(cols.bar - 12.0, BAR_H));
            paint_bar(&np, bar, node, m, view.scale, grow, analyzed, on_accent, p);
            if hovered_row == Some(r)
                && hover_x >= bar.left() - 4.0
                && let Some(tip) = bar_tooltip(node, m, view.scale, analyzed)
            {
                body.clone().on_hover_ui_at_pointer(|ui| {
                    ui.label(tip);
                });
            }
        }

        // Sticky header.
        let header = Rect::from_min_size(clip.min, vec2(clip.width(), HEADER_H));
        painter.rect_filled(header, CornerRadius::ZERO, p.header_bg);
        painter.line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
        let head_font = theme::bold_font(12.0);
        let hy = header.center().y;
        painter.text(pos2(clip.left() + PAD + 4.0, hy), Align2::LEFT_CENTER, "Step", head_font.clone(), p.text);
        if !cols.shown {
            return;
        }
        painter.line_segment(
            [pos2(metrics_left, clip.top()), pos2(metrics_left, clip.bottom())],
            Stroke::new(1.0, p.grid_line),
        );
        let head = |left: f32, w: f32, text: &str| {
            painter.text(pos2(left + w - 8.0, hy), Align2::RIGHT_CENTER, text, head_font.clone(), p.text);
        };
        let mut cx = metrics_left + PAD;
        head(cx, Columns::EST, "Est. rows");
        cx += Columns::EST;
        if analyzed {
            head(cx, Columns::ROWS, "Rows");
            head(cx + Columns::ROWS, Columns::LOOPS, "Loops");
            head(cx + Columns::ROWS + Columns::LOOPS, Columns::TIME, "Time");
            cx += Columns::ROWS + Columns::LOOPS + Columns::TIME;
        } else {
            head(cx, Columns::COST, "Cost");
            cx += Columns::COST;
        }
        bar_legend(&painter, pos2(cx + 4.0, hy), analyzed, view.cost_below, p);
    });
    view.scroll_id = Some(output.id);
    view.scroll_to_selected = false;

    if let Some(node) = toggle {
        let collapsed = !view.collapsed[node.index()];
        view.set_collapsed(node, collapsed);
        view.selected = Some(node);
    }
    if let Some(node) = clicked {
        view.selected = Some(node);
        view.detail_open = true;
    }
}

/// True when `node` is `ancestor` or below it.
fn is_within(plan: &Plan, node: NodeId, ancestor: NodeId) -> bool {
    let mut at = Some(node);
    while let Some(n) = at {
        if n == ancestor {
            return true;
        }
        at = plan.parent(n);
    }
    false
}

fn keyboard(ui: &mut Ui, view: &mut PlanView) {
    let pressed: Vec<Key> = ui.input(|i| {
        [
            Key::ArrowUp,
            Key::ArrowDown,
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
            Key::Space,
            Key::Enter,
            Key::Escape,
        ]
        .into_iter()
        .filter(|k| i.key_pressed(*k))
        .collect()
    });
    let copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
    let page = (ui.available_height() / ROW_H).max(1.0) as usize;
    for key in pressed {
        match next_move(view, key, page) {
            Some(Move::Row(r)) => view.select_row(r),
            Some(Move::Fold(node, collapsed)) => view.set_collapsed(node, collapsed),
            Some(Move::CloseDetail) => view.detail_open = false,
            Some(Move::Deselect) => view.selected = None,
            None => {}
        }
    }
    if copy
        && let Some(plan) = view.plan()
        && let Some(sel) = view.selected
    {
        ui.ctx().copy_text(node_text(plan.node(sel)));
    }
}

enum Move {
    Row(usize),
    Fold(NodeId, bool),
    CloseDetail,
    Deselect,
}

fn next_move(view: &PlanView, key: Key, page: usize) -> Option<Move> {
    let plan = view.plan()?;
    let row = view.selected.and_then(|s| view.visible.iter().position(|v| *v == s));
    let last = view.visible.len().saturating_sub(1);
    let position = |id: NodeId| view.visible.iter().position(|v| *v == id);
    let folded = |id: NodeId| view.collapsed[id.index()];
    Some(match (key, view.selected) {
        (Key::ArrowUp, _) => Move::Row(row.map_or(0, |r| r.saturating_sub(1))),
        (Key::ArrowDown, _) => Move::Row(row.map_or(0, |r| (r + 1).min(last))),
        (Key::PageUp, _) => Move::Row(row.unwrap_or(0).saturating_sub(page)),
        (Key::PageDown, _) => Move::Row((row.unwrap_or(0) + page).min(last)),
        (Key::Home, _) => Move::Row(0),
        (Key::End, _) => Move::Row(last),
        (Key::ArrowLeft, Some(sel)) if !plan.children(sel).is_empty() && !folded(sel) => Move::Fold(sel, true),
        (Key::ArrowLeft, Some(sel)) => Move::Row(position(plan.parent(sel)?)?),
        (Key::ArrowRight, Some(sel)) if folded(sel) => Move::Fold(sel, false),
        (Key::ArrowRight, Some(sel)) => Move::Row(position(*plan.children(sel).first()?)?),
        (Key::Space | Key::Enter, Some(sel)) => Move::Fold(sel, !folded(sel)),
        (Key::Escape, Some(_)) if view.detail_open => Move::CloseDetail,
        (Key::Escape, Some(_)) => Move::Deselect,
        (Key::Escape, None) if view.detail_open && !view.facts.is_empty() => Move::CloseDetail,
        _ => return None,
    })
}

fn full_label(node: &PlanNode) -> String {
    let parts =
        [node.role.as_ref(), Some(&node.operation), node.object.as_ref(), node.index.as_ref(), node.condition.as_ref()];
    parts.into_iter().flatten().map(String::as_str).collect::<Vec<_>>().join("  ·  ")
}

/// `Covering index lookup  p2  · idx_products_category_id   category_id=cat2.category_id`, elided.
fn label(ui: &Ui, node: &PlanNode, max_w: f32, on_accent: bool, p: &Palette) -> Arc<Galley> {
    let (text, muted, faint) = if on_accent {
        (p.accent_text, p.accent_text.gamma_multiply(0.85), p.accent_text.gamma_multiply(0.7))
    } else {
        (p.text, p.text_muted, p.text_faint)
    };
    let fmt = |font: FontId, color: Color32| egui::TextFormat { font_id: font, color, ..Default::default() };
    let mut job = LayoutJob::default();
    if let Some(role) = &node.role {
        job.append(role, 0.0, fmt(theme::ui_font(12.0), muted));
    }
    job.append(&node.operation, if node.role.is_some() { 6.0 } else { 0.0 }, fmt(theme::ui_font(13.0), text));
    if let Some(object) = &node.object {
        job.append(object, 6.0, fmt(theme::bold_font(13.0), text));
    }
    if let Some(index) = &node.index {
        job.append(&format!("· {index}"), 6.0, fmt(theme::ui_font(12.0), muted));
    }
    if let Some(condition) = &node.condition {
        job.append(&condition.replace('\n', " "), 10.0, fmt(theme::mono_font(11.5), faint));
    }
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

/// An outlined tag such as `temp table`. Returns its width.
pub(super) fn extra_badge(
    painter: &Painter,
    left_center: Pos2,
    galley: Arc<Galley>,
    color: Color32,
    border: Color32,
) -> f32 {
    let rect = Rect::from_min_size(pos2(left_center.x, left_center.y - 7.5), vec2(galley.size().x + BADGE_PAD, 15.0));
    painter.rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, border), egui::StrokeKind::Inside);
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
    rect.width()
}

fn chevron(painter: &Painter, center: Pos2, open: f32, color: Color32) {
    let (s, c) = (open * std::f32::consts::FRAC_PI_2).sin_cos();
    let rot = |x: f32, y: f32| center + vec2(x * c - y * s, x * s + y * c);
    painter.add(Shape::convex_polygon(vec![rot(-2.5, -4.0), rot(3.5, 0.0), rot(-2.5, 4.0)], color, Stroke::NONE));
}

/// `▲ 67×`: actual rows above (▲) or below (▼) the estimate.
fn misestimate_badge(painter: &Painter, right_center: Pos2, ratio: f64, color: Color32) {
    let factor = if ratio >= 1.0 { ratio } else { 1.0 / ratio };
    let g = painter.layout_no_wrap(format!("{}×", fmt_factor(factor)), theme::bold_font(10.5), color);
    let w = g.size().x + 17.0;
    let rect = Rect::from_min_size(pos2(right_center.x - w, right_center.y - 8.0), vec2(w, 16.0));
    painter.rect_filled(rect, CornerRadius::same(4), color.gamma_multiply(0.16));
    let (cx, cy) = (rect.left() + 7.0, rect.center().y);
    let tri = if ratio >= 1.0 {
        vec![pos2(cx - 3.0, cy + 2.5), pos2(cx + 3.0, cy + 2.5), pos2(cx, cy - 2.5)]
    } else {
        vec![pos2(cx - 3.0, cy - 2.5), pos2(cx + 3.0, cy - 2.5), pos2(cx, cy + 2.5)]
    };
    painter.add(Shape::convex_polygon(tri, color, Stroke::NONE));
    painter.galley(pos2(rect.left() + 12.0, cy - g.size().y / 2.0), g, color);
}

#[allow(clippy::too_many_arguments)]
fn paint_bar(
    painter: &Painter,
    bar: Rect,
    node: &PlanNode,
    m: &Metrics,
    scale: f64,
    grow: f32,
    analyzed: bool,
    on_accent: bool,
    p: &Palette,
) {
    let radius = CornerRadius::same((BAR_H / 2.0) as u8);
    let (track, fill, strong, below) = if on_accent {
        let c = p.accent_text;
        (c.gamma_multiply(0.1), c.gamma_multiply(0.45), c, c.gamma_multiply(0.22))
    } else if analyzed {
        (p.plan_bar_track, p.plan_bar, p.plan_bar_self, p.plan_bar)
    } else {
        (p.plan_bar_track, p.plan_cost, p.plan_cost, p.plan_cost_below)
    };
    let has_value = if analyzed { m.inclusive_ms.is_some() } else { m.cost_span > 0.0 };
    if !has_value || scale <= 0.0 {
        return;
    }
    painter.rect_filled(bar, radius, track);
    let width = |value: f64| bar.width() * (value / scale).clamp(0.0, 1.0) as f32 * grow;
    // Thin slivers stay thin: a dot on every short step would read as a hot spot.
    let segment = |w: f32, color: Color32| {
        if w > 0.0 {
            let w = w.max(1.5);
            let r = CornerRadius::same((w / 2.0).min(BAR_H / 2.0) as u8);
            painter.rect_filled(Rect::from_min_size(bar.min, vec2(w, bar.height())), r, color);
        }
    };
    if analyzed {
        let Some(total) = m.inclusive_ms else { return };
        let w = width(total);
        segment(w, fill);
        if let Some(own) = m.self_ms.filter(|own| *own > 0.0) {
            segment(width(own).min(w), strong);
        }
        if let Some(time) = node.actual.and_then(|a| a.time)
            && let Some(first) = time.first_row_ms
            && time.last_row_ms > 0.0
            && w >= 10.0
        {
            let x = bar.left() + w * (first / time.last_row_ms).clamp(0.0, 1.0) as f32;
            let tick = if on_accent { p.accent_text } else { p.plan_tick };
            painter.line_segment([pos2(x, bar.top() - 3.0), pos2(x, bar.bottom() + 3.0)], Stroke::new(1.5, tick));
        }
    } else {
        if m.spans_below() {
            segment(width(m.cost_span), below);
        }
        segment(width(m.cost.unwrap_or(0.0)), fill);
    }
}

fn bar_legend(painter: &Painter, left_center: Pos2, analyzed: bool, cost_below: bool, p: &Palette) {
    let font = theme::ui_font(11.0);
    let mut x = left_center.x;
    let mut item = |color: Color32, text: &str| {
        let swatch = Rect::from_center_size(pos2(x + 4.0, left_center.y), vec2(8.0, 8.0));
        painter.rect_filled(swatch, CornerRadius::same(2), color);
        let r = painter.text(pos2(x + 12.0, left_center.y), Align2::LEFT_CENTER, text, font.clone(), p.text_muted);
        x = r.right() + 10.0;
    };
    if analyzed {
        item(p.plan_bar_self, "self");
        item(p.plan_bar, "total");
    } else {
        item(p.plan_cost, "cost");
        if cost_below {
            item(p.plan_cost_below, "steps below");
        }
    }
}

fn bar_tooltip(node: &PlanNode, m: &Metrics, scale: f64, analyzed: bool) -> Option<String> {
    if analyzed {
        let total = m.inclusive_ms?;
        let mut tip = format!("{} including the steps below", fmt_ms(total));
        if let Some(own) = m.self_ms {
            tip.push_str(&format!("\n{} in this step itself", fmt_ms(own)));
        }
        if scale > 0.0 {
            tip.push_str(&format!("\n{:.0}% of the whole statement", 100.0 * total / scale));
        }
        if let Some(first) = node.actual.and_then(|a| a.time).and_then(|t| t.first_row_ms) {
            tip.push_str(&format!("\nFirst row after {} (per loop)", fmt_ms(first)));
        }
        Some(tip)
    } else {
        if scale <= 0.0 || m.cost_span <= 0.0 {
            return None;
        }
        let share = |cost: f64| format!("{:.0}% of the plan's cost", 100.0 * cost / scale);
        Some(match m.cost {
            Some(cost) if m.spans_below() => format!(
                "Cost {}\nThe steps below cost {}, which this step doesn't count.\nThe bar spans them: {}",
                fmt_rows(cost),
                fmt_rows(m.cost_span),
                share(m.cost_span)
            ),
            Some(cost) => format!("Cost {}, {}", fmt_rows(cost), share(cost)),
            None => format!(
                "Not costed by the server\nThe steps below cost {}, {}",
                fmt_rows(m.cost_span),
                share(m.cost_span)
            ),
        })
    }
}
