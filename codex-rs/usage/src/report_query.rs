//! Bounded-tail review queries over a pinned operation projection.
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use arrow_schema::DataType;
use duckdb::Connection;

use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::reader_query::CORRECTION_COLUMNS;
use crate::reader_query::RECORD_COLUMNS;
use crate::reader_query::empty_source;
use crate::reader_query::matching_files;
use crate::reader_query::milliseconds_expression;
use crate::reader_query::parquet_source;
use crate::reader_query::query_error;
use crate::reader_query::quoted;
use crate::reader_query::values_source;
use crate::review_index::select_review_contribution_files;
use crate::review_index::select_review_files;
use crate::review_projection::load_review_operations;
use crate::review_projection::load_review_projection_shared as load_review_projection;
use crate::review_state::ReviewClassification;
use crate::review_state::ReviewContribution;
use crate::review_state::ReviewOperation;
use crate::review_state::lifecycle;
use crate::review_storage::CLASSIFICATION_COLUMNS;
use crate::review_storage::CONTRIBUTION_COLUMNS;
use crate::review_storage::OPERATION_COLUMNS;

pub(crate) struct ReportQuery {
    pub(crate) connection: Connection,
    pub(crate) operations: String,
    pub(crate) operation_statistics: String,
    pub(crate) retained_operation_statistics: String,
    pub(crate) contributions: String,
    pub(crate) complete_from_sequence: u64,
}

