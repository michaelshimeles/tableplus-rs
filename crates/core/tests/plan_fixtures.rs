//! Plans from server versions outside the Docker stack (`fixtures/explain/README.md`), parsed
//! and compared with summaries read off the server output by hand.

use tableplus_core::model::{ColumnMeta, ResultSet};
use tableplus_core::plan::{self, NodeId, Plan, PlanFormat};
use tableplus_core::{Flavor, ServerInfo, Value, Version};

struct Case {
    file: &'static str,
    output: &'static str,
    server: ServerInfo,
    format: PlanFormat,
    nodes: usize,
    root: &'static str,
    total_ms: Option<f64>,
    /// `(depth, operation, object)` of some steps, in plan order.
    steps: &'static [(usize, &'static str, Option<&'static str>)],
}

const fn server(flavor: Flavor, major: u32, minor: u32, patch: u32) -> ServerInfo {
    ServerInfo::new(flavor, Version::new(major, minor, patch))
}

const CASES: &[Case] = &[
    Case {
        file: "mysql-5.7-dml.json",
        output: include_str!("fixtures/explain/mysql-5.7-dml.json"),
        server: server(Flavor::MySql, 5, 7, 44),
        format: PlanFormat::MySqlJson,
        nodes: 8,
        root: "Select #1",
        total_ms: None,
        steps: &[
            (1, "Full index scan", Some("products")),
            (2, "Select #3", None),
            (3, "Nested loop", None),
            (4, "Non-unique key lookup", Some("od")),
            (4, "Unique key lookup", Some("o")),
            (2, "Select #2", None),
            (3, "Unique key lookup into a subquery's table", Some("c")),
        ],
    },
    Case {
        file: "mysql-8.0.46-modern-analyze.txt",
        output: include_str!("fixtures/explain/mysql-8.0.46-modern-analyze.txt"),
        server: server(Flavor::MySql, 8, 0, 46),
        format: PlanFormat::MySqlTree,
        nodes: 34,
        root: "Sort",
        total_ms: Some(1.88),
        steps: &[
            (1, "Table scan", Some("<temporary>")),
            (8, "Materialize recursive CTE", Some("chain")),
            (9, "Repeat until convergence", None),
            (12, "Scan new records", Some("chain")),
            (6, "Materialize CTE", Some("ranked")),
            (18, "Index scan", Some("order_details")),
            (15, "Single-row index lookup", Some("o")),
        ],
    },
    Case {
        file: "mysql-9.7-dml-json-v2.json",
        output: include_str!("fixtures/explain/mysql-9.7-dml-json-v2.json"),
        server: server(Flavor::MySql, 9, 7, 2),
        format: PlanFormat::MySqlJsonV2,
        nodes: 11,
        root: "Update",
        total_ms: None,
        steps: &[
            (1, "Nested loop semijoin", None),
            (2, "Nested loop inner join", None),
            (4, "Table scan", Some("c")),
            (4, "Index lookup", Some("products")),
            (3, "Covering index lookup", Some("od")),
            (4, "Single-row index lookup", Some("o")),
        ],
    },
    Case {
        file: "mariadb-10.6-dml.json",
        output: include_str!("fixtures/explain/mariadb-10.6-dml.json"),
        server: server(Flavor::MariaDb, 10, 6, 28),
        format: PlanFormat::MariaDbJson,
        nodes: 8,
        root: "Select #1",
        total_ms: None,
        // 10.6 lists a join as repeated `table` keys in one object.
        steps: &[
            (1, "Update", Some("products")),
            (1, "Select #3", None),
            (2, "Nested loop", None),
            (3, "Index lookup", Some("od")),
            (3, "Single-row index lookup", Some("o")),
            (1, "Select #2", None),
            (2, "Unique subquery lookup", Some("c")),
        ],
    },
    Case {
        file: "postgres-12-dml.json",
        output: include_str!("fixtures/explain/postgres-12-dml.json"),
        server: server(Flavor::Postgres, 12, 22, 0),
        format: PlanFormat::PostgresJson,
        nodes: 9,
        root: "Update",
        total_ms: None,
        steps: &[
            (1, "Nested Loop Semi Join", None),
            (2, "Hash Join", None),
            (3, "Seq Scan", Some("products")),
            (3, "Hash", None),
            (4, "Seq Scan", Some("categories c")),
            (2, "Nested Loop", None),
            (3, "Index Scan", Some("order_details od")),
            (3, "Index Scan", Some("orders o")),
        ],
    },
    Case {
        file: "sqlite-3.53-modern-query-plan.tsv",
        output: include_str!("fixtures/explain/sqlite-3.53-modern-query-plan.tsv"),
        server: server(Flavor::Sqlite, 3, 53, 2),
        format: PlanFormat::SqliteQueryPlan,
        nodes: 26,
        root: "Query",
        total_ms: None,
        steps: &[
            (1, "Co-routine", Some("chain")),
            (2, "Setup", None),
            (3, "Scan", Some("employees")),
            (2, "Recursive step", None),
            (3, "Bloom filter", Some("e")),
            (1, "Materialize", Some("ranked")),
            (5, "Scan", Some("order_details")),
            (1, "Search", Some("m")),
        ],
    },
];

