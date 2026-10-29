//! Shared helpers for the integration tests.
//!
//! The tests talk to the databases from `docker/compose.yml`. When a database is unreachable
//! the test is skipped with a message, unless `TABLEPLUS_REQUIRE_DB=1` is set (as in CI and
//! the verification scripts), in which case it fails.

#![allow(dead_code)]

use std::time::Instant;

use tableplus_core::alter::{ColumnDef, StructureChange, alter_statements, create_table_sql};
use tableplus_core::config::{ConnectionConfig, Credentials, DriverKind};
use tableplus_core::driver::{self, DynConnection};
use tableplus_core::export::{CsvImportOptions, ExportFormat, ExportOptions, csv_to_inserts, export_to_vec};
use tableplus_core::plan::{self, Explain, ExplainMode, Plan};
use tableplus_core::sqlgen::{self, CellEdit, Filter, FilterOp, RowIdentity, SortKey};
use tableplus_core::{Dialect, StatementResult, Value};

pub fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

pub fn pg_config() -> ConnectionConfig {
    let mut c = ConnectionConfig::new(DriverKind::Postgres);
    c.name = "test pg".into();
    c.host = env_or("TP_PG_HOST", "127.0.0.1");
    c.port = env_or("TP_PG_PORT", "54329").parse().unwrap();
    c.user = "tp".into();
    c.database = "shop".into();
    c
}

pub fn mysql_config() -> ConnectionConfig {
    let mut c = ConnectionConfig::new(DriverKind::MySql);
    c.name = "test mysql".into();
    c.host = env_or("TP_MYSQL_HOST", "127.0.0.1");
    c.port = env_or("TP_MYSQL_PORT", "33069").parse().unwrap();
    c.user = "tp".into();
    c.database = "shop".into();
    c
}

pub fn mariadb_config() -> ConnectionConfig {
    let mut c = ConnectionConfig::new(DriverKind::MariaDb);
    c.name = "test mariadb".into();
    c.host = env_or("TP_MARIADB_HOST", "127.0.0.1");
    c.port = env_or("TP_MARIADB_PORT", "33070").parse().unwrap();
    c.user = "tp".into();
    c.database = "shop".into();
    c
}

pub fn password() -> Credentials {
    Credentials { password: Some("tp_pass".into()), ..Default::default() }
}

/// Connects, or returns `None` (skipping the test) when the database isn't running.
pub async fn connect_or_skip(config: &ConnectionConfig, creds: &Credentials) -> Option<DynConnection> {
    match driver::connect(config, creds).await {
        Ok(c) => Some(c),
        Err(e) if std::env::var_os("TABLEPLUS_REQUIRE_DB").is_none() => {
            eprintln!(
                "SKIPPED: cannot connect to {} ({e:#}); run `docker compose -f docker/compose.yml up -d --wait`",
                config.summary()
            );
            None
        }
        Err(e) => panic!("cannot connect to {}: {e:#}", config.summary()),
    }
}

/// Creates the seeded SQLite database in `dir` and returns its config.
pub async fn sqlite_fixture(dir: &std::path::Path) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(DriverKind::Sqlite);
    cfg.name = "test sqlite".into();
    cfg.path = dir.join("shop.sqlite").to_string_lossy().into_owned();
    let script = include_str!("../../../../docker/seed/sqlite.sql");
    let stmts = tableplus_core::export::sql_script_statements(script, Dialect::Sqlite);
    let conn = driver::connect(&cfg, &Credentials::default()).await.unwrap();
    conn.execute_in_transaction(&stmts).await.unwrap();
    cfg
}

pub async fn rows(conn: &DynConnection, sql: &str) -> tableplus_core::ResultSet {
    match conn.query(sql, None).await.unwrap_or_else(|e| panic!("{sql}: {e:#}")) {
        StatementResult::Rows(rs) => rs,
        StatementResult::Affected(n) => panic!("{sql}: expected rows, got {n} affected"),
    }
}

