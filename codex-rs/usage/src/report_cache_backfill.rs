//! Restartable pages of derived report history. New facts are handled by the
//! report triggers while these pages cover only the captured historical frontier.
use sqlx::SqliteConnection;
use sqlx::SqlitePool;

const PAGE_ROWS: i64 = 1_024;
const SOURCES: [&str; 4] = ["operations", "token_observations", "coverage_events", "activity_spans"];

const OPERATIONS: &str = r#"
INSERT INTO _usage_report_operations(
    operation_id, operation_kind, agent_id, started_at_ms,
    ended_at_ms, terminal_status, phase, activity,
    activity_state, attribution_provenance
)
SELECT operation.id, operation.operation_kind, operation.agent_id,
       operation.started_at_ms, terminal.occurred_at_ms, terminal.event_kind,
       COALESCE(effective.phase, operation.phase),
       COALESCE(effective.activity, operation.activity),
       COALESCE(effective.activity_state, operation.activity_state),
       COALESCE(effective.provenance, operation.attribution_provenance)
FROM operations AS operation
LEFT JOIN operation_events AS terminal
  ON terminal.operation_id = operation.id AND terminal.terminal = 1
LEFT JOIN effective_classification_events AS effective
  ON effective.operation_id = operation.id
WHERE operation.rowid > ?1 AND operation.rowid <= ?2;
"#;

const TOKENS_0: &str = r#"
INSERT INTO _usage_report_token_aggregates(
    category_path, repository_bucket, measurement_provenance,
    measured_tokens, unknown_observations, observation_count,
    has_gap, aggregate_overflow
)
SELECT token.category_path, token.repository_bucket,
       token.measurement_provenance, COALESCE(token.token_count, 0),
       token.token_count IS NULL, 1, token.coverage_state <> 'complete', 0
FROM token_observations AS token
WHERE token.rowid > ?1 AND token.rowid <= ?2
  AND token.category_path NOT GLOB 'attribution.items.*'
ON CONFLICT(category_path, repository_bucket, measurement_provenance)
DO UPDATE SET
    measured_tokens = CASE
        WHEN _usage_report_token_aggregates.aggregate_overflow = 1
          OR _usage_report_token_aggregates.measured_tokens
             > 9223372036854775807 - excluded.measured_tokens
        THEN _usage_report_token_aggregates.measured_tokens
        ELSE _usage_report_token_aggregates.measured_tokens + excluded.measured_tokens
    END,
    unknown_observations =
        _usage_report_token_aggregates.unknown_observations
        + excluded.unknown_observations,
    observation_count =
        _usage_report_token_aggregates.observation_count
        + excluded.observation_count,
    has_gap = MAX(_usage_report_token_aggregates.has_gap, excluded.has_gap),
    aggregate_overflow =
        _usage_report_token_aggregates.aggregate_overflow = 1
        OR _usage_report_token_aggregates.measured_tokens
           > 9223372036854775807 - excluded.measured_tokens
"#;

const TOKENS_1: &str = r#"
INSERT INTO _usage_report_activity_tokens(
    operation_id, measured_tokens, unknown_observations,
    has_gap, aggregate_overflow
)
SELECT COALESCE(request.operation_id, tool.operation_id),
       COALESCE(token.token_count, 0), token.token_count IS NULL,
       token.coverage_state <> 'complete', 0
FROM token_observations AS token
LEFT JOIN model_requests AS request ON request.id = token.model_request_id
LEFT JOIN tool_invocations AS tool ON tool.id = token.tool_invocation_id
WHERE token.rowid > ?1 AND token.rowid <= ?2
  AND token.category_path = 'total_tokens'
  AND token.measurement_provenance = 'provider_reported'
ON CONFLICT(operation_id) DO UPDATE SET
    measured_tokens = CASE
        WHEN _usage_report_activity_tokens.aggregate_overflow = 1
          OR _usage_report_activity_tokens.measured_tokens
             > 9223372036854775807 - excluded.measured_tokens
        THEN _usage_report_activity_tokens.measured_tokens
        ELSE _usage_report_activity_tokens.measured_tokens + excluded.measured_tokens
    END,
    unknown_observations =
        _usage_report_activity_tokens.unknown_observations
        + excluded.unknown_observations,
    has_gap = MAX(_usage_report_activity_tokens.has_gap, excluded.has_gap),
    aggregate_overflow =
        _usage_report_activity_tokens.aggregate_overflow = 1
        OR _usage_report_activity_tokens.measured_tokens
           > 9223372036854775807 - excluded.measured_tokens
