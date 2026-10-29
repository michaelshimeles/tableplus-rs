//! Integration tests against the PostgreSQL container (`docker/compose.yml`).

#![allow(clippy::approx_constant)]

mod common;

use std::time::Duration;

use common::*;
use tableplus_core::config::SslMode;
use tableplus_core::driver;
use tableplus_core::model::{ConstraintKind, ObjectKind};
use tableplus_core::plan::{self, ExplainMode};
use tableplus_core::sqlgen::{self, Filter, FilterOp, SortKey};
use tableplus_core::{Flavor, StatementResult, Value};

/// Paid orders per country: a join, a grouping and a sort.
const JOIN: &str = "SELECT u.country, count(*) FROM orders o JOIN users u ON u.id = o.user_id \
                    WHERE o.status = 'paid' GROUP BY u.country ORDER BY 2 DESC";

#[tokio::test(flavor = "multi_thread")]
async fn metadata() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    assert!(conn.server_version().starts_with("PostgreSQL 17"), "{}", conn.server_version());
    let info = conn.server_info();
    assert_eq!((info.flavor, info.version.major), (Flavor::Postgres, 17), "{info}");
    assert_eq!(conn.database(), "shop");
    assert!(conn.list_databases().await.unwrap().contains(&"shop".to_string()));
    let schemas = conn.list_schemas().await.unwrap();
    assert_eq!(schemas[0], "public");
    assert!(schemas.contains(&"analytics".to_string()));

    let objects = conn.list_objects().await.unwrap();
    let find = |schema: &str, name: &str| objects.iter().find(|o| o.schema == schema && o.name == name).map(|o| o.kind);
    assert_eq!(find("public", "users"), Some(ObjectKind::Table));
    assert_eq!(find("public", "active_users"), Some(ObjectKind::View));
    assert_eq!(find("analytics", "daily_events"), Some(ObjectKind::MaterializedView));
    assert_eq!(find("analytics", "events"), Some(ObjectKind::Table));
    assert_eq!(find("public", "order_total"), Some(ObjectKind::Function));
    let f = objects.iter().find(|o| o.name == "order_total").unwrap();
    assert_eq!(f.signature.as_deref(), Some("p_order_id bigint"));

    let users = conn.table_structure("public", "users").await.unwrap();
    let email = users.column("email").unwrap();
    assert_eq!(email.data_type, "character varying(255)");
    assert!(!email.nullable);
    assert_eq!(users.column("country").unwrap().default.as_deref(), Some("'US'::bpchar"));
    assert_eq!(users.column("metadata").unwrap().comment.as_deref(), Some("Free-form profile attributes"));
    assert_eq!(users.primary_key(), vec!["id"]);
    assert!(users.indexes.iter().any(|i| i.name == "users_created_at_idx"
        && i.columns == ["created_at"]
        && i.method.as_deref() == Some("btree")));
    assert!(users.indexes.iter().any(|i| i.primary));

    let orders = conn.table_structure("public", "orders").await.unwrap();
    let fk = orders.constraints.iter().find(|c| c.kind == ConstraintKind::ForeignKey).unwrap();
    assert_eq!(fk.columns, ["user_id"]);
    assert_eq!(fk.definition, "REFERENCES users(id) ON DELETE CASCADE");
    let idx = orders.indexes.iter().find(|i| i.name == "orders_status_placed_idx").unwrap();
    assert_eq!(idx.columns, ["status", "placed_at"]);

    let products = conn.table_structure("public", "products").await.unwrap();
    assert!(
        products.constraints.iter().any(|c| c.kind == ConstraintKind::Check && c.definition.contains("price >= 0"))
    );
    let items = conn.table_structure("public", "order_items").await.unwrap();
    assert_eq!(items.primary_key(), vec!["order_id", "product_id"]);
    let cats = conn.table_structure("public", "categories").await.unwrap();
    assert_eq!(cats.comment.as_deref(), Some("Product category tree"));

    let view = objects.iter().find(|o| o.name == "active_users").unwrap();
    let ddl = conn.object_definition(view).await.unwrap();
    assert!(ddl.starts_with("CREATE OR REPLACE VIEW \"public\".\"active_users\" AS"), "{ddl}");
    let func = conn.object_definition(f).await.unwrap();
    assert!(func.contains("CREATE OR REPLACE FUNCTION public.order_total"), "{func}");
    let table = objects.iter().find(|o| o.name == "orders" && o.schema == "public").unwrap();
    let ddl = conn.object_definition(table).await.unwrap();
    assert!(
        ddl.contains("CREATE TABLE \"public\".\"orders\"") && ddl.contains("CREATE INDEX orders_user_idx"),
        "{ddl}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_values() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    let rs = rows(&conn, "SELECT * FROM type_zoo ORDER BY id").await;
    let col = |n: &str| rs.column_index(n).unwrap();
    let r = &rs.rows[0];
    assert_eq!(r[col("id")], Value::Int(1));
    assert_eq!(r[col("c_uuid")], Value::text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"));
    assert_eq!(r[col("c_inet")], Value::text("192.168.1.10/24"));
    assert_eq!(r[col("c_interval")], Value::text("1 day 02:00:00"));
    assert_eq!(r[col("c_bytea")], Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef].into()));
    assert_eq!(r[col("c_bool")], Value::Bool(true));
    assert_eq!(r[col("c_double")], Value::Float(2.718281828459045));
    assert_eq!(r[col("c_int_arr")], Value::text("{1,2,3}"));
    assert_eq!(r[col("c_text")], Value::text("héllo wörld ✓"));
    assert!(rs.rows[1][1..].iter().all(Value::is_null));
    assert_eq!(rs.columns[col("c_int_arr")].type_name, "int4[]");
    assert_eq!(rs.columns[col("c_bytea")].type_name, "bytea");

    let rs = rows(&conn, "SELECT status, total FROM orders WHERE id = 1").await;
    assert_eq!(rs.columns[0].type_name, "order_status");
    assert_eq!(rs.rows[0][0], Value::text("paid"));
    assert!(matches!(rs.rows[0][1], Value::Decimal(_)));

    match conn.query("UPDATE orders SET note = note WHERE id <= 10", None).await.unwrap() {
        StatementResult::Affected(n) => assert_eq!(n, 10),
        other => panic!("{other:?}"),
    }
    let err = conn.query("SELECT * FROM no_such_table", None).await.unwrap_err().to_string();
    assert!(err.contains("relation \"no_such_table\" does not exist"), "{err}");
    let err = conn.query("SELEC 1", None).await.unwrap_err().to_string();
    assert!(err.contains("syntax error"), "{err}");
    // The connection keeps working after errors.
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT 41 + 1").await.unwrap().as_deref(), Some("42"));
}

