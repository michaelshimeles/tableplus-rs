//! The Plan pane's detail panel: the selected step's numbers and server values, or with no step
//! selected the plan-wide figures.

use eframe::egui::{self, Color32, CornerRadius, Rect, RichText, Sense, Stroke, Ui, vec2};
use tableplus_core::plan::{FactKind, FactValue, NodeId, PlanFact, PlanNode};

use super::PlanView;
use super::tree::{BADGE_PAD, extra_badge};
use crate::icons::Icon;
use crate::plan_graph;
use crate::theme::{self, palette};
use crate::widgets::{self, fmt_count, fmt_factor, fmt_ms, fmt_rows};

/// JIT and trigger time get the warning colour from a tenth of the execution time, or when there
/// is no execution time to compare with.
pub(super) fn stands_out(facts: &[PlanFact], fact: &PlanFact) -> bool {
    let ms = |f: &PlanFact| match f.value {
        FactValue::Ms(ms) => Some(ms),
        FactValue::Cost(_) => None,
    };
    let execution = facts.iter().find(|f| f.kind == FactKind::Execution).and_then(ms).filter(|ms| *ms > 0.0);
    fact.kind.notable() && execution.is_none_or(|e| ms(fact).is_some_and(|v| v >= e * 0.1))
}

pub(super) fn fmt_fact(value: FactValue) -> String {
    match value {
        FactValue::Ms(ms) => fmt_ms(ms),
        FactValue::Cost(cost) => fmt_rows(cost),
    }
}

