//! Painting the flame chart: its bars, tooltips, the zoom breadcrumb and legend, and what the
//! view shows for a plan without step times.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui, pos2,
    text::LayoutJob, vec2,
};
use tableplus_core::plan::{NodeId, Plan};

use super::{Bar, Flame, PAD, ancestry};
use crate::theme::{self, Palette, palette};
use crate::widgets::{fmt_count, fmt_ms, fmt_rows};

const BAR_H: f32 = 22.0;
const UNTIMED_H: f32 = 14.0;
/// Own time below this share of the statement stays a quiet blue; from it, amber warms to red.
const NOTABLE_SHARE: f64 = 0.01;
const HOT_SHARE: f64 = 0.3;

/// Own time as a color. Blue to amber has no clean blend, it goes through grey, so a share
/// too small to matter is blue and a notable one runs from amber to red.
fn heat_color(p: &Palette, share: f64) -> Color32 {
    if share < NOTABLE_SHARE {
        return p.plan_heat_low;
    }
    let t = ((share - NOTABLE_SHARE) / (HOT_SHARE - NOTABLE_SHARE)).clamp(0.0, 1.0).sqrt();
    p.plan_heat_mid.lerp_to_gamma(p.plan_heat_high, t as f32)
}

/// `4%`, or `<1%` for a share that would round to nothing.
fn fmt_share(share: f64) -> String {
    if share > 0.0 && share < 0.005 { "<1%".into() } else { format!("{:.0}%", share * 100.0) }
}