/// The server's result: one text cell, or for SQLite the tab-separated rows under their header.
fn result(case: &Case) -> ResultSet {
    if case.server.flavor != Flavor::Sqlite {
        return ResultSet {
            columns: vec![ColumnMeta::new("EXPLAIN", "")],
            rows: vec![vec![Value::text(case.output)].into()],
            truncated: false,
        };
    }
    let mut lines = case.output.lines();
    let columns = lines.next().unwrap().split('\t').map(|c| ColumnMeta::new(c, "")).collect();
    let cell = |v: &str| v.parse().map(Value::Int).unwrap_or_else(|_| Value::text(v));
    let rows = lines.map(|l| l.split('\t').map(cell).collect::<Vec<_>>().into()).collect();
    ResultSet { columns, rows, truncated: false }
}

fn steps(plan: &Plan) -> Vec<(usize, &str, Option<&str>)> {
    plan.nodes().map(|(id, n)| (plan.depth(id), n.operation.as_str(), n.object.as_deref())).collect()
}

#[test]
fn fixtures_parse_into_their_plan_trees() {
    for case in CASES {
        let plan = plan::parse(&case.server, &result(case))
            .unwrap_or_else(|| panic!("{}: not read as a plan", case.file))
            .unwrap_or_else(|e| panic!("{}: {e}", case.file));
        assert_eq!(plan.format, case.format, "{}", case.file);
        assert_eq!(plan.analyzed(), case.total_ms.is_some(), "{}", case.file);
        assert_eq!(plan.node_count(), case.nodes, "{}", case.file);
        assert_eq!(plan.node(NodeId::ROOT).operation, case.root, "{}", case.file);
        assert_eq!(plan.total_ms(), case.total_ms, "{}", case.file);
        let all = steps(&plan);
        let mut rest = all.iter();
        for step in case.steps {
            assert!(rest.any(|s| s == step), "{}: {step:?} in order in {all:#?}", case.file);
        }
    }
}

#[test]
fn mysql_tree_reads_times_in_exponent_notation() {
    let case = &CASES[1];
    let plan = plan::parse(&case.server, &result(case)).unwrap().unwrap();
    // `-> Scan new records on chain  (cost=2.73 rows=2) (actual time=312e-6..458e-6 rows=4.5 loops=2)`
    let (id, scan) = plan.nodes().find(|(_, n)| n.operation == "Scan new records").unwrap();
    let actual = scan.actual.unwrap();
    assert_eq!((actual.rows, actual.loops), (Some(4.5), 2));
    assert_eq!(actual.time.map(|t| (t.first_row_ms, t.last_row_ms)), Some((Some(312e-6), 458e-6)));
    assert_eq!(plan.inclusive_ms(id), Some(2.0 * 458e-6));
    assert_eq!(scan.estimate.map(|e| e.rows), Some(2.0));
}
