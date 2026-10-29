//! SQL editor tab: run or explain the statement under the cursor / the selection / everything,
//! with one result tab per statement (a grid, or a plan for EXPLAIN output) and a messages log.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use chrono::Utc;
use eframe::egui::{self, Align, CornerRadius, Layout, RichText, Sense, Stroke, Ui, vec2};
use tableplus_core::driver;
use tableplus_core::model::{QueryOutput, StatementResult};
use tableplus_core::plan::{self, ExplainMode, ExplainStatements};
use tableplus_core::sqltext::{split_statements, statement_at};
use tableplus_core::store::HistoryEntry;
use tableplus_core::{Dialect, ServerInfo, Value};

use crate::app::{Action, shortcut_label};
use crate::editor::CodeEditor;
use crate::export_dialog::ExportSource;
use crate::grid::{self, GridEvent, GridState, ResultData};
use crate::icons::Icon;
use crate::plan_view::{self, PlanContent, PlanView};
use crate::task::{Task, poll_slot};
use crate::theme::{self, palette};
use crate::widgets::{self, fmt_count, fmt_duration};
use crate::workspace::TabCtx;

/// One statement to run, as the user wrote it.
struct Job {
    sql: String,
    step: Result<Step, String>,
    /// Shown with the result, such as why Explain Analyze fell back to estimates.
    note: Option<String>,
}

enum Step {
    Run,
    Explain(ExplainStatements),
}

struct Outcome {
    sql: String,
    result: Result<QueryOutput, String>,
    elapsed: Duration,
    /// Set when the result is a query plan.
    plan: Option<PlanContent>,
    note: Option<String>,
    /// Why the session could not be put back after an EXPLAIN.
    reset_error: Option<String>,
}

enum PaneBody {
    Rows { grid: Box<GridState>, sort: Option<(usize, bool)> },
    Plan(Box<PlanView>),
}

struct ResultPane {
    output: QueryOutput,
    body: PaneBody,
}

/// Which result strip tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Pane(usize),
    Messages,
}

pub struct QueryTab {
    pub title: String,
    pub sql: String,
    editor: CodeEditor,
    cursor: usize,
    selection: Option<(usize, usize)>,
    panes: Vec<ResultPane>,
    messages: Vec<(bool, String)>,
    shown: Shown,
    running: Option<Task<Vec<Outcome>>>,
    started: Option<Instant>,
    total_elapsed: Option<Duration>,
}

impl QueryTab {
    pub fn new(title: String, sql: String) -> Self {
        let mut editor = CodeEditor::default();
        editor.set_cursor(sql.len());
        Self {
            title,
            sql,
            editor,
            cursor: 0,
            selection: None,
            panes: Vec::new(),
            messages: Vec::new(),
            shown: Shown::Messages,
            running: None,
            started: None,
            total_elapsed: None,
        }
    }

