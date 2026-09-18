//! Durable network incident evidence, independent of the rolling debug log.
//!
//! Only allowlisted diagnostic fields enter this database. There is no automatic
//! expiry: ordinary activity, log maintenance and restarts must not erase failures.

mod capture;
mod writer;

use crate::SqliteConfig;
use crate::SqlitePoolProfile;
use crate::open_sqlite_pool;
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::Path;

pub(crate) use capture::NetworkFields;
pub(crate) use writer::NetworkSink;

pub const DATABASE_FILENAME: &str = "network_diagnostics_1.sqlite";
const INCIDENT_PREDICATE: &str = "json_extract(details, '$.level') IN ('WARN', 'ERROR') OR json_extract(details, '$.failed') = 1 OR json_extract(details, '$.error') IS NOT NULL OR json_extract(details, '$.http_status') >= 400";

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkDiagnostic {
    pub id: i64,
    pub timestamp_ms: i64,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub event: String,
    pub details: BTreeMap<String, Value>,
}

#[derive(Debug, Default)]
pub struct NetworkQuery {
    pub incidents_only: bool,
    pub thread_id: Option<String>,
    pub since_ms: Option<i64>,
    pub before_id: Option<i64>,
    pub limit: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPage {
    pub data: Vec<NetworkDiagnostic>,
    pub next_before_id: Option<i64>,
}

/// Read a bounded page without initializing other runtime databases or deleting evidence.
pub async fn query(sqlite: &SqliteConfig, query: NetworkQuery) -> anyhow::Result<NetworkPage> {
    let path = sqlite.home().join(DATABASE_FILENAME);
    if !path.try_exists()? {
        return Ok(NetworkPage {
            data: Vec::new(),
            next_before_id: None,
        });
    }
    let pool = open_sqlite_pool(&path, SqlitePoolProfile::DurableEvents).await?;
    let limit = query.limit.clamp(/*min*/ 1, /*max*/ 100);
    let mut statement = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "SELECT id, timestamp_ms, thread_id, turn_id, event, details FROM network_events WHERE 1 = 1",
    );
    // Conditional predicates keep task lookups on the task index as the ledger grows.
    if let Some(thread_id) = &query.thread_id {
        statement.push(" AND thread_id = ").push_bind(thread_id);
    }
    if let Some(since_ms) = query.since_ms {
        statement.push(" AND timestamp_ms >= ").push_bind(since_ms);
    }
    if let Some(before_id) = query.before_id {
        statement.push(" AND id < ").push_bind(before_id);
    }
    if query.incidents_only {
        statement.push(" AND (").push(INCIDENT_PREDICATE).push(")");
    }
    statement
        .push(" ORDER BY id DESC LIMIT ")
        .push_bind(i64::from(limit) + 1);
    let rows = statement.build().fetch_all(&pool).await;
    pool.close().await;
    let mut data = rows?
        .into_iter()
        .map(|row| -> anyhow::Result<_> {
            Ok(NetworkDiagnostic {
                id: row.try_get("id")?,
                timestamp_ms: row.try_get("timestamp_ms")?,
                thread_id: row.try_get("thread_id")?,
                turn_id: row.try_get("turn_id")?,
                event: row.try_get("event")?,
                details: serde_json::from_str(row.try_get("details")?)?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let has_more = data.len() > limit as usize;
    data.truncate(limit as usize);
    let next_before_id = has_more.then(|| data.last().map(|row| row.id)).flatten();
    Ok(NetworkPage {
        data,
        next_before_id,
    })
}

async fn open(home: &Path) -> anyhow::Result<SqlitePool> {
    let path = home.join(DATABASE_FILENAME);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let pool = open_sqlite_pool(&path, SqlitePoolProfile::DurableEvents).await?;
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS network_events (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp_ms INTEGER NOT NULL, thread_id TEXT, turn_id TEXT, event TEXT NOT NULL, details TEXT NOT NULL); CREATE INDEX IF NOT EXISTS network_events_thread ON network_events(thread_id, id DESC); CREATE INDEX IF NOT EXISTS network_events_time ON network_events(timestamp_ms);")
        .execute(&pool).await?;
    sqlx::raw_sql(&format!("CREATE INDEX IF NOT EXISTS network_incidents_thread ON network_events(thread_id, id DESC) WHERE {INCIDENT_PREDICATE}; CREATE INDEX IF NOT EXISTS network_incidents_recent ON network_events(id DESC) WHERE {INCIDENT_PREDICATE};"))
        .execute(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
#[path = "network_diagnostics_tests.rs"]
mod tests;
