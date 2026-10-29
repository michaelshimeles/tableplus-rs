//! PostgreSQL via `tokio-postgres`.
//!
//! Queries run over the simple-query protocol so every type (enums, arrays, ranges, intervals…)
//! is rendered by the server's own output functions. A `prepare` round trip beforehand supplies
//! column types so numbers, booleans and binary data become typed [`Value`]s.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::{FutureExt, StreamExt, pin_mut};
use postgres_native_tls::MakeTlsConnector;
use tokio::sync::Mutex;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage};

use super::{BatchSink, Connection, Explained, explain_with};
use crate::alter::{ColumnDef, create_table_sql};
use crate::config::{ConnectionConfig, Credentials, SslMode};
use crate::dialect::Dialect;
use crate::model::{
    ColumnInfo, ColumnMeta, ConstraintInfo, ConstraintKind, IndexInfo, ObjectKind, ResultSet, Row, SchemaObject,
    StatementResult, TableStructure,
};
use crate::plan::ExplainStatements;
use crate::server::{Flavor, ServerInfo};
use crate::tunnel::{SshTunnel, open_tunnel};
use crate::value::{Value, parse_hex};

const D: Dialect = Dialect::Postgres;
const SEP: char = '\u{1f}';

pub struct PgConnection {
    client: Mutex<Client>,
    cancel_token: CancelToken,
    tls: MakeTlsConnector,
    version: String,
    database: String,
    conn_task: tokio::task::JoinHandle<()>,
    _tunnel: Option<SshTunnel>,
}

impl Drop for PgConnection {
    fn drop(&mut self) {
        self.conn_task.abort();
    }
}

pub(crate) fn tls_connector(mode: SslMode, ca_cert: &str) -> Result<native_tls::TlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    match mode {
        SslMode::Disable | SslMode::Prefer | SslMode::Require => {
            b.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
        }
        SslMode::VerifyCa => {
            b.danger_accept_invalid_hostnames(true);
        }
        SslMode::VerifyFull => {}
    }
    if matches!(mode, SslMode::VerifyCa | SslMode::VerifyFull) && !ca_cert.trim().is_empty() {
        let path = crate::tunnel::expand_tilde(ca_cert.trim());
        let pem = std::fs::read(&path).with_context(|| format!("reading CA certificate {}", path.display()))?;
        b.add_root_certificate(native_tls::Certificate::from_pem(&pem)?);
    }
    Ok(b.build()?)
}

/// Formats server errors like psql does (`ERROR: … DETAIL: … HINT: …`).
pub(crate) fn pg_error(e: tokio_postgres::Error) -> anyhow::Error {
    if let Some(db) = e.as_db_error() {
        let mut msg = format!("{}: {}", db.severity(), db.message());
        if let Some(d) = db.detail() {
            msg.push_str(&format!("\nDETAIL: {d}"));
        }
        if let Some(h) = db.hint() {
            msg.push_str(&format!("\nHINT: {h}"));
        }
        return anyhow!(msg);
    }
    anyhow!(super::describe(&e, std::error::Error::source(&e)))
}

fn type_label(ty: &Type) -> String {
    match ty.name().strip_prefix('_') {
        Some(elem) => format!("{elem}[]"),
        None => ty.name().to_string(),
    }
}

fn to_value(text: Option<&str>, ty: Option<&Type>) -> Value {
    let Some(t) = text else { return Value::Null };
    let Some(ty) = ty else { return Value::text(t) };
    match *ty {
        Type::BOOL => Value::Bool(t == "t"),
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => {
            t.parse().map(Value::Int).unwrap_or_else(|_| Value::text(t))
        }
        Type::FLOAT4 | Type::FLOAT8 => t.parse().map(Value::Float).unwrap_or_else(|_| Value::text(t)),
        Type::NUMERIC => Value::Decimal(t.into()),
        Type::BYTEA => parse_hex(t).map(|b| Value::Bytes(b.into())).unwrap_or_else(|| Value::text(t)),
        _ => Value::text(t),
    }
}

