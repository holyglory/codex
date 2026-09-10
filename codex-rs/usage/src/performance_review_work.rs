use super::PerformanceReviewQuery;
use super::ReviewWorkBindingReference;
use super::ReviewWorkBindings;
use super::number;
use super::query;
use crate::UsageStoreError;
use crate::detail_query_support::uuid;
use sqlx::Row;
use sqlx::SqliteConnection;

const WORK_SELECTION: &str = ", work_selected AS (
    SELECT effective.id, effective.thread_id, effective.started_at_ms,
      (SELECT MAX(binding.observed_at_ms) FROM work_bindings binding
       WHERE binding.thread_id = effective.thread_id AND binding.observed_at_ms <= effective.started_at_ms) binding_at_ms,
      ((SELECT COUNT(DISTINCT attribution.repository_id) FROM repository_attributions attribution
        WHERE attribution.operation_id = effective.id) > 1 OR EXISTS (
        SELECT 1 FROM repository_attributions attribution WHERE attribution.operation_id = effective.id
          AND attribution.attribution_kind = 'multi_repo')) multi_repository
    FROM effective
), relevant_bindings AS (
    SELECT binding.* FROM (SELECT DISTINCT thread_id, binding_at_ms FROM work_selected
      WHERE binding_at_ms IS NOT NULL) moment
    JOIN work_bindings binding ON binding.thread_id = moment.thread_id
      AND binding.observed_at_ms = moment.binding_at_ms
), binding_moments AS (
    SELECT thread_id, observed_at_ms, COUNT(*) variants, MIN(event_id) event_id
    FROM relevant_bindings GROUP BY thread_id, observed_at_ms
), work_attribution AS (
    SELECT operation.id, binding.event_id, binding.workstream_id,
      CASE WHEN operation.multi_repository THEN 'multi_repository'
           WHEN moment.variants > 1 THEN 'ambiguous'
           WHEN binding.observed_at_ms = operation.started_at_ms THEN 'boundary'
           WHEN binding.provenance = 'runtime_observed' THEN 'bound'
           ELSE 'unknown' END coverage
    FROM work_selected operation
    LEFT JOIN binding_moments moment ON moment.thread_id = operation.thread_id AND moment.observed_at_ms = operation.binding_at_ms
    LEFT JOIN relevant_bindings binding ON binding.event_id = moment.event_id
)";

pub(super) async fn read(
    connection: &mut SqliteConnection,
    request: &PerformanceReviewQuery,
    source: &query::ClassificationSource,
) -> Result<ReviewWorkBindings, UsageStoreError> {
    let mut builder = query::selection(request, source);
    let coverage = builder
        .push(WORK_SELECTION)
        .push(
            "SELECT COUNT(*) operations,
        COALESCE(SUM(coverage = 'bound'), 0) bound_operations,
        COALESCE(SUM(coverage = 'ambiguous'), 0) ambiguous_operations,
        COALESCE(SUM(coverage = 'boundary'), 0) boundary_operations,
        COALESCE(SUM(coverage = 'multi_repository'), 0) multi_repository_operations,
        COALESCE(SUM(coverage <> 'bound' OR workstream_id IS NULL), 0) unknown_workstream_operations
        FROM work_attribution",
        )
        .build()
        .fetch_one(&mut *connection)
        .await
        .map_err(UsageStoreError::Database)?;
    let mut builder = query::selection(request, source);
    let rows = builder.push(WORK_SELECTION).push("SELECT binding.*, COUNT(*) OVER () reference_count,
        (SELECT COUNT(*) FROM work_attribution operation WHERE operation.event_id = binding.event_id
          AND operation.coverage = 'bound') bound_operations
        FROM relevant_bindings binding ORDER BY bound_operations DESC, observed_at_ms DESC, event_id LIMIT 8")
        .build().fetch_all(&mut *connection).await.map_err(UsageStoreError::Database)?;
    let reference_count = rows
        .first()
        .map(|row| number(row, "reference_count"))
        .transpose()?
        .unwrap_or(0);
    let mut references = Vec::new();
    let mut reference_bytes = 0;
    for row in &rows {
        let binding = crate::work_binding::from_row(row)?;
        let provenance: String = row
            .try_get("provenance")
            .map_err(UsageStoreError::Database)?;
        if !matches!(provenance.as_str(), "runtime_observed" | "unknown") {
            return Err(UsageStoreError::InvalidFact);
        }
        let event_id = uuid(row.try_get("event_id").map_err(UsageStoreError::Database)?)?;
        let bytes = 256
            + event_id.len()
            + binding.thread_id.as_str().len()
            + binding.native_project_id.len()
            + binding.workstream_id.as_ref().map_or(0, String::len)
            + binding.outcome_id.as_ref().map_or(0, String::len)
            + binding.experiment_ref.as_ref().map_or(0, String::len);
        if reference_bytes + bytes > 3 * 1024 {
            break;
        }
        reference_bytes += bytes;
        references.push(ReviewWorkBindingReference {
            event_id,
            thread_id: binding.thread_id.as_str().to_string(),
            native_project_id: binding.native_project_id,
            workstream_id: binding.workstream_id,
            outcome_id: binding.outcome_id,
            experiment_ref: binding.experiment_ref,
            observed_at_ms: binding.observed_at_ms,
            provenance,
            bound_operations: number(row, "bound_operations")?,
        });
    }
    let bound_operations = number(&coverage, "bound_operations")?;
    Ok(ReviewWorkBindings {
        omitted_references: reference_count.saturating_sub(references.len() as u64),
        references,
        bound_operations,
        unknown_operations: number(&coverage, "operations")?.saturating_sub(bound_operations),
        ambiguous_operations: number(&coverage, "ambiguous_operations")?,
        boundary_operations: number(&coverage, "boundary_operations")?,
        multi_repository_operations: number(&coverage, "multi_repository_operations")?,
        unknown_workstream_operations: number(&coverage, "unknown_workstream_operations")?,
        basis: "Metadata-only thread/start-time correlation, not token or duration allocation. Latest activation must precede operation start; same-tick, conflicting, and multi-repository associations stay unknown. At most eight references within a byte budget; narrow the window for omitted references.",
    })
}
