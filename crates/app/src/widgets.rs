//! Small reusable widgets with the app's look.

use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Id, Response, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui, Vec2, vec2,
};
use tableplus_core::config::ColorTag;

use crate::icons::{self, Icon};
use crate::theme::{self, palette};

/// Borderless icon button that shows a rounded hover background.
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    icon_button_sized(ui, icon, tooltip, vec2(26.0, 24.0), true)
}

pub fn icon_button_sized(ui: &mut Ui, icon: Icon, tooltip: &str, size: Vec2, enabled: bool) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let p = palette(ui);
    if enabled && (resp.hovered() || resp.is_pointer_button_down_on()) {
        let fill = if resp.is_pointer_button_down_on() { p.border } else { p.hover };
        ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
    }
    let color = if enabled { p.text_muted } else { p.text_faint };
    let icon_rect = egui::Rect::from_center_size(rect.center(), Vec2::splat(size.y.min(size.x) * 0.6));
    icons::paint(ui.painter(), icon_rect, icon, if resp.hovered() && enabled { p.text } else { color });
    if tooltip.is_empty() { resp } else { resp.on_hover_text(tooltip) }
}

/// A framed button with an icon and a label (toolbar style).
pub fn icon_text_button(ui: &mut Ui, icon: Icon, text: &str) -> Response {
    let p = palette(ui);
    let font = theme::ui_font(12.5);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, p.text);
    let size = vec2(galley.size().x + 36.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.style().interact(&resp);
    ui.painter().rect(rect, CornerRadius::same(6), visuals.weak_bg_fill, visuals.bg_stroke, StrokeKind::Inside);
    let icon_rect = egui::Rect::from_center_size(egui::pos2(rect.left() + 15.0, rect.center().y), Vec2::splat(13.0));
    icons::paint(ui.painter(), icon_rect, icon, p.text_muted);
    ui.painter().galley(egui::pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0), galley, p.text);
    resp
}

/// Accent-colored call-to-action button.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    primary_button_enabled(ui, text, true)
}

pub fn primary_button_enabled(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let p = palette(ui);
    let fill = if enabled { p.accent } else { p.accent.gamma_multiply(0.45) };
    let button = egui::Button::new(RichText::new(text).color(p.accent_text).family(theme::semibold()))
        .fill(fill)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(6))
        .min_size(vec2(72.0, 26.0));
    ui.add_enabled(enabled, button)
}

/// A plain framed button with a minimum height matching the toolbar.
pub fn button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(text).min_size(vec2(0.0, 26.0)))
}

/// macOS-style segmented control. Returns true when the selection changed.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let p = palette(ui);
    let font = theme::ui_font(12.0);
    let widths: Vec<f32> = options
        .iter()
        .map(|(_, l)| ui.painter().layout_no_wrap((*l).to_string(), font.clone(), p.text).size().x + 22.0)
        .collect();
    let total = vec2(widths.iter().sum::<f32>() + 4.0, 24.0);
    let (rect, _) = ui.allocate_exact_size(total, Sense::hover());
    let painter = ui.painter();
    let track =
        if ui.visuals().dark_mode { Color32::from_rgb(0x2E, 0x2E, 0x31) } else { Color32::from_rgb(0xE4, 0xE4, 0xE6) };
    painter.rect_filled(rect, CornerRadius::same(7), track);
    let mut x = rect.left() + 2.0;
    let mut changed = false;
    for ((opt, label), w) in options.iter().zip(widths) {
        let seg = egui::Rect::from_min_size(egui::pos2(x, rect.top() + 2.0), vec2(w, rect.height() - 4.0));
        let resp = ui.interact(seg, ui.id().with(("seg", label)), Sense::click());
        let selected = *value == *opt;
        if selected {
            let fill = if ui.visuals().dark_mode { Color32::from_rgb(0x5A, 0x5A, 0x5F) } else { Color32::WHITE };
            ui.painter().rect_filled(seg, CornerRadius::same(5), fill);
            if !ui.visuals().dark_mode {
                ui.painter().rect_stroke(
                    seg,
                    CornerRadius::same(5),
                    Stroke::new(1.0, Color32::from_black_alpha(18)),
                    StrokeKind::Outside,
                );
            }
        } else if resp.hovered() {
            ui.painter().rect_filled(seg, CornerRadius::same(5), p.hover.gamma_multiply(0.6));
        }
        let color = if selected { p.text } else { p.text_muted };
        ui.painter().text(seg.center(), Align2::CENTER_CENTER, *label, font.clone(), color);
        if resp.clicked() && !selected {
            *value = *opt;
            changed = true;
        }
        x += w;
    }
    changed
}

