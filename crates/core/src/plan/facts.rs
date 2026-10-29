//! Plan-wide figures the engines report beside the steps: planning and execution time, JIT
//! compilation, triggers, and MySQL 5.7's query cost.

use super::{NodeId, Plan, PlanFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FactKind {
    Planning,
    Execution,
    /// PostgreSQL compiling expressions with LLVM.
    Jit,
    /// Time in the triggers a statement fired, which PostgreSQL counts outside every step.
    Triggers,
    /// PostgreSQL 17's SERIALIZE option: turning the result into its wire format.
    Serialization,
    /// MariaDB's ANALYZE: choosing the plan.
    Optimization,
    /// MySQL's JSON format version 1: the estimated cost of the whole statement.
    QueryCost,
}

impl FactKind {
    pub fn label(self) -> &'static str {
        match self {
            FactKind::Planning => "Planning",
            FactKind::Execution => "Execution",
            FactKind::Jit => "JIT",
            FactKind::Triggers => "Triggers",
            FactKind::Serialization => "Serialization",
            FactKind::Optimization => "Optimization",
            FactKind::QueryCost => "Query cost",
        }
    }

    /// Easy to miss in the steps, and able to dominate a short statement.
    pub fn notable(self) -> bool {
        matches!(self, FactKind::Jit | FactKind::Triggers)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FactValue {
    Ms(f64),
    Cost(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanFact {
    pub kind: FactKind,
    pub value: FactValue,
}

/// A trigger PostgreSQL's ANALYZE ran for the statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub name: String,
    pub relation: Option<String>,
    pub time_ms: Option<f64>,
    pub calls: Option<u64>,
}

/// `Statement 2 · Planning Time` → `Planning Time`, for PostgreSQL's several statements.
fn without_statement(key: &str) -> &str {
    match key.split_once(" · ") {
        Some((statement, rest)) if statement.starts_with("Statement ") => rest,
        _ => key,
    }
}

/// `Triggers[0] · Time` → `(0, "Time")`.
fn trigger_field(key: &str) -> Option<(usize, &str)> {
    let rest = without_statement(key).strip_prefix("Triggers[")?;
    let (index, field) = rest.split_once("] · ")?;
    Some((index.parse().ok()?, field))
}

impl Plan {
    /// The plan-wide figures worth a headline, in a fixed order. Several PostgreSQL statements
    /// add up.
    pub fn facts(&self) -> Vec<PlanFact> {
        let mut found: Vec<(FactKind, f64)> = Vec::new();
        let mut add = |kind: FactKind, value: f64| match found.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, total)) => *total += value,
            None => found.push((kind, value)),
        };
        for a in &self.attributes {
            let Ok(value) = a.value.parse::<f64>() else { continue };
            let kind = match (self.format, without_statement(&a.key)) {
                (PlanFormat::PostgresJson, "Planning Time") => FactKind::Planning,
                (PlanFormat::PostgresJson, "Execution Time") => FactKind::Execution,
                (PlanFormat::PostgresJson, "JIT · Timing · Total") => FactKind::Jit,
                (PlanFormat::PostgresJson, "Serialization · Time") => FactKind::Serialization,
                (PlanFormat::PostgresJson, _) if trigger_field(&a.key).is_some_and(|(_, f)| f == "Time") => {
                    FactKind::Triggers
                }
                (PlanFormat::MariaDbJson, "query_optimization.r_total_time_ms") => FactKind::Optimization,
                _ => continue,
            };
            add(kind, value);
        }
        // Version 1 keeps the statement's cost in its top query block.
        if self.format == PlanFormat::MySqlJson
            && let Some(cost) = self.node(NodeId::ROOT).attribute("query_cost").and_then(|v| v.parse().ok())
        {
            add(FactKind::QueryCost, cost);
        }
        let order = [
            FactKind::Planning,
            FactKind::Execution,
            FactKind::Jit,
            FactKind::Triggers,
            FactKind::Serialization,
            FactKind::Optimization,
            FactKind::QueryCost,
        ];
        order
            .into_iter()
            .filter_map(|kind| {
                let (_, value) = found.iter().find(|(k, _)| *k == kind)?;
                let value = if kind == FactKind::QueryCost { FactValue::Cost(*value) } else { FactValue::Ms(*value) };
                Some(PlanFact { kind, value })
            })
            .collect()
    }

    /// The triggers PostgreSQL ran, in its order.
    pub fn triggers(&self) -> Vec<Trigger> {
        let mut triggers: Vec<(usize, Trigger)> = Vec::new();
        for a in &self.attributes {
            let Some((index, field)) = trigger_field(&a.key) else { continue };
            let at = match triggers.iter().position(|(i, _)| *i == index) {
                Some(at) => at,
                None => {
                    let empty = Trigger { name: String::new(), relation: None, time_ms: None, calls: None };
                    triggers.push((index, empty));
                    triggers.len() - 1
                }
            };
            let trigger = &mut triggers[at].1;
            match field {
                "Trigger Name" => trigger.name = a.value.clone(),
                "Relation" => trigger.relation = Some(a.value.clone()),
                "Time" => trigger.time_ms = a.value.parse().ok(),
                "Calls" => trigger.calls = a.value.parse().ok(),
                _ => {}
            }
        }
        triggers.into_iter().map(|(_, t)| t).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnMeta, ResultSet};
    use crate::server::{Flavor, ServerInfo, Version};
    use crate::value::Value;

    fn plan(flavor: Flavor, version: Version, text: &str) -> Plan {
        let result = ResultSet {
            columns: vec![ColumnMeta::new("QUERY PLAN", "json")],
            rows: vec![vec![Value::text(text)].into()],
            truncated: false,
        };
        crate::plan::parse(&ServerInfo::new(flavor, version), &result).unwrap().unwrap()
    }

    fn postgres(text: &str) -> Plan {
        plan(Flavor::Postgres, Version::new(17, 0, 0), text)
    }

    const SCAN: &str =
        r#""Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Plan Rows": 830, "Total Cost": 22}"#;

    #[test]
    fn postgres_timing_jit_and_triggers() {
        let plan = postgres(&format!(
            r#"[{{{SCAN}, "Planning": {{"Memory Used": 1187}}, "Planning Time": 1.258,
                 "Triggers": [{{"Trigger Name": "audit_orders", "Relation": "orders", "Time": 2.5, "Calls": 10}},
                              {{"Trigger Name": "RI_ConstraintTrigger_c_1", "Relation": "order_details", "Time": 0.75, "Calls": 3}}],
                 "JIT": {{"Functions": 7, "Options": {{"Inlining": false}}, "Timing": {{"Generation": 1.1, "Total": 45.2}}}},
                 "Serialization": {{"Time": 0.006, "Output Volume": 2, "Format": "text"}},
                 "Execution Time": 51.94}}]"#
        ));
        let ms = |kind: FactKind, ms: f64| PlanFact { kind, value: FactValue::Ms(ms) };
        assert_eq!(
            plan.facts(),
            [
                ms(FactKind::Planning, 1.258),
                ms(FactKind::Execution, 51.94),
                ms(FactKind::Jit, 45.2),
                ms(FactKind::Triggers, 3.25),
                ms(FactKind::Serialization, 0.006),
            ]
        );
        assert_eq!(
            plan.triggers(),
            [
                Trigger {
                    name: "audit_orders".into(),
                    relation: Some("orders".into()),
                    time_ms: Some(2.5),
                    calls: Some(10)
                },
                Trigger {
                    name: "RI_ConstraintTrigger_c_1".into(),
                    relation: Some("order_details".into()),
                    time_ms: Some(0.75),
                    calls: Some(3)
                },
            ]
        );
    }

    #[test]
    fn postgres_estimates_and_empty_trigger_lists_have_no_facts() {
        assert_eq!(postgres(&format!("[{{{SCAN}}}]")).facts(), []);
        let plan = postgres(&format!(r#"[{{{SCAN}, "Planning Time": 0.1, "Triggers": [], "Execution Time": 0.4}}]"#));
        assert_eq!(plan.facts().iter().map(|f| f.kind).collect::<Vec<_>>(), [FactKind::Planning, FactKind::Execution]);
        assert_eq!(plan.triggers(), []);
    }

    #[test]
    fn several_postgres_statements_add_up() {
        let plan = postgres(&format!(
            r#"[{{{SCAN}, "Planning Time": 0.1, "Execution Time": 0.5}}, {{{SCAN}, "Planning Time": 0.2, "Execution Time": 1.0}}]"#
        ));
        let facts = plan.facts();
        assert_eq!(facts[0].kind, FactKind::Planning);
        assert!(matches!(facts[0].value, FactValue::Ms(ms) if (ms - 0.3).abs() < 1e-9));
        assert!(matches!(facts[1].value, FactValue::Ms(ms) if (ms - 1.5).abs() < 1e-9));
    }

    #[test]
    fn mariadb_optimization_and_mysql_query_cost() {
        let mariadb = plan(
            Flavor::MariaDb,
            Version::new(11, 8, 0),
            r#"{"query_optimization": {"r_total_time_ms": 0.302}, "query_block": {"select_id": 1, "r_loops": 1,
                "table": {"table_name": "a", "access_type": "ALL", "r_loops": 1, "rows": 4, "r_rows": 4}}}"#,
        );
        assert_eq!(mariadb.facts(), [PlanFact { kind: FactKind::Optimization, value: FactValue::Ms(0.302) }]);
        let mysql = plan(
            Flavor::MySql,
            Version::new(5, 7, 44),
            r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "6438.75"},
                "table": {"table_name": "o", "access_type": "ALL", "rows_examined_per_scan": 830}}}"#,
        );
        assert_eq!(mysql.facts(), [PlanFact { kind: FactKind::QueryCost, value: FactValue::Cost(6438.75) }]);
    }
}
