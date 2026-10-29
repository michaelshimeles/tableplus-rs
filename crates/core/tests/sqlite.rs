//! Integration tests against a seeded SQLite file.

#![allow(clippy::approx_constant)]

mod common;

use common::*;
use tableplus_core::config::Credentials;
use tableplus_core::driver;
use tableplus_core::model::{ConstraintKind, ObjectKind};
use tableplus_core::sqlgen::{self, SortKey};
use tableplus_core::{Flavor, StatementResult, Value};

/// Paid orders per country: a join, a grouping and a sort.
const JOIN: &str = "SELECT u.country, count(*) FROM orders o JOIN users u ON u.id = o.user_id \
                    WHERE o.status = 'paid' GROUP BY u.country ORDER BY 2 DESC";

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = sqlite_fixture(dir.path()).await;
    let conn = driver::connect(&cfg, &Credentials::default()).await.unwrap();
    assert!(conn.server_version().starts_with("SQLite 3."));
    assert_eq!((conn.server_info().flavor, conn.server_info().version.major), (Flavor::Sqlite, 3));
    assert_eq!(conn.database(), "shop.sqlite");
    assert_eq!(conn.list_schemas().await.unwrap(), vec!["main"]);

    let objects = conn.list_objects().await.unwrap();
    let kind = |n: &str| objects.iter().find(|o| o.name == n).map(|o| o.kind);
    assert_eq!(kind("users"), Some(ObjectKind::Table));
    assert_eq!(kind("order_summary"), Some(ObjectKind::View));
    assert_eq!(kind("sqlite_sequence"), None, "internal tables are hidden");

    let users = conn.table_structure("main", "users").await.unwrap();
    assert_eq!(users.primary_key(), vec!["id"]);
    assert_eq!(users.column("id").unwrap().extra.as_deref(), Some("AUTOINCREMENT"));
    assert_eq!(users.column("country").unwrap().default.as_deref(), Some("'US'"));
    assert!(!users.column("email").unwrap().nullable);
    assert!(users.indexes.iter().any(|i| i.name == "users_country_idx" && i.definition.is_some()));
    assert!(users.constraints.iter().any(|c| c.kind == ConstraintKind::Unique && c.columns == ["email"]));

    let items = conn.table_structure("main", "order_items").await.unwrap();
    assert_eq!(items.primary_key(), vec!["order_id", "product_id"]);
    let fks: Vec<_> = items.constraints.iter().filter(|c| c.kind == ConstraintKind::ForeignKey).collect();
    assert_eq!(fks.len(), 2);
    assert!(
        fks.iter()
            .any(|f| f.columns == ["order_id"] && f.definition == "REFERENCES \"orders\" (\"id\") ON DELETE CASCADE")
    );
    assert!(
        items.constraints.iter().any(|c| c.kind == ConstraintKind::Check && c.definition == "CHECK (quantity > 0)")
    );

    let view = objects.iter().find(|o| o.name == "active_users").unwrap();
    assert!(conn.object_definition(view).await.unwrap().starts_with("CREATE VIEW active_users"));

    let rs = rows(&conn, "SELECT * FROM type_zoo ORDER BY id").await;
    assert_eq!(rs.rows[0][1], Value::Int(42));
    assert_eq!(rs.rows[0][2], Value::Float(3.14));
    assert_eq!(rs.rows[0][3], Value::text("héllo wörld ✓"));
    assert_eq!(rs.rows[0][4], Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef].into()));
    assert_eq!(rs.columns[4].type_name, "BLOB");
    assert!(rs.rows[1][1..].iter().all(Value::is_null));

    assert_eq!(count(&conn, "main", "orders", None).await, 20_000);
    let page = rows(
        &conn,
        &sqlgen::select_page_sql(
            conn.dialect(),
            "main",
            "orders",
            None,
            &[SortKey { column: "id".into(), descending: true }],
            Some(100),
            100,
        ),
    )
    .await;
    assert_eq!(page.rows[0][0], Value::Int(19_900));
    match conn.query("SELECT * FROM orders", Some(10)).await.unwrap() {
        StatementResult::Rows(rs) => assert!(rs.truncated && rs.rows.len() == 10),
        other => panic!("{other:?}"),
    }
    let err = conn.query("SELECT * FROM nope", None).await.unwrap_err().to_string();
    assert!(err.contains("no such table"), "{err}");

    timed("sqlite streaming 20k rows", exercise_streaming(&conn, "SELECT * FROM orders", 20_000)).await;
    exercise_editing(&conn, "main").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_explain_plans() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = sqlite_fixture(dir.path()).await;
    let conn = driver::connect(&cfg, &Credentials::default()).await.unwrap();
    exercise_explain(&conn, JOIN, "UPDATE orders SET total = 0", "SELECT count(*), total(total) FROM orders").await;
    exercise_explain_one_statement(&conn, "PRAGMA user_version = 7", "PRAGMA user_version").await;
}