pub fn tag_color(tag: ColorTag) -> Option<Color32> {
    tag.rgb().map(|[r, g, b]| Color32::from_rgb(r, g, b))
}

/// Search box with a magnifier icon and a clear button.
pub fn search_field(ui: &mut Ui, text: &mut String, hint: &str, id: Id, width: f32) -> Response {
    let p = palette(ui);
    let height = 24.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    ui.painter().rect(rect, CornerRadius::same(6), p.field_bg, Stroke::new(1.0, p.border), StrokeKind::Inside);
    let icon_rect = egui::Rect::from_center_size(egui::pos2(rect.left() + 13.0, rect.center().y), Vec2::splat(12.0));
    icons::paint(ui.painter(), icon_rect, Icon::Search, p.text_faint);
    let clear_w = if text.is_empty() { 0.0 } else { 20.0 };
    let edit_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 24.0, rect.top()),
        egui::pos2(rect.right() - 4.0 - clear_w, rect.bottom()),
    );
    let resp = ui.put(
        edit_rect,
        TextEdit::singleline(text)
            .id(id)
            .hint_text(RichText::new(hint).color(p.text_faint))
            .frame(egui::Frame::NONE)
            .vertical_align(egui::Align::Center)
            .desired_width(edit_rect.width()),
    );
    if !text.is_empty() {
        let crect = egui::Rect::from_center_size(egui::pos2(rect.right() - 13.0, rect.center().y), Vec2::splat(16.0));
        let c = ui.interact(crect, id.with("clear"), Sense::click());
        ui.painter().circle_filled(crect.center(), 6.5, if c.hovered() { p.text_muted } else { p.text_faint });
        icons::paint(ui.painter(), crect.shrink(4.5), Icon::Close, p.content_bg);
        if c.clicked() {
            text.clear();
        }
    }
    resp
}

/// Small rounded label (`local`, `production`, `PK`…).
pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let font = theme::bold_font(10.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = galley.size() + vec2(10.0, 4.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(4), color.gamma_multiply(0.16));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    resp
}

pub fn env_color(env: tableplus_core::config::EnvTag, dark: bool) -> Color32 {
    use tableplus_core::config::EnvTag::*;
    let p = theme::palette_for(dark);
    match env {
        Local => p.text_muted,
        Development => Color32::from_rgb(0x2E, 0x9E, 0x5B),
        Testing => Color32::from_rgb(0x8E, 0x4E, 0xC6),
        Staging => Color32::from_rgb(0xD9, 0x8C, 0x00),
        Production => p.danger,
    }
}

pub fn section_title(ui: &mut Ui, text: &str) {
    let p = palette(ui);
    ui.label(RichText::new(text.to_uppercase()).font(theme::bold_font(10.5)).color(p.text_faint));
}

pub fn muted(ui: &Ui, text: impl Into<String>) -> RichText {
    RichText::new(text.into()).color(palette(ui).text_muted)
}

/// `1234567` → `1,234,567`.
/// `1 row`, `2 rows`, `1,000 rows`.
pub fn plural(n: u64, noun: &str) -> String {
    format!("{} {noun}{}", fmt_count(n), if n == 1 { "" } else { "s" })
}