impl ReportQuery {
    pub(crate) fn new(
        reader: &UsageReader,
        range: TimeRange,
        filter: &UsageFilter,
    ) -> Result<Self> {
        let connection = crate::duckdb_connection::open_in_memory().map_err(query_error)?;
        let descriptor = load_review_projection(&reader.root, &reader.manifest)?;
        let directory = reader.root.join("segments");
        let operation_columns = sql_columns(OPERATION_COLUMNS);
        let contribution_columns = sql_columns(CONTRIBUTION_COLUMNS);
        let classification_columns = sql_columns(CLASSIFICATION_COLUMNS);
        let mut operation_files = Vec::new();
        let mut contribution_files = Vec::new();
        let mut classification_files = Vec::new();
        let hourly_contribution_source = if reader.manifest.correction_files.is_empty()
            && reader.corrections.is_empty()
            && reader.manifest.review_retention_cutoff_at_ms.is_none()
        {
            hourly_contribution_source(&connection, reader, range, filter)?
        } else {
            None
        };
        let mut complete_from_sequence = reader.manifest.watermark.saturating_add(1);
        if let Some(descriptor) = &descriptor {
            complete_from_sequence = descriptor.complete_from_sequence;
            operation_files = select_review_files(&reader.root, descriptor, range, filter)?;
            if hourly_contribution_source.is_none() {
                contribution_files =
                    select_review_contribution_files(&reader.root, descriptor, range, filter)?;
                contribution_files.extend(descriptor.contribution_rollup_files.iter().cloned());
            }
            classification_files.clone_from(&descriptor.classification_files);
        }
        let tail = canonical_tail(reader, &connection)?;
        let operation_ids = tail
            .iter()
            .filter(|row| lifecycle(row).is_some())
            .filter_map(|row| row.operation_id.clone())
            .collect::<BTreeSet<_>>();
        let mut states = if let Some(descriptor) = &descriptor {
            load_review_operations(&reader.root, descriptor, &operation_ids, &connection)?
                .into_iter()
                .map(|row| (row.operation_id.clone(), row))
                .collect::<BTreeMap<_, _>>()
        } else {
            BTreeMap::new()
        };
        let mut contributions = Vec::new();
        let mut classifications = Vec::new();
        for record in tail {
            let sequence = reader.record_sequences[&record.id];
            contributions.push(ReviewContribution::from_fact(sequence, record));
            if lifecycle(record).is_some() {
                let id = record.operation_id.as_deref().unwrap_or_default();
                if let Some(state) = states.get_mut(id) {
                    state.merge(sequence, record)?;
                } else {
                    let state = ReviewOperation::from_fact(sequence, record)?;
                    states.insert(state.operation_id.clone(), state);
                }
            }
            if record.event.as_deref() == Some("classification_correction")
                && let Some(operation_id) = &record.operation_id
            {
                classifications.push(ReviewClassification {
                    record_id: record.id.clone(),
                    operation_id: operation_id.clone(),
                    source_sequence: sequence,
                    observed_at_ms: record.effective_observed_at_ms(),
                    phase: record.phase.clone(),
                    activity: record.activity.clone(),
                    classification: record.classification.clone(),
                });
            }
        }
        let operation_source = if operation_files.is_empty() {
            empty_source(&operation_columns)
        } else {
            parquet_source(&directory, &operation_files, &operation_columns)?
        };
        let mut states = states.into_values().collect::<Vec<_>>();
        crate::assign_work_dimensions(&reader.root, &mut states, &[]);
        let operation_tail = values_source(&states, &operation_columns)?;
        let classification_source = if classification_files.is_empty() {
            empty_source(&classification_columns)
        } else {
            parquet_source(&directory, &classification_files, &classification_columns)?
        };
        let classification_tail = values_source(&classifications, &classification_columns)?;
        let classifications = format!(
            "SELECT * FROM ({classification_source} UNION ALL {classification_tail}) \
             QUALIFY row_number() OVER (PARTITION BY operation_id ORDER BY source_sequence DESC) = 1"
        );
        let predicate = dimension_predicate(filter);
        let operations = format!(
            "WITH latest AS (SELECT * FROM ({operation_source} UNION ALL {operation_tail}) \
             QUALIFY row_number() OVER (PARTITION BY operation_id ORDER BY source_sequence DESC) = 1), \
             classifications AS ({classifications}) \
             SELECT r.* EXCLUDE (phase, activity, classification), \
                    COALESCE(c.phase,r.phase) phase, COALESCE(c.activity,r.activity) activity, \
                    COALESCE(c.classification,r.classification) classification \
             FROM latest r LEFT JOIN classifications c USING (operation_id) \
             WHERE {predicate} AND (started_at_ms IS NULL OR started_at_ms < {}) \
             AND (ended_at_ms IS NULL OR ended_at_ms > {} \
                  OR (started_at_ms = ended_at_ms AND ended_at_ms = {}))",
            range.end_ms(),
            range.start_ms(),
            range.start_ms()
        );
        let contribution_source =
            hourly_contribution_source.unwrap_or(if contribution_files.is_empty() {
                empty_source(&contribution_columns)
            } else {
                parquet_source(&directory, &contribution_files, &contribution_columns)?
            });
        let contribution_tail = values_source(&contributions, &contribution_columns)?;
        let correction_source = if reader.manifest.correction_files.is_empty() {
            empty_source(CORRECTION_COLUMNS)
        } else {
            parquet_source(
                &directory,
                &reader.manifest.correction_files,
                CORRECTION_COLUMNS,
            )?
        };
        let correction_tail = values_source(&reader.corrections, CORRECTION_COLUMNS)?;
        let eligible = crate::reader_query::eligible_corrections(&reader.manifest);
        let contributions = format!(
            "WITH facts AS (SELECT * FROM ({contribution_source} UNION ALL {contribution_tail}) \
             QUALIFY row_number() OVER (PARTITION BY record_id ORDER BY source_sequence) = 1), \
             corrections AS (SELECT * FROM ({correction_source} UNION ALL {correction_tail}) \
             WHERE {eligible} \
             QUALIFY row_number() OVER (PARTITION BY id ORDER BY observed_at) = 1), \
             deltas AS (SELECT record_id, SUM(input_tokens_delta) i, SUM(cached_input_tokens_delta) c, \
                       SUM(output_tokens_delta) o, SUM(reasoning_output_tokens_delta) r FROM corrections GROUP BY record_id) \
             SELECT r.* EXCLUDE (input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens), \
                    r.input_tokens::HUGEINT + COALESCE(d.i,0) input_tokens, \
                    r.cached_input_tokens::HUGEINT + COALESCE(d.c,0) cached_input_tokens, \
                    r.output_tokens::HUGEINT + COALESCE(d.o,0) output_tokens, \
                    r.reasoning_output_tokens::HUGEINT + COALESCE(d.r,0) reasoning_output_tokens \
             FROM facts r LEFT JOIN deltas d USING(record_id) WHERE {predicate} \
             AND observed_at_ms >= {} AND observed_at_ms < {}",
            range.start_ms(),
            range.end_ms()
        );
        let operation_statistics = crate::review_aggregate::statistics_source(
            &reader.root,
            descriptor.as_deref(),
            "SELECT * FROM selected_operations",
            range,
            filter,
        )?;
        let retained_operation_statistics = crate::review_aggregate::retained_statistics_source(
            &reader.root,
            descriptor.as_deref(),
            range,
            filter,
        )?;
        // Materializing only the selected indexed slice avoids repeating Parquet
        // reads for the small aggregate and detail queries that form one report.
        connection
            .execute_batch(&format!(
                "CREATE TEMP TABLE selected_operations AS {operations}; \
             CREATE TEMP TABLE selected_operation_statistics AS {operation_statistics}; \
             CREATE TEMP TABLE selected_contributions AS {contributions}; \
             CREATE TEMP TABLE selected_classifications AS {classifications};"
            ))
            .map_err(query_error)?;
        Ok(Self {
            connection,
            operations: "SELECT * FROM selected_operations".into(),
            operation_statistics: "SELECT * FROM selected_operation_statistics".into(),
            retained_operation_statistics,
            contributions: "SELECT * FROM selected_contributions".into(),
            complete_from_sequence,
        })
    }

    /// Corrected live and retained token effort, linked only to captured work.
    pub(crate) fn work_contributions(&self) -> String {
        crate::review_aggregate::work_contribution_source(&self.contributions, &self.operations)
    }
}