pub(super) fn detail(ui: &mut Ui, view: &PlanView, id: NodeId, open: &mut bool) {
    let p = palette(ui);
    let Some(plan) = view.plan() else { return };
    let node = plan.node(id);
    let m = &view.metrics[id.index()];
    egui::ScrollArea::vertical().id_salt(("plan_detail", id)).auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
            egui::Sides::new().shrink_left().show(
                ui,
                |ui| {
                    let title = RichText::new(&node.operation).font(theme::bold_font(14.5)).color(p.text);
                    ui.add(egui::Label::new(title).wrap());
                },
                |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close (Esc)").clicked() {
                        *open = false;
                    }
                    if widgets::icon_button(ui, Icon::Copy, "Copy this step").clicked() {
                        ui.ctx().copy_text(node_text(node));
                    }
                },
            );
            if let Some(role) = &node.role {
                ui.label(RichText::new(role).font(theme::ui_font(12.5)).color(p.text_muted));
            }
            if let Some(object) = &node.object {
                let text = match &node.index {
                    Some(index) => format!("{object}  ·  {index}"),
                    None => object.clone(),
                };
                ui.label(RichText::new(text).font(theme::ui_font(12.5)).color(p.text_muted));
            }
            let access = plan.access(id);
            if access.is_some() || !m.extras.is_empty() {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if let Some(access) = access {
                        let (r, _) = ui.allocate_exact_size(vec2(8.0, 15.0), Sense::hover());
                        let swatch = Rect::from_center_size(r.center(), vec2(8.0, 8.0));
                        let color = plan_graph::access_color(access, ui.visuals().dark_mode);
                        ui.painter().rect_filled(swatch, CornerRadius::same(2), color);
                        ui.label(RichText::new(access.label()).size(12.0).color(p.text_muted));
                    }
                    for extra in &m.extras {
                        let g = ui.painter().layout_no_wrap(extra.label().into(), theme::ui_font(10.5), p.text_muted);
                        let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + BADGE_PAD, 15.0), Sense::hover());
                        extra_badge(ui.painter(), r.left_center(), g, p.text_muted, p.border);
                        resp.on_hover_text(extra.description());
                    }
                });
            }
            if m.descendants > 0 {
                let below = format!("{} below", widgets::plural(m.descendants as u64, "step"));
                ui.label(RichText::new(below).size(12.0).color(p.text_faint));
            }
            if let Some(condition) = &node.condition {
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(p.row_alt)
                    .stroke(Stroke::new(1.0, p.grid_line))
                    .corner_radius(CornerRadius::same(5))
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(RichText::new(condition).font(theme::mono_font(11.5)).color(p.text))
                                .wrap(),
                        );
                    });
            }

            let mut measured: Vec<(&str, String, Option<Color32>)> = Vec::new();
            let facts = &mut measured;
            if let Some(total) = m.inclusive_ms {
                facts.push(("Time, with steps below", fmt_ms(total), None));
            }
            if let Some(own) = m.self_ms {
                facts.push(("Time in this step", fmt_ms(own), None));
            }
            if let Some(actual) = node.actual {
                if let Some(time) = actual.time {
                    if let Some(first) = time.first_row_ms {
                        facts.push(("First row, per loop", fmt_ms(first), None));
                        facts.push(("Last row, per loop", fmt_ms(time.last_row_ms), None));
                    } else {
                        facts.push(("Time per loop", fmt_ms(time.last_row_ms), None));
                    }
                }
                if let Some(rows) = actual.rows {
                    facts.push(("Rows per loop", fmt_rows(rows), None));
                }
                facts.push(("Loops", fmt_count(actual.loops), None));
                if let Some(rows) = actual.rows.filter(|_| actual.loops > 1) {
                    facts.push(("Rows in all loops", fmt_rows(rows * actual.loops as f64), None));
                }
            }
            let mut estimated: Vec<(&str, String, Option<Color32>)> = Vec::new();
            let facts = &mut estimated;
            if node.estimate.is_some() {
                let rows = plan.estimated_rows(id);
                let faint = rows.is_none().then_some(p.text_faint);
                facts.push(("Estimated rows per loop", rows.map_or_else(|| "not estimated".into(), fmt_rows), faint));
                if let Some(cost) = plan.estimated_cost(id) {
                    let text = match cost.startup {
                        Some(s) if s != cost.total => {
                            format!("{} to first row, {} total", fmt_rows(s), fmt_rows(cost.total))
                        }
                        _ => fmt_rows(cost.total),
                    };
                    facts.push(("Estimated cost", text, None));
                }
            }
            if let Some(ratio) = plan.row_estimate_ratio(id) {
                let (factor, way) = if ratio >= 1.0 { (ratio, "more") } else { (1.0 / ratio, "fewer") };
                let text = if factor < 1.05 {
                    "on target".to_string()
                } else {
                    format!("{}× {way} rows than estimated", fmt_factor(factor))
                };
                facts.push(("Estimate", text, plan.is_misestimated(id).then_some(p.warning)));
            }
            let heading = match (measured.is_empty(), estimated.is_empty()) {
                (false, false) => Some("Measured and estimated"),
                (false, true) => Some("Measured"),
                (true, false) => Some("Estimated"),
                (true, true) => None,
            };
            if let Some(heading) = heading {
                ui.add_space(12.0);
                widgets::section_title(ui, heading);
                egui::Grid::new(("plan_facts", id)).num_columns(2).striped(false).spacing([14.0, 6.0]).show(ui, |ui| {
                    for (k, v, color) in measured.into_iter().chain(estimated) {
                        ui.label(RichText::new(k).size(12.0).color(p.text_muted));
                        ui.label(RichText::new(v).size(12.5).color(color.unwrap_or(p.text)));
                        ui.end_row();
                    }
                });
            }

            ui.add_space(12.0);
            widgets::section_title(ui, "Reported by the server");
            egui::Grid::new(("plan_attrs", id)).num_columns(2).striped(false).spacing([14.0, 6.0]).show(ui, |ui| {
                for a in &node.attributes {
                    ui.label(RichText::new(&a.key).size(12.0).color(p.text_muted));
                    ui.add(egui::Label::new(RichText::new(&a.value).font(theme::mono_font(11.5)).color(p.text)).wrap());
                    ui.end_row();
                }
            });
        });
    });
}

