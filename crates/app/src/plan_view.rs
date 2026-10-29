//! The Plan pane: an EXPLAIN plan as a collapsible tree, a graph or a flame chart, a resizable
//! detail panel for the selected step, and the server's raw output.

use std::time::Instant;

use eframe::egui::{self, Color32, CornerRadius, Id, RichText, Sense, Stroke, Ui, text::LayoutJob, vec2};
use tableplus_core::ServerInfo;
use tableplus_core::model::ResultSet;
use tableplus_core::plan::{self, Access, Extra, NodeId, Plan, PlanFact, PlanFormat, SPAN_SLACK};

use crate::icons::Icon;
use crate::plan_flame::{self, Flame};
use crate::plan_graph::{self, GraphView};
use crate::theme::{self, Palette, palette};
use crate::widgets::{self, fmt_ms};
pub(crate) use detail::node_text;
use detail::{detail, fmt_fact, plan_summary, stands_out};

mod detail;
mod tree;

pub enum PlanContent {
    Tree(Box<Plan>),
    /// The server's output when no parser reads its format, and why.
    Raw {
        text: String,
        reason: String,
    },
}

impl PlanContent {
    pub fn explained(server: &ServerInfo, rows: &ResultSet, format: PlanFormat) -> Self {
        Self::read(server, rows, || format!("No visual plan for {} output yet.", format.label()))
    }

    /// A plan for the result of a statement that asks for one, such as a hand-typed
    /// `EXPLAIN ANALYZE`, with the server's text when no parser reads it. Ordinary results that
    /// only look like a plan, such as a JSON value, stay rows.
    pub fn detected(server: &ServerInfo, sql: &str, rows: &ResultSet) -> Option<Self> {
        plan::asks_for_plan(server, sql)
            .then(|| Self::read(server, rows, || "No visual plan for this output yet.".into()))
    }