pub async fn count(conn: &DynConnection, schema: &str, table: &str, filter: Option<&str>) -> i64 {
    let sql = sqlgen::count_sql(conn.dialect(), schema, table, filter);
    match &rows(conn, &sql).await.rows[0][0] {
        Value::Int(n) => *n,
        Value::UInt(n) => *n as i64,
        other => other.to_text().unwrap().parse().unwrap(),
    }
}

fn types(d: Dialect) -> [&'static str; 5] {
    match d {
        Dialect::Postgres => ["integer", "text", "numeric(10,2)", "text", "bytea"],
        Dialect::MySql => ["int", "varchar(100)", "decimal(10,2)", "varchar(255)", "blob"],
        Dialect::Sqlite => ["INTEGER", "TEXT", "NUMERIC", "TEXT", "BLOB"],
    }
}

/// End-to-end exercise of everything the grid and structure editor generate: create a
/// scratch table, insert/update/delete through staged-edit SQL, filter/sort/page, alter the
/// structure, and round-trip the data through CSV export/import.
pub async fn exercise_editing(conn: &DynConnection, schema: &str) {
    let d = conn.dialect();
    let table = format!("tp_scratch_{}", std::process::id());
    let t = types(d);
    let defs = vec![
        ColumnDef { name: "id".into(), data_type: t[0].into(), primary_key: true, ..Default::default() },
        ColumnDef { name: "name".into(), data_type: t[1].into(), ..Default::default() },
        ColumnDef { name: "score".into(), data_type: t[2].into(), nullable: true, ..Default::default() },
        ColumnDef { name: "note".into(), data_type: t[3].into(), nullable: true, ..Default::default() },
        ColumnDef { name: "data".into(), data_type: t[4].into(), nullable: true, ..Default::default() },
    ];
    let _ = conn.query(&format!("DROP TABLE IF EXISTS {}", d.qualified(schema, &table)), None).await;
    conn.execute_in_transaction(&[create_table_sql(d, schema, &table, &defs, &[])]).await.unwrap();

    let st = conn.table_structure(schema, &table).await.unwrap();
    assert_eq!(st.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "score", "note", "data"]);
    assert_eq!(st.primary_key(), vec!["id"]);
    assert!(!st.column("name").unwrap().nullable);
    assert!(st.column("note").unwrap().nullable);

    // Staged inserts.
    let inserts: Vec<String> = [
        vec![
            ("id", CellEdit::Text("1".into())),
            ("name", CellEdit::Text("O'Hara \\ \"quoted\"".into())),
            ("score", CellEdit::Text("10.5".into())),
            ("data", CellEdit::Text("0xDEADBEEF".into())),
        ],
        vec![
            ("id", CellEdit::Text("2".into())),
            ("name", CellEdit::Text("Ünïcödé ✓".into())),
            ("note", CellEdit::Null),
        ],
        vec![
            ("id", CellEdit::Text("3".into())),
            ("name", CellEdit::Text("third".into())),
            ("score", CellEdit::Text("-1".into())),
            ("note", CellEdit::Text("line1\nline2".into())),
        ],
    ]
    .into_iter()
    .map(|vals| {
        let vals: Vec<(String, CellEdit)> = vals.into_iter().map(|(c, e)| (c.to_string(), e)).collect();
        sqlgen::insert_sql(d, schema, &table, &st.columns, &vals)
    })
    .collect();
    assert_eq!(conn.execute_in_transaction(&inserts).await.unwrap(), 3);

    let all = rows(
        conn,
        &sqlgen::select_page_sql(
            d,
            schema,
            &table,
            None,
            &[SortKey { column: "id".into(), descending: false }],
            Some(100),
            0,
        ),
    )
    .await;
    assert_eq!(all.rows.len(), 3);
    assert_eq!(all.rows[0][1], Value::text("O'Hara \\ \"quoted\""));
    assert_eq!(all.rows[0][4], Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF].into()));
    assert_eq!(all.rows[1][1], Value::text("Ünïcödé ✓"));
    assert!(all.rows[1][3].is_null());
    assert_eq!(all.rows[2][3], Value::text("line1\nline2"));
    assert!(all.rows[0][2].is_numeric(), "score should be numeric: {:?}", all.rows[0][2]);

    // Filter + sort + paging.
    let names: Vec<String> = st.columns.iter().map(|c| c.name.clone()).collect();
    let f = Filter { column: Some("name".into()), op: FilterOp::Contains, value: "IR".into(), ..Default::default() };
    let w = sqlgen::where_clause(d, &[f], &names);
    let filtered = rows(conn, &sqlgen::select_page_sql(d, schema, &table, w.as_deref(), &[], Some(10), 0)).await;
    assert_eq!(filtered.rows.len(), 1, "case-insensitive contains");
    assert_eq!(filtered.rows[0][0], Value::Int(3));
    let any = Filter { column: None, op: FilterOp::Contains, value: "hara".into(), ..Default::default() };
    let w = sqlgen::where_clause(d, &[any], &names);
    assert_eq!(count(conn, schema, &table, w.as_deref()).await, 1);
    let desc = rows(
        conn,
        &sqlgen::select_page_sql(
            d,
            schema,
            &table,
            None,
            &[SortKey { column: "id".into(), descending: true }],
            Some(1),
            1,
        ),
    )
    .await;
    assert_eq!(desc.rows[0][0], Value::Int(2));

    // Update + delete through row identities.
    let id_row2 = RowIdentity::new(&st.columns, &all.rows[1]);
    let upd = sqlgen::update_sql(
        d,
        schema,
        &table,
        &st.columns,
        &id_row2,
        &[("note".into(), CellEdit::Text("edited".into())), ("score".into(), CellEdit::Null)],
    );
    let del = sqlgen::delete_sql(d, schema, &table, &RowIdentity::new(&st.columns, &all.rows[2]));
    assert_eq!(conn.execute_in_transaction(&[upd, del]).await.unwrap(), 2);
    let after = rows(
        conn,
        &sqlgen::select_page_sql(
            d,
            schema,
            &table,
            None,
            &[SortKey { column: "id".into(), descending: false }],
            None,
            0,
        ),
    )
    .await;
    assert_eq!(after.rows.len(), 2);
    assert_eq!(after.rows[1][3], Value::text("edited"));

    // A failing batch rolls back completely.
    let ok = sqlgen::update_sql(
        d,
        schema,
        &table,
        &st.columns,
        &RowIdentity::new(&st.columns, &after.rows[0]),
        &[("name".into(), CellEdit::Text("should roll back".into()))],
    );
    let bad = format!("INSERT INTO {} (id, name) VALUES (1, 'duplicate pk')", d.qualified(schema, &table));
    assert!(conn.execute_in_transaction(&[ok, bad]).await.is_err());
    let check = rows(
        conn,
        &sqlgen::select_page_sql(
            d,
            schema,
            &table,
            None,
            &[SortKey { column: "id".into(), descending: false }],
            None,
            0,
        ),
    )
    .await;
    assert_eq!(check.rows[0][1], Value::text("O'Hara \\ \"quoted\""));

    // Rows without a primary key are matched on every column.
    let nopk = format!("{table}_nopk");
    let _ = conn.query(&format!("DROP TABLE IF EXISTS {}", d.qualified(schema, &nopk)), None).await;
    let nopk_defs: Vec<ColumnDef> =
        defs.iter().take(3).map(|c| ColumnDef { primary_key: false, nullable: true, ..c.clone() }).collect();
    conn.execute_in_transaction(&[
        create_table_sql(d, schema, &nopk, &nopk_defs, &[]),
        format!("INSERT INTO {} VALUES (1, 'dup', NULL), (1, 'dup', NULL), (2, 'x', 1.25)", d.qualified(schema, &nopk)),
    ])
    .await
    .unwrap();
    let nst = conn.table_structure(schema, &nopk).await.unwrap();
    let nrows = rows(conn, &sqlgen::select_page_sql(d, schema, &nopk, None, &[], None, 0)).await;
    let dup = nrows.rows.iter().find(|r| r[1] == Value::text("dup")).unwrap();
    let del = sqlgen::delete_sql(d, schema, &nopk, &RowIdentity::new(&nst.columns, dup));
    assert_eq!(conn.execute_in_transaction(&[del]).await.unwrap(), 1, "only one of two identical rows is deleted");
    let x = nrows.rows.iter().find(|r| r[1] == Value::text("x")).unwrap();
    let upd = sqlgen::update_sql(
        d,
        schema,
        &nopk,
        &nst.columns,
        &RowIdentity::new(&nst.columns, x),
        &[("name".into(), CellEdit::Text("y".into()))],
    );
    assert_eq!(conn.execute_in_transaction(&[upd]).await.unwrap(), 1, "match on decimal value");
    assert_eq!(count(conn, schema, &nopk, None).await, 2);
    conn.execute_in_transaction(&[format!("DROP TABLE {}", d.qualified(schema, &nopk))]).await.unwrap();

    // Structure editing.
    let mut renamed = ColumnDef::from(st.column("note").unwrap());
    renamed.name = "remark".into();
    let changes = vec![
        StructureChange::AddColumn(ColumnDef {
            name: "age".into(),
            data_type: if d == Dialect::Sqlite { "INTEGER".into() } else { "integer".into() },
            nullable: true,
            default: Some("18".into()),
            ..Default::default()
        }),
        StructureChange::ModifyColumn { original: "note".into(), new: renamed },
        StructureChange::AddIndex { name: format!("{table}_name_idx"), columns: vec!["name".into()], unique: false },
    ];
    let ddl = alter_statements(d, schema, &table, &st, &changes).unwrap();
    for stmt in &ddl {
        conn.query(stmt, None).await.unwrap_or_else(|e| panic!("{stmt}: {e:#}"));
    }
    let st2 = conn.table_structure(schema, &table).await.unwrap();
    assert!(st2.column("remark").is_some(), "{ddl:#?}");
    assert!(st2.column("note").is_none());
    let age = st2.column("age").unwrap();
    assert_eq!(age.default.as_deref().map(|d| d.trim_matches(|c| c == '\'' || c == '(' || c == ')')), Some("18"));
    assert!(st2.indexes.iter().any(|i| i.name == format!("{table}_name_idx") && i.columns == ["name"]));
    let edited = rows(
        conn,
        &sqlgen::select_page_sql(
            d,
            schema,
            &table,
            None,
            &[SortKey { column: "id".into(), descending: false }],
            None,
            0,
        ),
    )
    .await;
    assert_eq!(edited.rows[1][edited.column_index("remark").unwrap()], Value::text("edited"));

    // Make "remark" NOT NULL with a default (SQLite rebuilds the table for this).
    let mut nn = ColumnDef::from(st2.column("remark").unwrap());
    nn.nullable = false;
    nn.default = Some("'n/a'".into());
    conn.query(&format!("UPDATE {} SET remark = 'n/a' WHERE remark IS NULL", d.qualified(schema, &table)), None)
        .await
        .unwrap();
    let ddl = alter_statements(
        d,
        schema,
        &table,
        &st2,
        &[StructureChange::ModifyColumn { original: "remark".into(), new: nn }],
    )
    .unwrap();
    conn.execute_in_transaction(&ddl).await.unwrap_or_else(|e| panic!("{ddl:#?}: {e:#}"));
    let st3 = conn.table_structure(schema, &table).await.unwrap();
    assert!(!st3.column("remark").unwrap().nullable, "{ddl:#?}");
    assert_eq!(st3.primary_key(), vec!["id"]);
    assert!(st3.indexes.iter().any(|i| i.name == format!("{table}_name_idx")), "indexes survive: {ddl:#?}");
    assert_eq!(count(conn, schema, &table, None).await, 2);

    // CSV export → delete → import round trip.
    let data = rows(
        conn,
        &sqlgen::select_page_sql(
            d,
            schema,
            &table,
            None,
            &[SortKey { column: "id".into(), descending: false }],
            None,
            0,
        ),
    )
    .await;
    let cols: Vec<String> = data.columns.iter().map(|c| c.name.clone()).collect();
    let csv =
        export_to_vec(ExportOptions { format: ExportFormat::Csv, ..Default::default() }, &cols, &data.rows).unwrap();
    conn.query(&format!("DELETE FROM {}", d.qualified(schema, &table)), None).await.unwrap();
    let (stmts, n) =
        csv_to_inserts(d, schema, &table, &st3.columns, csv.as_slice(), &CsvImportOptions::default()).unwrap();
    assert_eq!(n, 2);
    conn.execute_in_transaction(&stmts).await.unwrap();
    let back = rows(
        conn,
        &sqlgen::select_page_sql(
            d,
            schema,
            &table,
            None,
            &[SortKey { column: "id".into(), descending: false }],
            None,
            0,
        ),
    )
    .await;
    assert_eq!(back.rows, data.rows, "CSV round trip preserves every value");

    // SQL INSERT export executes cleanly.
    let sql = export_to_vec(
        ExportOptions {
            format: ExportFormat::SqlInsert,
            dialect: d,
            table: d.qualified(schema, &table),
            ..Default::default()
        },
        &cols,
        &data.rows,
    )
    .unwrap();
    conn.query(&format!("DELETE FROM {}", d.qualified(schema, &table)), None).await.unwrap();
    for stmt in tableplus_core::export::sql_script_statements(std::str::from_utf8(&sql).unwrap(), d) {
        conn.query(&stmt, None).await.unwrap();
    }
    assert_eq!(count(conn, schema, &table, None).await, 2);

    conn.execute_in_transaction(&[format!("DROP TABLE {}", d.qualified(schema, &table))]).await.unwrap();
}