    /// Each statement without leading whitespace, no-break spaces pasted from a web page included.
    /// The end keeps them: there one belongs to the last name.
    fn statements_to_run(&self, dialect: Dialect, all: bool) -> Vec<String> {
        let (src, spans) = if let Some((a, b)) = self.selection.filter(|_| !all) {
            let sel = &self.sql[a.min(self.sql.len())..b.min(self.sql.len())];
            (sel, split_statements(sel, dialect))
        } else if all {
            (self.sql.as_str(), split_statements(&self.sql, dialect))
        } else {
            (self.sql.as_str(), statement_at(&self.sql, self.cursor, dialect).into_iter().collect())
        };
        spans.iter().map(|s| s.text(src).trim_start()).filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    fn run(&mut self, ctx: &mut TabCtx, all: bool) {
        let jobs = self
            .statements_to_run(ctx.dialect, all)
            .into_iter()
            .map(|sql| Job { sql, step: Ok(Step::Run), note: None })
            .collect();
        self.start(ctx, jobs);
    }

    /// Explains the statement under the cursor, or each selected statement.
    fn explain(&mut self, ctx: &mut TabCtx, mode: ExplainMode) {
        let server = ctx.conn.server_info();
        let jobs =
            self.statements_to_run(ctx.dialect, false).into_iter().map(|sql| explain_job(&server, sql, mode)).collect();
        self.start(ctx, jobs);
    }

    fn start(&mut self, ctx: &mut TabCtx, jobs: Vec<Job>) {
        self.editor.close_popup();
        if self.running.is_some() || jobs.is_empty() {
            return;
        }
        let conn = ctx.conn.clone();
        let server = conn.server_info();
        let limit = ctx.settings.row_limit();
        self.started = Some(Instant::now());
        self.running = Some(ctx.spawner.spawn(async move {
            let mut out = Vec::new();
            for job in jobs {
                let start = Instant::now();
                let mut reset_error = None;
                let (result, plan) = match job.step {
                    Err(reason) => (Err(reason), None),
                    Ok(Step::Run) => {
                        let result = driver::run(conn.as_ref(), &job.sql, limit).await.map_err(|e| format!("{e:#}"));
                        let plan = match &result {
                            Ok(out) => out.rows().and_then(|rs| PlanContent::detected(&server, &job.sql, rs)),
                            Err(_) => None,
                        };
                        (result, plan)
                    }
                    Ok(Step::Explain(req)) => match conn.explain(&req).await {
                        Ok(explained) => {
                            reset_error = explained.reset_error;
                            let plan =
                                explained.output.rows().map(|rs| PlanContent::explained(&server, rs, req.format));
                            (Ok(explained.output), plan)
                        }
                        Err(e) => (Err(format!("{e:#}")), None),
                    },
                };
                let failed = result.is_err();
                out.push(Outcome { sql: job.sql, result, elapsed: start.elapsed(), plan, note: job.note, reset_error });
                if failed {
                    break;
                }
            }
            Ok(out)
        }));
    }

    pub fn poll(&mut self, ctx: &mut TabCtx) {
        let Some(result) = poll_slot(&mut self.running) else { return };
        self.total_elapsed = self.started.take().map(|s| s.elapsed());
        let outcomes = match result {
            Ok(o) => o,
            Err(e) => vec![Outcome {
                sql: String::new(),
                result: Err(format!("{e:#}")),
                elapsed: Duration::ZERO,
                plan: None,
                note: None,
                reset_error: None,
            }],
        };
        self.panes.clear();
        self.messages.clear();
        let mut ddl = false;
        for o in outcomes {
            let first_word = o.sql.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
            ddl |= matches!(first_word.as_str(), "CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE");
            let (rows, error) = match &o.result {
                Ok(out) => match &out.result {
                    StatementResult::Rows(rs) => (Some(rs.rows.len() as u64), None),
                    StatementResult::Affected(n) => (Some(*n), None),
                },
                Err(e) => (None, Some(e.clone())),
            };
            if !o.sql.is_empty() {
                ctx.actions.push(Action::RecordHistory(HistoryEntry {
                    connection_id: ctx.connection.id,
                    connection_name: ctx.connection.display_name(),
                    sql: o.sql.clone(),
                    executed_at: Utc::now(),
                    duration_ms: o.elapsed.as_millis() as u64,
                    rows,
                    error: error.clone(),
                }));
            }
            let short: String = o.sql.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
            if let Some(note) = &o.note {
                self.messages.push((false, format!("{note}\n→ {short}")));
            }
            if let Some(reset) = &o.reset_error {
                self.messages.push((true, format!("{reset}\n→ {short}")));
            }
            match o.result {
                Ok(out) => {
                    let msg = match (&out.result, &o.plan) {
                        (_, Some(PlanContent::Tree(plan))) => {
                            let kind = if plan.analyzed() { "Analyzed plan" } else { "Estimated plan" };
                            format!(
                                "{kind}, {} · {} · {short}",
                                widgets::plural(plan.node_count() as u64, "step"),
                                fmt_duration(out.elapsed)
                            )
                        }
                        (_, Some(PlanContent::Raw { .. })) => format!("Plan · {} · {short}", fmt_duration(out.elapsed)),
                        (StatementResult::Rows(rs), None) => {
                            let more = if rs.truncated { " (truncated by row limit)" } else { "" };
                            format!(
                                "{} returned{more} · {} · {short}",
                                widgets::plural(rs.rows.len() as u64, "row"),
                                fmt_duration(out.elapsed)
                            )
                        }
                        (StatementResult::Affected(n), None) => {
                            format!("{} affected · {} · {short}", widgets::plural(*n, "row"), fmt_duration(out.elapsed))
                        }
                    };
                    self.messages.push((false, msg));
                    let body = match o.plan {
                        Some(content) => Some(PaneBody::Plan(Box::new(PlanView::new(content, o.note)))),
                        None => out.rows().map(|_| PaneBody::Rows { grid: Box::default(), sort: None }),
                    };
                    if let Some(body) = body {
                        self.panes.push(ResultPane { output: out, body });
                    }
                }
                Err(e) => self.messages.push((true, format!("{e}\n→ {short}"))),
            }
        }
        self.shown = if self.panes.is_empty() || self.messages.iter().any(|(err, _)| *err) {
            Shown::Messages
        } else {
            Shown::Pane(0)
        };
        if ddl {
            ctx.actions.push(Action::RefreshObjects);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, ctx: &mut TabCtx) {
        let sc = *ctx.shortcuts;
        if sc.run {
            self.run(ctx, false);
        }
        if sc.run_all {
            self.run(ctx, true);
        }
        if sc.explain {
            self.explain(ctx, ExplainMode::Estimate);
        }
        if sc.explain_analyze {
            self.explain(ctx, ExplainMode::Analyze);
        }
        let p = palette(ui);

        egui::Panel::top(egui::Id::new(("query_toolbar", &self.title))).exact_size(38.0).frame(egui::Frame::NONE).show(
            ui,
            |ui| {
                let rect = ui.max_rect();
                ui.painter().rect_filled(rect, CornerRadius::ZERO, p.header_bg);
                ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
                ui.horizontal_centered(|ui| {
                    ui.add_space(8.0);
                    let running = self.running.is_some();
                    let label = if self.selection.is_some() { "Run selection" } else { "Run current" };
                    if ui
                        .add_enabled_ui(!running, |ui| widgets::icon_text_button(ui, Icon::Play, label))
                        .inner
                        .on_hover_text(shortcut_label("↵"))
                        .clicked()
                    {
                        self.run(ctx, false);
                    }
                    if ui
                        .add_enabled_ui(!running, |ui| widgets::icon_text_button(ui, Icon::PlayAll, "Run all"))
                        .inner
                        .on_hover_text(shortcut_label("⇧↵"))
                        .clicked()
                    {
                        self.run(ctx, true);
                    }
                    if ui
                        .add_enabled_ui(!running, |ui| widgets::icon_text_button(ui, Icon::Plan, "Explain"))
                        .inner
                        .on_hover_text(format!(
                            "Show the planner's estimated plan, without running ({})",
                            shortcut_label("E")
                        ))
                        .clicked()
                    {
                        self.explain(ctx, ExplainMode::Estimate);
                    }
                    if ui
                        .add_enabled_ui(!running, |ui| {
                            widgets::icon_text_button(ui, Icon::Stopwatch, "Explain Analyze")
                        })
                        .inner
                        .on_hover_text(format!(
                            "Run the statement and measure every step of its plan ({})",
                            shortcut_label("⇧E")
                        ))
                        .clicked()
                    {
                        self.explain(ctx, ExplainMode::Analyze);
                    }
                    if running {
                        if widgets::icon_text_button(ui, Icon::Stop, "Stop").clicked() {
                            let conn = ctx.conn.clone();
                            ctx.spawner.detach(async move {
                                let _ = conn.cancel().await;
                            });
                        }
                        ui.add(egui::Spinner::new().size(14.0));
                        if let Some(s) = self.started {
                            ui.label(widgets::muted(ui, fmt_duration(s.elapsed())));
                            ui.ctx().request_repaint_after(Duration::from_millis(100));
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(10.0);
                        let limit = match ctx.settings.row_limit() {
                            Some(n) => format!("Limit {} rows", fmt_count(n as u64)),
                            None => "No row limit".into(),
                        };
                        ui.label(RichText::new(limit).size(11.5).color(p.text_faint));
                    });
                });
            },
        );

        let editor_h = (ui.available_height() * 0.42).clamp(140.0, 420.0);
        egui::Panel::top(egui::Id::new(("query_editor", &self.title)))
            .resizable(true)
            .default_size(editor_h)
            .size_range(80.0..=ui.available_height() - 80.0)
            .frame(egui::Frame::new().fill(p.editor_bg))
            .show(ui, |ui| {
                let id = egui::Id::new(("sql_editor", &self.title));
                let out =
                    self.editor.show(ui, id, &mut self.sql, ctx.dialect, ctx.catalog, ctx.settings.editor_font_size);
                self.cursor = out.cursor;
                self.selection = out.selection;
                for (schema, table) in out.missing_columns {
                    ctx.actions.push(Action::LoadStructure { schema, table });
                }
            });

        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            self.results(ui, ctx);
        });
    }