pub(super) struct Look {
    pub(super) hovered: bool,
    pub(super) selected: bool,
    pub(super) focused: bool,
    /// Rows fade in when the chart first shows, and context above a zoomed step stays faded.
    pub(super) fade: f32,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_bar(
    painter: &egui::Painter,
    chart: Rect,
    plan: &Plan,
    node: NodeId,
    bar: &Bar,
    (x0, x1): (f32, f32),
    cy: f32,
    to_x: impl Fn(f64) -> f32,
    look: Look,
    p: &Palette,
) {
    let timed = bar.heat.is_some();
    let h = if timed { BAR_H } else { UNTIMED_H };
    // Clamped, so a zoomed-in ancestor millions of points wide stays a sane rectangle.
    let (left, right) = (x0.max(chart.left() - 4.0), x1.min(chart.right() + 4.0));
    let mut rect = Rect::from_min_max(pos2(left, cy - h / 2.0), pos2(right, cy + h / 2.0));
    if rect.width() > 3.0 {
        rect = rect.shrink2(vec2(0.5, 0.0));
    } else {
        rect = Rect::from_center_size(rect.center(), vec2(rect.width().max(1.5), h));
    }
    let radius = (rect.width() / 2.0).min(3.0) as u8;
    let lift = |c: Color32| {
        let c = if look.hovered { c.lerp_to_gamma(p.text, 0.12) } else { c };
        c.gamma_multiply(look.fade)
    };

    if timed {
        let own = bar.heat.map_or(p.plan_bar, |share| heat_color(p, share));
        let own_left = to_x(bar.span.right() - bar.own).max(rect.left());
        if own_left <= rect.left() + 0.5 {
            painter.rect_filled(rect, CornerRadius::same(radius), lift(own));
        } else {
            painter.rect_filled(rect, CornerRadius::same(radius), lift(p.plan_bar));
            if rect.right() - own_left >= 0.5 {
                let part = Rect::from_min_max(pos2(own_left, rect.top()), rect.max);
                let corners = CornerRadius { nw: 0, sw: 0, ne: radius, se: radius };
                painter.rect_filled(part, corners, lift(own));
            }
        }
        if bar.stretched.is_some() && rect.width() > 10.0 {
            let y = rect.bottom() - 2.0;
            stretch_mark(painter, rect.left() + 3.0, rect.right() - 3.0, y, p.warning.gamma_multiply(look.fade));
        }
    } else {
        let outline = Stroke::new(1.0, p.border.gamma_multiply(look.fade));
        painter.rect(rect, radius, lift(p.plan_bar_track), outline, StrokeKind::Inside);
    }
    if look.selected {
        let color = if look.focused { p.accent } else { p.accent.gamma_multiply(0.7) }.gamma_multiply(look.fade);
        painter.rect_stroke(rect.expand(1.0), radius + 1, Stroke::new(2.0, color), StrokeKind::Inside);
    }

    let text_left = left.max(chart.left()) + 6.0;
    let room = right.min(chart.right()) - 6.0 - text_left;
    if room < 22.0 {
        return;
    }
    let node_ref = plan.node(node);
    let (text, muted) = if timed {
        (p.text.gamma_multiply(look.fade), p.text.gamma_multiply(0.62 * look.fade))
    } else {
        (p.text_muted.gamma_multiply(look.fade), p.text_faint.gamma_multiply(look.fade))
    };
    let size = if timed { 12.0 } else { 11.0 };
    let label = painter.layout_job(label_job(node_ref, size, room, text));
    let full_w = if label.elided { f32::INFINITY } else { label.size().x };
    painter.galley(pos2(text_left, cy - label.size().y / 2.0), label, text);
    if let Some(ms) = plan.inclusive_ms(node) {
        let g = painter.layout_no_wrap(fmt_ms(ms), theme::ui_font(11.0), muted);
        if full_w + g.size().x + 14.0 <= room {
            painter.galley(pos2(text_left + room - g.size().x, cy - g.size().y / 2.0), g, muted);
        }
    }
}

/// `Table scan c`: the operation, and the object it reads in bold, elided to `max_w`.
fn label_job(node: &tableplus_core::plan::PlanNode, size: f32, max_w: f32, color: Color32) -> LayoutJob {
    let fmt = |font: FontId| egui::TextFormat { font_id: font, color, ..Default::default() };
    let mut job = LayoutJob::default();
    job.append(&node.operation, 0.0, fmt(theme::ui_font(size)));
    if let Some(object) = &node.object {
        job.append(object, 5.0, fmt(theme::bold_font(size)));
    }
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job
}

/// A dashed underline: the bar is wider than the step's own reported time.
fn stretch_mark(painter: &egui::Painter, left: f32, right: f32, y: f32, color: Color32) {
    let mut x = left;
    while x < right {
        painter.line_segment([pos2(x, y), pos2((x + 4.0).min(right), y)], Stroke::new(1.5, color));
        x += 7.0;
    }
}

pub(super) fn tooltip(ui: &mut Ui, plan: &Plan, id: NodeId, bar: &Bar, total_ms: f64) {
    let p = palette(ui);
    let node = plan.node(id);
    ui.set_max_width(380.0);
    ui.label(RichText::new(&node.operation).font(theme::bold_font(13.0)).color(p.text));
    let object: Vec<&str> = [&node.object, &node.index].into_iter().flatten().map(String::as_str).collect();
    if !object.is_empty() {
        ui.label(RichText::new(object.join("  ·  ")).size(12.0).color(p.text_muted));
    }
    if let Some(access) = plan.access(id) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(8.0, 12.0), Sense::hover());
            let color = crate::plan_graph::access_color(access, ui.visuals().dark_mode);
            ui.painter().rect_filled(Rect::from_center_size(r.center(), vec2(8.0, 8.0)), CornerRadius::same(2), color);
            ui.label(RichText::new(access.label()).size(12.0).color(p.text_muted));
        });
    }
    for extra in plan.extras(id) {
        ui.label(RichText::new(extra.description()).size(12.0).color(p.text_muted));
    }
    if let Some(condition) = &node.condition {
        ui.add(egui::Label::new(RichText::new(condition).font(theme::mono_font(11.5)).color(p.text_muted)).wrap());
    }
    ui.add_space(4.0);
    egui::Grid::new("plan_flame_tip").num_columns(2).striped(false).spacing([14.0, 3.0]).show(ui, |ui| {
        let mut fact = |key: &str, value: String| {
            ui.label(RichText::new(key).size(12.0).color(p.text_muted));
            ui.label(RichText::new(value).size(12.0).color(p.text));
            ui.end_row();
        };
        match plan.inclusive_ms(id) {
            Some(ms) => fact("Time", format!("{}  ·  {} of the statement", fmt_ms(ms), fmt_share(ms / total_ms))),
            None => fact("Time", "not timed".into()),
        }
        match plan.self_ms(id) {
            Some(own) if own > 0.0 => {
                fact("In this step", format!("{}  ·  {}", fmt_ms(own), fmt_share(own / total_ms)))
            }
            Some(_) => fact("In this step", "none".into()),
            None => {}
        }
        if let Some(actual) = node.actual {
            if actual.loops == 0 {
                fact("Rows", "never ran".into());
            } else {
                if let Some(rows) = actual.rows {
                    let per = if actual.loops > 1 { " per loop" } else { "" };
                    fact("Rows", format!("{}{per}", fmt_rows(rows)));
                }
                fact("Loops", fmt_count(actual.loops));
            }
        }
        match plan.estimated_rows(id) {
            Some(rows) => fact("Estimated rows", fmt_rows(rows)),
            None if node.estimate.is_some() => fact("Estimated rows", "not estimated".into()),
            None => {}
        }
    });
    let note = |ui: &mut Ui, text: String, color: Color32| {
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(text).size(11.5).color(color)).wrap());
    };
    if let Some(below) = bar.stretched {
        let own = plan.inclusive_ms(id).map_or_else(String::new, |ms| format!("'s {}", fmt_ms(ms)));
        note(
            ui,
            format!(
                "The steps below report {} in all, more than this step{own}. Steps are timed separately, \
                 and MySQL averages over loops, so nested times don't always add up. The bar spans the \
                 steps below.",
                fmt_ms(below)
            ),
            p.warning,
        );
    } else if bar.heat.is_none() {
        note(ui, "The server didn't time this step. Its bar spans the steps below it.".into(), p.text_muted);
    }
}