/// Read finite dimension contributions from the hourly membership index. The
/// immutable review contribution fragments are intentionally append-only and
/// broad, so scanning them for one repository would decode the full history.
/// Hourly segments retain the same facts with a much narrower file set.
fn hourly_contribution_source(
    connection: &Connection,
    reader: &UsageReader,
    range: TimeRange,
    filter: &UsageFilter,
) -> Result<Option<String>> {
    if range == TimeRange::all()
        || filter.model.is_some()
        || filter.account_id.is_none()
            && filter.repository_id.is_none()
            && filter.thread_id.is_none()
            && filter.thread_ids.is_none()
        || reader.manifest.membership_files.is_empty() && reader.manifest.membership_file.is_none()
    {
        return Ok(None);
    }
    let files = matching_files(connection, reader, range, filter)?;
    if files.is_empty() {
        return Ok(Some(crate::reader_query::empty_source(&sql_columns(
            CONTRIBUTION_COLUMNS,
        ))));
    }
    let source = parquet_source(&reader.root.join("segments"), &files, RECORD_COLUMNS)?;
    let predicate = raw_dimension_predicate(filter);
    let observed_at_ms = milliseconds_expression("observed_at");
    let fields = [
        "id AS record_id".into(),
        format!("row_number() OVER (ORDER BY {observed_at_ms}, id)::UBIGINT AS source_sequence"),
        format!("{observed_at_ms} AS observed_at_ms"),
        "operation_id".into(),
        "response_id".into(),
        "account_id".into(),
        "thread_id".into(),
        "repository_id".into(),
        "model".into(),
        "tool".into(),
        "event".into(),
        "coverage".into(),
        "phase".into(),
        "activity".into(),
        "classification".into(),
        "input_tokens".into(),
        "cached_input_tokens".into(),
        "output_tokens".into(),
        "reasoning_output_tokens".into(),
        "provider_total_tokens".into(),
        "cache_write_input_tokens".into(),
        "missing_observations".into(),
        "NULL::UBIGINT observation_count".into(),
        "NULL::UBIGINT provider_total_unknowns".into(),
        "NULL::UBIGINT cache_write_unknowns".into(),
        "NULL::UBIGINT missing_observations_sum".into(),
        "attempt_number".into(),
        "rework_of_operation_id".into(),
        "NULL::VARCHAR work_native_project_id".into(),
        "NULL::VARCHAR workstream_id".into(),
        "NULL::VARCHAR work_outcome_id".into(),
    ]
    .join(", ");
    Ok(Some(format!(
        "SELECT * FROM (SELECT {fields} FROM ({source}) AS r WHERE {predicate}) AS facts \
         QUALIFY row_number() OVER (PARTITION BY record_id ORDER BY source_sequence) = 1"
    )))
}

fn raw_dimension_predicate(filter: &UsageFilter) -> String {
    let mut predicates = Vec::new();
    for (column, value) in [
        ("account_id", &filter.account_id),
        ("repository_id", &filter.repository_id),
        ("thread_id", &filter.thread_id),
    ] {
        if let Some(value) = value {
            predicates.push(format!("{column} = {}", quoted(value)));
        }
    }
    if let Some(threads) = &filter.thread_ids {
        predicates.push(if threads.is_empty() {
            "false".into()
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
    if predicates.is_empty() {
        "true".into()
    } else {
        predicates.join(" AND ")
    }
}

pub(crate) fn dimension_predicate(filter: &UsageFilter) -> String {
    let mut predicates = Vec::new();
    for (column, value) in [
        ("account_id", &filter.account_id),
        ("repository_id", &filter.repository_id),
        ("thread_id", &filter.thread_id),
        ("model", &filter.model),
    ] {
        if let Some(value) = value {
            predicates.push(format!("r.{column} = {}", quoted(value)));
        }
    }
    if let Some(threads) = &filter.thread_ids {
        predicates.push(if threads.is_empty() {
            "false".into()
        } else {
            format!(
                "r.thread_id IN ({})",
                threads
                    .iter()
                    .map(|id| quoted(id))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }
    if predicates.is_empty() {
        "true".into()
    } else {
        predicates.join(" AND ")
    }
}

pub(crate) fn sql_columns(
    columns: &'static [(&str, DataType)],
) -> Vec<(&'static str, &'static str)> {
    columns
        .iter()
        .map(|(name, kind)| {
            (
                *name,
                match kind {
                    DataType::Utf8 => "VARCHAR",
                    DataType::Int64 => "BIGINT",
                    DataType::UInt64 => "UBIGINT",
                    DataType::Boolean => "BOOLEAN",
                    _ => unreachable!("review schema uses primitive scalar types"),
                },
            )
        })
        .collect()
}

pub(crate) fn canonical_tail<'a>(
    reader: &'a UsageReader,
    _connection: &Connection,
) -> Result<Vec<&'a crate::UsageRecord>> {
    // Opening the reader has already reconciled this bounded tail against the
    // immutable exact-ID index. Reuse that result across every report action.
    let mut tail = reader.records.iter().collect::<Vec<_>>();
    tail.sort_by_key(|row| reader.record_sequences[&row.id]);
    Ok(tail)
}
