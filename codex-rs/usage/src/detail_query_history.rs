use crate::AccountProfileRef;
use crate::Activity;
use crate::ActivityState;
use crate::AttributionProvenance;
use crate::CoverageReasonCode;
use crate::CoverageScopeKind;
use crate::CoverageState;
use crate::Phase;
use crate::RepositoryId;
use crate::SafeRepositoryLabel;
use crate::ThreadId;
use crate::ThreadSourceKind;
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
use super::detail_query_support::operation;
use super::detail_query_support::optional_uuid;
use super::detail_query_support::page_from_rows;
use super::detail_query_support::positive;
use super::detail_query_support::required_enum;
use super::detail_query_support::thread;
use super::detail_query_support::uuid;

pub(crate) async fn list_history_details(
    store: &UsageStore,
    kind: UsageDetailKind,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    match kind {
        UsageDetailKind::Classifications => classifications(store, query).await,
        UsageDetailKind::Coverage => coverage(store, query).await,
        UsageDetailKind::ActivitySpans => activity_spans(store, query).await,
        UsageDetailKind::LifecycleEvents => lifecycle_events(store, query).await,
        UsageDetailKind::RepositoryIdentities => {
            super::detail_query_repository::list_repository_identities(store, query).await
        }
        UsageDetailKind::RepositoryEvents => repository_events(store, query).await,
        UsageDetailKind::Taxonomies => taxonomies(store, query).await,
        _ => Err(UsageStoreError::InvalidFact),
    }
}

async fn classifications(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let repository = canonical_repository(store, query).await?;
    let rows = sqlx::query(
        r#"
        SELECT classification.*,
               NOT EXISTS(SELECT 1 FROM classification_events AS successor
                          WHERE successor.supersedes_event_id = classification.event_id)
                   AS effective
        FROM classification_events AS classification
        JOIN operations AS operation ON operation.id = classification.operation_id
        LEFT JOIN model_requests AS request ON request.operation_id = operation.id
        LEFT JOIN turns AS turn ON turn.id = operation.turn_id
        WHERE (? IS NULL OR operation.thread_id = ?)
          AND (? IS NULL OR COALESCE(request.account_profile_ref, turn.account_profile_ref) = ?)
          AND (? IS NULL OR classification.occurred_at_ms >= ?)
          AND (? IS NULL OR classification.occurred_at_ms < ?)
          AND (? IS NULL OR EXISTS (
              WITH RECURSIVE family(id) AS (
                  SELECT ? UNION ALL SELECT merge.source_repository_id FROM family
                  JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
              ) SELECT 1 FROM repository_attributions AS attribution
                WHERE attribution.operation_id = operation.id
                  AND attribution.repository_id IN (SELECT id FROM family)
          ))
          AND (? IS NULL OR classification.occurred_at_ms < ?
               OR (classification.occurred_at_ms = ? AND classification.event_id > ?))
        ORDER BY classification.occurred_at_ms DESC, classification.event_id ASC LIMIT ?
        "#,
    )
    .bind(query.thread_id.as_ref().map(ThreadId::as_str))
    .bind(query.thread_id.as_ref().map(ThreadId::as_str))
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
        Ok(UsageDetailRecord::Classification {
            event_id: uuid(row.get("event_id"))?,
            operation_id: operation(row.get("operation_id"))?,
            taxonomy_version: positive(row.get("taxonomy_version"))?,
            phase: required_enum(row.get("phase"), Phase::parse, Phase::as_str)?,
            activity: required_enum(row.get("activity"), Activity::parse, Activity::as_str)?,
            activity_state: required_enum(
                row.get("activity_state"),
                ActivityState::parse,
                ActivityState::as_str,
            )?,
            provenance: required_enum(
                row.get("provenance"),
                AttributionProvenance::parse,
                AttributionProvenance::as_str,
            )?,
            supersedes_event_id: optional_uuid(row.get("supersedes_event_id"))?,
            occurred_at_ms: row.get("occurred_at_ms"),
            effective: row.get("effective"),
        })
    })
}