    fn read(server: &ServerInfo, rows: &ResultSet, unread: impl FnOnce() -> String) -> Self {
        match plan::parse(server, rows) {
            Some(Ok(plan)) => PlanContent::Tree(Box::new(plan)),
            Some(Err(e)) => PlanContent::Raw { text: plan::raw_text(rows), reason: format!("{e}.") },
            None => PlanContent::Raw {
                text: plan::raw_text(rows),
                reason: format!("{} This is the server's own text.", unread()),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Tree,
    Graph,
    Flame,
    Raw,
}

struct Metrics {
    inclusive_ms: Option<f64>,
    self_ms: Option<f64>,
    /// The planner's total cost, without MySQL's placeholder.
    cost: Option<f64>,
    /// The cost, or what the steps below cost in all when that is more.
    cost_span: f64,
    extras: Vec<Extra>,
    access: Option<Access>,
    descendants: usize,
}

impl Metrics {
    /// The steps below cost more than this step counts, as above a MySQL temporary table.
    fn spans_below(&self) -> bool {
        self.cost_span > self.cost.unwrap_or(0.0) * SPAN_SLACK
    }
}

pub enum PlanEvent {
    Select(NodeId),
    CloseDetail,
    Deselect,
}

impl PlanEvent {
    pub fn apply(&self, selected: &mut Option<NodeId>, detail_open: &mut bool) {
        match *self {
            PlanEvent::Select(node) => {
                *selected = Some(node);
                *detail_open = true;
            }
            PlanEvent::CloseDetail => *detail_open = false,
            PlanEvent::Deselect => *selected = None,
        }
    }
}

pub struct PlanView {
    content: PlanContent,
    /// Why the plan differs from what was asked for, such as Analyze falling back to estimates.
    note: Option<String>,
    mode: Mode,
    graph: GraphView,
    collapsed: Vec<bool>,
    /// Rows on screen, top to bottom: the nodes whose ancestors are all expanded.
    visible: Vec<NodeId>,
    metrics: Vec<Metrics>,
    /// Bar length for 100%: the statement's time, or without actuals what the whole plan costs.
    scale: f64,
    cost_below: bool,
    selected: Option<NodeId>,
    detail_open: bool,
    scroll_to_selected: bool,
    scroll_id: Option<Id>,
    shown_at: Option<Instant>,
    /// `None` when no step was timed.
    flame: Option<Flame>,
    facts: Vec<PlanFact>,
}

impl PlanView {
    pub fn new(content: PlanContent, note: Option<String>) -> Self {
        let mut view = Self {
            content,
            note,
            mode: Mode::Tree,
            graph: GraphView::default(),
            collapsed: Vec::new(),
            visible: Vec::new(),
            metrics: Vec::new(),
            scale: 0.0,
            cost_below: false,
            selected: None,
            detail_open: true,
            scroll_to_selected: false,
            scroll_id: None,
            shown_at: None,
            flame: None,
            facts: Vec::new(),
        };
        if let PlanContent::Tree(plan) = &view.content {
            view.collapsed = vec![false; plan.node_count()];
            let cost_spans = plan.cost_spans();
            view.metrics = plan
                .nodes()
                .map(|(id, _)| Metrics {
                    inclusive_ms: plan.inclusive_ms(id),
                    self_ms: plan.self_ms(id),
                    cost: plan.estimated_cost(id).map(|c| c.total),
                    cost_span: cost_spans[id.index()],
                    extras: plan.extras(id),
                    access: plan.access(id),
                    descendants: plan.descendant_count(id),
                })
                .collect();
            view.cost_below = view.metrics.iter().any(Metrics::spans_below);
            view.scale =
                if plan.analyzed() { plan.total_ms().unwrap_or(0.0) } else { cost_spans[NodeId::ROOT.index()] };
            view.flame = Flame::new(plan);
            view.facts = plan.facts();
            view.rebuild_visible();
        }
        view
    }

    pub fn plan(&self) -> Option<&Plan> {
        match &self.content {
            PlanContent::Tree(plan) => Some(plan),
            PlanContent::Raw { .. } => None,
        }
    }

    pub fn raw_text(&self) -> &str {
        match &self.content {
            PlanContent::Tree(plan) => &plan.raw,
            PlanContent::Raw { text, .. } => text,
        }
    }

    /// `56 steps · 4.68 ms`, for the result strip.
    pub fn summary(&self) -> String {
        match self.plan() {
            Some(plan) => {
                let steps = widgets::plural(plan.node_count() as u64, "step");
                match plan.total_ms() {
                    Some(ms) => format!("{steps} · {}", fmt_ms(ms)),
                    None if plan.analyzed() => steps,
                    None => format!("{steps} · estimated"),
                }
            }
            None => "Server output".into(),
        }
    }

    fn rebuild_visible(&mut self) {
        let Some(plan) = self.plan() else { return };
        let mut visible = Vec::with_capacity(plan.node_count());
        let mut stack = vec![NodeId::ROOT];
        while let Some(id) = stack.pop() {
            visible.push(id);
            if !self.collapsed[id.index()] {
                stack.extend(plan.children(id).iter().rev());
            }
        }
        self.visible = visible;
    }

    fn set_collapsed(&mut self, id: NodeId, collapsed: bool) {
        let Some(plan) = self.plan() else { return };
        if plan.children(id).is_empty() || self.collapsed[id.index()] == collapsed {
            return;
        }
        self.collapsed[id.index()] = collapsed;
        self.rebuild_visible();
    }

    /// Collapsing keeps the root open.
    pub fn set_all_collapsed(&mut self, collapsed: bool) {
        let Some(plan) = self.plan() else { return };
        let parents: Vec<bool> = plan.nodes().map(|(id, _)| !plan.children(id).is_empty()).collect();
        for (c, parent) in self.collapsed.iter_mut().zip(parents) {
            *c = collapsed && parent;
        }
        if let Some(root) = self.collapsed.first_mut() {
            *root = false;
        }
        self.rebuild_visible();
        if let Some(sel) = self.selected
            && !self.visible.contains(&sel)
        {
            self.selected = self.visible_ancestor(sel);
        }
    }

    fn visible_ancestor(&self, id: NodeId) -> Option<NodeId> {
        let plan = self.plan()?;
        let mut at = Some(id);
        while let Some(n) = at {
            if self.visible.contains(&n) {
                return Some(n);
            }
            at = plan.parent(n);
        }
        None
    }

    fn apply(&mut self, event: PlanEvent) {
        event.apply(&mut self.selected, &mut self.detail_open);
        if let PlanEvent::Select(_) = event {
            self.reveal_selected();
        }
    }

    fn reveal_selected(&mut self) {
        let Some((plan, sel)) = self.plan().zip(self.selected) else { return };
        let ancestors: Vec<NodeId> = std::iter::successors(plan.parent(sel), |n| plan.parent(*n)).collect();
        for a in ancestors {
            self.collapsed[a.index()] = false;
        }
        self.rebuild_visible();
        self.scroll_to_selected = true;
    }

    pub fn select_row(&mut self, index: usize) {
        if let Some(id) = self.visible.get(index.min(self.visible.len().saturating_sub(1))) {
            self.selected = Some(*id);
            self.detail_open = true;
            self.scroll_to_selected = true;
        }
    }
}

pub fn show(ui: &mut Ui, id: Id, view: &mut PlanView) {
    let p = palette(ui);
    toolbar(ui, id, view, p);
    if let Some(note) = &view.note {
        banner(ui, id.with("note"), note, p.warning, p);
    }
    if view.mode == Mode::Raw || view.plan().is_none() {
        raw_view(ui, id, view.raw_text());
        return;
    }
    if view.detail_open && (view.selected.is_some() || !view.facts.is_empty()) {
        let mut open = true;
        egui::Panel::right(id.with("detail"))
            .resizable(true)
            .default_size(300.0)
            .size_range(240.0..=680.0)
            .frame(egui::Frame::new().fill(p.content_bg).stroke(Stroke::new(1.0, p.border)))
            .show(ui, |ui| match view.selected {
                Some(sel) => detail(ui, view, sel, &mut open),
                None => plan_summary(ui, view, &mut open),
            });
        view.detail_open = open;
    }
    let events =
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| match (view.mode, &view.content) {
            (Mode::Graph, PlanContent::Tree(plan)) => {
                plan_graph::show(ui, id.with("graph"), plan, &mut view.graph, view.selected, view.detail_open)
            }
            (Mode::Flame, PlanContent::Tree(plan)) => {
                plan_flame::show(ui, id.with("flame"), plan, view.flame.as_mut(), view.selected, view.detail_open)
            }
            _ => {
                tree::show(ui, id, view);
                Vec::new()
            }
        });
    for event in events.inner {
        view.apply(event);
    }
}

fn toolbar(ui: &mut Ui, id: Id, view: &mut PlanView, p: &Palette) {
    egui::Panel::top(id.with("toolbar")).exact_size(34.0).frame(egui::Frame::NONE).show(ui, |ui| {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, CornerRadius::ZERO, p.content_bg);
        ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
        ui.horizontal_centered(|ui| {
            ui.add_space(10.0);
            if let Some(plan) = view.plan() {
                let kind = if plan.analyzed() { "Analyzed" } else { "Estimated" };
                let label = format!("{kind} · {}", plan.format.label());
                let modes = [(Mode::Tree, "Tree"), (Mode::Graph, "Graph"), (Mode::Flame, "Flame"), (Mode::Raw, "Raw")];
                if widgets::segmented(ui, &mut view.mode, &modes) && view.mode == Mode::Tree {
                    view.reveal_selected();
                }
                if view.mode == Mode::Tree {
                    ui.add_space(6.0);
                    if text_button(ui, "Expand all").clicked() {
                        view.set_all_collapsed(false);
                    }
                    if text_button(ui, "Collapse all").clicked() {
                        view.set_all_collapsed(true);
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new(label).size(12.0).color(p.text_faint));
            }
            if let PlanContent::Raw { reason, .. } = &view.content {
                ui.add_space(4.0);
                let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.5, p.text_faint);
                ui.label(RichText::new(reason).size(12.0).color(p.text_muted));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if widgets::icon_button(ui, Icon::Copy, "Copy the server's output").clicked() {
                    ui.ctx().copy_text(view.raw_text().to_string());
                }
                if !view.facts.is_empty() && facts_button(ui, &view.facts, p).clicked() {
                    view.selected = None;
                    view.detail_open = true;
                }
            });
        });
    });
}

