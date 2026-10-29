//! Colors, fonts and egui style for the light and dark themes.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Ui,
    Visuals,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "System",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }
}

/// Semantic colors used by the custom-drawn parts of the UI.
pub struct Palette {
    pub toolbar_bg: Color32,
    pub sidebar_bg: Color32,
    pub content_bg: Color32,
    pub row_alt: Color32,
    pub header_bg: Color32,
    pub grid_line: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_faint: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub selection: Color32,
    pub selection_inactive: Color32,
    pub hover: Color32,
    pub null_text: Color32,
    pub modified_bg: Color32,
    pub inserted_bg: Color32,
    pub deleted_bg: Color32,
    pub deleted_text: Color32,
    pub danger: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub syntax_keyword: Color32,
    pub syntax_string: Color32,
    pub syntax_number: Color32,
    pub syntax_comment: Color32,
    pub syntax_function: Color32,
    pub syntax_ident: Color32,
    pub syntax_param: Color32,
    pub editor_bg: Color32,
    pub gutter_text: Color32,
    pub field_bg: Color32,
    /// Query plan time bar: time including the steps below, and the step's own time.
    pub plan_bar: Color32,
    pub plan_bar_self: Color32,
    pub plan_bar_track: Color32,
    /// Bar for estimate-only plans, which show relative cost instead of time, and its paler
    /// extension over steps below whose cost the step doesn't count.
    pub plan_cost: Color32,
    pub plan_cost_below: Color32,
    /// Where the first row came out, on the time bar.
    pub plan_tick: Color32,
    /// Plan tree indent guides, and the guide of the selected step's subtree.
    pub plan_guide: Color32,
    pub plan_guide_active: Color32,
    /// Flame chart own time: too small a share of the statement to matter, then notable through
    /// to the largest.
    pub plan_heat_low: Color32,
    pub plan_heat_mid: Color32,
    pub plan_heat_high: Color32,
    /// Plan graph: its canvas and dot grid, the step cards and their borders, and the edges.
    pub plan_canvas: Color32,
    pub plan_dots: Color32,
    pub plan_card: Color32,
    pub plan_card_border: Color32,
    pub plan_edge: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub static LIGHT: Palette = Palette {
    toolbar_bg: hex(0xE8E8E8),
    sidebar_bg: hex(0xF0F0EF),
    content_bg: hex(0xFFFFFF),
    row_alt: hex(0xF6F7F8),
    header_bg: hex(0xF2F2F2),
    grid_line: hex(0xE4E4E4),
    border: hex(0xD4D4D4),
    text: hex(0x1D1D1F),
    text_muted: hex(0x6E6E73),
    text_faint: hex(0xA1A1A6),
    accent: hex(0x2F7CF6),
    accent_text: hex(0xFFFFFF),
    selection: hex(0x2F7CF6),
    selection_inactive: hex(0xD6E4FC),
    hover: hex(0xE4E7EC),
    null_text: hex(0xB4B4B8),
    modified_bg: hex(0xFFF1BF),
    inserted_bg: hex(0xD9F4DF),
    deleted_bg: hex(0xFFDCDD),
    deleted_text: hex(0xC0392B),
    danger: hex(0xE5484D),
    success: hex(0x2E9E5B),
    warning: hex(0xD98C00),
    syntax_keyword: hex(0xAD3DA4),
    syntax_string: hex(0xC41A16),
    syntax_number: hex(0x1C00CF),
    syntax_comment: hex(0x707F8C),
    syntax_function: hex(0x326D74),
    syntax_ident: hex(0x1D1D1F),
    syntax_param: hex(0x804FB8),
    editor_bg: hex(0xFFFFFF),
    gutter_text: hex(0xA8A8AD),
    field_bg: hex(0xFFFFFF),
    plan_bar: hex(0xBDD5FB),
    plan_bar_self: hex(0x2F7CF6),
    plan_bar_track: hex(0xECEEF2),
    plan_cost: hex(0xA9B6CC),
    plan_cost_below: hex(0xD3DAE5),
    plan_tick: hex(0x1D1D1F),
    plan_guide: hex(0xE1E3E8),
    plan_guide_active: hex(0x8DB4F6),
    plan_heat_low: hex(0x8FB9F7),
    plan_heat_mid: hex(0xF4C870),
    plan_heat_high: hex(0xF49A95),
    plan_canvas: hex(0xF5F6F8),
    plan_dots: hex(0xDDDFE4),
    plan_card: hex(0xFFFFFF),
    plan_card_border: hex(0xD8DADF),
    plan_edge: hex(0xB9BEC8),
};

pub static DARK: Palette = Palette {
    toolbar_bg: hex(0x2C2C2E),
    sidebar_bg: hex(0x232325),
    content_bg: hex(0x1C1C1E),
    row_alt: hex(0x222224),
    header_bg: hex(0x2A2A2C),
    grid_line: hex(0x343437),
    border: hex(0x3C3C3F),
    text: hex(0xE8E8EA),
    text_muted: hex(0x9C9CA1),
    text_faint: hex(0x67676C),
    accent: hex(0x3B8CFF),
    accent_text: hex(0xFFFFFF),
    selection: hex(0x2E5ECF),
    selection_inactive: hex(0x2B3A57),
    hover: hex(0x323236),
    null_text: hex(0x6A6A70),
    modified_bg: hex(0x4B4220),
    inserted_bg: hex(0x1D3C27),
    deleted_bg: hex(0x4C2427),
    deleted_text: hex(0xFF7B72),
    danger: hex(0xFF5F57),
    success: hex(0x3DD68C),
    warning: hex(0xF5B942),
    syntax_keyword: hex(0xFC5FA3),
    syntax_string: hex(0xFC6A5D),
    syntax_number: hex(0xD0BF69),
    syntax_comment: hex(0x7F8C98),
    syntax_function: hex(0x67B7A4),
    syntax_ident: hex(0xE8E8EA),
    syntax_param: hex(0xB281EB),
    editor_bg: hex(0x1C1C1E),
    gutter_text: hex(0x5E5E63),
    field_bg: hex(0x1F1F21),
    plan_bar: hex(0x2A4877),
    plan_bar_self: hex(0x4C95FF),
    plan_bar_track: hex(0x2B2B2E),
    plan_cost: hex(0x56627A),
    plan_cost_below: hex(0x3F4555),
    plan_tick: hex(0xF2F2F4),
    plan_guide: hex(0x343438),
    plan_guide_active: hex(0x3F69B0),
    plan_heat_low: hex(0x3866AE),
    plan_heat_mid: hex(0x8A6422),
    plan_heat_high: hex(0x9B3B37),
    plan_canvas: hex(0x1C1C1E),
    plan_dots: hex(0x2D2D31),
    plan_card: hex(0x28282B),
    plan_card_border: hex(0x3D3D42),
    plan_edge: hex(0x585960),
};

pub fn palette(ui: &Ui) -> &'static Palette {
    palette_for(ui.visuals().dark_mode)
}

