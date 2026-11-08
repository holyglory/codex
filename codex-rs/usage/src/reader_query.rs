use std::path::Path;

use duckdb::Connection;
use serde::Serialize;

use crate::RecordCursor;
use crate::RecordKind;
use crate::RepositoryUsage;
use crate::Result;
use crate::StorageError;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::UsageRecord;
use crate::UsageTotals;

pub(crate) const RECORD_COLUMNS: &[(&str, &str)] = &[
    ("id", "VARCHAR"),
    ("observed_at", "BIGINT"),
    ("operation_id", "VARCHAR"),
    ("thread_id", "VARCHAR"),
    ("repository_id", "VARCHAR"),
    ("account_id", "VARCHAR"),
    ("model", "VARCHAR"),
    ("tool", "VARCHAR"),
    ("activity", "VARCHAR"),
    ("phase", "VARCHAR"),
    ("classification", "VARCHAR"),
    ("event", "VARCHAR"),
    ("coverage", "VARCHAR"),
    ("started_at", "BIGINT"),
    ("ended_at", "BIGINT"),
    ("input_tokens", "UBIGINT"),
    ("cached_input_tokens", "UBIGINT"),
    ("output_tokens", "UBIGINT"),
    ("reasoning_output_tokens", "UBIGINT"),
    ("parent_thread_id", "VARCHAR"),
    ("turn_id", "VARCHAR"),
    ("response_id", "VARCHAR"),
    ("outcome", "VARCHAR"),
    ("duration_ms", "UBIGINT"),
    ("attempt_number", "UBIGINT"),
    ("provider_total_tokens", "UBIGINT"),
    ("cache_write_input_tokens", "UBIGINT"),
    ("missing_observations", "UBIGINT"),
    ("rework_of_operation_id", "VARCHAR"),
    ("observed_at_ms", "BIGINT"),
    ("started_at_ms", "BIGINT"),
    ("ended_at_ms", "BIGINT"),
    ("process_id", "VARCHAR"),
    ("agent_id", "VARCHAR"),
    ("approval_decision", "VARCHAR"),
    ("approval_source", "VARCHAR"),
    ("taxonomy_version", "UBIGINT"),
    ("heartbeat_count", "UBIGINT"),
    ("retry_of_operation_id", "VARCHAR"),
];
pub(crate) const CORRECTION_COLUMNS: &[(&str, &str)] = &[
    ("id", "VARCHAR"),
    ("record_id", "VARCHAR"),
    ("observed_at", "BIGINT"),
    ("input_tokens_delta", "BIGINT"),
    ("cached_input_tokens_delta", "BIGINT"),
    ("output_tokens_delta", "BIGINT"),
    ("reasoning_output_tokens_delta", "BIGINT"),
];

pub(crate) struct SnapshotQuery {
    pub(crate) connection: Connection,
    pub(crate) source: String,
    pub(crate) corrections: String,
    rollups: String,
}