impl PgConnection {
    pub async fn connect(config: &ConnectionConfig, creds: &Credentials) -> Result<Self> {
        let mut pg = tokio_postgres::Config::new();
        pg.user(&config.user)
            .dbname(if config.database.is_empty() { &config.user } else { &config.database })
            .application_name("tableplus-rs")
            .connect_timeout(Duration::from_secs(10))
            .keepalives(true)
            .ssl_mode(match config.tls.mode {
                SslMode::Disable => tokio_postgres::config::SslMode::Disable,
                SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
                _ => tokio_postgres::config::SslMode::Require,
            });
        if let Some(pw) = creds.password.as_deref().filter(|p| !p.is_empty()) {
            pg.password(pw);
        }
        let tunnel = if config.ssh.enabled {
            let t = open_tunnel(&config.ssh, creds, &config.host, config.port).await?;
            // Keep the real host name for TLS verification; dial the local tunnel end.
            pg.host(&config.host).hostaddr(IpAddr::from([127, 0, 0, 1])).port(t.local_addr.port());
            Some(t)
        } else {
            pg.host(&config.host).port(config.port);
            None
        };
        let tls = MakeTlsConnector::new(tls_connector(config.tls.mode, &config.tls.ca_cert)?);
        let (client, connection) = pg.connect(tls.clone()).await.map_err(pg_error)?;
        let conn_task = tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!("postgres connection closed: {e}");
            }
        });
        let version = match client.simple_query("SHOW server_version").await {
            Ok(msgs) => msgs
                .into_iter()
                .find_map(|m| match m {
                    SimpleQueryMessage::Row(r) => r.get(0).map(str::to_owned),
                    _ => None,
                })
                .unwrap_or_default(),
            Err(e) => return Err(pg_error(e)),
        };
        let version = version.split_whitespace().next().unwrap_or_default().to_string();
        let database = pg.get_dbname().unwrap_or_default().to_string();
        Ok(Self {
            cancel_token: client.cancel_token(),
            client: Mutex::new(client),
            tls,
            version,
            database,
            conn_task,
            _tunnel: tunnel,
        })
    }

    /// Runs a catalog query and returns every row as optional strings.
    async fn meta(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let client = self.client.lock().await;
        let msgs = client.simple_query(sql).await.map_err(pg_error)?;
        Ok(msgs
            .into_iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).map(str::to_owned)).collect()),
                _ => None,
            })
            .collect())
    }

    fn regclass(schema: &str, table: &str) -> String {
        format!("{}::regclass", D.quote_str(&D.qualified(schema, table)))
    }
}

/// Runs one statement, reporting column metadata once and then each row until `on_row`
/// returns false. Returns whether the statement produced a result set, and the command count.
async fn stream(
    client: &Client,
    sql: &str,
    on_columns: &mut (dyn FnMut(&[ColumnMeta]) + Send),
    on_row: &mut (dyn FnMut(Row) -> Result<bool> + Send),
) -> Result<(bool, u64)> {
    let types: Option<Vec<Type>> = match client.prepare(sql).await {
        Ok(stmt) => Some(stmt.columns().iter().map(|c| c.type_().clone()).collect()),
        // Multi-statement strings can't be prepared; run them untyped.
        Err(e)
            if e.as_db_error()
                .is_some_and(|d| d.code().code() == "42601" && d.message().contains("multiple commands")) =>
        {
            None
        }
        Err(e) => return Err(pg_error(e)),
    };
    let stream = client.simple_query_raw(sql).await.map_err(pg_error)?;
    pin_mut!(stream);
    let mut has_rows = false;
    let mut affected = 0;
    while let Some(msg) = stream.next().await {
        match msg.map_err(pg_error)? {
            SimpleQueryMessage::RowDescription(desc) if !has_rows => {
                has_rows = true;
                let cols: Vec<ColumnMeta> = desc
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let ty = types.as_ref().and_then(|t| t.get(i));
                        ColumnMeta::new(c.name(), ty.map(type_label).unwrap_or_default())
                    })
                    .collect();
                on_columns(&cols);
            }
            SimpleQueryMessage::Row(r) => {
                let row: Row =
                    (0..r.len()).map(|i| to_value(r.get(i), types.as_ref().and_then(|t| t.get(i)))).collect();
                if !on_row(row)? {
                    break;
                }
            }
            SimpleQueryMessage::CommandComplete(n) => {
                affected = n;
                if has_rows {
                    break;
                }
            }
            _ => {}
        }
    }
    Ok((has_rows, affected))
}