    /// Chip labels: `Result`/`Plan` alone, numbered when a kind repeats (`Result 2`, `Plan 2`).
    fn chip_labels(&self) -> Vec<String> {
        let is_plan = |p: &ResultPane| matches!(p.body, PaneBody::Plan(_));
        let plans = self.panes.iter().filter(|p| is_plan(p)).count();
        let grids = self.panes.len() - plans;
        let (mut plan_n, mut grid_n) = (0, 0);
        self.panes
            .iter()
            .map(|p| {
                let (name, n, count) = if is_plan(p) {
                    plan_n += 1;
                    ("Plan", plan_n, plans)
                } else {
                    grid_n += 1;
                    ("Result", grid_n, grids)
                };
                if count == 1 { name.to_string() } else { format!("{name} {n}") }
            })
            .collect()
    }

    fn results(&mut self, ui: &mut Ui, ctx: &mut TabCtx) {
        let p = palette(ui);
        let shown = match self.shown {
            Shown::Pane(i) if i < self.panes.len() => Shown::Pane(i),
            _ => Shown::Messages,
        };
        // Result tab strip.
        egui::Panel::top(egui::Id::new(("result_tabs", &self.title))).exact_size(30.0).frame(egui::Frame::NONE).show(
            ui,
            |ui| {
                let rect = ui.max_rect();
                ui.painter().rect_filled(rect, CornerRadius::ZERO, p.header_bg);
                ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
                ui.horizontal_centered(|ui| {
                    ui.add_space(6.0);
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (i, label) in self.chip_labels().iter().enumerate() {
                        if result_tab(ui, label, shown == Shown::Pane(i)) {
                            self.shown = Shown::Pane(i);
                        }
                    }
                    let errors = self.messages.iter().filter(|(e, _)| *e).count();
                    let label = if errors > 0 { format!("Messages ({errors} error)") } else { "Messages".to_string() };
                    if result_tab(ui, &label, shown == Shown::Messages) {
                        self.shown = Shown::Messages;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(8.0);
                        let Shown::Pane(i) = shown else { return };
                        let pane = &self.panes[i];
                        match (&pane.body, pane.output.rows()) {
                            (PaneBody::Plan(view), _) => {
                                ui.label(RichText::new(view.summary()).size(12.0).color(p.text_muted))
                                    .on_hover_text(&pane.output.sql);
                            }
                            (PaneBody::Rows { .. }, Some(rs)) => {
                                if widgets::icon_button(ui, Icon::Export, "Export result…").clicked() {
                                    ctx.actions.push(Action::Export(ExportSource::Rows {
                                        name: "query_result".into(),
                                        table: ctx.dialect.quote_ident("query_result"),
                                        columns: rs.columns.clone(),
                                        rows: rs.rows.clone(),
                                    }));
                                }
                                let trunc = if rs.truncated { " (limited)" } else { "" };
                                ui.label(
                                    RichText::new(format!(
                                        "{}{trunc} · {}",
                                        widgets::plural(rs.rows.len() as u64, "row"),
                                        fmt_duration(pane.output.elapsed)
                                    ))
                                    .size(12.0)
                                    .color(p.text_muted),
                                )
                                .on_hover_text(&pane.output.sql);
                            }
                            (PaneBody::Rows { .. }, None) => {}
                        }
                    });
                });
            },
        );

