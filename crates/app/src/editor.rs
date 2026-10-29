//! SQL code editor: syntax highlighting, line numbers and context-aware autocomplete.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Key, Modifiers, Order, Pos2, RichText, Sense, Stroke, StrokeKind,
    TextEdit, Ui, text::CCursor, text::CCursorRange, text::LayoutJob, vec2,
};
use tableplus_core::Dialect;
use tableplus_core::completion::{Catalog, Suggestion, SuggestionKind, complete};
use tableplus_core::sqltext::{TokenKind, tokenize};

use crate::theme::{self, palette};

/// Builds a colored layout job for SQL text.
pub fn highlight(ui: &Ui, text: &str, dialect: Dialect, size: f32, wrap_width: f32) -> LayoutJob {
    let p = palette(ui);
    let font = FontId::monospace(size);
    let tokens = tokenize(text, dialect);
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    for (i, t) in tokens.iter().enumerate() {
        let next_is_paren =
            tokens[i + 1..].iter().find(|t| t.kind != TokenKind::Whitespace).is_some_and(|n| n.text(text) == "(");
        let color = match t.kind {
            TokenKind::Keyword => p.syntax_keyword,
            TokenKind::String => p.syntax_string,
            TokenKind::Number => p.syntax_number,
            TokenKind::Comment => p.syntax_comment,
            TokenKind::Param => p.syntax_param,
            TokenKind::Ident if next_is_paren => p.syntax_function,
            TokenKind::QuotedIdent => p.syntax_function,
            _ => p.syntax_ident,
        };
        let format = egui::TextFormat {
            font_id: font.clone(),
            color,
            italics: t.kind == TokenKind::Comment,
            ..Default::default()
        };
        job.append(t.text(text), 0.0, format);
    }
    if text.is_empty() {
        job.append("", 0.0, egui::TextFormat { font_id: font, color: p.text, ..Default::default() });
    }
    job
}

struct Popup {
    items: Vec<Suggestion>,
    selected: usize,
    replace_start: usize,
    cursor: usize,
    anchor: Pos2,
}

/// Completion that runs again once a table it needs has loaded its columns.
struct Retry {
    cursor: usize,
    tables: Vec<(String, String)>,
}

#[derive(Default)]
pub struct CodeEditor {
    popup: Option<Popup>,
    pending_cursor: Option<usize>,
    last_text_len: usize,
    retry: Option<Retry>,
}

pub struct EditorOutput {
    /// Cursor position in bytes.
    pub cursor: usize,
    /// Selected byte range, if non-empty.
    pub selection: Option<(usize, usize)>,
    pub changed: bool,
    /// Tables whose columns autocomplete wants loaded.
    pub missing_columns: Vec<(String, String)>,
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices().nth(char_index).map_or(text.len(), |(b, _)| b)
}

fn char_index(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].chars().count()
}

impl CodeEditor {
    /// Replaces the whole text and puts the cursor at `cursor` (bytes).
    pub fn set_cursor(&mut self, cursor: usize) {
        self.pending_cursor = Some(cursor);
    }

