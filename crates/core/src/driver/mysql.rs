//! MySQL and MariaDB via `mysql_async` (text protocol).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use futures_util::FutureExt;
use mysql_async::consts::{ColumnFlags, ColumnType, StatusFlags};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};
use tokio::sync::Mutex;

use super::{BatchSink, Connection, Explained, explain_with};
use crate::config::{ConnectionConfig, Credentials, SslMode};
use crate::dialect::Dialect;
use crate::model::{
    ColumnInfo, ColumnMeta, ConstraintInfo, ConstraintKind, IndexInfo, ObjectKind, ResultSet, Row, SchemaObject,
    StatementResult, TableStructure,
};
use crate::plan::ExplainStatements;
use crate::server::{Flavor, ServerInfo};
use crate::tunnel::{SshTunnel, open_tunnel};
use crate::value::Value;

const D: Dialect = Dialect::MySql;
const SYSTEM_SCHEMAS: &str = "('information_schema','mysql','performance_schema','sys')";

pub struct MyConnection {
    conn: Mutex<Conn>,
    opts: Opts,
    connection_id: u32,
    version: String,
    mariadb: bool,
    database: String,
    _tunnel: Option<SshTunnel>,
}

pub(crate) fn my_error(e: mysql_async::Error) -> anyhow::Error {
    match e {
        mysql_async::Error::Server(s) => anyhow!("ERROR {} ({}): {}", s.code, s.state, s.message),
        other => anyhow!(super::describe(&other, std::error::Error::source(&other))),
    }
}

fn ssl_opts(mode: SslMode, ca_cert: &str) -> Option<SslOpts> {
    let base = SslOpts::default();
    let with_ca = |o: SslOpts| {
        if ca_cert.trim().is_empty() {
            o
        } else {
            o.with_root_certs(vec![crate::tunnel::expand_tilde(ca_cert.trim()).into()])
        }
    };
    match mode {
        SslMode::Disable => None,
        SslMode::Prefer | SslMode::Require => Some(base.with_danger_accept_invalid_certs(true)),
        SslMode::VerifyCa => Some(with_ca(base).with_danger_skip_domain_validation(true)),
        SslMode::VerifyFull => Some(with_ca(base)),
    }
}

fn type_label(t: ColumnType, flags: ColumnFlags, charset: u16) -> String {
    use ColumnType::*;
    let unsigned = if flags.contains(ColumnFlags::UNSIGNED_FLAG) { " UNSIGNED" } else { "" };
    let binary = charset == 63;
    let base = match t {
        MYSQL_TYPE_TINY => "TINYINT",
        MYSQL_TYPE_SHORT => "SMALLINT",
        MYSQL_TYPE_INT24 => "MEDIUMINT",
        MYSQL_TYPE_LONG => "INT",
        MYSQL_TYPE_LONGLONG => "BIGINT",
        MYSQL_TYPE_FLOAT => "FLOAT",
        MYSQL_TYPE_DOUBLE => "DOUBLE",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "DECIMAL",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "DATE",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "TIME",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "DATETIME",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "TIMESTAMP",
        MYSQL_TYPE_YEAR => "YEAR",
        MYSQL_TYPE_BIT => "BIT",
        MYSQL_TYPE_JSON => "JSON",
        MYSQL_TYPE_ENUM => "ENUM",
        MYSQL_TYPE_SET => "SET",
        MYSQL_TYPE_GEOMETRY => "GEOMETRY",
        MYSQL_TYPE_VECTOR => "VECTOR",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            if binary {
                "BLOB"
            } else {
                "TEXT"
            }
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
            if binary {
                "VARBINARY"
            } else {
                "VARCHAR"
            }
        }
        MYSQL_TYPE_STRING => {
            if flags.contains(ColumnFlags::ENUM_FLAG) {
                "ENUM"
            } else if flags.contains(ColumnFlags::SET_FLAG) {
                "SET"
            } else if binary {
                "BINARY"
            } else {
                "CHAR"
            }
        }
        MYSQL_TYPE_NULL => "NULL",
        _ => "UNKNOWN",
    };
    format!("{base}{unsigned}")
}