        let Shown::Pane(active) = shown else {
            self.messages_view(ui);
            return;
        };
        let pane = &mut self.panes[active];
        let (grid_state, sort) = match &mut pane.body {
            PaneBody::Plan(view) => {
                plan_view::show(ui, egui::Id::new(("result_plan", &self.title, active)), view);
                return;
            }
            PaneBody::Rows { grid, sort } => (grid, sort),
        };
        let Some(rs) = (match &mut pane.output.result {
            StatementResult::Rows(rs) => Some(rs),
            StatementResult::Affected(_) => None,
        }) else {
            return;
        };
        let id = egui::Id::new(("result_grid", &self.title, active));
        let events = grid::show(ui, id, grid_state, &SortedData { inner: ResultData { rs }, sort: *sort });
        for ev in events {
            match ev {
                GridEvent::Sort { col, .. } => {
                    let desc = match *sort {
                        Some((c, false)) if c == col => true,
                        Some((c, true)) if c == col => {
                            *sort = None;
                            continue;
                        }
                        _ => false,
                    };
                    *sort = Some((col, desc));
                    rs.rows.sort_by(|a, b| {
                        let o = cmp_values(&a[col], &b[col]);
                        if desc { o.reverse() } else { o }
                    });
                }
                GridEvent::CopyAs { rows, format } => {
                    let obj = tableplus_core::model::SchemaObject::new(
                        "",
                        "query_result",
                        tableplus_core::model::ObjectKind::Table,
                    );
                    ctx.egui.copy_text(crate::table_tab::copy_rows(rs, &rows, format, ctx.dialect, &obj));
                }
                GridEvent::FilterByValue { .. } => {}
                _ => {}
            }
        }
    }

    fn messages_view(&self, ui: &mut Ui) {
        let p = palette(ui);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(8.0);
            if self.messages.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(widgets::muted(
                        ui,
                        format!(
                            "Write SQL above and press {} to run the statement under the cursor, or {} to run everything. {} explains it, and {} explains it with measured times.",
                            shortcut_label("↵"),
                            shortcut_label("⇧↵"),
                            shortcut_label("E"),
                            shortcut_label("⇧E")
                        ),
                    ));
                });
            }
            for (err, msg) in &self.messages {
                ui.horizontal_top(|ui| {
                    ui.add_space(14.0);
                    let (r, _) = ui.allocate_exact_size(vec2(8.0, 18.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, if *err { p.danger } else { p.success });
                    ui.add(egui::Label::new(RichText::new(msg).font(theme::mono_font(12.0)).color(if *err { p.danger } else { p.text })).wrap());
                });
                ui.add_space(4.0);
            }
            if let Some(t) = self.total_elapsed {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(RichText::new(format!("Total time {}", fmt_duration(t))).size(11.5).color(p.text_faint));
                });
            }
        });
    }
}