/// A later statement in a transaction has a later timestamp than the transaction. The simple
/// protocol stamps the query once; the extended one stamps parse and execute apart. An aborted
/// transaction refuses the query but is still open.
async fn in_transaction(client: &Client) -> Result<bool> {
    let messages = match client.simple_query("SELECT transaction_timestamp() <> statement_timestamp()").await {
        Ok(messages) => messages,
        Err(e) if e.code() == Some(&SqlState::IN_FAILED_SQL_TRANSACTION) => return Ok(true),
        Err(e) => return Err(pg_error(e)),
    };
    let open = messages.iter().find_map(|m| match m {
        SimpleQueryMessage::Row(row) => row.get(0),
        _ => None,
    });
    Ok(open == Some("t"))
}

async fn query_on(client: &Client, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
    let limit = max_rows.unwrap_or(usize::MAX);
    let mut cols = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut truncated = false;
    let (has_rows, affected) = stream(client, sql, &mut |c| cols = c.to_vec(), &mut |row| {
        if rows.len() >= limit {
            truncated = true;
            return Ok(false);
        }
        rows.push(row);
        Ok(true)
    })
    .await?;
    Ok(if has_rows {
        StatementResult::Rows(ResultSet { columns: cols, rows, truncated })
    } else {
        StatementResult::Affected(affected)
    })
}

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn split_list(v: &Option<String>) -> Vec<String> {
    v.as_deref().filter(|s| !s.is_empty()).map(|s| s.split(SEP).map(str::to_owned).collect()).unwrap_or_default()
}

#[async_trait]
impl Connection for PgConnection {
    fn dialect(&self) -> Dialect {
        D
    }

    fn server_version(&self) -> String {
        format!("PostgreSQL {}", self.version)
    }

    fn server_info(&self) -> ServerInfo {
        ServerInfo::from_version_text(Flavor::Postgres, &self.version)
    }

    fn database(&self) -> String {
        self.database.clone()
    }

    fn default_schema(&self) -> String {
        "public".into()
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows =
            self.meta("SELECT datname FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY 1").await?;
        Ok(rows.iter().map(|r| s(&r[0])).collect())
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let rows = self
            .meta(
                "SELECT nspname FROM pg_namespace WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
                 ORDER BY nspname = 'public' DESC, nspname",
            )
            .await?;
        Ok(rows.iter().map(|r| s(&r[0])).collect())
    }

    async fn list_objects(&self) -> Result<Vec<SchemaObject>> {
        let mut out = Vec::new();
        let rels = self
            .meta(
                "SELECT n.nspname, c.relname, c.relkind FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relkind IN ('r','p','v','m','f') AND n.nspname NOT LIKE 'pg\\_%' \
                   AND n.nspname <> 'information_schema' AND NOT c.relispartition \
                 ORDER BY 1, 2",
            )
            .await?;
        for r in rels {
            let kind = match r[2].as_deref() {
                Some("v") => ObjectKind::View,
                Some("m") => ObjectKind::MaterializedView,
                _ => ObjectKind::Table,
            };
            out.push(SchemaObject::new(s(&r[0]), s(&r[1]), kind));
        }
        let procs = self
            .meta(
                "SELECT n.nspname, p.proname, p.prokind, pg_get_function_identity_arguments(p.oid) FROM pg_proc p \
                 JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
                   AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e') \
                 ORDER BY 1, 2",
            )
            .await?;
        for r in procs {
            let kind = if r[2].as_deref() == Some("p") { ObjectKind::Procedure } else { ObjectKind::Function };
            let mut o = SchemaObject::new(s(&r[0]), s(&r[1]), kind);
            o.signature = r[3].clone();
            out.push(o);
        }
        Ok(out)
    }