fn to_value(v: Option<mysql_async::Value>, t: ColumnType, flags: ColumnFlags, charset: u16) -> Value {
    use ColumnType::*;
    use mysql_async::Value as V;
    let bytes = match v {
        None | Some(V::NULL) => return Value::Null,
        Some(V::Bytes(b)) => b,
        Some(V::Int(i)) => return Value::Int(i),
        Some(V::UInt(u)) => return Value::UInt(u),
        Some(V::Float(f)) => return Value::Float(f64::from(f)),
        Some(V::Double(f)) => return Value::Float(f),
        Some(other) => return Value::text(other.as_sql(true).trim_matches('\'')),
    };
    let text = || String::from_utf8_lossy(&bytes).into_owned();
    match t {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_YEAR => {
            let s = text();
            if flags.contains(ColumnFlags::UNSIGNED_FLAG) {
                s.parse().map(Value::UInt).unwrap_or_else(|_| Value::text(s))
            } else {
                s.parse().map(Value::Int).unwrap_or_else(|_| Value::text(s))
            }
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let s = text();
            s.parse().map(Value::Float).unwrap_or_else(|_| Value::text(s))
        }
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Value::Decimal(text().into()),
        MYSQL_TYPE_BIT => {
            // BIT(n) arrives as big-endian bytes; show it as a number.
            let n = bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
            Value::UInt(n)
        }
        MYSQL_TYPE_GEOMETRY => Value::Bytes(bytes.into()),
        _ if charset == 63
            && !matches!(
                t,
                MYSQL_TYPE_JSON
                    | MYSQL_TYPE_DATE
                    | MYSQL_TYPE_DATETIME
                    | MYSQL_TYPE_DATETIME2
                    | MYSQL_TYPE_TIMESTAMP
                    | MYSQL_TYPE_TIMESTAMP2
                    | MYSQL_TYPE_TIME
                    | MYSQL_TYPE_TIME2
                    | MYSQL_TYPE_NEWDATE
            ) =>
        {
            Value::Bytes(bytes.into())
        }
        _ => match String::from_utf8(bytes) {
            Ok(s) => Value::Text(s.into()),
            Err(e) => Value::Bytes(e.into_bytes().into()),
        },
    }
}

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

/// Prepares `sql` on the server and drops it. The text goes in as a hex literal, so the server
/// reads it in the session's character set and sql_mode; `mysql_async`'s own prepare fails under
/// NO_BACKSLASH_ESCAPES.
async fn prepare(conn: &mut Conn, sql: &str) -> Result<()> {
    // A packet over max_allowed_packet closes the connection, and hex doubles the text, so it is
    // checked first and staged in pieces.
    let packet: u64 =
        conn.query_first("SELECT @@max_allowed_packet").await.map_err(my_error)?.context("no max_allowed_packet")?;
    let packet = usize::try_from(packet).unwrap_or(usize::MAX);
    ensure!(
        sql.len() + 2 <= packet,
        "The plan request is {} bytes, more than the server's max_allowed_packet of {packet} bytes allows.",
        sql.len()
    );
    let chunk = (packet.saturating_sub(128) / 2).clamp(1, 256 * 1024);
    let prepared = stage_and_prepare(conn, sql.as_bytes(), chunk).await;
    conn.query_drop("SET @tableplus_explain_sql = NULL").await.map_err(my_error)?;
    prepared.map_err(my_error)?;
    conn.query_drop("DEALLOCATE PREPARE tableplus_explain").await.map_err(my_error)
}

async fn stage_and_prepare(conn: &mut Conn, sql: &[u8], chunk: usize) -> mysql_async::Result<()> {
    conn.query_drop("SET @tableplus_explain_sql = X''").await?;
    for chunk in sql.chunks(chunk) {
        let hex: String = chunk.iter().map(|b| format!("{b:02X}")).collect();
        conn.query_drop(format!("SET @tableplus_explain_sql = CONCAT(@tableplus_explain_sql, X'{hex}')")).await?;
    }
    conn.query_drop("PREPARE tableplus_explain FROM @tableplus_explain_sql").await
}

/// The server flags an open transaction in every OK packet; `DO 0` fetches a fresh one.
async fn in_transaction(conn: &mut Conn) -> Result<bool> {
    conn.query_drop("DO 0").await.map_err(my_error)?;
    let flags = conn.last_ok_packet().map(|ok| ok.status_flags()).context("the server sent no status")?;
    Ok(flags.contains(StatusFlags::SERVER_STATUS_IN_TRANS))
}

