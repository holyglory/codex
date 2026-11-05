use std::path::Path;

use duckdb::Connection;

use crate::error::Result;
use crate::error::StorageError;
use crate::model::TimeRange;
use crate::model::UsageFilter;
use crate::model::UsageRecord;
use crate::model::UsageTotals;

/// Read-only DuckDB view over the immutable Parquet snapshot.
///
/// A connection is created per query so the mutable journal/tail never shares
/// a transaction or connection with readers. The manifest supplies the exact
/// segment list; no retired SQLite path or wildcard outside that list is read.
#[derive(Clone, Copy, Debug, Default)]
pub struct DuckDbSnapshot;

impl DuckDbSnapshot {
    /// Read immutable observations in stable timestamp/id order.
    pub fn query(
        &self,
        segments_directory: &Path,
        segment_files: &[String],
        range: TimeRange,
        filter: &UsageFilter,
    ) -> Result<Vec<UsageRecord>> {
        if segment_files.is_empty() {
            return Ok(Vec::new());
        }
        let connection = Connection::open_in_memory().map_err(duckdb_error)?;
        let query = format!(
            "{} ORDER BY observed_at, id",
            self.select_query(segments_directory, segment_files, range, filter)
        );
        let mut statement = connection.prepare(&query).map_err(duckdb_error)?;
        let mut rows = statement.query([]).map_err(duckdb_error)?;
        let mut records = Vec::new();
        while let Some(row) = rows.next().map_err(duckdb_error)? {
            records.push(UsageRecord {
                id: row.get(0).map_err(duckdb_error)?,
                observed_at: row.get(1).map_err(duckdb_error)?,
                operation_id: row.get(2).map_err(duckdb_error)?,
                thread_id: row.get(3).map_err(duckdb_error)?,
                repository_id: row.get(4).map_err(duckdb_error)?,
                account_id: row.get(5).map_err(duckdb_error)?,
                model: row.get(6).map_err(duckdb_error)?,
                tool: row.get(7).map_err(duckdb_error)?,
                activity: row.get(8).map_err(duckdb_error)?,
                phase: row.get(9).map_err(duckdb_error)?,
                classification: row.get(10).map_err(duckdb_error)?,
                event: row.get(11).map_err(duckdb_error)?,
                coverage: row.get(12).map_err(duckdb_error)?,
                started_at: row.get(13).map_err(duckdb_error)?,
                ended_at: row.get(14).map_err(duckdb_error)?,
                input_tokens: row.get(15).map_err(duckdb_error)?,
                cached_input_tokens: row.get(16).map_err(duckdb_error)?,
                output_tokens: row.get(17).map_err(duckdb_error)?,
                reasoning_output_tokens: row.get(18).map_err(duckdb_error)?,
            });
        }
        Ok(records)
    }

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
        let connection = Connection::open_in_memory().map_err(duckdb_error)?;
        let source = self.select_query(segments_directory, segment_files, range, filter);
        let query = format!(
            "SELECT CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(cached_input_tokens), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(reasoning_output_tokens), 0) AS BIGINT), \
                    COUNT(*) FROM ({source}) AS usage_rows"
        );
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

    fn select_query(
        self,
        segments_directory: &Path,
        segment_files: &[String],
        range: TimeRange,
        filter: &UsageFilter,
    ) -> String {
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
        format!(
            "SELECT id, observed_at, operation_id, thread_id, repository_id, account_id, model, tool, activity, phase, classification, event, coverage, started_at, ended_at, input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens FROM read_parquet([{paths}]) WHERE {}",
            predicates.join(" AND ")
        )
    }
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn duckdb_error(error: duckdb::Error) -> StorageError {
    StorageError::Columnar(format!("DuckDB snapshot query failed: {error}"))
}
