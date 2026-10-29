//! The database abstraction used by the UI, plus the PostgreSQL, MySQL/MariaDB and SQLite
//! implementations.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, bail};
use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::config::{ConnectionConfig, Credentials, DriverKind};
use crate::dialect::Dialect;
use crate::model::{ColumnMeta, QueryOutput, Row, SchemaObject, StatementResult, TableStructure};
use crate::plan::{Explain, ExplainStatements};
use crate::server::ServerInfo;

/// Receives rows in batches from [`Connection::query_batches`].
pub type BatchSink<'a> = dyn FnMut(&[ColumnMeta], Vec<Row>) -> Result<()> + Send + 'a;

pub mod mysql;
pub mod postgres;
pub mod sqlite;

/// An open connection. All methods take `&self`; implementations serialize access
/// internally so a connection can be shared between UI tasks.
#[async_trait]
pub trait Connection: Send + Sync {
    fn dialect(&self) -> Dialect;
    /// e.g. `PostgreSQL 17.6`.
    fn server_version(&self) -> String;
    /// The engine and version, for behavior that depends on them.
    fn server_info(&self) -> ServerInfo;
    /// The database this connection is bound to (file name for SQLite).
    fn database(&self) -> String;
    /// Schema selected by default in the sidebar.
    fn default_schema(&self) -> String;

    async fn list_databases(&self) -> Result<Vec<String>>;
    async fn list_schemas(&self) -> Result<Vec<String>>;
    async fn list_objects(&self) -> Result<Vec<SchemaObject>>;
    async fn table_structure(&self, schema: &str, table: &str) -> Result<TableStructure>;
    /// DDL for a table, view or routine.
    async fn object_definition(&self, object: &SchemaObject) -> Result<String>;
    /// Executes one statement. Row-returning statements stop reading after `max_rows`.
    async fn query(&self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult>;
    /// Streams a row-returning statement to `sink` in batches of up to `batch` rows without
    /// holding the whole result in memory (used by exports). Returns the number of rows.
    async fn query_batches(&self, sql: &str, batch: usize, sink: &mut BatchSink<'_>) -> Result<u64>;
    /// Runs all statements in one transaction (rolled back on the first error) and returns
    /// the total number of affected rows.
    async fn execute_in_transaction(&self, statements: &[String]) -> Result<u64>;
    /// Runs the request's setup, its plan request, then its reset, which runs even when the plan
    /// fails, without another caller's statements in between.
    async fn explain(&self, req: &ExplainStatements) -> Result<Explained>;
    /// Best-effort cancellation of the statement currently running.
    async fn cancel(&self) -> Result<()>;
    async fn ping(&self) -> Result<()>;
}

pub type DynConnection = Arc<dyn Connection>;

/// Opens a connection (and SSH tunnel, if configured).
pub async fn connect(config: &ConnectionConfig, creds: &Credentials) -> Result<DynConnection> {
    Ok(match config.driver {
        DriverKind::Postgres => Arc::new(postgres::PgConnection::connect(config, creds).await?),
        DriverKind::MySql | DriverKind::MariaDb => Arc::new(mysql::MyConnection::connect(config, creds).await?),
        DriverKind::Sqlite => Arc::new(sqlite::SqliteConnection::open(config).await?),
    })
}

/// Executes `sql` and records how long it took.
pub async fn run(conn: &dyn Connection, sql: &str, max_rows: Option<usize>) -> Result<QueryOutput> {
    let started = Instant::now();
    let result = conn.query(sql, max_rows).await?;
    Ok(QueryOutput { sql: sql.to_string(), result, elapsed: started.elapsed() })
}

/// A plan from [`Connection::explain`]. Its time covers the plan request alone.
#[derive(Debug)]
pub struct Explained {
    pub output: QueryOutput,
    /// Set when a reset statement failed, so the session may keep the setup's settings, such as
    /// read-only transactions.
    pub reset_error: Option<String>,
}

/// Runs one statement on a session the caller holds.
type Query<S> = for<'a> fn(&'a mut S, &'a str) -> BoxFuture<'a, Result<StatementResult>>;

/// Prepares one statement on a session the caller holds without running it.
type Prepare<S> = for<'a> fn(&'a mut S, &'a str) -> BoxFuture<'a, Result<()>>;

/// Whether a session the caller holds has a transaction open.
type InTransaction<S> = for<'a> fn(&'a mut S) -> BoxFuture<'a, Result<bool>>;

/// [`Connection::explain`] for drivers that ask for every plan with an EXPLAIN statement.
async fn explain_with<S: Send>(
    session: &mut S,
    req: &ExplainStatements,
    query: Query<S>,
    prepare: Prepare<S>,
    in_transaction: InTransaction<S>,
) -> Result<Explained> {
    let Explain::Statement(sql) = &req.explain else {
        bail!("this database measures plans through EXPLAIN statements");
    };
    if req.needs_idle_session && in_transaction(session).await? {
        bail!(
            "Explain Analyze runs the statement read-only, and this session has a transaction open whose writes \
             it can't stop. Commit or roll back first."
        );
    }
    let plan = async {
        for setup in &req.setup {
            query(session, setup).await?;
        }
        // A plain query runs every statement in its text; preparing refuses a second one.
        prepare(session, sql).await?;
        let started = Instant::now();
        let result = query(session, sql).await?;
        Ok(QueryOutput { sql: sql.clone(), result, elapsed: started.elapsed() })
    }
    .await;
    let mut failed = Vec::new();
    for reset in &req.reset {
        if let Err(e) = query(session, reset).await {
            failed.push(format!("{reset}: {e:#}"));
        }
    }
    explained(plan, failed)
}

/// The plan's outcome, with the reset statements that failed reported beside it.
fn explained(plan: Result<QueryOutput>, failed_resets: Vec<String>) -> Result<Explained> {
    let reset_error = (!failed_resets.is_empty())
        .then(|| format!("Could not put the session's settings back after the plan.\n{}", failed_resets.join("\n")));
    match (plan, reset_error) {
        (Ok(output), reset_error) => Ok(Explained { output, reset_error }),
        (Err(e), None) => Err(e),
        (Err(e), Some(reset)) => Err(anyhow::anyhow!("{e:#}\n{reset}")),
    }
}

/// Reads a single scalar (first column of first row) as text.
pub async fn scalar(conn: &dyn Connection, sql: &str) -> Result<Option<String>> {
    match conn.query(sql, Some(1)).await? {
        StatementResult::Rows(rs) => Ok(rs.rows.first().and_then(|r| r.first()).and_then(|v| v.to_text())),
        StatementResult::Affected(_) => Ok(None),
    }
}

/// Opens and immediately closes a connection, returning the server version.
pub async fn test_connection(config: &ConnectionConfig, creds: &Credentials) -> Result<String> {
    let conn = connect(config, creds).await?;
    conn.ping().await?;
    Ok(conn.server_version())
}

/// Builds a readable error from driver error chains (`Error: db error: ERROR: …`).
pub(crate) fn describe(err: impl std::fmt::Display, source: Option<&(dyn std::error::Error + 'static)>) -> String {
    match source {
        Some(s) => format!("{err}: {s}"),
        None => err.to_string(),
    }
}