async fn query_on(conn: &mut Conn, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
    let mut result = conn.query_iter(sql).await.map_err(my_error)?;
    let columns = result.columns().filter(|c| !c.is_empty());
    let Some(columns) = columns else {
        let affected = result.affected_rows();
        result.drop_result().await.map_err(my_error)?;
        return Ok(StatementResult::Affected(affected));
    };
    let meta: Vec<ColumnMeta> = columns
        .iter()
        .map(|c| ColumnMeta::new(c.name_str(), type_label(c.column_type(), c.flags(), c.character_set())))
        .collect();
    let limit = max_rows.unwrap_or(usize::MAX);
    let mut rows: Vec<Row> = Vec::new();
    let mut truncated = false;
    while let Some(row) = result.next().await.map_err(my_error)? {
        if rows.len() >= limit {
            truncated = true;
            continue; // keep draining so the connection stays usable
        }
        let row: Row = row
            .unwrap_raw()
            .into_iter()
            .zip(columns.iter())
            .map(|(v, c)| to_value(v, c.column_type(), c.flags(), c.character_set()))
            .collect();
        rows.push(row);
    }
    result.drop_result().await.map_err(my_error)?;
    Ok(StatementResult::Rows(ResultSet { columns: meta, rows, truncated }))
}

impl MyConnection {
    pub async fn connect(config: &ConnectionConfig, creds: &Credentials) -> Result<Self> {
        let tunnel = if config.ssh.enabled {
            Some(open_tunnel(&config.ssh, creds, &config.host, config.port).await?)
        } else {
            None
        };
        let (host, port) = match &tunnel {
            Some(t) => ("127.0.0.1".to_string(), t.local_addr.port()),
            None => (config.host.clone(), config.port),
        };
        let builder = |ssl: Option<SslOpts>| {
            let mut b = OptsBuilder::default()
                .ip_or_hostname(host.clone())
                .tcp_port(port)
                .user(Some(config.user.clone()))
                .pass(creds.password.clone().filter(|p| !p.is_empty()))
                .db_name(Some(config.database.clone()).filter(|d| !d.is_empty()))
                .prefer_socket(false)
                .stmt_cache_size(0)
                .init(vec!["SET NAMES utf8mb4"]);
            if let Some(mut ssl) = ssl {
                if tunnel.is_some() {
                    ssl = ssl.with_danger_tls_hostname_override(Some(config.host.clone()));
                }
                b = b.ssl_opts(ssl);
            }
            Opts::from(b)
        };
        let ssl = ssl_opts(config.tls.mode, &config.tls.ca_cert);
        let mut opts = builder(ssl.clone());
        let conn = match tokio::time::timeout(Duration::from_secs(15), Conn::new(opts.clone())).await {
            Err(_) => bail!("connection to {host}:{port} timed out"),
            Ok(Ok(c)) => c,
            // "Prefer" falls back to plaintext if the server has no TLS.
            Ok(Err(e))
                if config.tls.mode == SslMode::Prefer
                    && ssl.is_some()
                    && !matches!(e, mysql_async::Error::Server(_)) =>
            {
                tracing::info!("TLS connection failed ({e}), retrying without TLS");
                opts = builder(None);
                Conn::new(opts.clone()).await.map_err(my_error)?
            }
            Ok(Err(e)) => return Err(my_error(e)),
        };
        let mut conn = conn;
        let version: Option<String> = conn.query_first("SELECT VERSION()").await.map_err(my_error)?;
        let version = version.unwrap_or_default();
        let database: Option<String> = conn.query_first("SELECT DATABASE()").await.map_err(my_error)?;
        Ok(Self {
            connection_id: conn.id(),
            conn: Mutex::new(conn),
            opts,
            mariadb: version.contains("MariaDB"),
            version,
            database: database.unwrap_or_default(),
            _tunnel: tunnel,
        })
    }