/// The zoom breadcrumb and the colour legend. Returns a clicked crumb.
pub(super) fn header_strip(ui: &mut Ui, id: Id, rect: Rect, plan: &Plan, flame: &Flame, p: &Palette) -> Option<NodeId> {
    let painter = ui.painter().clone();
    painter.rect_filled(rect, CornerRadius::ZERO, p.header_bg);
    painter.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
    let cy = rect.center().y;
    let legend_left = legend(&painter, pos2(rect.right() - PAD, cy), flame, p);

    let mut path: Vec<NodeId> = ancestry(plan, flame.camera.focus).collect();
    path.reverse();
    let galleys: Vec<_> =
        path.iter().map(|n| painter.layout_job(label_job(plan.node(*n), 12.0, 220.0, p.text))).collect();
    const SEP: f32 = 18.0;
    let room = legend_left - 24.0 - (rect.left() + PAD);
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 12.0).collect();
    // Keep the root and as many of the innermost crumbs as fit, with `…` between.
    let mut shown: Vec<Option<usize>> = (0..path.len()).map(Some).collect();
    if widths.iter().sum::<f32>() + SEP * (path.len() - 1) as f32 > room && path.len() > 2 {
        let ellipsis = 24.0 + SEP;
        let mut used = widths[0] + SEP + ellipsis;
        let mut keep = path.len();
        while keep > 1 && used + widths[keep - 1] + SEP <= room {
            keep -= 1;
            used += widths[keep] + SEP;
        }
        let keep = keep.min(path.len() - 1);
        shown = std::iter::once(Some(0)).chain(std::iter::once(None)).chain((keep..path.len()).map(Some)).collect();
    }

    let mut clicked = None;
    let mut x = rect.left() + PAD - 6.0;
    for (i, crumb) in shown.iter().enumerate() {
        if i > 0 {
            chevron(&painter, pos2(x + SEP / 2.0, cy), p.text_faint);
            x += SEP;
        }
        let Some(k) = *crumb else {
            painter.text(pos2(x + 6.0, cy), Align2::LEFT_CENTER, "…", theme::ui_font(12.0), p.text_muted);
            x += 24.0;
            continue;
        };
        let galley = galleys[k].clone();
        let w = widths[k];
        let r = Rect::from_min_size(pos2(x, cy - 11.0), vec2(w, 22.0));
        let last = k == path.len() - 1;
        let resp = ui.interact(r, id.with(("crumb", k)), Sense::click());
        if resp.hovered() && !last {
            painter.rect_filled(r, CornerRadius::same(5), p.hover);
        }
        let color = if last || resp.hovered() { p.text } else { p.text_muted };
        painter.galley(pos2(r.left() + 6.0, cy - galley.size().y / 2.0), galley, color);
        if resp.clicked() && !last {
            clicked = Some(path[k]);
        }
        x += w;
    }
    let hints = match plan.inclusive_ms(flame.camera.focus) {
        _ if flame.camera.focus == NodeId::ROOT => vec!["Double-click a step to zoom in".to_string()],
        Some(ms) => {
            let share = fmt_share(ms / flame.total_ms);
            vec![format!("{}  ·  {share} of the statement", fmt_ms(ms)), format!("{}  ·  {share}", fmt_ms(ms))]
        }
        None => vec!["not timed".into()],
    };
    for hint in hints {
        let g = painter.layout_no_wrap(hint, theme::ui_font(11.5), p.text_faint);
        if x + 10.0 + g.size().x <= legend_left - 20.0 {
            painter.galley(pos2(x + 10.0, cy - g.size().y / 2.0), g, p.text_faint);
            break;
        }
    }
    clicked
}

fn chevron(painter: &egui::Painter, center: Pos2, color: Color32) {
    let points = vec![center + vec2(-1.5, -3.5), center + vec2(2.0, 0.0), center + vec2(-1.5, 3.5)];
    painter.add(Shape::line(points, Stroke::new(1.3, color)));
}