impl SnapshotQuery {
    pub(crate) fn new(
        reader: &UsageReader,
        range: TimeRange,
        filter: &UsageFilter,
    ) -> Result<Self> {
        let connection = crate::duckdb_connection::open_in_memory().map_err(query_error)?;
        let directory = reader.root.join("segments");
        let files = matching_files(&connection, reader, range, filter)?;
        let immutable = parquet_source(&directory, &files, RECORD_COLUMNS)?;
        let tail = values_source(
            &crate::report_query::canonical_tail(reader, &connection)?,
            RECORD_COLUMNS,
        )?;
        let mut predicates = vec![format!(
            "effective_observed_at_ms >= {} AND effective_observed_at_ms < {}",
            range.start_ms(),
            range.end_ms()
        )];
        for (column, value) in [
            ("account_id", &filter.account_id),
            ("thread_id", &filter.thread_id),
            ("repository_id", &filter.repository_id),
            ("model", &filter.model),
        ] {
            if let Some(value) = value {
                predicates.push(format!("{column} = {}", quoted(value)));
            }
        }
        if let Some(threads) = &filter.thread_ids {
            predicates.push(if threads.is_empty() {
                "false".to_string()
            } else {
                format!(
                    "thread_id IN ({})",
                    threads
                        .iter()
                        .map(|id| quoted(id))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
        }
        let milliseconds = ["observed_at", "started_at", "ended_at"]
            .iter()
            .map(|column| {
                format!(
                    "{} AS effective_{column}_ms",
                    milliseconds_expression(column)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!(
            "SELECT * EXCLUDE (source_priority) FROM \
             (SELECT *, {milliseconds} FROM \
              (SELECT *, 0 AS source_priority FROM ({immutable}) UNION ALL \
               SELECT *, 1 AS source_priority FROM ({tail}))) \
             WHERE {} QUALIFY row_number() OVER (PARTITION BY id ORDER BY source_priority) = 1",
            predicates.join(" AND ")
        );
        let correction_segments = parquet_source(
            &directory,
            &reader.manifest.correction_files,
            CORRECTION_COLUMNS,
        )?;
        let correction_tail = values_source(&reader.corrections, CORRECTION_COLUMNS)?;
        let eligible = eligible_corrections(&reader.manifest);
        let corrections = format!(
            "SELECT * FROM ({correction_segments} UNION ALL {correction_tail}) \
             WHERE {eligible} \
             QUALIFY row_number() OVER (PARTITION BY id ORDER BY observed_at) = 1"
        );
        Ok(Self {
            connection,
            source,
            corrections,
            rollups: crate::token_rollup::source(reader, range, filter)?,
        })
    }

    pub(crate) fn totals(&self) -> Result<UsageTotals> {
        let raw = format!(
            "WITH records AS ({}), corrections AS ({}) \
             SELECT CAST(COALESCE(SUM(r.input_tokens::HUGEINT + COALESCE(c.i, 0)), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(r.cached_input_tokens::HUGEINT + COALESCE(c.c, 0)), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(r.output_tokens::HUGEINT + COALESCE(c.o, 0)), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(r.reasoning_output_tokens::HUGEINT + COALESCE(c.r, 0)), 0) AS BIGINT), \
                    COUNT(*) FROM records r LEFT JOIN \
             (SELECT record_id, SUM(input_tokens_delta) i, SUM(cached_input_tokens_delta) c, \
                     SUM(output_tokens_delta) o, SUM(reasoning_output_tokens_delta) r \
              FROM corrections GROUP BY record_id) c ON r.id = c.record_id",
            self.source, self.corrections
        );
        let query = format!(
            "WITH combined(input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,records) AS \
             ({raw} UNION ALL SELECT input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,records FROM ({})) \
             SELECT COALESCE(sum(input_tokens),0)::BIGINT, COALESCE(sum(cached_input_tokens),0)::BIGINT, \
             COALESCE(sum(output_tokens),0)::BIGINT, COALESCE(sum(reasoning_output_tokens),0)::BIGINT, \
             COALESCE(sum(records),0)::UBIGINT FROM combined",
            self.rollups
        );
        self.connection
            .query_row(&query, [], |row| {
                Ok(UsageTotals {
                    input_tokens: row.get(0)?,
                    cached_input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    reasoning_output_tokens: row.get(3)?,
                    records: row.get(4)?,
                })
            })
            .map_err(query_error)
    }

    pub(crate) fn records(
        &self,
        kind: RecordKind,
        after: Option<&RecordCursor>,
        limit: usize,
    ) -> Result<Vec<UsageRecord>> {
        let mut predicates = vec![match kind {
            RecordKind::All => "true".to_string(),
            RecordKind::Tool => "tool IS NOT NULL".to_string(),
            RecordKind::Activity => "activity IS NOT NULL".to_string(),
            RecordKind::Tokens => "event = 'token_observation' OR provider_total_tokens IS NOT NULL OR input_tokens <> 0 OR output_tokens <> 0".to_string(),
            RecordKind::Threads => "event IN ('thread_started','thread_stopped')".to_string(),
            RecordKind::Turns => "event IN ('turn_started','turn_finished')".to_string(),
            RecordKind::Classifications => "event = 'classification_correction' OR (starts_with(event,'activity_') AND NOT starts_with(event,'activity_span_'))".to_string(),
            RecordKind::ActivitySpans => "starts_with(event,'activity_span_')".to_string(),
            RecordKind::Coverage => "event = 'coverage_gap' OR missing_observations IS NOT NULL".to_string(),
            RecordKind::Lifecycle => "starts_with(event,'process_') OR starts_with(event,'thread_') OR starts_with(event,'turn_') OR starts_with(event,'agent_') OR starts_with(event,'activity_') OR starts_with(event,'retry_wait_')".to_string(),
            RecordKind::Repositories => "starts_with(event,'repository_')".to_string(),
            RecordKind::RepositoryIdentities => "event = 'repository_identity_observed'".to_string(),
            RecordKind::RepositoryAttributions => "event = 'repository_attributed'".to_string(),
            RecordKind::RepositoryEvents => "event = 'repository_seen'".to_string(),
            RecordKind::Approvals => "event = 'tool_approval' OR starts_with(event,'tool_approval_')".to_string(),
            RecordKind::Processes => "starts_with(event,'process_')".to_string(),
            RecordKind::Agents => "starts_with(event,'agent_')".to_string(),
            RecordKind::Taxonomies => "event = 'taxonomy_registered'".to_string(),
        }];
        // Select the latest entity/span state before applying the page cursor.
        // Applying the cursor first would resurrect older snapshots on later pages.
        let entity_key = match kind {
            RecordKind::Processes => Some("process_id"),
            RecordKind::Agents => Some("COALESCE(operation_id, agent_id)"),
            RecordKind::ActivitySpans => Some("operation_id"),
            RecordKind::RepositoryIdentities => Some("repository_id"),
            RecordKind::Taxonomies => Some("taxonomy_version"),
            RecordKind::All
            | RecordKind::Tool
            | RecordKind::Activity
            | RecordKind::Tokens
            | RecordKind::Threads
            | RecordKind::Turns
            | RecordKind::Classifications
            | RecordKind::Coverage
            | RecordKind::Lifecycle
            | RecordKind::Repositories
            | RecordKind::RepositoryAttributions
            | RecordKind::RepositoryEvents
            | RecordKind::Approvals => None,
        };
        let source = entity_key.map_or_else(|| self.source.clone(), |key| format!(
            "SELECT * FROM ({}) WHERE ({}) QUALIFY row_number() OVER \
             (PARTITION BY {key} ORDER BY effective_observed_at_ms DESC, effective_ended_at_ms DESC NULLS LAST, heartbeat_count DESC NULLS LAST, id DESC) = 1",
            self.source, predicates[0]
        ));
        if let Some(after) = after {
            predicates.push(format!(
                "(effective_observed_at_ms, id) > ({}, {})",
                after.observed_at_ms,
                quoted(&after.id)
            ));
        }
        let query = format!(
            "SELECT * FROM ({}) WHERE {} ORDER BY effective_observed_at_ms, id LIMIT {limit}",
            source,
            predicates
                .iter()
                .map(|predicate| format!("({predicate})"))
                .collect::<Vec<_>>()
                .join(" AND ")
        );
        let mut statement = self.connection.prepare(&query).map_err(query_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(UsageRecord {
                    id: row.get(0)?,
                    observed_at: row.get(1)?,
                    operation_id: row.get(2)?,
                    thread_id: row.get(3)?,
                    repository_id: row.get(4)?,
                    account_id: row.get(5)?,
                    model: row.get(6)?,
                    tool: row.get(7)?,
                    activity: row.get(8)?,
                    phase: row.get(9)?,
                    classification: row.get(10)?,
                    event: row.get(11)?,
                    coverage: row.get(12)?,
                    started_at: row.get(13)?,
                    ended_at: row.get(14)?,
                    input_tokens: row.get(15)?,
                    cached_input_tokens: row.get(16)?,
                    output_tokens: row.get(17)?,
                    reasoning_output_tokens: row.get(18)?,
                    parent_thread_id: row.get(19)?,
                    turn_id: row.get(20)?,
                    response_id: row.get(21)?,
                    outcome: row.get(22)?,
                    duration_ms: row.get(23)?,
                    attempt_number: row.get(24)?,
                    provider_total_tokens: row.get(25)?,
                    cache_write_input_tokens: row.get(26)?,
                    missing_observations: row.get(27)?,
                    rework_of_operation_id: row.get(28)?,
                    observed_at_ms: row.get(29)?,
                    started_at_ms: row.get(30)?,
                    ended_at_ms: row.get(31)?,
                    process_id: row.get(32)?,
                    agent_id: row.get(33)?,
                    approval_decision: row
                        .get::<_, Option<String>>(34)?
                        .map(|value| value.parse())
                        .transpose()
                        .map_err(|_| duckdb::Error::InvalidQuery)?,
                    approval_source: row
                        .get::<_, Option<String>>(35)?
                        .map(|value| value.parse())
                        .transpose()
                        .map_err(|_| duckdb::Error::InvalidQuery)?,
                    taxonomy_version: row.get(36)?,
                    heartbeat_count: row.get(37)?,
                    retry_of_operation_id: row.get(38)?,
                })
            })
            .map_err(query_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(query_error)
    }

    pub(crate) fn repositories(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RepositoryUsage>> {
        let after = after.map_or_else(
            || "true".to_string(),
            |id| format!("repository_id > {}", quoted(id)),
        );
        let raw = format!(
            "WITH records AS ({}), corrections AS ({}) \
             SELECT repository_id, MIN(r.effective_observed_at_ms), MAX(r.effective_observed_at_ms), \
                    CAST(COALESCE(SUM(r.input_tokens::HUGEINT + COALESCE(c.i, 0)), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(r.cached_input_tokens::HUGEINT + COALESCE(c.c, 0)), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(r.output_tokens::HUGEINT + COALESCE(c.o, 0)), 0) AS BIGINT), \
                    CAST(COALESCE(SUM(r.reasoning_output_tokens::HUGEINT + COALESCE(c.r, 0)), 0) AS BIGINT), \
                    COUNT(*) FROM records r LEFT JOIN \
             (SELECT record_id, SUM(input_tokens_delta) i, SUM(cached_input_tokens_delta) c, \
                     SUM(output_tokens_delta) o, SUM(reasoning_output_tokens_delta) r \
              FROM corrections GROUP BY record_id) c ON r.id = c.record_id \
             WHERE repository_id IS NOT NULL GROUP BY repository_id",
            self.source, self.corrections
        );
        let query = format!(
            "WITH combined(repository_id,first_at_ms,last_at_ms,input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,records) AS \
             ({raw} UNION ALL SELECT repository_id,first_at_ms,last_at_ms,input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,records FROM ({})) \
             SELECT repository_id,min(first_at_ms),max(last_at_ms),sum(input_tokens)::BIGINT, \
             sum(cached_input_tokens)::BIGINT,sum(output_tokens)::BIGINT,sum(reasoning_output_tokens)::BIGINT, \
             sum(records)::UBIGINT FROM combined WHERE repository_id IS NOT NULL AND {after} \
             GROUP BY repository_id ORDER BY repository_id LIMIT {limit}",
            self.rollups
        );
        let mut statement = self.connection.prepare(&query).map_err(query_error)?;
        let rows = statement
            .query_map([], |row| {
                let first_observed_at_ms: i64 = row.get(1)?;
                let last_observed_at_ms: i64 = row.get(2)?;
                Ok(RepositoryUsage {
                    id: row.get(0)?,
                    first_observed_at: first_observed_at_ms.div_euclid(1_000),
                    last_observed_at: last_observed_at_ms.div_euclid(1_000),
                    first_observed_at_ms,
                    last_observed_at_ms,
                    totals: UsageTotals {
                        input_tokens: row.get(3)?,
                        cached_input_tokens: row.get(4)?,
                        output_tokens: row.get(5)?,
                        reasoning_output_tokens: row.get(6)?,
                        records: row.get(7)?,
                    },
                })
            })
            .map_err(query_error)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(query_error)
    }
}

pub(crate) fn eligible_corrections(manifest: &crate::UsageManifest) -> String {
    if manifest.expired_correction_ids.is_empty() {
        "true".into()
    } else {
        format!(
            "id NOT IN ({})",
            manifest
                .expired_correction_ids
                .iter()
                .map(|id| quoted(id))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

pub(crate) fn matching_files(
    connection: &Connection,
    reader: &UsageReader,
    range: TimeRange,
    filter: &UsageFilter,
) -> Result<Vec<String>> {
    let (coarse_start, coarse_end) = range.coarse_seconds();
    let dimension = filter
        .repository_id
        .as_deref()
        .map(|id| ("repository", id))
        .or_else(|| filter.thread_id.as_deref().map(|id| ("thread", id)))
        .or_else(|| filter.account_id.as_deref().map(|id| ("account", id)));
    if filter
        .thread_ids
        .as_ref()
        .is_some_and(std::collections::BTreeSet::is_empty)
    {
        return Ok(Vec::new());
    }
    let membership_files = if reader.manifest.membership_files.is_empty() {
        reader
            .manifest
            .membership_file
            .iter()
            .cloned()
            .collect::<Vec<_>>()
    } else {
        reader.manifest.membership_files.clone()
    };
    if !membership_files.is_empty() && (dimension.is_some() || filter.thread_ids.is_some()) {
        let source = parquet_source(
            &reader.root.join("segments"),
            &membership_files,
            &[
                ("dimension_kind", "VARCHAR"),
                ("dimension_id", "VARCHAR"),
                ("hour_start", "BIGINT"),
                ("segment_file", "VARCHAR"),
            ],
        )?;
        let dimension_predicate = if let Some((kind, id)) = dimension {
            format!(
                "dimension_kind = {} AND dimension_id = {}",
                quoted(kind),
                quoted(id)
            )
        } else if let Some(threads) = &filter.thread_ids {
            format!(
                "dimension_kind = 'thread' AND dimension_id IN ({})",
                threads
                    .iter()
                    .map(|id| quoted(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            unreachable!("an indexed dimension is required above")
        };
        let query = format!(
            "SELECT DISTINCT segment_file FROM ({source}) WHERE {dimension_predicate} \
             AND hour_start >= ? AND hour_start < ? ORDER BY segment_file"
        );
        let mut statement = connection.prepare(&query).map_err(query_error)?;
        let rows = statement
            .query_map(
                duckdb::params![
                    coarse_start.saturating_sub(crate::HOUR_SECONDS - 1),
                    coarse_end
                ],
                |row| row.get(0),
            )
            .map_err(query_error)?;
        return rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(query_error);
    }
    Ok(reader
        .manifest
        .segment_files
        .iter()
        .filter(|file| {
            let hour = file
                .strip_prefix("segment-")
                .and_then(|name| name.strip_suffix(".parquet"))
                .and_then(|name| name.split_once('-'))
                .and_then(|(_, hour)| hour.parse::<i64>().ok());
            hour.is_none_or(|hour| {
                hour >= coarse_start.saturating_sub(crate::HOUR_SECONDS - 1) && hour < coarse_end
            })
        })
        .cloned()
        .collect())
}

pub(crate) fn parquet_source(
    directory: &Path,
    files: &[String],
    columns: &[(&str, &str)],
) -> Result<String> {
    if files.is_empty() {
        return Ok(empty_source(columns));
    }
    let mut paths = Vec::with_capacity(files.len());
    for file in files {
        if Path::new(file).file_name().and_then(|name| name.to_str()) != Some(file) {
            return Err(StorageError::InvalidData(
                "snapshot segment must be a file name".into(),
            ));
        }
        paths.push(quoted(&directory.join(file).to_string_lossy()));
    }
    let names = columns
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let optional = columns
        .iter()
        .copied()
        .filter(|(name, _)| {
            matches!(
                *name,
                "observed_at_ms"
                    | "started_at_ms"
                    | "ended_at_ms"
                    | "rework_of_operation_id"
                    | "process_id"
                    | "agent_id"
                    | "approval_decision"
                    | "approval_source"
                    | "taxonomy_version"
                    | "heartbeat_count"
                    | "retry_of_operation_id"
                    | "retry_operations"
                    | "observed_attempts"
                    | "linked_operations"
            )
        })
        .collect::<Vec<_>>();
    if optional.is_empty() {
        return Ok(format!(
            "SELECT {names} FROM read_parquet([{}])",
            paths.join(", ")
        ));
    }
    Ok(format!(
        "SELECT {names} FROM (SELECT * FROM read_parquet([{}], union_by_name=true) \
         UNION ALL BY NAME {})",
        paths.join(", "),
        empty_source(&optional)
    ))
}

/// Match Rust's saturating legacy-seconds fallback without overflowing BIGINT.
pub(crate) fn milliseconds_expression(column: &str) -> String {
    format!(
        "COALESCE({column}_ms, CASE WHEN {column} IS NOT NULL THEN CAST(GREATEST({}, LEAST({}, {column}::HUGEINT * 1000)) AS BIGINT) END)",
        i64::MIN,
        i64::MAX
    )
}

pub(crate) fn values_source<T: Serialize>(rows: &[T], columns: &[(&str, &str)]) -> Result<String> {
    if rows.is_empty() {
        return Ok(empty_source(columns));
    }
    let values = rows
        .iter()
        .map(|row| {
            let row = serde_json::to_value(row)?;
            let values = columns
                .iter()
                .map(|(name, ty)| {
                    let value = match &row[name] {
                        serde_json::Value::String(value) => quoted(value),
                        serde_json::Value::Number(value) => value.to_string(),
                        serde_json::Value::Bool(value) => value.to_string(),
                        _ => "NULL".to_string(),
                    };
                    format!("{value}::{ty}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!("({values})"))
        })
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let names = columns
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("SELECT * FROM (VALUES {values}) AS tail({names})"))
}

pub(crate) fn empty_source(columns: &[(&str, &str)]) -> String {
    let columns = columns
        .iter()
        .map(|(name, ty)| format!("NULL::{ty} AS {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {columns} WHERE false")
}

pub(crate) fn quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn query_error(error: duckdb::Error) -> StorageError {
    StorageError::Columnar(format!("DuckDB snapshot query failed: {error}"))
}

impl duckdb::types::FromSql for crate::UsageOutcome {
    fn column_result(value: duckdb::types::ValueRef<'_>) -> duckdb::types::FromSqlResult<Self> {
        <String as duckdb::types::FromSql>::column_result(value)?
            .parse()
            .map_err(|error: StorageError| duckdb::types::FromSqlError::Other(Box::new(error)))
    }
}