async fn coverage(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let repository = canonical_repository(store, query).await?;
    let rows = sqlx::query(
        r#"
        SELECT coverage.* FROM coverage_events AS coverage
        LEFT JOIN operations AS operation ON operation.id = coverage.operation_id
        LEFT JOIN model_requests AS request ON request.operation_id = operation.id
        LEFT JOIN turns AS turn ON turn.id = operation.turn_id
        WHERE (? IS NULL OR operation.thread_id = ?)
          AND (? IS NULL OR COALESCE(request.account_profile_ref, turn.account_profile_ref) = ?)
          AND (? IS NULL OR coverage.occurred_at_ms >= ?)
          AND (? IS NULL OR coverage.occurred_at_ms < ?)
          AND (? IS NULL OR EXISTS (
              WITH RECURSIVE family(id) AS (
                  SELECT ? UNION ALL SELECT merge.source_repository_id FROM family
                  JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
              ) SELECT 1 FROM repository_attributions AS attribution
                WHERE attribution.operation_id = operation.id
                  AND attribution.repository_id IN (SELECT id FROM family)
          ))
          AND (? IS NULL OR coverage.occurred_at_ms < ?
               OR (coverage.occurred_at_ms = ? AND coverage.event_id > ?))
        ORDER BY coverage.occurred_at_ms DESC, coverage.event_id ASC LIMIT ?
        "#,
    )
    .bind(query.thread_id.as_ref().map(ThreadId::as_str))
    .bind(query.thread_id.as_ref().map(ThreadId::as_str))
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
        Ok(UsageDetailRecord::Coverage {
            event_id: uuid(row.get("event_id"))?,
            operation_id: row
                .get::<Option<String>, _>("operation_id")
                .map(operation)
                .transpose()?,
            scope_kind: CoverageScopeKind::new(row.get::<String, _>("scope_kind"))
                .map_err(|_| UsageStoreError::InvalidFact)?
                .as_str()
                .to_string(),
            coverage: required_enum(
                row.get("coverage_state"),
                CoverageState::parse,
                CoverageState::as_str,
            )?,
            reason_code: row
                .get::<Option<String>, _>("reason_code")
                .map(CoverageReasonCode::new)
                .transpose()
                .map_err(|_| UsageStoreError::InvalidFact)?
                .map(|reason| reason.as_str().to_string()),
            occurred_at_ms: row.get("occurred_at_ms"),
        })
    })
}

async fn activity_spans(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let repository = canonical_repository(store, query).await?;
    let rows = sqlx::query(
        r#"
        SELECT span.*, ended.occurred_at_ms AS ended_at_ms,
               (SELECT COUNT(*) FROM activity_span_events AS heartbeat
                WHERE heartbeat.activity_span_id = span.id
                  AND heartbeat.event_kind = 'heartbeat') AS heartbeat_count
        FROM activity_spans AS span
        JOIN operations AS operation ON operation.id = span.operation_id
        LEFT JOIN model_requests AS request ON request.operation_id = operation.id
        LEFT JOIN turns AS turn ON turn.id = operation.turn_id
        LEFT JOIN activity_span_events AS ended
          ON ended.activity_span_id = span.id AND ended.event_kind = 'ended'
        WHERE (? IS NULL OR operation.thread_id = ?)
          AND (? IS NULL OR COALESCE(request.account_profile_ref, turn.account_profile_ref) = ?)
          AND (? IS NULL OR span.started_at_ms >= ?)
          AND (? IS NULL OR span.started_at_ms < ?)
          AND (? IS NULL OR EXISTS (
              WITH RECURSIVE family(id) AS (
                  SELECT ? UNION ALL SELECT merge.source_repository_id FROM family
                  JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
              ) SELECT 1 FROM repository_attributions AS attribution
                WHERE attribution.operation_id = operation.id
                  AND attribution.repository_id IN (SELECT id FROM family)
          ))
          AND (? IS NULL OR span.started_at_ms < ?
               OR (span.started_at_ms = ? AND span.id > ?))
        ORDER BY span.started_at_ms DESC, span.id ASC LIMIT ?
        "#,
    )
    .bind(query.thread_id.as_ref().map(ThreadId::as_str))
    .bind(query.thread_id.as_ref().map(ThreadId::as_str))
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
    page_from_rows(&rows, query, "started_at_ms", "id", |row| {
        let heartbeat_count = row
            .get::<i64, _>("heartbeat_count")
            .try_into()
            .map_err(|_| UsageStoreError::InvalidFact)?;
        Ok(UsageDetailRecord::ActivitySpan {
            id: uuid(row.get("id"))?,
            operation_id: operation(row.get("operation_id"))?,
            activity_state: required_enum(
                row.get("activity_state"),
                ActivityState::parse,
                ActivityState::as_str,
            )?,
            started_at_ms: row.get("started_at_ms"),
            ended_at_ms: row.get("ended_at_ms"),
            heartbeat_count,
        })
    })
}