/// Borderless text button with a hover background, quieter than the framed toolbar buttons.
fn text_button(ui: &mut Ui, text: &str) -> egui::Response {
    let p = palette(ui);
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::ui_font(12.0), p.text_muted);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 24.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(5), p.hover);
    }
    let color = if resp.hovered() { p.text } else { p.text_muted };
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    resp
}

/// `Planning 0.42 ms · Execution 3.91 ms`. Ordinary figures give way first when the toolbar is narrow.
fn facts_button(ui: &mut Ui, facts: &[PlanFact], p: &Palette) -> egui::Response {
    let job = |shown: &[&PlanFact]| {
        let mut job = LayoutJob::default();
        for (i, fact) in shown.iter().enumerate() {
            let format =
                |color: Color32| egui::TextFormat { font_id: theme::ui_font(12.0), color, ..Default::default() };
            if i > 0 {
                job.append("·", 7.0, format(p.text_faint));
            }
            job.append(fact.kind.label(), if i > 0 { 7.0 } else { 0.0 }, format(p.text_faint));
            let value = if stands_out(facts, fact) { p.warning } else { p.text_muted };
            job.append(&fmt_fact(fact.value), 4.0, format(value));
        }
        job
    };
    let mut shown: Vec<&PlanFact> = facts.iter().collect();
    let room = ui.available_width() - 24.0;
    let mut galley = ui.painter().layout_job(job(&shown));
    while galley.size().x > room && shown.len() > 1 {
        let drop = shown.iter().rposition(|f| !f.kind.notable()).unwrap_or(shown.len() - 1);
        shown.remove(drop);
        galley = ui.painter().layout_job(job(&shown));
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 24.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(5), p.hover);
    }
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text_muted);
    resp.on_hover_text("Show the plan's details")
}

