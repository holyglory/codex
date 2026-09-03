use crate::AccountProfileRef;
use crate::ApprovalOutcome;
use crate::ApprovalProvenance;
use crate::CoverageState;
use crate::MeasurementProvenance;
use crate::RepositoryId;
use crate::TokenCategoryPath;
use crate::TokenUnit;
use crate::UsageDetailKind;
use crate::UsageDetailListQuery;
use crate::UsageDetailRecord;
use crate::UsagePage;
use crate::UsageStore;
use crate::UsageStoreError;
use sqlx::Row;

use super::detail_query_support::cursor_at;
use super::detail_query_support::cursor_id;
use super::detail_query_support::fetch_limit;
use super::detail_query_support::model_request;
use super::detail_query_support::operation;
use super::detail_query_support::page_from_rows;
use super::detail_query_support::tool_invocation;
use super::detail_query_support::uuid;

pub(crate) async fn list_fact_details(
    store: &UsageStore,
    kind: UsageDetailKind,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    match kind {
        UsageDetailKind::Tokens => tokens(store, query).await,
        UsageDetailKind::Approvals => approvals(store, query).await,
        UsageDetailKind::RepositoryAttributions => repository_attributions(store, query).await,
        _ => super::detail_query_history::list_history_details(store, kind, query).await,
    }
}

async fn tokens(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let repository = canonical_repository(store, query).await?;
    let rows = sqlx::query(
        r#"
        SELECT token.*, operation.thread_id,
               COALESCE(operation_request.account_profile_ref, turn.account_profile_ref)
                   AS effective_account
        FROM token_observations AS token
        LEFT JOIN model_requests AS source_request ON source_request.id = token.model_request_id
        LEFT JOIN tool_invocations AS source_tool ON source_tool.id = token.tool_invocation_id
        JOIN operations AS operation
          ON operation.id = COALESCE(source_request.operation_id, source_tool.operation_id)
        LEFT JOIN model_requests AS operation_request
          ON operation_request.operation_id = operation.id
        LEFT JOIN turns AS turn ON turn.id = operation.turn_id
        WHERE (? IS NULL OR operation.thread_id = ?)
          AND (? IS NULL OR COALESCE(operation_request.account_profile_ref, turn.account_profile_ref) = ?)
          AND (? IS NULL OR token.observed_at_ms >= ?)
          AND (? IS NULL OR token.observed_at_ms < ?)
          AND (? IS NULL OR EXISTS (
              WITH RECURSIVE family(id) AS (
                  SELECT ? UNION ALL SELECT merge.source_repository_id FROM family
                  JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
              )
              SELECT 1 FROM repository_attributions AS attribution
              WHERE attribution.operation_id = operation.id
                AND attribution.repository_id IN (SELECT id FROM family)
          ))
          AND (? IS NULL OR token.observed_at_ms < ?
               OR (token.observed_at_ms = ? AND token.id > ?))
        ORDER BY token.observed_at_ms DESC, token.id ASC LIMIT ?
        "#,
    )
    .bind(query.thread_id.as_ref().map(crate::ThreadId::as_str))
    .bind(query.thread_id.as_ref().map(crate::ThreadId::as_str))
    .bind(query.account_profile_ref.as_ref().map(AccountProfileRef::as_str))
    .bind(query.account_profile_ref.as_ref().map(AccountProfileRef::as_str))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(repository.as_ref().map(RepositoryId::as_str))
    .bind(repository.as_ref().map(RepositoryId::as_str))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_id(query))
    .bind(fetch_limit(query)?)
    .fetch_all(&store.pool)
    .await
    .map_err(UsageStoreError::Database)?;
    page_from_rows(&rows, query, "observed_at_ms", "id", |row| {
        let bucket = row.get::<String, _>("repository_bucket");
        if !matches!(bucket.as_str(), "multi_repo" | "unknown") {
            RepositoryId::new(bucket.clone()).map_err(|_| UsageStoreError::InvalidFact)?;
        }
        Ok(UsageDetailRecord::Token {
            id: uuid(row.get("id"))?,
            model_request_id: row
                .get::<Option<String>, _>("model_request_id")
                .map(model_request)
                .transpose()?,
            tool_invocation_id: row
                .get::<Option<String>, _>("tool_invocation_id")
                .map(tool_invocation)
                .transpose()?,
            source_event_id: uuid(row.get("source_event_id"))?,
            category: TokenCategoryPath::new(row.get::<String, _>("category_path"))
                .map_err(|_| UsageStoreError::InvalidFact)?
                .as_str()
                .to_string(),
            count: row.get("token_count"),
            unit: required_enum(row.get("unit"), TokenUnit::parse, TokenUnit::as_str)?,
            measurement_provenance: required_enum(
                row.get("measurement_provenance"),
                MeasurementProvenance::parse,
                MeasurementProvenance::as_str,
            )?,
            coverage: required_enum(
                row.get("coverage_state"),
                CoverageState::parse,
                CoverageState::as_str,
            )?,
            repository_bucket: bucket,
            observed_at_ms: row.get("observed_at_ms"),
        })
    })
}