    pub fn close_popup(&mut self) {
        self.popup = None;
        self.retry = None;
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        id: Id,
        text: &mut String,
        dialect: Dialect,
        catalog: &dyn Catalog,
        font_size: f32,
    ) -> EditorOutput {
        let p = palette(ui);
        let mut missing = Vec::new();
        let mut accept: Option<Suggestion> = None;

        // Keys for the completion popup are consumed before the text edit sees them.
        if let Some(popup) = &mut self.popup {
            let n = popup.items.len();
            // Enter on a word that already matches the suggestion starts a new line instead.
            let typed = text.get(popup.replace_start..popup.cursor.min(text.len())).unwrap_or("");
            let complete_word = tableplus_core::sqltext::is_keyword(typed)
                || popup
                    .items
                    .iter()
                    .any(|s| s.insert.eq_ignore_ascii_case(typed) || s.label.eq_ignore_ascii_case(typed));
            let mut close = false;
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    popup.selected = (popup.selected + 1) % n;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    popup.selected = (popup.selected + n - 1) % n;
                }
                if i.consume_key(Modifiers::NONE, Key::Tab)
                    || (!complete_word && i.consume_key(Modifiers::NONE, Key::Enter))
                {
                    accept = popup.items.get(popup.selected).cloned();
                } else if i.key_pressed(Key::Enter) {
                    close = true;
                }
                if i.consume_key(Modifiers::NONE, Key::Escape) {
                    close = true;
                }
            });
            if close {
                self.close_popup();
            }
        }
        if let Some(s) = accept
            && let Some(popup) = self.popup.take()
        {
            let end = popup.cursor.min(text.len());
            let start = popup.replace_start.min(end);
            text.replace_range(start..end, &s.insert);
            self.pending_cursor = Some(start + s.insert.len());
        }
        let force_complete = ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space));

        let gutter_w = 14.0 + (text.lines().count().max(1).to_string().len() as f32) * font_size * 0.62;
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
            let job = highlight(ui, buf.as_str(), dialect, font_size, f32::INFINITY);
            ui.painter().layout_job(job)
        };

        if let Some(c) = self.pending_cursor.take() {
            let ci = char_index(text, c);
            let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(ci))));
            TextEdit::store_state(ui.ctx(), id, state);
            ui.memory_mut(|m| m.request_focus(id));
        }

        let mut out = EditorOutput { cursor: 0, selection: None, changed: false, missing_columns: Vec::new() };
        egui::ScrollArea::both().id_salt(id.with("scroll")).auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let (gutter, _) =
                    ui.allocate_exact_size(vec2(gutter_w, ui.available_height().max(100.0)), Sense::hover());
                let output = TextEdit::multiline(text)
                    .id(id)
                    .code_editor()
                    .lock_focus(true)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin { left: 8, right: 8, top: 6, bottom: 6 })
                    .desired_width(f32::INFINITY)
                    .desired_rows(12)
                    .min_size(vec2(ui.available_width(), ui.available_height().max(100.0)))
                    .layouter(&mut layouter)
                    .show(ui);
                let painter = ui.painter();
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        gutter.min,
                        egui::pos2(gutter.max.x, gutter.max.y.max(output.response.rect.bottom())),
                    ),
                    CornerRadius::ZERO,
                    p.header_bg,
                );
                let cursor_y = output.cursor_range.map(|r| output.galley.pos_from_cursor(r.primary).center().y);
                let mut line = 1;
                let mut new_line = true;
                for row in output.galley.rows.iter() {
                    if new_line {
                        let y = output.galley_pos.y + row.pos.y + row.rect().height() / 2.0;
                        let r = row.rect();
                        let current = cursor_y.is_some_and(|cy| cy >= r.top() && cy <= r.bottom());
                        let color = if current { p.text_muted } else { p.gutter_text };
                        painter.text(
                            egui::pos2(gutter.right() - 6.0, y),
                            Align2::RIGHT_CENTER,
                            line.to_string(),
                            FontId::monospace(font_size * 0.85),
                            color,
                        );
                        line += 1;
                    }
                    new_line = row.ends_with_newline;
                }

                let changed = output.response.changed();
                out.changed = changed;
                if let Some(range) = output.cursor_range {
                    let primary = byte_index(text, range.primary.index.0);
                    let secondary = byte_index(text, range.secondary.index.0);
                    out.cursor = primary;
                    if primary != secondary {
                        out.selection = Some((primary.min(secondary), primary.max(secondary)));
                    }
                    let typed_ident = changed
                        && text.len() > self.last_text_len
                        && text[..primary].chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
                    let retry = match self.retry.take() {
                        Some(r) if !changed && r.cursor == primary => {
                            let loaded = r.tables.iter().any(|(s, t)| catalog.columns(s, t).is_some());
                            if !loaded {
                                self.retry = Some(r);
                            }
                            loaded
                        }
                        _ => false,
                    };
                    if (typed_ident || force_complete || retry || (self.popup.is_some() && changed))
                        && output.response.has_focus()
                    {
                        let res = complete(text, primary, dialect, catalog);
                        missing = res.missing_columns;
                        if !missing.is_empty() {
                            self.retry = Some(Retry { cursor: primary, tables: missing.clone() });
                        }
                        // A lone suggestion identical to the typed word adds nothing.
                        let only_exact = matches!(res.suggestions.as_slice(), [s] if s.label.eq_ignore_ascii_case(&res.context.prefix));
                        let show = !res.suggestions.is_empty()
                            && (force_complete || !only_exact)
                            && (force_complete
                                || res.context.qualifier.is_some()
                                || (self.popup.is_some() && !res.context.prefix.is_empty())
                                || res.context.prefix.chars().count() >= 2);
                        if show {
                            let cpos = output.galley.pos_from_cursor(range.primary);
                            let anchor = output.galley_pos + cpos.left_bottom().to_vec2() + vec2(0.0, 2.0);
                            let selected = self.popup.as_ref().map_or(0, |p| p.selected).min(res.suggestions.len() - 1);
                            self.popup = Some(Popup {
                                items: res.suggestions,
                                selected,
                                replace_start: res.context.replace_start,
                                cursor: primary,
                                anchor,
                            });
                        } else {
                            self.popup = None;
                        }
                    } else if (changed || !output.response.has_focus())
                        && self.popup.as_ref().is_some_and(|pp| pp.cursor != primary)
                    {
                        self.popup = None;
                    }
                }
                self.last_text_len = text.len();
            });
        });
        out.missing_columns = missing;

        // Completion popup.
        let mut clicked = None;
        if let Some(popup) = &self.popup {
            egui::Area::new(id.with("completion"))
                .order(Order::Foreground)
                .fade_in(false)
                .fixed_pos(popup.anchor)
                .show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(p.content_bg)
                        .stroke(Stroke::new(1.0, p.border))
                        .corner_radius(CornerRadius::same(7))
                        .shadow(ui.visuals().popup_shadow)
                        .inner_margin(egui::Margin::same(4))
                        .show(ui, |ui| {
                            ui.set_width(340.0);
                            let visible = 10;
                            let start = popup.selected.saturating_sub(visible - 1);
                            for (i, s) in popup.items.iter().enumerate().skip(start).take(visible) {
                                let (rect, resp) = ui.allocate_exact_size(vec2(332.0, 22.0), Sense::click());
                                let selected = i == popup.selected;
                                if selected {
                                    ui.painter().rect_filled(rect, CornerRadius::same(4), p.selection);
                                } else if resp.hovered() {
                                    ui.painter().rect_filled(rect, CornerRadius::same(4), p.hover);
                                }
                                let (letter, color) = match s.kind {
                                    SuggestionKind::Column => ("c", Color32::from_rgb(0xD9, 0x8C, 0x00)),
                                    SuggestionKind::Table => ("t", Color32::from_rgb(0x3D, 0x7E, 0xD8)),
                                    SuggestionKind::View => ("v", Color32::from_rgb(0x8E, 0x4E, 0xC6)),
                                    SuggestionKind::Function => ("f", Color32::from_rgb(0x2E, 0x8B, 0x57)),
                                    SuggestionKind::Schema => ("s", Color32::from_rgb(0x6E, 0x6E, 0x73)),
                                    SuggestionKind::Keyword => ("k", Color32::from_rgb(0xAD, 0x3D, 0xA4)),
                                };
                                let badge = egui::Rect::from_center_size(
                                    rect.left_center() + vec2(12.0, 0.0),
                                    vec2(16.0, 16.0),
                                );
                                ui.painter().rect_filled(
                                    badge,
                                    CornerRadius::same(4),
                                    color.gamma_multiply(if selected { 0.9 } else { 0.18 }),
                                );
                                ui.painter().text(
                                    badge.center(),
                                    Align2::CENTER_CENTER,
                                    letter,
                                    theme::bold_font(10.5),
                                    if selected { Color32::WHITE } else { color },
                                );
                                let tc = if selected { p.accent_text } else { p.text };
                                ui.painter().text(
                                    rect.left_center() + vec2(26.0, 0.0),
                                    Align2::LEFT_CENTER,
                                    &s.label,
                                    FontId::monospace(12.5),
                                    tc,
                                );
                                let dc = if selected { p.accent_text.gamma_multiply(0.8) } else { p.text_faint };
                                ui.painter().with_clip_rect(rect).text(
                                    rect.right_center() - vec2(6.0, 0.0),
                                    Align2::RIGHT_CENTER,
                                    &s.detail,
                                    theme::ui_font(11.0),
                                    dc,
                                );
                                if resp.clicked() {
                                    clicked = Some(s.clone());
                                }
                            }
                            ui.painter().rect_stroke(
                                ui.min_rect(),
                                CornerRadius::same(4),
                                Stroke::NONE,
                                StrokeKind::Inside,
                            );
                            ui.label(RichText::new("↑↓ select · ↵ insert · esc close").size(10.5).color(p.text_faint));
                        });
                });
        }
        if let Some(s) = clicked
            && let Some(popup) = self.popup.take()
        {
            let end = popup.cursor.min(text.len());
            let start = popup.replace_start.min(end);
            text.replace_range(start..end, &s.insert);
            self.pending_cursor = Some(start + s.insert.len());
            out.changed = true;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tableplus_core::model::{ColumnInfo, ObjectKind, SchemaObject};

    use super::*;

    /// A catalog whose table columns load one table at a time.
    struct Cat {
        objects: Vec<SchemaObject>,
        columns: HashMap<String, Vec<ColumnInfo>>,
    }

    impl Cat {
        fn new(tables: &[&str]) -> Self {
            let objects = tables.iter().map(|t| SchemaObject::new("public", *t, ObjectKind::Table)).collect();
            Self { objects, columns: HashMap::new() }
        }

        fn load(&mut self, table: &str, column: &str) {
            let col = ColumnInfo { name: column.into(), data_type: "text".into(), ..Default::default() };
            self.columns.insert(table.into(), vec![col]);
        }
    }

    impl Catalog for Cat {
        fn objects(&self) -> &[SchemaObject] {
            &self.objects
        }
        fn columns(&self, _schema: &str, table: &str) -> Option<&[ColumnInfo]> {
            self.columns.get(table).map(Vec::as_slice)
        }
        fn default_schema(&self) -> &str {
            "public"
        }
    }

    /// Runs the editor headless, one egui frame per call, with the cursor at the end of `sql`.
    struct Harness {
        ctx: egui::Context,
        editor: CodeEditor,
        sql: String,
    }

    impl Harness {
        fn new(sql: &str, cat: &Cat) -> Self {
            let mut editor = CodeEditor::default();
            editor.set_cursor(sql.len());
            let ctx = egui::Context::default();
            theme::install_fonts(&ctx);
            let mut h = Self { ctx, editor, sql: sql.into() };
            h.frame(cat);
            h
        }

        fn frame_with(&mut self, cat: &Cat, events: Vec<egui::Event>) -> EditorOutput {
            let Self { ctx, editor, sql } = self;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
                events,
                ..Default::default()
            };
            let mut out = None;
            ctx.run_ui(input, |ui| {
                out = Some(editor.show(ui, Id::new("sql"), sql, Dialect::Postgres, cat, 13.0));
            })
            .drop_without_applying_deltas();
            out.expect("editor ran")
        }

        fn frame(&mut self, cat: &Cat) -> EditorOutput {
            self.frame_with(cat, Vec::new())
        }

        fn ctrl_space(&mut self, cat: &Cat) -> EditorOutput {
            let key = egui::Event::Key {
                key: Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL,
            };
            self.frame_with(cat, vec![key])
        }

        /// Column suggestions in the open popup, sorted by name.
        fn popup_columns(&self) -> Vec<String> {
            let items = self.editor.popup.as_ref().map_or(&[][..], |p| p.items.as_slice());
            let mut cols: Vec<String> =
                items.iter().filter(|s| s.kind == SuggestionKind::Column).map(|s| s.label.clone()).collect();
            cols.sort();
            cols
        }
    }

    #[test]
    fn columns_from_each_joined_table_appear_as_they_load() {
        let mut cat = Cat::new(&["alpha", "beta"]);
        let mut h = Harness::new("select * from alpha a join beta b on true where name", &cat);
        h.ctrl_space(&cat);

        cat.load("alpha", "name_alpha");
        h.frame(&cat);
        assert_eq!(h.popup_columns(), ["name_alpha"]);

        cat.load("beta", "name_beta");
        h.frame(&cat);
        assert_eq!(h.popup_columns(), ["name_alpha", "name_beta"]);
    }

    #[test]
    fn closed_popup_stays_closed_after_columns_load() {
        let mut cat = Cat::new(&["orders"]);
        let mut h = Harness::new("select * from orders o where o.", &cat);
        h.ctrl_space(&cat);
        cat.load("orders", "id");
        h.frame(&cat);
        assert_eq!(h.popup_columns(), ["id"]);

        h.editor.close_popup();
        for _ in 0..3 {
            h.frame(&cat);
        }
        assert!(h.editor.popup.is_none());
    }

    #[test]
    fn table_that_never_loads_is_requested_once() {
        let cat = Cat::new(&["orders"]);
        let mut h = Harness::new("select * from orders o where o.", &cat);
        assert_eq!(h.ctrl_space(&cat).missing_columns, [("public".to_string(), "orders".to_string())]);
        for _ in 0..3 {
            assert!(h.frame(&cat).missing_columns.is_empty());
        }
    }
}