/// Explain Analyze falls back to the estimated plan, with a note saying why, where the server
/// can't measure or where running the statement would change data.
fn explain_job(server: &ServerInfo, sql: String, mode: ExplainMode) -> Job {
    let (step, note) = match plan::request(server, &sql, mode) {
        Ok(req) => (Ok(Step::Explain(req)), None),
        Err(why) if mode == ExplainMode::Analyze => match plan::request(server, &sql, ExplainMode::Estimate) {
            Ok(req) => (Ok(Step::Explain(req)), Some(format!("{why} Showing the estimated plan instead."))),
            Err(e) => (Err(e.0), None),
        },
        Err(why) => (Err(why.0), None),
    };
    Job { sql, step, note }
}

fn result_tab(ui: &mut Ui, label: &str, selected: bool) -> bool {
    let p = palette(ui);
    let font = if selected { theme::bold_font(12.0) } else { theme::ui_font(12.0) };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, if selected { p.text } else { p.text_muted });
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 22.0, 24.0), Sense::click());
    if selected {
        ui.painter().rect(
            rect,
            CornerRadius::same(6),
            p.content_bg,
            Stroke::new(1.0, p.border),
            egui::StrokeKind::Inside,
        );
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
    }
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text);
    resp.clicked()
}

/// Orders values for client-side sorting of query results.
pub fn cmp_values(a: &Value, b: &Value) -> Ordering {
    fn num(v: &Value) -> Option<f64> {
        match v {
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            Value::Float(f) => Some(*f),
            Value::Decimal(d) => d.parse().ok(),
            _ => None,
        }
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => match (a, b) {
                (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
                (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
                _ => a.display().cmp(&b.display()),
            },
        },
    }
}

/// Wraps a result to show the client-side sort indicator.
struct SortedData<'a> {
    inner: ResultData<'a>,
    sort: Option<(usize, bool)>,
}