async fn lifecycle_events(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let rows = sqlx::query(
        r#"
        SELECT lifecycle.*,
               lifecycle.owner_type || ':' || lifecycle.event_id AS cursor_id FROM (
            SELECT event_id, 'process' AS owner_type, process_id AS owner_id,
                   NULL AS thread_id, event_kind, occurred_at_ms FROM process_events
            UNION ALL SELECT event_id, 'thread', thread_id, thread_id,
                   event_kind, occurred_at_ms FROM thread_events
            UNION ALL SELECT event.event_id, 'turn', event.turn_id, turn.thread_id,
                   event.event_kind, event.occurred_at_ms FROM turn_events AS event
                   JOIN turns AS turn ON turn.id = event.turn_id
            UNION ALL SELECT event.event_id, 'agent', event.agent_id, agent.thread_id,
                   event.event_kind, event.occurred_at_ms FROM agent_events AS event
                   JOIN agents AS agent ON agent.id = event.agent_id
            UNION ALL SELECT event.event_id, 'activity_span', event.activity_span_id,
                   operation.thread_id, event.event_kind, event.occurred_at_ms
                   FROM activity_span_events AS event
                   JOIN activity_spans AS span ON span.id = event.activity_span_id
                   JOIN operations AS operation ON operation.id = span.operation_id
        ) AS lifecycle
        WHERE (? IS NULL OR lifecycle.thread_id = ?)
          AND (? IS NULL OR lifecycle.occurred_at_ms >= ?)
          AND (? IS NULL OR lifecycle.occurred_at_ms < ?)
          AND (? IS NULL OR lifecycle.occurred_at_ms < ?
               OR (lifecycle.occurred_at_ms = ?
                   AND (lifecycle.owner_type || ':' || lifecycle.event_id) > ?))
        ORDER BY lifecycle.occurred_at_ms DESC, cursor_id ASC LIMIT ?
        "#,
    )
    .bind(query.thread_id.as_ref().map(ThreadId::as_str))
    .bind(query.thread_id.as_ref().map(ThreadId::as_str))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_id(query))
    .bind(fetch_limit(query)?)
    .fetch_all(&store.pool)
    .await
    .map_err(UsageStoreError::Database)?;
    page_from_rows(&rows, query, "occurred_at_ms", "cursor_id", |row| {
        Ok(UsageDetailRecord::LifecycleEvent {
            event_id: uuid(row.get("event_id"))?,
            owner_type: fixed(
                row.get("owner_type"),
                &["process", "thread", "turn", "agent", "activity_span"],
            )?,
            owner_id: bounded_id(row.get("owner_id"))?,
            thread_id: row
                .get::<Option<String>, _>("thread_id")
                .map(thread)
                .transpose()?,
            event: bounded_id(row.get("event_kind"))?,
            occurred_at_ms: row.get("occurred_at_ms"),
        })
    })
}

