//! Explain Analyze on SQLite: the bundled library measures each plan step while the statement
//! runs, and the driver reads those measurements through `sqlite3_stmt_scanstatus_v2`. Times
//! change with every run, so these tests check structure and counts, never measured times.

mod common;

use std::time::Duration;

use common::*;
use tableplus_core::config::Credentials;
use tableplus_core::driver::{self, DynConnection};
use tableplus_core::plan::{self, Explain, ExplainMode, ExplainStatements, NodeId, Plan, PlanFormat, PlanNode};
use tableplus_core::{StatementResult, Value};

#[tokio::test(flavor = "multi_thread")]
async fn the_bundled_library_measures_plan_steps() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = sqlite_fixture(dir.path()).await;
    let conn = driver::connect(&cfg, &Credentials::default()).await.unwrap();
    let options = rows(&conn, "PRAGMA compile_options").await;
    let options: Vec<String> = options.rows.iter().filter_map(|r| r[0].to_text()).collect();
    assert!(
        options.iter().any(|o| o == "ENABLE_STMT_SCANSTATUS"),
        "built without SQLITE_ENABLE_STMT_SCANSTATUS; cargo reads LIBSQLITE3_FLAGS from .cargo/config.toml: {options:?}"
    );
    assert!(conn.server_info().scan_status);
}

fn detail(node: &PlanNode) -> Option<&str> {
    node.attributes.iter().find(|a| a.key == "detail").map(|a| a.value.as_str())
}

/// Each step's `EXPLAIN QUERY PLAN` text with its parent's, in plan order.
fn shape(plan: &Plan) -> Vec<(Option<&str>, &str)> {
    plan.nodes().filter_map(|(id, n)| Some((plan.parent(id).and_then(|p| detail(plan.node(p))), detail(n)?))).collect()
}

fn is_loop(detail: &str) -> bool {
    (detail.starts_with("SCAN ") || detail.starts_with("SEARCH ")) && detail != "SCAN CONSTANT ROW"
}

/// Explain Analyze as the app runs it.
async fn measured(conn: &DynConnection, sql: &str) -> Plan {
    let req = plan::request(&conn.server_info(), sql, ExplainMode::Analyze).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(matches!(req.explain, Explain::Measure(_)), "SQLite measures the statement itself: {req:?}");
    let explained = conn.explain(&req).await.unwrap_or_else(|e| panic!("{sql}: {e:#}"));
    assert_eq!(explained.reset_error, None);
    let rs = explained.output.rows().expect("measured rows");
    let plan = plan::parse(&conn.server_info(), rs).expect("a plan").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(plan.format, PlanFormat::SqliteScanStatus);
    plan
}

/// Measures `sql` and checks the plan against `EXPLAIN QUERY PLAN` and the statement's rows.
async fn check_measured(conn: &DynConnection, sql: &str) -> Plan {
    let plan = measured(conn, sql).await;
    let estimated = rows(conn, &format!("EXPLAIN QUERY PLAN {sql}")).await;
    let estimated = plan::parse(&conn.server_info(), &estimated).unwrap().unwrap();
    // Only a measured run reports building an automatic index, as a step of its own.
    let mut steps = shape(&plan);
    steps.retain(|(_, d)| !d.starts_with("CREATE AUTOMATIC INDEX ON "));
    assert_eq!(steps, shape(&estimated), "every EXPLAIN QUERY PLAN step, under the same parent");

    let returned = rows(conn, sql).await.rows.len() as f64;
    let root = plan.node(NodeId::ROOT).actual.expect("the statement ran");
    assert_eq!((root.rows, root.loops), (Some(returned), 1), "the root counts the statement's rows");
    assert!(plan.total_ms().is_some_and(|ms| ms > 0.0));

    for (id, node) in plan.nodes() {
        let text = detail(node).unwrap_or("the statement");
        if is_loop(text) {
            let actual = node.actual.unwrap_or_else(|| panic!("{text} ran, so it has loop counts"));
            assert!(actual.loops > 0 && actual.rows.is_some(), "{text}: {actual:?}");
            assert!(node.estimate.is_some(), "{text} has the planner's estimate");
        }
        if let (Some(total), Some(own)) = (plan.inclusive_ms(id), plan.self_ms(id)) {
            assert!((0.0..=total).contains(&own), "{text}: self {own} ms, inclusive {total} ms");
        }
    }
    plan
}

async fn number(conn: &DynConnection, sql: &str) -> f64 {
    rows(conn, sql).await.rows[0][0].to_text().and_then(|n| n.parse().ok()).expect("a number")
}