/// `■ steps below  ■■■ own time  ▭ not timed`, right-aligned at `right_center`; returns its left.
fn legend(painter: &egui::Painter, right_center: Pos2, flame: &Flame, p: &Palette) -> f32 {
    let font = theme::ui_font(11.0);
    let untimed = flame.bars.iter().any(|b| b.heat.is_none());
    let stretched = flame.bars.iter().any(|b| b.stretched.is_some());
    let mut x = right_center.x;
    let y = right_center.y;
    let mut item = |text: &str, swatch_w: f32, paint: &dyn Fn(Rect)| {
        let r = painter.text(pos2(x, y), Align2::RIGHT_CENTER, text, font.clone(), p.text_muted);
        let swatch = Rect::from_min_max(pos2(r.left() - 5.0 - swatch_w, y - 4.0), pos2(r.left() - 5.0, y + 4.0));
        paint(swatch);
        x = swatch.left() - 12.0;
    };
    if stretched {
        item("wider than its time", 15.0, &|r| stretch_mark(painter, r.left(), r.right(), r.center().y, p.warning));
    }
    if untimed {
        let outline = Stroke::new(1.0, p.text_faint);
        item("not timed", 10.0, &|r| {
            painter.rect(r, 2.0, p.plan_bar_track, outline, StrokeKind::Inside);
        });
    }
    item("own time", 24.0, &|r| {
        let w = r.width() / 3.0;
        for (i, share) in [0.0, NOTABLE_SHARE, HOT_SHARE].into_iter().enumerate() {
            let part = Rect::from_min_size(pos2(r.left() + i as f32 * w, r.top()), vec2(w, r.height()));
            let corners = match i {
                0 => CornerRadius { nw: 2, sw: 2, ne: 0, se: 0 },
                2 => CornerRadius { nw: 0, sw: 0, ne: 2, se: 2 },
                _ => CornerRadius::ZERO,
            };
            painter.rect_filled(part, corners, heat_color(p, share));
        }
    });
    item("steps below", 10.0, &|r| {
        painter.rect_filled(r, CornerRadius::same(2), p.plan_bar);
    });
    x
}

/// What the Flame view shows for a plan without step times.
pub(super) fn needs_times(ui: &mut Ui, plan: &Plan) {
    let p = palette(ui);
    let rect = ui.available_rect_before_wrap();
    ui.painter().rect_filled(rect, CornerRadius::ZERO, p.content_bg);
    let (title, body) = if plan.analyzed() {
        (
            "No step times to chart",
            "The server counted rows for these steps but didn't time them, so there is nothing to size the bars by."
                .to_string(),
        )
    } else {
        (
            "The flame chart needs measured times",
            format!(
                "Explain Analyze ({}) runs the statement and times every step. Estimated costs don't add up \
                 through a plan the way times do. A nested loop's inner side is costed once per loop, and steps \
                 above a temporary table cost next to nothing. The Tree view compares each step's estimated cost.",
                crate::app::shortcut_label("⇧E")
            ),
        )
    };
    let width = rect.width().min(440.0) - 40.0;
    let mut top = rect.top() + (rect.height() * 0.32 - 70.0).max(24.0);
    let icon_w = 64.0;
    let icon_left = rect.center().x - icon_w / 2.0;
    let (cool, warm, hot) = (p.plan_bar, p.plan_heat_mid, p.plan_heat_high);
    let rows: [&[(f32, f32, Color32)]; 3] = [
        &[(0.0, 1.0, cool)],
        &[(0.0, 0.62, cool), (0.64, 0.24, cool), (0.88, 0.12, warm)],
        &[(0.0, 0.34, hot), (0.36, 0.2, warm), (0.64, 0.16, cool)],
    ];
    for (row, bars) in rows.iter().enumerate() {
        for &(left, w, color) in bars.iter() {
            let at = pos2(icon_left + left * icon_w, top + row as f32 * 10.0);
            ui.painter().rect_filled(
                Rect::from_min_size(at, vec2(w * icon_w - 2.0, 8.0)),
                2.0,
                color.gamma_multiply(0.8),
            );
        }
    }
    top += 46.0;
    let title_g = ui.painter().layout_no_wrap(title.into(), theme::bold_font(14.0), p.text);
    ui.painter().galley(pos2(rect.center().x - title_g.size().x / 2.0, top), title_g, p.text);
    top += 26.0;
    let mut job = LayoutJob::simple(body, theme::ui_font(12.5), p.text_muted, width);
    job.halign = egui::Align::Center;
    let body_g = ui.painter().layout_job(job);
    ui.painter().galley(pos2(rect.center().x, top), body_g, p.text_muted);
}