async fn repository_events(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let rows = sqlx::query(
        r#"
        SELECT event.*,
               event.event_kind || ':' || event.event_id AS cursor_id FROM (
            SELECT event_id, 'seen' AS event_kind, repository_id,
                   NULL AS target_repository_id, NULL AS safe_alias, occurred_at_ms
                   FROM repository_seen_events
            UNION ALL SELECT event_id, 'alias', repository_id, NULL, safe_alias, occurred_at_ms
                   FROM repository_alias_events
            UNION ALL SELECT event_id, 'merge', source_repository_id,
                   target_repository_id, NULL, occurred_at_ms FROM repository_merge_events
        ) AS event
        WHERE (? IS NULL OR event.repository_id = ? OR event.target_repository_id = ?)
          AND (? IS NULL OR event.occurred_at_ms >= ?)
          AND (? IS NULL OR event.occurred_at_ms < ?)
          AND (? IS NULL OR event.occurred_at_ms < ?
               OR (event.occurred_at_ms = ?
                   AND (event.event_kind || ':' || event.event_id) > ?))
        ORDER BY event.occurred_at_ms DESC, cursor_id ASC LIMIT ?
        "#,
    )
    .bind(query.repository_id.as_ref().map(RepositoryId::as_str))
    .bind(query.repository_id.as_ref().map(RepositoryId::as_str))
    .bind(query.repository_id.as_ref().map(RepositoryId::as_str))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_id(query))
    .bind(fetch_limit(query)?)
    .fetch_all(&store.pool)
    .await
    .map_err(UsageStoreError::Database)?;
    page_from_rows(&rows, query, "occurred_at_ms", "cursor_id", |row| {
        Ok(UsageDetailRecord::RepositoryEvent {
            event_id: uuid(row.get("event_id"))?,
            event: fixed(row.get("event_kind"), &["seen", "alias", "merge"])?,
            repository_id: repository(row.get("repository_id"))?,
            target_repository_id: row
                .get::<Option<String>, _>("target_repository_id")
                .map(repository)
                .transpose()?,
            safe_alias: row
                .get::<Option<String>, _>("safe_alias")
                .map(safe_label)
                .transpose()?,
            occurred_at_ms: row.get("occurred_at_ms"),
        })
    })
}

async fn taxonomies(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let rows = sqlx::query(
        r#"
        SELECT version, schema_migration, mapping_key, supersedes_version,
               version AS cursor_at, mapping_key AS cursor_id
        FROM taxonomy_versions
        WHERE (? IS NULL OR version < ? OR (version = ? AND mapping_key > ?))
        ORDER BY version DESC, mapping_key ASC LIMIT ?
        "#,
    )
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_id(query))
    .bind(fetch_limit(query)?)
    .fetch_all(&store.pool)
    .await
    .map_err(UsageStoreError::Database)?;
    page_from_rows(&rows, query, "cursor_at", "cursor_id", |row| {
        Ok(UsageDetailRecord::Taxonomy {
            version: positive(row.get("version"))?,
            schema_migration: positive(row.get("schema_migration"))?,
            mapping_key: bounded_id(row.get("mapping_key"))?,
            supersedes_version: row
                .get::<Option<i64>, _>("supersedes_version")
                .map(positive)
                .transpose()?,
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

fn repository(value: String) -> Result<String, UsageStoreError> {
    RepositoryId::new(value)
        .map(|id| id.as_str().to_string())
        .map_err(|_| UsageStoreError::InvalidFact)
}

fn safe_label(value: String) -> Result<String, UsageStoreError> {
    SafeRepositoryLabel::new(value)
        .map(|label| label.as_str().to_string())
        .map_err(|_| UsageStoreError::InvalidFact)
}

fn bounded_id(value: String) -> Result<String, UsageStoreError> {
    ThreadSourceKind::new(value)
        .map(|value| value.as_str().to_string())
        .map_err(|_| UsageStoreError::InvalidFact)
}

fn fixed(value: String, allowed: &[&str]) -> Result<String, UsageStoreError> {
    allowed
        .contains(&value.as_str())
        .then_some(value)
        .ok_or(UsageStoreError::InvalidFact)
}
