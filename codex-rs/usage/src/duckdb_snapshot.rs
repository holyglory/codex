use std::path::Path;

use duckdb::Connection;

use crate::error::Result;
use crate::error::StorageError;
use crate::model::TimeRange;
use crate::model::UsageFilter;
use crate::model::UsageTotals;

/// Read-only DuckDB view over the immutable Parquet snapshot.
///
/// A connection is created per query so the mutable journal/tail never shares
/// a transaction or connection with readers. The manifest supplies the exact
/// segment list; no retired SQLite path or wildcard outside that list is read.
#[derive(Clone, Copy, Debug, Default)]
pub struct DuckDbSnapshot;

impl DuckDbSnapshot {
    /// Aggregate immutable segment rows for one range and dimension filter.
    pub fn totals(
        &self,
        segments_directory: &Path,
        segment_files: &[String],
        range: TimeRange,
        filter: &UsageFilter,
    ) -> Result<UsageTotals> {
        if segment_files.is_empty() {
            return Ok(UsageTotals::default());
        }
        let paths = segment_files
            .iter()
            .map(|file| {
                let path = segments_directory.join(file);
                sql_string_literal(&path.to_string_lossy())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut predicates = vec![format!(
            "observed_at >= {} AND observed_at < {}",
            range.start, range.end
        )];
        if let Some(account) = filter.account_id.as_deref() {
            predicates.push(format!("account_id = {}", sql_string_literal(account)));
        }
        if let Some(thread) = filter.thread_id.as_deref() {
            predicates.push(format!("thread_id = {}", sql_string_literal(thread)));
        }
        if let Some(repository) = filter.repository_id.as_deref() {
            predicates.push(format!(
                "repository_id = {}",
                sql_string_literal(repository)
            ));
        }
        if let Some(model) = filter.model.as_deref() {
            predicates.push(format!("model = {}", sql_string_literal(model)));
        }
        let query = format!(
            "SELECT CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(cached_input_tokens), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(reasoning_output_tokens), 0) AS BIGINT), \
                    COUNT(*) FROM read_parquet([{paths}]) WHERE {}",
            predicates.join(" AND ")
        );
        let connection = Connection::open_in_memory().map_err(duckdb_error)?;
        let mut statement = connection.prepare(&query).map_err(duckdb_error)?;
        let row = statement
            .query_row([], |row| {
                Ok(UsageTotals {
                    input_tokens: row.get(0)?,
                    cached_input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    reasoning_output_tokens: row.get(3)?,
                    records: row.get(4)?,
                })
            })
            .map_err(duckdb_error)?;
        Ok(row)
    }
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn duckdb_error(error: duckdb::Error) -> StorageError {
    StorageError::Columnar(format!("DuckDB snapshot query failed: {error}"))
}
