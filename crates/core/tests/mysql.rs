//! Integration tests against the MySQL 8.4 and MariaDB 11 containers.

#![allow(clippy::approx_constant)]

mod common;

use std::time::Duration;

use common::*;
use tableplus_core::config::SslMode;
use tableplus_core::driver::{self, DynConnection};
use tableplus_core::model::{ConstraintKind, ObjectKind};
use tableplus_core::plan::{self, ExplainMode};
use tableplus_core::sqlgen::{self, Filter, FilterOp, SortKey};
use tableplus_core::{Flavor, StatementResult, Value};

/// Paid orders per country: a join, a grouping and a sort.
const JOIN: &str = "SELECT u.country, count(*) FROM orders o JOIN users u ON u.id = o.user_id \
                    WHERE o.status = 'paid' GROUP BY u.country ORDER BY 2 DESC";

#[tokio::test(flavor = "multi_thread")]
async fn mysql_metadata() {
    let Some(conn) = connect_or_skip(&mysql_config(), &password()).await else {
        return;
    };
    assert!(conn.server_version().starts_with("MySQL 8.4"), "{}", conn.server_version());
    let info = conn.server_info();
    assert_eq!((info.flavor, info.version.major, info.version.minor), (Flavor::MySql, 8, 4), "{info}");
    assert_eq!(conn.default_schema(), "shop");
    let schemas = conn.list_schemas().await.unwrap();
    assert_eq!(schemas, vec!["analytics", "shop"]);
    let objects = conn.list_objects().await.unwrap();
    let find = |schema: &str, name: &str| objects.iter().find(|o| o.schema == schema && o.name == name).map(|o| o.kind);
    assert_eq!(find("shop", "users"), Some(ObjectKind::Table));
    assert_eq!(find("shop", "active_users"), Some(ObjectKind::View));
    assert_eq!(find("shop", "order_total"), Some(ObjectKind::Function));
    assert_eq!(find("shop", "recent_orders"), Some(ObjectKind::Procedure));
    assert_eq!(find("analytics", "events"), Some(ObjectKind::Table));

    let users = conn.table_structure("shop", "users").await.unwrap();
    assert_eq!(users.column("email").unwrap().data_type, "varchar(255)");
    assert_eq!(users.column("country").unwrap().default.as_deref(), Some("'US'"));
    assert_eq!(users.column("is_active").unwrap().default.as_deref(), Some("1"));
    assert_eq!(users.column("created_at").unwrap().default.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(users.column("id").unwrap().extra.as_deref(), Some("auto_increment"));
    assert_eq!(users.column("metadata").unwrap().comment.as_deref(), Some("Free-form profile attributes"));
    assert_eq!(users.primary_key(), vec!["id"]);
    assert!(users.indexes.iter().any(|i| i.name == "users_country_idx" && !i.unique));
    assert!(users.indexes.iter().any(|i| i.name == "email" && i.unique));

    let items = conn.table_structure("shop", "order_items").await.unwrap();
    assert_eq!(items.primary_key(), vec!["order_id", "product_id"]);
    let fk = items.constraints.iter().find(|c| c.name == "fk_items_order").unwrap();
    assert_eq!(fk.kind, ConstraintKind::ForeignKey);
    assert_eq!(fk.definition, "REFERENCES `shop`.`orders` (`id`) ON DELETE CASCADE");
    let chk = items.constraints.iter().find(|c| c.name == "chk_quantity").unwrap();
    assert!(chk.definition.contains("quantity"), "{}", chk.definition);
    let cats = conn.table_structure("shop", "categories").await.unwrap();
    assert_eq!(cats.comment.as_deref(), Some("Product category tree"));

    let proc = objects.iter().find(|o| o.name == "recent_orders").unwrap();
    assert!(conn.object_definition(proc).await.unwrap().contains("PROCEDURE `recent_orders`"));
    let table = objects.iter().find(|o| o.name == "orders").unwrap();
    assert!(conn.object_definition(table).await.unwrap().starts_with("CREATE TABLE `orders`"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_typed_values() {
    let Some(conn) = connect_or_skip(&mysql_config(), &password()).await else {
        return;
    };
    let rs = rows(&conn, "SELECT * FROM type_zoo ORDER BY id").await;
    let col = |n: &str| rs.column_index(n).unwrap();
    let r = &rs.rows[0];
    assert_eq!(r[col("id")], Value::Int(1));
    assert_eq!(r[col("c_date")], Value::text("2024-02-29"));
    assert_eq!(r[col("c_time")], Value::text("13:37:00"));
    assert_eq!(r[col("c_blob")], Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef].into()));
    assert_eq!(r[col("c_double")], Value::Float(2.718281828459045));
    assert_eq!(r[col("c_bit")], Value::UInt(0b1010_0101));
    assert_eq!(r[col("c_set")], Value::text("a,c"));
    assert_eq!(r[col("c_text")], Value::text("héllo wörld ✓"));
    assert!(matches!(&r[col("c_json")], Value::Text(t) if t.contains("[1, 2, 3]")));
    assert!(rs.rows[1][1..].iter().all(Value::is_null));
    assert_eq!(rs.columns[col("c_blob")].type_name, "BLOB");
    assert_eq!(rs.columns[col("c_text")].type_name, "TEXT");

    let rs = rows(&conn, "SELECT status, total, CAST(1 AS UNSIGNED) u FROM orders WHERE id = 1").await;
    assert_eq!(rs.columns[0].type_name, "ENUM");
    assert_eq!(rs.rows[0][0], Value::text("paid"));
    assert!(matches!(rs.rows[0][1], Value::Decimal(_)));
    assert_eq!(rs.rows[0][2], Value::UInt(1));

    match conn.query("UPDATE orders SET note = CONCAT(COALESCE(note, ''), '') WHERE id <= 10", None).await.unwrap() {
        StatementResult::Affected(_) => {}
        other => panic!("{other:?}"),
    }
    let err = conn.query("SELECT * FROM no_such_table", None).await.unwrap_err().to_string();
    assert!(err.contains("1146") && err.contains("doesn't exist"), "{err}");
    // CALL returns a result set followed by an OK packet.
    let rs = rows(&conn, "CALL recent_orders(3)").await;
    assert_eq!(rs.rows.len(), 3);
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT 41 + 1").await.unwrap().as_deref(), Some("42"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_large_table_paging() {
    let Some(conn) = connect_or_skip(&mysql_config(), &password()).await else {
        return;
    };
    assert_eq!(count(&conn, "analytics", "events", None).await, 1_000_000);
    let sort = [SortKey { column: "id".into(), descending: true }];
    let page = timed("mysql 1000-row page at offset 900,000", async {
        rows(&conn, &sqlgen::select_page_sql(conn.dialect(), "analytics", "events", None, &sort, Some(1000), 900_000))
            .await
    })
    .await;
    assert_eq!(page.rows.len(), 1000);
    assert_eq!(page.rows[0][0], Value::Int(100_000));
    let cols: Vec<String> = page.columns.iter().map(|c| c.name.clone()).collect();
    let f =
        Filter { column: Some("event_type".into()), op: FilterOp::Equal, value: "signup".into(), ..Default::default() };
    let w = sqlgen::where_clause(conn.dialect(), &[f], &cols);
    assert_eq!(count(&conn, "analytics", "events", w.as_deref()).await, 166_667);
    let rs = match conn.query("SELECT * FROM analytics.events", Some(10_000)).await.unwrap() {
        StatementResult::Rows(rs) => rs,
        other => panic!("{other:?}"),
    };
    assert!(rs.truncated);
    assert_eq!(rs.rows.len(), 10_000);
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT 1").await.unwrap().as_deref(), Some("1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_editing_round_trip() {
    let Some(conn) = connect_or_skip(&mysql_config(), &password()).await else {
        return;
    };
    exercise_editing(&conn, "shop").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_cancel_and_tls() {
    let mut cfg = mysql_config();
    cfg.tls.mode = SslMode::Require;
    let Some(conn) = connect_or_skip(&cfg, &password()).await else {
        return;
    };
    let rs = rows(&conn, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await;
    assert!(!rs.rows[0][1].to_text().unwrap().is_empty(), "TLS cipher negotiated");

    let c2 = conn.clone();
    let task = tokio::spawn(async move { c2.query("SELECT SLEEP(30)", None).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    conn.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("cancel took effect").unwrap();
    // KILL QUERY makes SLEEP() return 1 early (or raise "interrupted").
    match res {
        Ok(StatementResult::Rows(rs)) => assert_eq!(rs.rows[0][0], Value::Int(1)),
        Ok(other) => panic!("{other:?}"),
        Err(e) => assert!(e.to_string().contains("interrupted"), "{e}"),
    }
    cfg.tls.mode = SslMode::VerifyFull;
    assert!(driver::connect(&cfg, &password()).await.is_err(), "self-signed server cert must not verify");
}

#[tokio::test(flavor = "multi_thread")]
async fn mariadb_metadata_and_editing() {
    let Some(conn) = connect_or_skip(&mariadb_config(), &password()).await else {
        return;
    };
    assert!(conn.server_version().starts_with("MariaDB 11"), "{}", conn.server_version());
    let info = conn.server_info();
    assert_eq!((info.flavor, info.version.major), (Flavor::MariaDb, 11), "{info}");
    let objects = conn.list_objects().await.unwrap();
    assert!(objects.iter().any(|o| o.name == "vip_customers" && o.kind == ObjectKind::View));
    let customers = conn.table_structure("shop", "customers").await.unwrap();
    assert_eq!(customers.column("vip").unwrap().default.as_deref(), Some("0"));
    assert_eq!(customers.column("created_at").unwrap().default.as_deref(), Some("current_timestamp()"));
    assert!(customers.column("name").unwrap().default.is_none());
    let invoices = conn.table_structure("shop", "invoices").await.unwrap();
    assert!(
        invoices
            .constraints
            .iter()
            .any(|c| c.kind == ConstraintKind::ForeignKey && c.definition.contains("`customers`"))
    );
    assert_eq!(count(&conn, "shop", "invoices", None).await, 20_000);
    exercise_editing(&conn, "shop").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_streaming() {
    let Some(conn) = connect_or_skip(&mysql_config(), &password()).await else {
        return;
    };
    timed(
        "mysql streaming 250k rows",
        exercise_streaming(&conn, "SELECT * FROM analytics.events WHERE id <= 250000", 250_000),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_explain_plans() {
    let Some(conn) = connect_or_skip(&mysql_config(), &password()).await else {
        return;
    };
    exercise_explain(&conn, JOIN, "UPDATE orders SET total = 0", "SELECT COUNT(*), SUM(total) FROM orders").await;
    exercise_explain_one_statement(&conn, "SET @tableplus_second = 1", "SELECT @tableplus_second").await;
    conn.query("SET explain_json_format_version = 1", None).await.unwrap();
    explain(&conn, JOIN, ExplainMode::Analyze).await;
    let version = driver::scalar(conn.as_ref(), "SELECT @@SESSION.explain_json_format_version").await.unwrap();
    assert_eq!(version.as_deref(), Some("1"), "the session's own JSON format is put back");
    exercise_read_only_explain(
        &conn,
        "SELECT SLEEP(0.5)",
        "UPDATE orders SET note = note WHERE id = 1",
        "SET SESSION transaction_read_only = 1",
    )
    .await;
}

/// With autocommit off, Explain Analyze opens no transaction that outlives it, and refuses once a
/// statement has opened one. Neither `completion_type` nor `LOCK TABLES` changes what the session
/// is left with. `touch` writes to `table`, and `other` is a second table.
async fn exercise_explain_transactions(conn: &DynConnection, touch: &str, table: &str, other: &str) {
    exercise_explain_in_transaction(conn, "START TRANSACTION", touch).await;
    let autocommit = async || driver::scalar(conn.as_ref(), "SELECT @@SESSION.autocommit").await.unwrap();
    explain(conn, "SELECT 1", ExplainMode::Analyze).await;
    assert_eq!(autocommit().await.as_deref(), Some("1"));
    conn.query("SET autocommit = 0", None).await.unwrap();
    explain(conn, "SELECT 1", ExplainMode::Analyze).await;
    assert_eq!(autocommit().await.as_deref(), Some("0"));
    conn.query(touch, None).await.unwrap_or_else(|e| panic!("no read-only transaction is left open: {e:#}"));
    exercise_explain_in_transaction(conn, "SELECT 1", touch).await;
    conn.query("SET autocommit = 1", None).await.unwrap();

    let sql_mode = driver::scalar(conn.as_ref(), "SELECT @@SESSION.sql_mode").await.unwrap().unwrap_or_default();
    conn.query("SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'", None).await.unwrap();
    for mode in [ExplainMode::Estimate, ExplainMode::Analyze] {
        explain(conn, r"SELECT '\' AS slash, '?:named' AS txt", mode).await;
    }
    conn.query(&format!("SET SESSION sql_mode = '{sql_mode}'"), None).await.unwrap();

    for completion in ["1", "2"] {
        conn.query(&format!("SET completion_type = {completion}"), None).await.unwrap();
        explain(conn, "SELECT 1", ExplainMode::Analyze).await;
        conn.query(touch, None).await.unwrap_or_else(|e| panic!("completion_type {completion}: {e:#}"));
        conn.query("SET completion_type = 0", None).await.unwrap();
    }

    conn.query(&format!("LOCK TABLES {table} READ"), None).await.unwrap();
    explain(conn, &format!("SELECT COUNT(*) FROM {table}"), ExplainMode::Analyze).await;
    let err = conn.query(&format!("SELECT 1 FROM {other} LIMIT 1"), None).await.unwrap_err().to_string();
    assert!(err.contains("was not locked"), "the session still holds its table locks: {err}");
    conn.query("UNLOCK TABLES", None).await.unwrap();
}

/// A statement over half of `max_allowed_packet` explains, and the connection stays open.
async fn exercise_explain_large_statement(conn: &DynConnection) {
    let packet = driver::scalar(conn.as_ref(), "SELECT @@max_allowed_packet").await.unwrap().unwrap();
    let packet = packet.parse::<usize>().unwrap();
    let select = |length: usize| format!("SELECT LENGTH('{}')", "a".repeat(length));
    explain(conn, &select(packet * 6 / 10), ExplainMode::Estimate).await;
    assert!(conn.query("SELECT 1", None).await.is_ok(), "the connection stays open");

    // A plan request one byte short of the limit leaves no room for the query packet's command byte.
    let server = conn.server_info();
    let prefix = plan::request(&server, &select(0), ExplainMode::Estimate).unwrap().explain.sql().len();
    let req = plan::request(&server, &select(packet - 1 - prefix), ExplainMode::Estimate).unwrap();
    assert_eq!(req.explain.sql().len(), packet - 1);
    let err = conn.explain(&req).await.unwrap_err().to_string();
    assert!(err.contains("max_allowed_packet"), "{err}");
    assert!(conn.query("SELECT 1", None).await.is_ok(), "the connection stays open");
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_explain_large_statement() {
    let Some(conn) = connect_or_skip(&mysql_config(), &password()).await else {
        return;
    };
    exercise_explain_large_statement(&conn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mariadb_explain_large_statement() {
    let Some(conn) = connect_or_skip(&mariadb_config(), &password()).await else {
        return;
    };
    exercise_explain_large_statement(&conn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_explain_transactions() {
    let Some(conn) = connect_or_skip(&mysql_config(), &password()).await else {
        return;
    };
    exercise_explain_transactions(&conn, "UPDATE orders SET note = note WHERE id = 1", "orders", "users").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mariadb_explain_transactions() {
    let Some(conn) = connect_or_skip(&mariadb_config(), &password()).await else {
        return;
    };
    exercise_explain_transactions(&conn, "UPDATE invoices SET paid = paid WHERE id = 1", "invoices", "customers").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mariadb_explain_plans() {
    let Some(conn) = connect_or_skip(&mariadb_config(), &password()).await else {
        return;
    };
    exercise_explain(
        &conn,
        "SELECT c.vip, COUNT(*), SUM(i.amount) FROM invoices i JOIN customers c ON c.id = i.customer_id \
         WHERE i.paid GROUP BY c.vip ORDER BY 3 DESC",
        "UPDATE invoices SET amount = 0",
        "SELECT COUNT(*), SUM(amount) FROM invoices",
    )
    .await;
    exercise_explain_one_statement(&conn, "SET @tableplus_second = 1", "SELECT @tableplus_second").await;
    exercise_read_only_explain(
        &conn,
        "SELECT SLEEP(0.5)",
        "UPDATE invoices SET paid = paid WHERE id = 1",
        "SET SESSION tx_read_only = 1",
    )
    .await;
}