pub(super) fn plan_summary(ui: &mut Ui, view: &PlanView, open: &mut bool) {
    let p = palette(ui);
    let Some(plan) = view.plan() else { return };
    egui::ScrollArea::vertical().id_salt("plan_summary").auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
            egui::Sides::new().shrink_left().show(
                ui,
                |ui| {
                    ui.label(RichText::new("Plan").font(theme::bold_font(14.5)).color(p.text));
                },
                |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close (Esc)").clicked() {
                        *open = false;
                    }
                    if widgets::icon_button(ui, Icon::Copy, "Copy the plan-wide values").clicked() {
                        let text: Vec<String> =
                            plan.attributes.iter().map(|a| format!("{}: {}", a.key, a.value)).collect();
                        ui.ctx().copy_text(text.join("\n"));
                    }
                },
            );
            let kind = if plan.analyzed() { "Analyzed" } else { "Estimated" };
            let subtitle =
                format!("{kind} · {} · {}", plan.format.label(), widgets::plural(plan.node_count() as u64, "step"));
            ui.label(RichText::new(subtitle).font(theme::ui_font(12.5)).color(p.text_muted));

            ui.add_space(12.0);
            widgets::section_title(ui, if plan.analyzed() { "Measured" } else { "Estimated" });
            egui::Grid::new("plan_summary_facts").num_columns(2).striped(false).spacing([14.0, 6.0]).show(ui, |ui| {
                let mut row = |key: &str, value: String, color: Color32| {
                    ui.label(RichText::new(key).size(12.0).color(p.text_muted));
                    ui.label(RichText::new(value).size(12.5).color(color));
                    ui.end_row();
                };
                if let Some(ms) = plan.total_ms() {
                    row("Time in the plan", fmt_ms(ms), p.text);
                }
                for fact in &view.facts {
                    let color = if stands_out(&view.facts, fact) { p.warning } else { p.text };
                    row(fact.kind.label(), fmt_fact(fact.value), color);
                }
            });

            let triggers = plan.triggers();
            if !triggers.is_empty() {
                ui.add_space(12.0);
                widgets::section_title(ui, "Triggers");
                egui::Grid::new("plan_summary_triggers").num_columns(2).striped(false).spacing([14.0, 6.0]).show(
                    ui,
                    |ui| {
                        for t in &triggers {
                            let name = match &t.relation {
                                Some(relation) => format!("{}  ·  {relation}", t.name),
                                None => t.name.clone(),
                            };
                            ui.label(RichText::new(name).size(12.0).color(p.text));
                            let calls = t.calls.map(|c| widgets::plural(c, "call"));
                            let value: Vec<String> = t.time_ms.map(fmt_ms).into_iter().chain(calls).collect();
                            ui.label(RichText::new(value.join("  ·  ")).size(12.5).color(p.text_muted));
                            ui.end_row();
                        }
                    },
                );
            }

            ui.add_space(12.0);
            widgets::section_title(ui, "Reported by the server");
            egui::Grid::new("plan_summary_attrs").num_columns(2).striped(false).spacing([14.0, 6.0]).show(ui, |ui| {
                for a in &plan.attributes {
                    ui.label(RichText::new(&a.key).size(12.0).color(p.text_muted));
                    ui.add(egui::Label::new(RichText::new(&a.value).font(theme::mono_font(11.5)).color(p.text)).wrap());
                    ui.end_row();
                }
            });
        });
    });
}

/// A step as `key: value` lines, for the clipboard.
pub(crate) fn node_text(node: &PlanNode) -> String {
    node.attributes.iter().map(|a| format!("{}: {}", a.key, a.value)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(kind: FactKind, ms: f64) -> PlanFact {
        PlanFact { kind, value: FactValue::Ms(ms) }
    }

    #[test]
    fn jit_and_triggers_stand_out_from_a_tenth_of_the_execution_time() {
        let facts = [ms(FactKind::Planning, 0.2), ms(FactKind::Execution, 50.0), ms(FactKind::Jit, 4.0)];
        assert!(!stands_out(&facts, &facts[2]), "8% of execution");
        let facts = [ms(FactKind::Execution, 50.0), ms(FactKind::Triggers, 5.0)];
        assert!(stands_out(&facts, &facts[1]));
        assert!(!stands_out(&facts, &facts[0]), "execution itself is never a warning");
        let facts = [ms(FactKind::Jit, 4.0)];
        assert!(stands_out(&facts, &facts[0]), "nothing to compare with");
    }
}
