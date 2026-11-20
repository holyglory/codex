//! Retained statistics without observation or operation identities.
use std::collections::BTreeSet;
use std::path::Path;

use duckdb::Connection;

use crate::Result;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageManifest;
use crate::reader_query::CORRECTION_COLUMNS;
use crate::reader_query::eligible_corrections;
use crate::reader_query::empty_source;
use crate::reader_query::parquet_source as typed_source;
use crate::reader_query::values_source;
use crate::report_query::dimension_predicate;
use crate::report_query::sql_columns;
use crate::review_projection::ReviewProjectionDescriptor;
use crate::review_state::ReviewOperation;
use crate::review_storage::CLASSIFICATION_COLUMNS;
use crate::review_storage::OPERATION_COLUMNS;
use crate::review_storage::parquet_source;
use crate::review_storage::query_rows;
use crate::review_storage::write_query;

const DIMENSIONS: &str = "account_id, thread_id, repository_id, model, tool, kind, outcome, phase, activity, classification, coverage, work_native_project_id, workstream_id, work_outcome_id";
const METRICS: &[&str] = &[
    "operation_count",
    "interval_sum_ms",
    "unknown_intervals",
    "unfinished",
    "rework_operations",
    "retry_operations",
    "observed_attempts",
    "linked_operations",
];
const COLUMNS: &[(&str, &str)] = &[
    ("bucket_at_ms", "BIGINT"),
    ("account_id", "VARCHAR"),
    ("thread_id", "VARCHAR"),
    ("repository_id", "VARCHAR"),
    ("model", "VARCHAR"),
    ("tool", "VARCHAR"),
    ("kind", "VARCHAR"),
    ("outcome", "VARCHAR"),
    ("phase", "VARCHAR"),
    ("activity", "VARCHAR"),
    ("classification", "VARCHAR"),
    ("coverage", "VARCHAR"),
    ("work_native_project_id", "VARCHAR"),
    ("workstream_id", "VARCHAR"),
    ("work_outcome_id", "VARCHAR"),
    ("operation_count", "UBIGINT"),
    ("interval_sum_ms", "UBIGINT"),
    ("unknown_intervals", "UBIGINT"),
    ("unfinished", "UBIGINT"),
    ("rework_operations", "UBIGINT"),
    ("retry_operations", "UBIGINT"),
    ("observed_attempts", "UBIGINT"),
    ("linked_operations", "UBIGINT"),
];

pub(crate) const TOKEN_PREDICATE: &str = "event = 'token_observation' OR provider_total_tokens IS NOT NULL OR input_tokens <> 0 OR output_tokens <> 0 OR cached_input_tokens <> 0 OR reasoning_output_tokens <> 0";

fn classifications(root: &Path, descriptor: &ReviewProjectionDescriptor) -> Result<String> {
    let source = typed_source(
        &root.join("segments"),
        &descriptor.classification_files,
        &sql_columns(CLASSIFICATION_COLUMNS),
    )?;
    Ok(format!(
        "SELECT * FROM ({source}) QUALIFY row_number() OVER \
         (PARTITION BY operation_id ORDER BY source_sequence DESC) = 1"
    ))
}

/// Keep the operation's taxonomy available until its token links have been
/// replaced by self-contained aggregates. The candidate set is one partition.
pub(crate) fn referenced_operations(
    root: &Path,
    connection: &Connection,
    descriptor: &ReviewProjectionDescriptor,
    operations: &[ReviewOperation],
) -> Result<BTreeSet<String>> {
    if descriptor.contribution_fragments.is_empty() || operations.is_empty() {
        return Ok(BTreeSet::new());
    }
    let files = descriptor
        .contribution_fragments
        .iter()
        .map(|fragment| fragment.file.clone())
        .collect::<Vec<_>>();
    let source = parquet_source(root, &files)?;
    let candidates = values_source(operations, &sql_columns(OPERATION_COLUMNS))?;
    let rows = query_rows::<serde_json::Value>(
        connection,
        &format!(
            "SELECT DISTINCT o.operation_id FROM ({candidates}) o WHERE EXISTS \
             (SELECT 1 FROM {source} c WHERE ({TOKEN_PREDICATE}) AND \
              (c.operation_id = o.operation_id OR \
               (c.response_id IS NOT NULL AND c.response_id = o.response_id)))"
        ),
        operations.len(),
    )?;
    Ok(rows
        .into_iter()
        .filter_map(|row| row["operation_id"].as_str().map(str::to_owned))
        .collect())
}