    async fn meta(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let mut conn = self.conn.lock().await;
        let rows: Vec<mysql_async::Row> = conn.query(sql).await.map_err(my_error)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                r.unwrap_raw()
                    .into_iter()
                    .map(|v| match v {
                        None | Some(mysql_async::Value::NULL) => None,
                        Some(mysql_async::Value::Bytes(b)) => Some(String::from_utf8_lossy(&b).into_owned()),
                        Some(other) => Some(other.as_sql(true).trim_matches('\'').to_string()),
                    })
                    .collect()
            })
            .collect())
    }

    /// Converts `COLUMN_DEFAULT` into an SQL expression. MySQL reports bare literal values
    /// (and expressions flagged `DEFAULT_GENERATED`); MariaDB already reports SQL.
    fn default_expr(&self, raw: Option<String>, data_type: &str, extra: &str) -> Option<String> {
        let raw = raw?;
        if self.mariadb {
            return (raw != "NULL").then_some(raw);
        }
        if extra.contains("DEFAULT_GENERATED") {
            let upper = raw.to_ascii_uppercase();
            return Some(
                if upper.starts_with("CURRENT_TIMESTAMP") || upper.starts_with("NOW(") || raw.starts_with('(') {
                    raw
                } else {
                    format!("({raw})")
                },
            );
        }
        let numeric = matches!(
            data_type,
            "tinyint" | "smallint" | "mediumint" | "int" | "bigint" | "decimal" | "float" | "double" | "bit" | "year"
        );
        Some(if numeric && raw.parse::<f64>().is_ok() { raw } else { D.quote_str(&raw) })
    }
}

#[async_trait]
impl Connection for MyConnection {
    fn dialect(&self) -> Dialect {
        D
    }

    fn server_version(&self) -> String {
        let v = self.version.split('-').next().unwrap_or_default();
        if self.mariadb { format!("MariaDB {v}") } else { format!("MySQL {v}") }
    }

    fn server_info(&self) -> ServerInfo {
        ServerInfo::from_version_text(if self.mariadb { Flavor::MariaDb } else { Flavor::MySql }, &self.version)
    }

    fn database(&self) -> String {
        self.database.clone()
    }

    fn default_schema(&self) -> String {
        self.database.clone()
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.list_schemas().await
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let rows = self
            .meta(&format!(
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME NOT IN {SYSTEM_SCHEMAS} ORDER BY 1"
            ))
            .await?;
        Ok(rows.iter().map(|r| s(&r[0])).collect())
    }

    async fn list_objects(&self) -> Result<Vec<SchemaObject>> {
        let mut out = Vec::new();
        let rels = self
            .meta(&format!(
                "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA NOT IN {SYSTEM_SCHEMAS} ORDER BY 1, 2"
            ))
            .await?;
        for r in rels {
            let kind = if r[2].as_deref() == Some("VIEW") { ObjectKind::View } else { ObjectKind::Table };
            out.push(SchemaObject::new(s(&r[0]), s(&r[1]), kind));
        }
        let routines = self
            .meta(&format!(
                "SELECT ROUTINE_SCHEMA, ROUTINE_NAME, ROUTINE_TYPE FROM information_schema.ROUTINES \
                 WHERE ROUTINE_SCHEMA NOT IN {SYSTEM_SCHEMAS} ORDER BY 1, 2"
            ))
            .await?;
        for r in routines {
            let kind = if r[2].as_deref() == Some("PROCEDURE") { ObjectKind::Procedure } else { ObjectKind::Function };
            out.push(SchemaObject::new(s(&r[0]), s(&r[1]), kind));
        }
        Ok(out)
    }