impl grid::GridData for SortedData<'_> {
    fn columns(&self) -> &[tableplus_core::model::ColumnMeta] {
        self.inner.columns()
    }
    fn row_count(&self) -> usize {
        self.inner.row_count()
    }
    fn cell(&self, row: usize, col: usize) -> grid::CellView<'_> {
        self.inner.cell(row, col)
    }
    fn row_state(&self, row: usize) -> grid::CellState {
        self.inner.row_state(row)
    }
    fn full_text(&self, row: usize, col: usize) -> Option<String> {
        self.inner.full_text(row, col)
    }
    fn sort_of(&self, col: usize) -> Option<bool> {
        self.sort.filter(|(c, _)| *c == col).map(|(_, d)| d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_ordering() {
        let mut v = [
            Value::text("b"),
            Value::Null,
            Value::Int(10),
            Value::Float(2.5),
            Value::Decimal("3".into()),
            Value::text("a"),
        ];
        v.sort_by(cmp_values);
        assert_eq!(v[0], Value::Null);
        assert_eq!(v[1], Value::Float(2.5));
        assert_eq!(v[2], Value::Decimal("3".into()));
        assert_eq!(v[3], Value::Int(10));
    }

    fn explained(job: &Job) -> Option<&str> {
        match &job.step {
            Ok(Step::Explain(req)) => Some(req.explain.sql()),
            _ => None,
        }
    }

    #[test]
    fn explain_analyze_never_runs_a_write() {
        use tableplus_core::plan::Explain;
        use tableplus_core::{Flavor, Version};
        let mysql = ServerInfo::new(Flavor::MySql, Version::new(8, 4, 11));
        let job = explain_job(&mysql, "UPDATE products SET price = 0".into(), ExplainMode::Analyze);
        assert_eq!(explained(&job), Some("EXPLAIN FORMAT=JSON UPDATE products SET price = 0"));
        assert!(job.note.as_deref().is_some_and(|n| n.contains("UPDATE") && n.contains("estimated plan")));
        assert_eq!(job.sql, "UPDATE products SET price = 0", "history keeps the user's statement");

        let job = explain_job(&mysql, "select 1".into(), ExplainMode::Analyze);
        assert_eq!((explained(&job), job.note.as_deref()), (Some("EXPLAIN ANALYZE FORMAT=JSON select 1"), None));

        let sqlite = ServerInfo::new(Flavor::Sqlite, Version::new(3, 53, 2)).with_scan_status(true);
        let job = explain_job(&sqlite, "select 1".into(), ExplainMode::Analyze);
        assert!(matches!(&job.step, Ok(Step::Explain(r)) if r.explain == Explain::Measure("select 1".into())));
        assert_eq!(job.note, None);
        let job = explain_job(&sqlite, "delete from t".into(), ExplainMode::Analyze);
        assert_eq!(explained(&job), Some("EXPLAIN QUERY PLAN delete from t"));
        assert!(job.note.as_deref().is_some_and(|n| n.contains("DELETE") && n.contains("estimated plan")));

        let job = explain_job(&mysql, "explain select 1".into(), ExplainMode::Estimate);
        assert!(job.step.is_err());
    }

    #[test]
    fn chips_number_results_and_plans_separately() {
        let output =
            || QueryOutput { sql: String::new(), result: StatementResult::Affected(0), elapsed: Duration::ZERO };
        let rows = || ResultPane { output: output(), body: PaneBody::Rows { grid: Box::default(), sort: None } };
        let plan = || ResultPane {
            output: output(),
            body: PaneBody::Plan(Box::new(PlanView::new(
                PlanContent::Raw { text: String::new(), reason: String::new() },
                None,
            ))),
        };
        let mut t = QueryTab::new("q".into(), String::new());
        t.panes = vec![rows(), plan(), rows()];
        assert_eq!(t.chip_labels(), ["Result 1", "Plan", "Result 2"]);
        t.panes = vec![plan(), plan()];
        assert_eq!(t.chip_labels(), ["Plan 1", "Plan 2"]);
    }

    #[test]
    fn picks_statements() {
        let mut t = QueryTab::new("q".into(), "select 1;\nselect 2;\nselect 3".into());
        t.cursor = 12;
        assert_eq!(t.statements_to_run(Dialect::Postgres, false), vec!["select 2"]);
        assert_eq!(t.statements_to_run(Dialect::Postgres, true).len(), 3);
        t.selection = Some((0, 18));
        assert_eq!(t.statements_to_run(Dialect::Postgres, false), vec!["select 1", "select 2"]);
        let pasted = QueryTab::new("q".into(), "select 1;\n\u{a0}\u{a0}select 2;\u{a0};delete from t\u{a0}".into());
        assert_eq!(
            pasted.statements_to_run(Dialect::Postgres, true),
            vec!["select 1", "select 2", "delete from t\u{a0}"]
        );
    }
}