pub(crate) fn retain_operations(
    root: &Path,
    connection: &Connection,
    descriptor: &mut ReviewProjectionDescriptor,
    operations: &[ReviewOperation],
    inactive: &str,
    stamp: &str,
) -> Result<()> {
    if operations.is_empty() {
        return Ok(());
    }
    let source = values_source(operations, &sql_columns(OPERATION_COLUMNS))?;
    let classifications = classifications(root, descriptor)?;
    let source = format!(
        "SELECT o.* EXCLUDE (phase, activity, classification), \
         COALESCE(c.phase,o.phase) phase, COALESCE(c.activity,o.activity) activity, \
         COALESCE(c.classification,o.classification) classification \
         FROM ({source}) o LEFT JOIN ({classifications}) c USING (operation_id)"
    );
    let at = "COALESCE(ended_at_ms, started_at_ms, observed_at_ms)::HUGEINT";
    let bucket = format!(
        "CASE WHEN {inactive} THEN NULL ELSE {at} - (({at} % 3600000 + 3600000) % 3600000) END"
    );
    let file = format!("review-operation-rollup-{stamp}.parquet");
    let statistics = detail_statistics(&source, TimeRange::all());
    write_query(
        connection,
        root,
        &file,
        &aggregate_query(&statistics, &bucket),
    )?;
    descriptor.operation_rollup_files.push(file);
    Ok(())
}

fn aggregate_query(source: &str, bucket: &str) -> String {
    let metrics = METRICS
        .iter()
        .map(|name| format!("sum({name})::UBIGINT {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT ({bucket})::BIGINT bucket_at_ms, {DIMENSIONS}, {metrics} \
         FROM ({source}) GROUP BY ALL"
    )
}

pub(crate) fn merge_operation_rollups(
    root: &Path,
    connection: &Connection,
    descriptor: &mut ReviewProjectionDescriptor,
    inactive: &str,
    stamp: &str,
) -> Result<bool> {
    let inputs = descriptor
        .operation_rollup_files
        .iter()
        .take(8)
        .cloned()
        .collect::<Vec<_>>();
    if inputs.is_empty() {
        return Ok(false);
    }
    let source = format!(
        "({})",
        typed_source(&root.join("segments"), &inputs, COLUMNS)?
    );
    let needs_conversion = inputs.len() > 1
        || connection
            .query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM {source} WHERE {inactive} AND bucket_at_ms IS NOT NULL)"
                ),
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(crate::review_storage::query_error)?;
    if !needs_conversion {
        return Ok(false);
    }
    let file = format!("review-operation-merged-{stamp}.parquet");
    write_query(
        connection,
        root,
        &file,
        &aggregate_query(
            &format!("SELECT * FROM {source}"),
            &format!("CASE WHEN {inactive} THEN NULL ELSE bucket_at_ms END"),
        ),
    )?;
    descriptor.operation_rollup_files.drain(..inputs.len());
    descriptor.operation_rollup_files.push(file);
    Ok(true)
}

/// Detail is clipped exactly. Retained rows carry full measured intervals;
/// finite reports explicitly disclose the discarded interval boundaries.
pub(crate) fn detail_statistics(source: &str, range: TimeRange) -> String {
    let start = range.start_ms();
    let end = range.end_ms();
    format!(
        "SELECT *, 1::UBIGINT operation_count, \
         CASE WHEN started_at_ms IS NOT NULL AND ended_at_ms IS NOT NULL THEN \
         GREATEST(0, LEAST(ended_at_ms::HUGEINT,{end}) - GREATEST(started_at_ms::HUGEINT,{start})) \
         ELSE 0 END::UBIGINT interval_sum_ms, \
         (started_at_ms IS NULL OR ended_at_ms IS NULL)::UBIGINT unknown_intervals, \
         (NOT terminal_observed)::UBIGINT unfinished, \
         (kind <> 'wait' AND rework_of_operation_id IS NOT NULL)::UBIGINT rework_operations, \
         (kind = 'model_request' AND retry_of_operation_id IS NOT NULL)::UBIGINT retry_operations, \
         (kind = 'model_request' AND attempt_number IS NOT NULL)::UBIGINT observed_attempts, \
         (kind <> 'wait' AND (retry_of_operation_id IS NOT NULL OR rework_of_operation_id IS NOT NULL))::UBIGINT linked_operations FROM ({source})"
    )
}

pub(crate) fn statistics_source(
    root: &Path,
    descriptor: Option<&ReviewProjectionDescriptor>,
    operations: &str,
    range: TimeRange,
    filter: &UsageFilter,
) -> Result<String> {
    let retained = retained_statistics_source(root, descriptor, range, filter)?;
    let metrics = METRICS.join(", ");
    let detail = detail_statistics(operations, range);
    Ok(format!(
        "SELECT {DIMENSIONS}, {metrics} FROM ({detail}) UNION ALL {retained}"
    ))
}

/// Retained-only metrics let exceptional live-operation overrides compose with
/// immutable totals without counting either source twice.
pub(crate) fn retained_statistics_source(
    root: &Path,
    descriptor: Option<&ReviewProjectionDescriptor>,
    range: TimeRange,
    filter: &UsageFilter,
) -> Result<String> {
    let retained = if let Some(descriptor) = descriptor {
        typed_source(
            &root.join("segments"),
            &descriptor.operation_rollup_files,
            COLUMNS,
        )?
    } else {
        empty_source(COLUMNS)
    };
    let time = if range == TimeRange::all() {
        "true".into()
    } else {
        format!(
            "bucket_at_ms >= {} AND bucket_at_ms < {}",
            range.start_ms(),
            range.end_ms()
        )
    };
    let metrics = METRICS.join(", ");
    Ok(format!(
        "SELECT {DIMENSIONS}, {metrics} FROM ({retained}) r \
         WHERE {} AND {time}",
        dimension_predicate(filter)
    ))
}