pub fn palette_for(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

pub const SEMIBOLD: &str = "semibold";

pub fn semibold() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}

pub fn ui_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn bold_font(size: f32) -> FontId {
    FontId::new(size, semibold())
}

pub fn mono_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Inter for UI text, JetBrains Mono for code; egui's bundled fonts stay as fallbacks
/// for symbols and emoji.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("inter".into(), Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf"))));
    fonts.font_data.insert(
        "inter-semibold".into(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-SemiBold.ttf"))),
    );
    fonts.font_data.insert(
        "jetbrains-mono".into(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"))),
    );
    let fallbacks: Vec<String> = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mono_fallbacks: Vec<String> = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    let mut prop = vec!["inter".to_string()];
    prop.extend(fallbacks.iter().cloned());
    let mut bold = vec!["inter-semibold".to_string()];
    bold.extend(fallbacks.iter().cloned());
    let mut mono = vec!["jetbrains-mono".to_string()];
    mono.extend(mono_fallbacks);
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(semibold(), bold);
    fonts.families.insert(FontFamily::Monospace, mono);
    ctx.set_fonts(fonts);
}

/// Applies our look to both of egui's themes.
pub fn install_style(ctx: &egui::Context) {
    for (theme, dark) in [(egui::Theme::Light, false), (egui::Theme::Dark, true)] {
        ctx.style_mut_of(theme, |style| apply(style, dark));
    }
}

fn apply(style: &mut egui::Style, dark: bool) {
    let p = palette_for(dark);
    style.text_styles = [
        (TextStyle::Small, ui_font(11.0)),
        (TextStyle::Body, ui_font(13.0)),
        (TextStyle::Button, ui_font(13.0)),
        (TextStyle::Heading, bold_font(19.0)),
        (TextStyle::Monospace, mono_font(12.5)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.interact_size = egui::vec2(24.0, 24.0);
    style.spacing.window_margin = Margin::same(16);
    style.spacing.menu_margin = Margin::same(6);
    style.spacing.combo_height = 320.0;
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.floating = true;
    style.interaction.tooltip_delay = 0.4;
    style.animation_time = 0.12;
    style.url_in_tooltip = false;

    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = p.content_bg;
    v.window_fill = p.content_bg;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(7);
    v.window_shadow =
        Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(if dark { 110 } else { 45 }) };
    v.popup_shadow =
        Shadow { offset: [0, 4], blur: 14, spread: 0, color: Color32::from_black_alpha(if dark { 90 } else { 35 }) };
    v.extreme_bg_color = p.field_bg;
    v.text_edit_bg_color = Some(p.field_bg);
    v.faint_bg_color = p.row_alt;
    v.code_bg_color = p.row_alt;
    v.hyperlink_color = p.accent;
    v.error_fg_color = p.danger;
    v.warn_fg_color = p.warning;
    v.selection.bg_fill = if dark { p.accent.gamma_multiply(0.55) } else { hex(0xB7D3FF) };
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.text_cursor.stroke = Stroke::new(1.5, p.accent);
    v.override_text_color = Some(p.text);
    v.weak_text_color = Some(p.text_muted);
    v.striped = true;
    v.indent_has_left_vline = false;

    let radius = CornerRadius::same(5);
    let w = &mut v.widgets;
    let (btn, btn_hover, btn_active) = if dark {
        (hex(0x3A3A3D), hex(0x45454A), hex(0x505056))
    } else {
        (hex(0xFFFFFF), hex(0xF4F4F5), hex(0xE6E6E8))
    };
    w.noninteractive.bg_fill = p.content_bg;
    w.noninteractive.weak_bg_fill = p.content_bg;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = radius;
    w.inactive.bg_fill = btn;
    w.inactive.weak_bg_fill = btn;
    w.inactive.bg_stroke = Stroke::new(1.0, if dark { hex(0x48484C) } else { hex(0xCFCFD3) });
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = btn_hover;
    w.hovered.weak_bg_fill = btn_hover;
    w.hovered.bg_stroke = Stroke::new(1.0, if dark { hex(0x5A5A60) } else { hex(0xBDBDC2) });
    w.hovered.fg_stroke = Stroke::new(1.0, p.text);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = btn_active;
    w.active.weak_bg_fill = btn_active;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, p.text);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;
    w.open.bg_fill = btn_hover;
    w.open.weak_bg_fill = btn_hover;
    w.open.bg_stroke = Stroke::new(1.0, p.accent);
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = radius;
    style.visuals = v;
}