/// Streams `sql` through `query_batches`, checking batch sizes and the total row count.
pub async fn exercise_streaming(conn: &DynConnection, sql: &str, expected: u64) {
    let mut batches = 0u64;
    let mut seen = 0u64;
    let mut max_batch = 0;
    let total = conn
        .query_batches(sql, 1000, &mut |cols, rows| {
            assert!(!cols.is_empty());
            batches += 1;
            seen += rows.len() as u64;
            max_batch = max_batch.max(rows.len());
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(total, expected);
    assert_eq!(seen, expected);
    assert!(max_batch <= 1000);
    assert!(batches >= expected / 1000);
    // A failing sink aborts the stream and leaves the connection usable.
    let err = conn.query_batches(sql, 10, &mut |_, _| anyhow::bail!("stop")).await.unwrap_err();
    assert_eq!(err.to_string(), "stop");
    assert!(conn.query("SELECT 1", None).await.is_ok());
}

/// Explains `sql` as the query tab does and parses the plan.
pub async fn explain(conn: &DynConnection, sql: &str, mode: ExplainMode) -> Plan {
    let req = plan::request(&conn.server_info(), sql, mode).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let explained = conn.explain(&req).await.unwrap_or_else(|e| panic!("{sql}: {e:#}"));
    assert_eq!(explained.reset_error, None, "{sql}");
    let rs = explained.output.rows().unwrap_or_else(|| panic!("{sql}: the plan has rows"));
    let plan = plan::parse(&conn.server_info(), rs).expect("a plan").unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(plan.format, req.format, "{sql}");
    plan
}

/// Explain and Explain Analyze of a `query` that joins tables. `write` would change the rows that
/// `checksum` sums, so Explain Analyze refuses it and only its estimate runs.
pub async fn exercise_explain(conn: &DynConnection, query: &str, write: &str, checksum: &str) {
    let estimate = explain(conn, query, ExplainMode::Estimate).await;
    assert!(!estimate.analyzed() && estimate.node_count() > 2, "{estimate:#?}");
    let analyzed = explain(conn, query, ExplainMode::Analyze).await;
    assert!(analyzed.analyzed() && analyzed.total_ms().is_some(), "{analyzed:#?}");
    assert!(analyzed.node_count() > 2);

    let before = rows(conn, checksum).await.rows;
    let refused = plan::request(&conn.server_info(), write, ExplainMode::Analyze).unwrap_err();
    assert!(refused.0.contains("UPDATE"), "{refused}");
    assert!(!explain(conn, write, ExplainMode::Estimate).await.analyzed());
    assert_eq!(rows(conn, checksum).await.rows, before, "the write never ran");

    let mut req = plan::request(&conn.server_info(), query, ExplainMode::Estimate).unwrap();
    req.reset.push("SELECT * FROM tableplus_no_such_table".into());
    let explained = conn.explain(&req).await.unwrap();
    let reset = explained.reset_error.expect("the failed reset is reported");
    assert!(reset.contains("tableplus_no_such_table"), "{reset}");
    req.explain =
        plan::request(&conn.server_info(), "SELECT * FROM tableplus_missing", ExplainMode::Estimate).unwrap().explain;
    let err = format!("{:#}", conn.explain(&req).await.unwrap_err());
    assert!(err.contains("tableplus_missing") && err.contains("tableplus_no_such_table"), "{err}");
}

/// The server refuses a plan request whose text holds a second statement, even one the statement
/// check never saw. `second` changes session state that `probe` reads.
pub async fn exercise_explain_one_statement(conn: &DynConnection, second: &str, probe: &str) {
    let before = rows(conn, probe).await.rows;
    let mut req = plan::request(&conn.server_info(), "SELECT 1", ExplainMode::Estimate).unwrap();
    req.explain = Explain::Statement(format!("{}; {second}", req.explain.sql()));
    assert!(conn.explain(&req).await.is_err(), "the server refuses a second statement");
    assert_eq!(rows(conn, probe).await.rows, before, "the second statement never ran");
}

/// Explain Analyze runs in a read-only session and leaves the session as it found it, before any
/// other statement on the connection runs. `slow` takes half a second, `touch` changes no data
/// but needs a writable session, and `set_read_only` makes the session read-only.
pub async fn exercise_read_only_explain(conn: &DynConnection, slow: &str, touch: &str, set_read_only: &str) {
    let started = Instant::now();
    let task = {
        let conn = conn.clone();
        let slow = slow.to_string();
        tokio::spawn(async move { explain(&conn, &slow, ExplainMode::Analyze).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    conn.query(touch, None).await.unwrap_or_else(|e| panic!("{touch} runs after the reset: {e:#}"));
    assert!(started.elapsed().as_millis() >= 450, "{touch} waited for the Explain Analyze");
    task.await.unwrap();
    conn.query(touch, None).await.unwrap();

    conn.query(set_read_only, None).await.unwrap();
    explain(conn, "SELECT 1", ExplainMode::Analyze).await;
    let err = conn.query(touch, None).await.unwrap_err().to_string();
    assert!(err.to_ascii_lowercase().replace('-', " ").contains("read only"), "{err}");
}

/// Explain Analyze refuses a session that has its own transaction open, because its read-only
/// transaction can't start inside the user's, and leaves the user's transaction alone. `begin`
/// opens a transaction and `touch` needs a writable one.
pub async fn exercise_explain_in_transaction(conn: &DynConnection, begin: &str, touch: &str) {
    conn.query(begin, None).await.unwrap();
    let req = plan::request(&conn.server_info(), "SELECT 1", ExplainMode::Analyze).unwrap();
    let err = conn.explain(&req).await.unwrap_err().to_string();
    assert!(err.contains("transaction open"), "{err}");
    conn.query(touch, None).await.unwrap_or_else(|e| panic!("the user's transaction stays writable: {e:#}"));
    conn.query("ROLLBACK", None).await.unwrap();
    explain(conn, "SELECT 1", ExplainMode::Analyze).await;
}

/// Times a closure for perf assertions/logging.
pub async fn timed<T>(label: &str, f: impl std::future::Future<Output = T>) -> T {
    let start = Instant::now();
    let out = f.await;
    eprintln!("{label}: {:?}", start.elapsed());
    out
}