fn find<'a>(plan: &'a Plan, text: &str) -> &'a PlanNode {
    plan.nodes().map(|(_, n)| n).find(|n| detail(n) == Some(text)).unwrap_or_else(|| panic!("no step {text}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn measures_every_step_of_a_statement() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = sqlite_fixture(dir.path()).await;
    let conn = driver::connect(&cfg, &Credentials::default()).await.unwrap();

    // The seed has 20,000 orders, and every fifth one is paid.
    let plan = check_measured(&conn, "SELECT id, total FROM orders WHERE status = 'paid'").await;
    assert_eq!(plan.node(NodeId::ROOT).actual.unwrap().rows, Some(4000.0));
    let scan = find(&plan, "SCAN orders").actual.unwrap();
    assert_eq!((scan.rows, scan.loops), (Some(20_000.0), 1), "the scan visits every order once");

    let plan = check_measured(
        &conn,
        "SELECT c.name AS category, count(*) AS orders, sum(t.items) AS items,
                (SELECT max(p2.price) FROM products p2 WHERE p2.category_id = c.id) AS top_price
         FROM (SELECT order_id, sum(quantity) AS items FROM order_items GROUP BY order_id) t
         JOIN orders o ON o.id = t.order_id
         JOIN order_items oi ON oi.order_id = o.id
         JOIN products p ON p.id = oi.product_id
         JOIN categories c ON c.id = p.category_id
         WHERE o.status = 'paid' AND EXISTS (SELECT 1 FROM users u WHERE u.id = o.user_id AND u.country = 'DE')
         GROUP BY c.name
         ORDER BY items DESC",
    )
    .await;
    let coroutine = find(&plan, "CO-ROUTINE t").actual.expect("the co-routine takes its reader's numbers");
    let orders = number(&conn, "SELECT count(DISTINCT order_id) FROM order_items").await;
    assert_eq!((coroutine.rows, coroutine.loops), (Some(orders), 1), "one row per order, from one run");
    let reader = find(&plan, "SCAN t").actual.unwrap();
    assert_eq!((reader.rows, reader.time), (Some(orders), None), "the co-routine's time is not counted twice");
    let lookup = find(&plan, "SEARCH o USING INTEGER PRIMARY KEY (rowid=?)").actual.unwrap();
    assert_eq!((lookup.rows, lookup.loops as f64), (Some(1.0), orders), "one lookup per order, finding that order");
    let inner = find(&plan, "SCAN order_items USING INDEX sqlite_autoindex_order_items_1").actual.unwrap();
    assert_eq!(inner.rows, Some(number(&conn, "SELECT count(*) FROM order_items").await));
    let returned = plan.node(NodeId::ROOT).actual.unwrap().rows.unwrap();
    assert!(returned > 0.0, "paid orders all belong to German users in the seed");
    let subquery = find(&plan, "CORRELATED SCALAR SUBQUERY 1").actual.unwrap();
    assert_eq!(subquery.loops as f64, returned, "the top price runs once per category returned");

    let plan = check_measured(
        &conn,
        "WITH RECURSIVE tree(id, depth) AS (
             SELECT id, 0 FROM categories WHERE parent_id IS NULL
             UNION ALL
             SELECT c.id, tree.depth + 1 FROM categories c JOIN tree ON c.parent_id = tree.id)
         SELECT tree.depth, count(*) FROM tree JOIN products p ON p.category_id = tree.id GROUP BY tree.depth",
    )
    .await;
    assert_eq!(find(&plan, "SCAN categories").actual.unwrap().rows, Some(10.0), "SETUP reads all 10 categories");
    assert_eq!(find(&plan, "SCAN tree").actual.unwrap().loops, 10, "one recursive step per category in the tree");
    assert!(find(&plan, "CO-ROUTINE tree").actual.is_none(), "read through an automatic index, not a SCAN");
    let index = find(&plan, "CREATE AUTOMATIC INDEX ON tree(id, depth)");
    assert_eq!(index.actual.map(|a| (a.rows, a.loops)), Some((Some(10.0), 1)), "one entry per category");
}

/// Asks the driver to measure `sql` as it is, without the statement check in `plan::request`.
fn measure(sql: &str) -> ExplainStatements {
    ExplainStatements {
        setup: Vec::new(),
        explain: Explain::Measure(sql.into()),
        reset: Vec::new(),
        format: PlanFormat::SqliteScanStatus,
        needs_idle_session: false,
    }
}

async fn checksum(conn: &DynConnection) -> Option<String> {
    rows(conn, "SELECT count(*) || ':' || total(total) || ':' || group_concat(status, '') FROM orders").await.rows[0][0]
        .to_text()
}

#[tokio::test(flavor = "multi_thread")]
async fn never_measures_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = sqlite_fixture(dir.path()).await;
    let conn = driver::connect(&cfg, &Credentials::default()).await.unwrap();
    let before = checksum(&conn).await;
    for (sql, verb) in [
        ("UPDATE orders SET total = 0", "UPDATE"),
        ("DELETE FROM orders", "DELETE"),
        ("WITH t AS (SELECT 1) INSERT INTO orders (user_id, placed_at) SELECT 1, '2024-01-01' FROM t", "INSERT"),
    ] {
        let refused = plan::request(&conn.server_info(), sql, ExplainMode::Analyze).unwrap_err();
        assert!(refused.0.contains(verb), "{sql}: {refused}");
        // The driver refuses on its own too, by SQLite's account of the statement.
        let err = conn.explain(&measure(sql)).await.unwrap_err().to_string();
        assert!(err.contains("would change data"), "{sql}: {err}");
    }
    assert_eq!(checksum(&conn).await, before);
    match conn.query("SELECT count(*) FROM orders", None).await.unwrap() {
        StatementResult::Rows(rs) => assert_eq!(rs.rows[0][0], Value::Int(20_000)),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_interrupts_a_measured_run() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = sqlite_fixture(dir.path()).await;
    let conn = driver::connect(&cfg, &Credentials::default()).await.unwrap();
    let endless = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n";
    let task = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.explain(&measure(endless)).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    conn.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("cancel took effect").unwrap();
    let err = res.unwrap_err().to_string();
    assert!(err.contains("interrupt"), "{err}");
    assert_eq!(rows(&conn, "SELECT 1").await.rows[0][0], Value::Int(1), "the connection still works");
}