    async fn table_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        let (sq, tq) = (D.quote_str(schema), D.quote_str(table));
        let cols = self
            .meta(&format!(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, EXTRA, COLUMN_COMMENT, DATA_TYPE \
                 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {sq} AND TABLE_NAME = {tq} ORDER BY ORDINAL_POSITION"
            ))
            .await?;
        if cols.is_empty() {
            bail!("{schema}.{table} has no columns or does not exist");
        }
        let columns = cols
            .into_iter()
            .map(|r| {
                let extra_raw = s(&r[5]);
                let extra = extra_raw.replace("DEFAULT_GENERATED", "").trim().to_string();
                ColumnInfo {
                    name: s(&r[0]),
                    data_type: s(&r[1]),
                    nullable: r[2].as_deref() == Some("YES"),
                    default: self.default_expr(r[3].clone(), r[7].as_deref().unwrap_or_default(), &extra_raw),
                    primary_key: r[4].as_deref() == Some("PRI"),
                    extra: (!extra.is_empty()).then_some(extra),
                    comment: r[6].clone().filter(|c| !c.is_empty()),
                }
            })
            .collect();
        let idx = self
            .meta(&format!(
                "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, INDEX_TYPE FROM information_schema.STATISTICS \
                 WHERE TABLE_SCHEMA = {sq} AND TABLE_NAME = {tq} ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX"
            ))
            .await?;
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in idx {
            let name = s(&r[0]);
            let col = r[2].clone().unwrap_or_else(|| "(expression)".into());
            match indexes.last_mut().filter(|i| i.name == name) {
                Some(i) => i.columns.push(col),
                None => indexes.push(IndexInfo {
                    primary: name == "PRIMARY",
                    unique: r[1].as_deref() == Some("0"),
                    method: r[3].clone(),
                    definition: None,
                    columns: vec![col],
                    name,
                }),
            }
        }
        let cons = self
            .meta(&format!(
                "SELECT tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, \
                   k.REFERENCED_TABLE_NAME, k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
                 FROM information_schema.TABLE_CONSTRAINTS tc \
                 LEFT JOIN information_schema.KEY_COLUMN_USAGE k ON k.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                   AND k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND k.TABLE_NAME = tc.TABLE_NAME \
                 LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS rc ON rc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA \
                   AND rc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME AND rc.TABLE_NAME = tc.TABLE_NAME \
                 WHERE tc.TABLE_SCHEMA = {sq} AND tc.TABLE_NAME = {tq} \
                 ORDER BY FIELD(tc.CONSTRAINT_TYPE, 'PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY', 'CHECK'), \
                   tc.CONSTRAINT_NAME, k.ORDINAL_POSITION"
            ))
            .await?;
        let checks = self
            .meta(&format!(
                "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS cc \
                 JOIN information_schema.TABLE_CONSTRAINTS tc ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA \
                   AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME \
                 WHERE tc.TABLE_SCHEMA = {sq} AND tc.TABLE_NAME = {tq} AND tc.CONSTRAINT_TYPE = 'CHECK'"
            ))
            .await
            .unwrap_or_default();
        let mut constraints: Vec<ConstraintInfo> = Vec::new();
        let mut fk_targets: Vec<(String, String, Vec<String>, String, String)> = Vec::new();
        for r in cons {
            let name = s(&r[0]);
            let kind = match r[1].as_deref() {
                Some("PRIMARY KEY") => ConstraintKind::PrimaryKey,
                Some("UNIQUE") => ConstraintKind::Unique,
                Some("FOREIGN KEY") => ConstraintKind::ForeignKey,
                Some("CHECK") => ConstraintKind::Check,
                _ => continue,
            };
            let col = r[2].clone();
            match constraints.last_mut().filter(|c| c.name == name && c.kind == kind) {
                Some(c) => {
                    if let Some(col) = col {
                        c.columns.push(col);
                    }
                    if kind == ConstraintKind::ForeignKey
                        && let (Some(fk), Some(rc)) = (fk_targets.last_mut(), r[5].clone())
                    {
                        fk.2.push(rc);
                    }
                }
                None => {
                    if kind == ConstraintKind::ForeignKey {
                        fk_targets.push((
                            D.qualified(&s(&r[3]), &s(&r[4])),
                            name.clone(),
                            r[5].iter().cloned().collect(),
                            s(&r[6]),
                            s(&r[7]),
                        ));
                    }
                    constraints.push(ConstraintInfo {
                        name,
                        kind,
                        columns: col.into_iter().collect(),
                        definition: String::new(),
                    });
                }
            }
        }
        for c in &mut constraints {
            let cols = c.columns.iter().map(|x| D.quote_ident(x)).collect::<Vec<_>>().join(", ");
            c.definition = match c.kind {
                ConstraintKind::PrimaryKey => format!("PRIMARY KEY ({cols})"),
                ConstraintKind::Unique => format!("UNIQUE ({cols})"),
                ConstraintKind::ForeignKey => match fk_targets.iter().find(|f| f.1 == c.name) {
                    Some((target, _, ref_cols, upd, del)) => {
                        let refs = ref_cols.iter().map(|x| D.quote_ident(x)).collect::<Vec<_>>().join(", ");
                        let mut d = format!("REFERENCES {target} ({refs})");
                        if !upd.is_empty() && upd != "RESTRICT" && upd != "NO ACTION" {
                            d.push_str(&format!(" ON UPDATE {upd}"));
                        }
                        if !del.is_empty() && del != "RESTRICT" && del != "NO ACTION" {
                            d.push_str(&format!(" ON DELETE {del}"));
                        }
                        d
                    }
                    None => String::new(),
                },
                ConstraintKind::Check => checks
                    .iter()
                    .find(|r| r[0].as_deref() == Some(c.name.as_str()))
                    .map(|r| format!("CHECK ({})", s(&r[1])))
                    .unwrap_or_else(|| "CHECK".into()),
            };
        }
        let comment = self
            .meta(&format!(
                "SELECT TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = {sq} AND TABLE_NAME = {tq}"
            ))
            .await?
            .into_iter()
            .next()
            .and_then(|r| r[0].clone())
            .filter(|c| !c.is_empty());
        Ok(TableStructure { columns, indexes, constraints, comment })
    }

    async fn object_definition(&self, o: &SchemaObject) -> Result<String> {
        let target = D.qualified(&o.schema, &o.name);
        let (sql, col) = match o.kind {
            ObjectKind::Table => (format!("SHOW CREATE TABLE {target}"), 1),
            ObjectKind::View | ObjectKind::MaterializedView => (format!("SHOW CREATE VIEW {target}"), 1),
            ObjectKind::Function => (format!("SHOW CREATE FUNCTION {target}"), 2),
            ObjectKind::Procedure => (format!("SHOW CREATE PROCEDURE {target}"), 2),
        };
        let rows = self.meta(&sql).await?;
        rows.into_iter()
            .next()
            .and_then(|r| r.into_iter().nth(col).flatten())
            .context("definition not available (missing privileges?)")
    }

    async fn query(&self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
        query_on(&mut *self.conn.lock().await, sql, max_rows).await
    }

    async fn query_batches(&self, sql: &str, batch: usize, sink: &mut BatchSink<'_>) -> Result<u64> {
        let mut conn = self.conn.lock().await;
        let mut result = conn.query_iter(sql).await.map_err(my_error)?;
        let Some(columns) = result.columns().filter(|c| !c.is_empty()) else {
            result.drop_result().await.map_err(my_error)?;
            bail!("statement returned no rows");
        };
        let meta: Vec<ColumnMeta> = columns
            .iter()
            .map(|c| ColumnMeta::new(c.name_str(), type_label(c.column_type(), c.flags(), c.character_set())))
            .collect();
        let mut buf: Vec<Row> = Vec::with_capacity(batch);
        let mut total = 0u64;
        let mut failed = None;
        while let Some(row) = result.next().await.map_err(my_error)? {
            if failed.is_some() {
                continue; // drain so the connection stays usable
            }
            buf.push(
                row.unwrap_raw()
                    .into_iter()
                    .zip(columns.iter())
                    .map(|(v, c)| to_value(v, c.column_type(), c.flags(), c.character_set()))
                    .collect(),
            );
            total += 1;
            if buf.len() >= batch
                && let Err(e) = sink(&meta, std::mem::take(&mut buf))
            {
                failed = Some(e);
            }
        }
        result.drop_result().await.map_err(my_error)?;
        if let Some(e) = failed {
            return Err(e);
        }
        sink(&meta, buf)?;
        Ok(total)
    }

    async fn execute_in_transaction(&self, statements: &[String]) -> Result<u64> {
        let mut conn = self.conn.lock().await;
        conn.query_drop("START TRANSACTION").await.map_err(my_error)?;
        let mut total = 0;
        for (i, stmt) in statements.iter().enumerate() {
            let outcome = async {
                let result = conn.query_iter(stmt.as_str()).await?;
                let n = result.affected_rows();
                result.drop_result().await?;
                Ok::<_, mysql_async::Error>(n)
            }
            .await;
            match outcome {
                Ok(n) => total += n,
                Err(e) => {
                    let _ = conn.query_drop("ROLLBACK").await;
                    return Err(my_error(e).context(format!("statement {} failed: {stmt}", i + 1)));
                }
            }
        }
        conn.query_drop("COMMIT").await.map_err(my_error)?;
        Ok(total)
    }

    async fn explain(&self, req: &ExplainStatements) -> Result<Explained> {
        explain_with(
            &mut *self.conn.lock().await,
            req,
            |conn, sql| query_on(conn, sql, None).boxed(),
            |conn, sql| prepare(conn, sql).boxed(),
            |conn| in_transaction(conn).boxed(),
        )
        .await
    }

    async fn cancel(&self) -> Result<()> {
        let mut killer = Conn::new(self.opts.clone()).await.map_err(my_error)?;
        killer.query_drop(format!("KILL QUERY {}", self.connection_id)).await.map_err(my_error)?;
        let _ = killer.disconnect().await;
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        self.conn.lock().await.ping().await.map_err(my_error)
    }
}