    async fn table_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        let oid = Self::regclass(schema, table);
        let cols = self
            .meta(&format!(
                "SELECT a.attname, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                   pg_get_expr(d.adbin, d.adrelid), \
                   EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = a.attrelid AND i.indisprimary AND a.attnum = ANY(i.indkey)), \
                   a.attidentity, a.attgenerated, col_description(a.attrelid, a.attnum) \
                 FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = {oid} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
            ))
            .await?;
        if cols.is_empty() {
            bail!("{schema}.{table} has no columns or does not exist");
        }
        let columns = cols
            .iter()
            .map(|r| {
                let extra = match (r[5].as_deref(), r[6].as_deref()) {
                    (Some("a"), _) => Some("GENERATED ALWAYS AS IDENTITY".to_string()),
                    (Some("d"), _) => Some("GENERATED BY DEFAULT AS IDENTITY".to_string()),
                    (_, Some("s")) => Some("GENERATED (stored)".to_string()),
                    _ => None,
                };
                ColumnInfo {
                    name: s(&r[0]),
                    data_type: s(&r[1]),
                    nullable: r[2].as_deref() == Some("t"),
                    default: r[3].clone(),
                    primary_key: r[4].as_deref() == Some("t"),
                    extra,
                    comment: r[7].clone(),
                }
            })
            .collect();
        let idx = self
            .meta(&format!(
                "SELECT i.relname, ix.indisunique, ix.indisprimary, am.amname, pg_get_indexdef(ix.indexrelid), \
                   array_to_string(ARRAY(SELECT pg_get_indexdef(ix.indexrelid, k, true) \
                     FROM generate_series(1, ix.indnkeyatts) k), chr(31)) \
                 FROM pg_index ix JOIN pg_class i ON i.oid = ix.indexrelid JOIN pg_am am ON am.oid = i.relam \
                 WHERE ix.indrelid = {oid} ORDER BY ix.indisprimary DESC, i.relname"
            ))
            .await?;
        let indexes = idx
            .iter()
            .map(|r| IndexInfo {
                name: s(&r[0]),
                unique: r[1].as_deref() == Some("t"),
                primary: r[2].as_deref() == Some("t"),
                method: r[3].clone(),
                definition: r[4].clone(),
                columns: split_list(&r[5]),
            })
            .collect();
        let cons = self
            .meta(&format!(
                "SELECT c.conname, c.contype, pg_get_constraintdef(c.oid, true), \
                   array_to_string(ARRAY(SELECT a.attname FROM unnest(c.conkey) WITH ORDINALITY k(n, ord) \
                     JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.n ORDER BY k.ord), chr(31)) \
                 FROM pg_constraint c WHERE c.conrelid = {oid} \
                 ORDER BY array_position(ARRAY['p','u','f','c']::\"char\"[], c.contype), c.conname"
            ))
            .await?;
        let constraints = cons
            .iter()
            .filter_map(|r| {
                let kind = match r[1].as_deref()? {
                    "p" => ConstraintKind::PrimaryKey,
                    "u" => ConstraintKind::Unique,
                    "f" => ConstraintKind::ForeignKey,
                    "c" => ConstraintKind::Check,
                    _ => return None,
                };
                let def = s(&r[2]);
                let definition = match kind {
                    ConstraintKind::ForeignKey => def.find("REFERENCES").map(|p| def[p..].to_string()).unwrap_or(def),
                    _ => def,
                };
                Some(ConstraintInfo { name: s(&r[0]), kind, columns: split_list(&r[3]), definition })
            })
            .collect();
        let comment = self.meta(&format!("SELECT obj_description({oid}, 'pg_class')")).await?;
        Ok(TableStructure { columns, indexes, constraints, comment: comment.first().and_then(|r| r[0].clone()) })
    }

    async fn object_definition(&self, o: &SchemaObject) -> Result<String> {
        let one = |rows: Vec<Vec<Option<String>>>| rows.into_iter().next().and_then(|r| r.into_iter().next().flatten());
        match o.kind {
            ObjectKind::View | ObjectKind::MaterializedView => {
                let oid = Self::regclass(&o.schema, &o.name);
                let body = one(self.meta(&format!("SELECT pg_get_viewdef({oid}, true)")).await?).unwrap_or_default();
                let kw = if o.kind == ObjectKind::View { "CREATE OR REPLACE VIEW" } else { "CREATE MATERIALIZED VIEW" };
                Ok(format!("{kw} {} AS\n{}", D.qualified(&o.schema, &o.name), body.trim_end()))
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let sig = o.signature.clone().unwrap_or_default();
                let sql = format!(
                    "SELECT pg_get_functiondef(p.oid) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                     WHERE n.nspname = {} AND p.proname = {} AND pg_get_function_identity_arguments(p.oid) = {} LIMIT 1",
                    D.quote_str(&o.schema),
                    D.quote_str(&o.name),
                    D.quote_str(&sig)
                );
                one(self.meta(&sql).await?).ok_or_else(|| anyhow!("definition not found"))
            }
            ObjectKind::Table => {
                let st = self.table_structure(&o.schema, &o.name).await?;
                let cols: Vec<ColumnDef> = st.columns.iter().map(ColumnDef::from).collect();
                let extra: Vec<String> = st
                    .constraints
                    .iter()
                    .filter(|c| c.kind != ConstraintKind::PrimaryKey)
                    .map(|c| {
                        let body = match c.kind {
                            ConstraintKind::ForeignKey => {
                                let cols = c.columns.iter().map(|x| D.quote_ident(x)).collect::<Vec<_>>().join(", ");
                                format!("FOREIGN KEY ({cols}) {}", c.definition)
                            }
                            _ => c.definition.clone(),
                        };
                        format!("CONSTRAINT {} {body}", D.quote_ident(&c.name))
                    })
                    .collect();
                let mut ddl = create_table_sql(D, &o.schema, &o.name, &cols, &extra);
                ddl.push(';');
                for i in st.indexes.iter().filter(|i| !i.primary && !st.constraints.iter().any(|c| c.name == i.name)) {
                    if let Some(def) = &i.definition {
                        ddl.push_str(&format!("\n{def};"));
                    }
                }
                Ok(ddl)
            }
        }
    }

    async fn query(&self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
        query_on(&*self.client.lock().await, sql, max_rows).await
    }

    async fn query_batches(&self, sql: &str, batch: usize, sink: &mut BatchSink<'_>) -> Result<u64> {
        let cols = std::sync::Mutex::new(Vec::new());
        let mut buf: Vec<Row> = Vec::with_capacity(batch);
        let mut total = 0u64;
        let mut failed = None;
        let (has_rows, _) =
            stream(&*self.client.lock().await, sql, &mut |c| *cols.lock().expect("cols") = c.to_vec(), &mut |row| {
                buf.push(row);
                total += 1;
                if buf.len() >= batch
                    && let Err(e) = sink(&cols.lock().expect("cols"), std::mem::take(&mut buf))
                {
                    failed = Some(e);
                    return Ok(false);
                }
                Ok(true)
            })
            .await?;
        if let Some(e) = failed {
            return Err(e);
        }
        if !has_rows {
            bail!("statement returned no rows");
        }
        sink(&cols.lock().expect("cols"), buf)?;
        Ok(total)
    }

    async fn execute_in_transaction(&self, statements: &[String]) -> Result<u64> {
        let mut client = self.client.lock().await;
        let tx = client.transaction().await.map_err(pg_error)?;
        let mut total = 0;
        for (i, stmt) in statements.iter().enumerate() {
            let msgs = tx
                .simple_query(stmt)
                .await
                .map_err(|e| pg_error(e).context(format!("statement {} failed: {stmt}", i + 1)))?;
            total += msgs
                .iter()
                .filter_map(|m| match m {
                    SimpleQueryMessage::CommandComplete(n) => Some(*n),
                    _ => None,
                })
                .sum::<u64>();
        }
        tx.commit().await.map_err(pg_error)?;
        Ok(total)
    }

    async fn explain(&self, req: &ExplainStatements) -> Result<Explained> {
        explain_with(
            &mut *self.client.lock().await,
            req,
            |client, sql| query_on(client, sql, None).boxed(),
            |client, sql| async move { client.prepare(sql).await.map(|_| ()).map_err(pg_error) }.boxed(),
            |client| in_transaction(client).boxed(),
        )
        .await
    }

    async fn cancel(&self) -> Result<()> {
        self.cancel_token.cancel_query(self.tls.clone()).await.map_err(pg_error)
    }

    async fn ping(&self) -> Result<()> {
        self.meta("SELECT 1").await.map(|_| ())
    }
}