async fn approvals(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let repository = canonical_repository(store, query).await?;
    let rows = sqlx::query(
        r#"
        SELECT approval.* FROM tool_approval_events AS approval
        JOIN tool_invocations AS tool ON tool.id = approval.tool_invocation_id
        JOIN operations AS operation ON operation.id = tool.operation_id
        LEFT JOIN model_requests AS request ON request.operation_id = operation.id
        LEFT JOIN turns AS turn ON turn.id = operation.turn_id
        WHERE (? IS NULL OR operation.thread_id = ?)
          AND (? IS NULL OR COALESCE(request.account_profile_ref, turn.account_profile_ref) = ?)
          AND (? IS NULL OR approval.occurred_at_ms >= ?)
          AND (? IS NULL OR approval.occurred_at_ms < ?)
          AND (? IS NULL OR EXISTS (
              WITH RECURSIVE family(id) AS (
                  SELECT ? UNION ALL SELECT merge.source_repository_id FROM family
                  JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
              ) SELECT 1 FROM repository_attributions AS attribution
                WHERE attribution.operation_id = operation.id
                  AND attribution.repository_id IN (SELECT id FROM family)
          ))
          AND (? IS NULL OR approval.occurred_at_ms < ?
               OR (approval.occurred_at_ms = ? AND approval.event_id > ?))
        ORDER BY approval.occurred_at_ms DESC, approval.event_id ASC LIMIT ?
        "#,
    )
    .bind(query.thread_id.as_ref().map(crate::ThreadId::as_str))
    .bind(query.thread_id.as_ref().map(crate::ThreadId::as_str))
    .bind(
        query
            .account_profile_ref
            .as_ref()
            .map(AccountProfileRef::as_str),
    )
    .bind(
        query
            .account_profile_ref
            .as_ref()
            .map(AccountProfileRef::as_str),
    )
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(repository.as_ref().map(RepositoryId::as_str))
    .bind(repository.as_ref().map(RepositoryId::as_str))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_id(query))
    .bind(fetch_limit(query)?)
    .fetch_all(&store.pool)
    .await
    .map_err(UsageStoreError::Database)?;
    page_from_rows(&rows, query, "occurred_at_ms", "event_id", |row| {
        Ok(UsageDetailRecord::Approval {
            event_id: uuid(row.get("event_id"))?,
            tool_invocation_id: tool_invocation(row.get("tool_invocation_id"))?,
            outcome: required_enum(
                row.get("outcome"),
                ApprovalOutcome::parse,
                ApprovalOutcome::as_str,
            )?,
            provenance: required_enum(
                row.get("provenance"),
                ApprovalProvenance::parse,
                ApprovalProvenance::as_str,
            )?,
            occurred_at_ms: row.get("occurred_at_ms"),
        })
    })
}

async fn repository_attributions(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let repository = canonical_repository(store, query).await?;
    let rows = sqlx::query(
        r#"
        SELECT attribution.* FROM repository_attributions AS attribution
        JOIN operations AS operation ON operation.id = attribution.operation_id
        LEFT JOIN model_requests AS request ON request.operation_id = operation.id
        LEFT JOIN turns AS turn ON turn.id = operation.turn_id
        WHERE (? IS NULL OR operation.thread_id = ?)
          AND (? IS NULL OR COALESCE(request.account_profile_ref, turn.account_profile_ref) = ?)
          AND (? IS NULL OR attribution.occurred_at_ms >= ?)
          AND (? IS NULL OR attribution.occurred_at_ms < ?)
          AND (? IS NULL OR attribution.repository_id IN (
              WITH RECURSIVE family(id) AS (
                  SELECT ? UNION ALL SELECT merge.source_repository_id FROM family
                  JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
              ) SELECT id FROM family
          ))
          AND (? IS NULL OR attribution.occurred_at_ms < ?
               OR (attribution.occurred_at_ms = ? AND attribution.event_id > ?))
        ORDER BY attribution.occurred_at_ms DESC, attribution.event_id ASC LIMIT ?
        "#,
    )
    .bind(query.thread_id.as_ref().map(crate::ThreadId::as_str))
    .bind(query.thread_id.as_ref().map(crate::ThreadId::as_str))
    .bind(
        query
            .account_profile_ref
            .as_ref()
            .map(AccountProfileRef::as_str),
    )
    .bind(
        query
            .account_profile_ref
            .as_ref()
            .map(AccountProfileRef::as_str),
    )
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(repository.as_ref().map(RepositoryId::as_str))
    .bind(repository.as_ref().map(RepositoryId::as_str))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_id(query))
    .bind(fetch_limit(query)?)
    .fetch_all(&store.pool)
    .await
    .map_err(UsageStoreError::Database)?;
    page_from_rows(&rows, query, "occurred_at_ms", "event_id", |row| {
        let kind = fixed(
            row.get("attribution_kind"),
            &[
                "primary",
                "observed_cwd",
                "file_change",
                "multi_repo",
                "unknown",
            ],
        )?;
        let provenance = fixed(
            row.get("provenance"),
            &["runtime_observed", "imported", "unknown"],
        )?;
        Ok(UsageDetailRecord::RepositoryAttribution {
            event_id: uuid(row.get("event_id"))?,
            operation_id: operation(row.get("operation_id"))?,
            repository_id: row
                .get::<Option<String>, _>("repository_id")
                .map(|id| RepositoryId::new(id).map(|id| id.as_str().to_string()))
                .transpose()
                .map_err(|_| UsageStoreError::InvalidFact)?,
            attribution_kind: kind,
            provenance,
            occurred_at_ms: row.get("occurred_at_ms"),
        })
    })
}

async fn canonical_repository(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<Option<RepositoryId>, UsageStoreError> {
    match &query.repository_id {
        Some(repository) => store.canonical_repository_id(repository).await.map(Some),
        None => Ok(None),
    }
}

fn required_enum<T: Copy>(
    value: String,
    parse: impl Fn(&str) -> Option<T>,
    display: impl Fn(T) -> &'static str,
) -> Result<String, UsageStoreError> {
    super::detail_query_support::required_enum(value, parse, display)
}

fn fixed(value: String, allowed: &[&str]) -> Result<String, UsageStoreError> {
    allowed
        .contains(&value.as_str())
        .then_some(value)
        .ok_or(UsageStoreError::InvalidFact)
}