/// Read-only, syntax-highlighted SQL in a scroll area (selectable for copying).
pub fn sql_view(ui: &mut Ui, sql: &str, dialect: tableplus_core::Dialect, max_height: f32) {
    let mut text = sql.to_string();
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
        let job = crate::editor::highlight(ui, buf.as_str(), dialect, 12.5, wrap);
        ui.painter().layout_job(job)
    };
    egui::Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .stroke(Stroke::new(1.0, palette(ui).border))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(max_height).show(ui, |ui| {
                ui.add(
                    TextEdit::multiline(&mut text)
                        .code_editor()
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY)
                        .layouter(&mut layouter),
                );
            });
        });
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn fmt_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{:.2} ms", ms)
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else if ms < 60_000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        format!("{}m {}s", (ms / 60_000.0) as u64, ((ms % 60_000.0) / 1000.0) as u64)
    }
}

/// Plan row counts: `287`, `127,044`, `33.7`, `1.95`, `0.31`, `12.3M`.
pub fn fmt_rows(n: f64) -> String {
    if n >= 10_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 100.0 || n.fract() == 0.0 {
        fmt_count(n.round() as u64)
    } else if n >= 10.0 {
        format!("{n:.1}")
    } else {
        let s = format!("{n:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Plan step times: `4.68 ms`, `61.2 ms`, `578 µs`, `1.24 s`.
pub fn fmt_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 100.0 {
        format!("{ms:.0} ms")
    } else if ms >= 10.0 {
        format!("{ms:.1} ms")
    } else if ms >= 1.0 {
        format!("{ms:.2} ms")
    } else if ms >= 0.0005 {
        format!("{:.0} µs", ms * 1000.0)
    } else if ms > 0.0 {
        "<1 µs".into()
    } else {
        "0".into()
    }
}

/// `67`, `4,539`: how many times off an estimate is.
pub fn fmt_factor(f: f64) -> String {
    if f >= 1000.0 {
        fmt_count(f.round() as u64)
    } else if f >= 10.0 {
        format!("{f:.0}")
    } else {
        format!("{f:.1}").trim_end_matches(".0").to_string()
    }
}

pub fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// A modal dialog with the app's frame (padding, rounded corners, shadow).
pub fn modal(ctx: &egui::Context, id: impl std::hash::Hash + std::fmt::Debug) -> egui::Modal {
    let style = ctx.global_style();
    let p = theme::palette_for(style.visuals.dark_mode);
    egui::Modal::new(Id::new(id)).frame(
        egui::Frame::new()
            .fill(p.content_bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(12))
            .shadow(style.visuals.window_shadow)
            .inner_margin(egui::Margin::same(20)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(2_000_000), "2,000,000");
        assert_eq!(fmt_duration(Duration::from_millis(23)), "23 ms");
        assert_eq!(fmt_duration(Duration::from_millis(1500)), "1.50 s");
        assert_eq!(fmt_rows(287.0), "287");
        assert_eq!(fmt_rows(127_044.0), "127,044");
        assert_eq!(fmt_rows(33.7), "33.7");
        assert_eq!(fmt_rows(1.95), "1.95");
        assert_eq!(fmt_rows(0.31), "0.31");
        assert_eq!(fmt_rows(0.889), "0.89");
        assert_eq!(fmt_rows(0.0), "0");
        assert_eq!(fmt_ms(4.68), "4.68 ms");
        assert_eq!(fmt_ms(0.578), "578 µs");
        assert_eq!(fmt_ms(244e-6), "<1 µs");
        assert_eq!(fmt_ms(1240.0), "1.24 s");
        assert_eq!(fmt_factor(67.2), "67");
        assert_eq!(fmt_factor(1.18), "1.2");
        assert_eq!(fmt_factor(3.0), "3");
        assert_eq!(fmt_factor(4539.0), "4,539");
    }
}