fn banner(ui: &mut Ui, id: Id, text: &str, dot: Color32, p: &Palette) {
    egui::Panel::top(id).exact_size(28.0).frame(egui::Frame::NONE).show(ui, |ui| {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, CornerRadius::ZERO, p.header_bg);
        ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
        ui.horizontal_centered(|ui| {
            ui.add_space(14.0);
            let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 3.5, dot);
            ui.label(RichText::new(text).size(12.0).color(p.text_muted));
        });
    });
}

fn raw_view(ui: &mut Ui, id: Id, text: &str) {
    let p = palette(ui);
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let job = LayoutJob::simple(buf.as_str().to_string(), theme::mono_font(12.0), p.text, f32::INFINITY);
        ui.painter().layout_job(job)
    };
    egui::ScrollArea::both().id_salt(id.with("raw")).auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut &*text)
                    .id(id.with("raw_text"))
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .layouter(&mut layouter),
            );
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tableplus_core::model::ColumnMeta;
    use tableplus_core::{Flavor, Value, Version};

    #[test]
    fn json_query_results_stay_rows() {
        let one_cell = |column: &str, text: &str| ResultSet {
            columns: vec![ColumnMeta { name: column.into(), type_name: "json".into() }],
            rows: vec![vec![Value::Text(text.into())].into_boxed_slice()],
            truncated: false,
        };
        let postgres = ServerInfo::new(Flavor::Postgres, Version::new(17, 6, 0));
        let mariadb = ServerInfo::new(Flavor::MariaDb, Version::new(11, 8, 0));
        let mysql = ServerInfo::new(Flavor::MySql, Version::new(8, 4, 0));
        assert!(PlanContent::detected(&postgres, "select 1", &one_cell("json_agg", "[1,2,3]")).is_none());
        assert!(PlanContent::detected(&mariadb, "select 1", &one_cell("doc", r#"{"customer_id":1}"#)).is_none());
        let plan_shaped = one_cell("payload", r#"{"operation": "refund", "access_type": "table"}"#);
        assert!(PlanContent::detected(&mysql, "select payload from t", &plan_shaped).is_none());
        let tree = one_cell("EXPLAIN", "-> Table scan on t  (cost=0.35 rows=1)");
        assert!(PlanContent::detected(&mysql, "explain format=tree select * from t", &tree).is_some());
        assert!(PlanContent::detected(&mysql, "select '-> Table scan on t  (cost=0.35 rows=1)'", &tree).is_none());
        let text_plan = one_cell("QUERY PLAN", "Seq Scan on t  (cost=0.00..35.50 rows=2550 width=4)");
        let shown = PlanContent::detected(&postgres, "explain select * from t", &text_plan);
        assert!(matches!(shown, Some(PlanContent::Raw { .. })), "a plan format without a parser shows its text");
        let columns = one_cell("Field", "id");
        assert!(PlanContent::detected(&mysql, "describe t", &columns).is_none(), "a table's columns stay rows");
        let explained = PlanContent::explained(&postgres, &one_cell("QUERY PLAN", "[1,2,3]"), PlanFormat::PostgresJson);
        assert!(matches!(explained, PlanContent::Raw { .. }), "an explicit Explain still shows the server's text");
    }
}