#[tokio::test(flavor = "multi_thread")]
async fn large_table_paging() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    assert_eq!(count(&conn, "analytics", "events", None).await, 2_000_000);
    let cols: Vec<String> =
        conn.table_structure("analytics", "events").await.unwrap().columns.into_iter().map(|c| c.name).collect();
    let sort = [SortKey { column: "id".into(), descending: true }];
    let page = timed("pg 1000-row page at offset 1,500,000", async {
        rows(&conn, &sqlgen::select_page_sql(conn.dialect(), "analytics", "events", None, &sort, Some(1000), 1_500_000))
            .await
    })
    .await;
    assert_eq!(page.rows.len(), 1000);
    assert_eq!(page.rows[0][0], Value::Int(500_000));
    let f =
        Filter { column: Some("event_type".into()), op: FilterOp::Equal, value: "signup".into(), ..Default::default() };
    let w = sqlgen::where_clause(conn.dialect(), &[f], &cols);
    assert_eq!(count(&conn, "analytics", "events", w.as_deref()).await, 333_334);
    // Row limits truncate without breaking the connection.
    let rs = match conn.query("SELECT * FROM analytics.events", Some(50_000)).await.unwrap() {
        StatementResult::Rows(rs) => rs,
        other => panic!("{other:?}"),
    };
    assert!(rs.truncated);
    assert_eq!(rs.rows.len(), 50_000);
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT 1").await.unwrap().as_deref(), Some("1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn editing_round_trip() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    exercise_editing(&conn, "public").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_long_query() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    let c2 = conn.clone();
    let task = tokio::spawn(async move { c2.query("SELECT pg_sleep(30)", None).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    conn.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("cancel took effect").unwrap();
    let err = res.unwrap_err().to_string();
    assert!(err.contains("canceling statement"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_modes() {
    let mut cfg = pg_config();
    cfg.tls.mode = SslMode::Require;
    let Some(conn) = connect_or_skip(&cfg, &password()).await else {
        return;
    };
    let ssl =
        driver::scalar(conn.as_ref(), "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()").await.unwrap();
    assert_eq!(ssl.as_deref(), Some("true"), "require mode negotiates TLS");

    cfg.tls.mode = SslMode::Disable;
    let conn = driver::connect(&cfg, &password()).await.unwrap();
    let ssl =
        driver::scalar(conn.as_ref(), "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()").await.unwrap();
    assert_eq!(ssl.as_deref(), Some("false"));

    // The container uses a self-signed snakeoil certificate, so full verification must fail.
    cfg.tls.mode = SslMode::VerifyFull;
    assert!(driver::connect(&cfg, &password()).await.is_err());

    let mut bad = pg_config();
    bad.user = "nobody".into();
    let err = driver::test_connection(&bad, &password()).await.unwrap_err().to_string();
    assert!(err.contains("password authentication failed"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    timed(
        "pg streaming 250k rows",
        exercise_streaming(&conn, "SELECT * FROM analytics.events WHERE id <= 250000", 250_000),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explain_plans() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    exercise_explain(&conn, JOIN, "UPDATE orders SET total = 0", "SELECT count(*), sum(total) FROM orders").await;
    exercise_explain_one_statement(&conn, "SET application_name = 'tableplus_second'", "SHOW application_name").await;
    exercise_read_only_explain(
        &conn,
        "SELECT pg_sleep(0.5)",
        "UPDATE orders SET note = note WHERE id = 1",
        "SET default_transaction_read_only = on",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explain_analyze_refuses_an_open_transaction() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    exercise_explain_in_transaction(&conn, "BEGIN", "UPDATE orders SET note = note WHERE id = 1").await;
    conn.query("BEGIN", None).await.unwrap();
    assert!(conn.query("SELECT 1 / 0", None).await.is_err());
    let req = plan::request(&conn.server_info(), "SELECT 1", ExplainMode::Analyze).unwrap();
    let err = conn.explain(&req).await.unwrap_err().to_string();
    assert!(err.contains("transaction open"), "an aborted transaction is still open: {err}");
    conn.query("ROLLBACK", None).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn explain_analyze_blocks_writes_through_functions() {
    let Some(conn) = connect_or_skip(&pg_config(), &password()).await else {
        return;
    };
    let last_value = "SELECT last_value, is_called FROM categories_id_seq";
    let before = rows(&conn, last_value).await.rows;
    let req = plan::request(&conn.server_info(), "SELECT nextval('categories_id_seq')", ExplainMode::Analyze).unwrap();
    let err = conn.explain(&req).await.unwrap_err().to_string();
    assert!(err.contains("read-only transaction"), "{err}");
    assert_eq!(rows(&conn, last_value).await.rows, before, "the sequence did not move");
    conn.query("UPDATE orders SET note = note WHERE id = 1", None).await.unwrap();
}