"#;

const TOKENS_2: &str = r#"
INSERT INTO _usage_report_token_coverage(coverage_state, observation_count)
SELECT coverage_state, COUNT(*)
FROM token_observations
WHERE rowid > ?1 AND rowid <= ?2
  AND category_path NOT GLOB 'attribution.items.*'
GROUP BY coverage_state
ON CONFLICT(coverage_state) DO UPDATE SET
observation_count = _usage_report_token_coverage.observation_count + excluded.observation_count
"#;

const COVERAGE: &str = r#"
INSERT INTO _usage_report_coverage(coverage_state, observation_count)
SELECT coverage_state, COUNT(*) FROM coverage_events
WHERE rowid > ?1 AND rowid <= ?2 GROUP BY coverage_state
ON CONFLICT(coverage_state) DO UPDATE SET
observation_count = _usage_report_coverage.observation_count + excluded.observation_count;
"#;

const SPANS: &str = r#"
INSERT INTO _usage_report_spans(
    span_id, operation_id, activity_state, started_at_ms, ended_at_ms
)
SELECT span.id, span.operation_id, span.activity_state, span.started_at_ms,
       ended.occurred_at_ms
FROM activity_spans AS span
LEFT JOIN activity_span_events AS ended
  ON ended.activity_span_id = span.id AND ended.event_kind = 'ended'
WHERE span.rowid > ?1 AND span.rowid <= ?2;
"#;

pub(super) async fn initialize(connection: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query("CREATE TABLE _usage_report_backfill (source TEXT PRIMARY KEY NOT NULL, cursor INTEGER NOT NULL, high_water INTEGER NOT NULL) STRICT")
        .execute(&mut *connection).await?;
    for source in SOURCES {
        // Source names are a closed internal list, never caller input.
        sqlx::QueryBuilder::<sqlx::Sqlite>::new("INSERT INTO _usage_report_backfill SELECT ")
            .push_bind(source).push(", 0, COALESCE(MAX(rowid), 0) FROM ").push(source)
            .build().execute(&mut *connection).await?;
    }
    Ok(())
}

/// Commits at most one page. Transaction drop rolls back both the page and its
/// cursor on cancellation, so another opener can resume without double counting.
pub(crate) async fn step(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let ready: i64 = sqlx::query_scalar("SELECT ready FROM _usage_report_cache_meta WHERE singleton = 1")
        .fetch_one(&mut *tx).await?;
    if ready == 1 {
        return Ok(true);
    }
    let pending: Option<(String, i64, i64)> = sqlx::query_as("SELECT source, cursor, high_water FROM _usage_report_backfill WHERE cursor < high_water ORDER BY source LIMIT 1")
        .fetch_optional(&mut *tx).await?;
    let Some((source, cursor, high_water)) = pending else {
        sqlx::query("UPDATE _usage_report_cache_meta SET ready = 1 WHERE singleton = 1")
            .execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(true);
    };
    let (source, statements): (&'static str, &[&'static str]) = match source.as_str() {
        "operations" => ("operations", &[OPERATIONS]),
        "token_observations" => ("token_observations", &[TOKENS_0, TOKENS_1, TOKENS_2, super::token_hours::BACKFILL]),
        "coverage_events" => ("coverage_events", &[COVERAGE]),
        "activity_spans" => ("activity_spans", &[SPANS]),
        _ => return Err(sqlx::Error::Protocol("unknown usage backfill source".into())),
    };
    let end: i64 = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT COALESCE(MAX(rowid), ")
        .push_bind(high_water).push(") FROM (SELECT rowid FROM ").push(source)
        .push(" WHERE rowid > ").push_bind(cursor).push(" AND rowid <= ").push_bind(high_water)
        .push(" ORDER BY rowid LIMIT ").push_bind(PAGE_ROWS).push(")")
        .build_query_scalar().fetch_one(&mut *tx).await?;
    for statement in statements {
        sqlx::query(*statement).bind(cursor).bind(end).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE _usage_report_backfill SET cursor = ? WHERE source = ?")
        .bind(end).bind(source).execute(&mut *tx).await?;
    tx.commit().await?;
    // This never waits for readers or truncates their snapshots. Short write
    // transactions let SQLite's normal WAL checkpoints make progress.
    sqlx::query("PRAGMA wal_checkpoint(PASSIVE)").execute(pool).await?;
    Ok(false)
}