/// Assign token effort only through captured canonical links. Retained rows
/// already contain their assignment; missing or conflicting links stay null.
pub(crate) fn work_contribution_source(contributions: &str, operations: &str) -> String {
    format!(
        "WITH facts AS ({contributions}), ops AS ({operations}), \
         links AS (SELECT response_id,min(operation_id) operation_id FROM ops \
             WHERE response_id IS NOT NULL GROUP BY response_id HAVING count(*)=1), \
         attributed AS (SELECT f.*, CASE WHEN d.operation_id IS NOT NULL \
             AND l.operation_id IS NOT NULL AND d.operation_id <> l.operation_id THEN NULL \
             ELSE COALESCE(d.operation_id,l.operation_id) END linked_operation \
             FROM facts f LEFT JOIN ops d ON f.operation_id=d.operation_id \
             LEFT JOIN links l ON f.response_id=l.response_id) \
         SELECT f.* EXCLUDE (linked_operation,work_native_project_id,workstream_id,work_outcome_id), \
             COALESCE(f.work_native_project_id,o.work_native_project_id) work_native_project_id, \
             CASE WHEN f.work_native_project_id IS NOT NULL THEN f.workstream_id ELSE o.workstream_id END workstream_id, \
             CASE WHEN f.work_native_project_id IS NOT NULL THEN f.work_outcome_id ELSE o.work_outcome_id END work_outcome_id, \
             f.linked_operation work_operation_id \
         FROM attributed f LEFT JOIN ops o ON f.linked_operation=o.operation_id"
    )
}

/// Apply signed corrections and inherited taxonomy while record/response links
/// still exist. The resulting rollup no longer depends on correction sidecars.
pub(crate) fn retained_contribution_source(
    root: &Path,
    manifest: &UsageManifest,
    descriptor: &ReviewProjectionDescriptor,
    source: &str,
) -> Result<String> {
    let files = descriptor
        .partitions
        .iter()
        .map(|partition| partition.file.clone())
        .collect::<Vec<_>>();
    let operations = typed_source(
        &root.join("segments"),
        &files,
        &sql_columns(OPERATION_COLUMNS),
    )?;
    let classifications = classifications(root, descriptor)?;
    let corrections = typed_source(
        &root.join("segments"),
        &manifest.correction_files,
        CORRECTION_COLUMNS,
    )?;
    let eligible = eligible_corrections(manifest);
    let work = work_contribution_source(&format!("SELECT * FROM {source}"), &operations);
    Ok(format!(
        "WITH facts AS ({work}), \
         ops AS (SELECT * FROM ({operations}) WHERE operation_id IN \
             (SELECT operation_id FROM facts WHERE operation_id IS NOT NULL) OR response_id IN \
             (SELECT response_id FROM facts WHERE response_id IS NOT NULL)), \
         links AS (SELECT response_id,min(operation_id) operation_id FROM ops \
             WHERE response_id IS NOT NULL GROUP BY response_id HAVING count(*)=1), \
         deltas AS (SELECT record_id,sum(input_tokens_delta)::HUGEINT i, \
             sum(cached_input_tokens_delta)::HUGEINT c,sum(output_tokens_delta)::HUGEINT o, \
             sum(reasoning_output_tokens_delta)::HUGEINT r FROM \
             (SELECT * FROM ({corrections}) WHERE {eligible} \
              QUALIFY row_number() OVER (PARTITION BY id ORDER BY observed_at)=1) GROUP BY record_id), \
         attributed AS (SELECT f.*,COALESCE(o.operation_id,l.operation_id,f.operation_id) linked_operation \
             FROM facts f LEFT JOIN ops o ON f.operation_id=o.operation_id \
             LEFT JOIN links l ON f.response_id=l.response_id) \
         SELECT f.* EXCLUDE (linked_operation,input_tokens,cached_input_tokens,output_tokens,reasoning_output_tokens,phase,activity,classification), \
         f.input_tokens::HUGEINT+COALESCE(d.i,0) input_tokens, \
         f.cached_input_tokens::HUGEINT+COALESCE(d.c,0) cached_input_tokens, \
         f.output_tokens::HUGEINT+COALESCE(d.o,0) output_tokens, \
         f.reasoning_output_tokens::HUGEINT+COALESCE(d.r,0) reasoning_output_tokens, \
         COALESCE(c.phase,o.phase,f.phase) phase, COALESCE(c.activity,o.activity,f.activity) activity, \
         COALESCE(c.classification,o.classification,f.classification) classification \
         FROM attributed f LEFT JOIN deltas d USING(record_id) \
         LEFT JOIN ops o ON f.linked_operation=o.operation_id \
         LEFT JOIN ({classifications}) c ON f.linked_operation=c.operation_id"
    ))
}
