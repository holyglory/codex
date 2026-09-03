use sqlx::SqliteConnection;
use sqlx::SqlitePool;

const REPORT_CACHE_SCHEMA_VERSION: i64 = 1;
const CACHE_META_TABLE: &str = "_usage_report_cache_meta";

const RESET_CACHE_SQL: &str = r#"
DROP TRIGGER IF EXISTS _usage_report_operation_insert;
DROP TRIGGER IF EXISTS _usage_report_operation_terminal;
DROP TRIGGER IF EXISTS _usage_report_classification;
DROP TRIGGER IF EXISTS _usage_report_token;
DROP TRIGGER IF EXISTS _usage_report_activity_token;
DROP TRIGGER IF EXISTS _usage_report_coverage_insert;
DROP TRIGGER IF EXISTS _usage_report_span_insert;
DROP TRIGGER IF EXISTS _usage_report_span_end;
DROP TABLE IF EXISTS _usage_report_cache_meta;
DROP TABLE IF EXISTS _usage_report_operations;
DROP TABLE IF EXISTS _usage_report_token_aggregates;
DROP TABLE IF EXISTS _usage_report_activity_tokens;
DROP TABLE IF EXISTS _usage_report_token_coverage;
DROP TABLE IF EXISTS _usage_report_coverage;
DROP TABLE IF EXISTS _usage_report_spans;
"#;

const CREATE_CACHE_SQL: &str = r#"
CREATE TABLE _usage_report_cache_meta (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    schema_version INTEGER NOT NULL,
    ready INTEGER NOT NULL CHECK (ready IN (0, 1))
) STRICT;

CREATE TABLE _usage_report_operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    operation_kind TEXT NOT NULL,
    agent_id TEXT,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    terminal_status TEXT,
    phase TEXT NOT NULL,
    activity TEXT NOT NULL,
    activity_state TEXT NOT NULL,
    attribution_provenance TEXT NOT NULL
) STRICT;

CREATE TABLE _usage_report_token_aggregates (
    category_path TEXT NOT NULL,
    repository_bucket TEXT NOT NULL,
    measurement_provenance TEXT NOT NULL,
    measured_tokens INTEGER NOT NULL,
    unknown_observations INTEGER NOT NULL,
    observation_count INTEGER NOT NULL,
    has_gap INTEGER NOT NULL CHECK (has_gap IN (0, 1)),
    aggregate_overflow INTEGER NOT NULL CHECK (aggregate_overflow IN (0, 1)),
    PRIMARY KEY (category_path, repository_bucket, measurement_provenance)
) STRICT;

CREATE TABLE _usage_report_activity_tokens (
    operation_id TEXT PRIMARY KEY NOT NULL,
    measured_tokens INTEGER NOT NULL,
    unknown_observations INTEGER NOT NULL,
    has_gap INTEGER NOT NULL CHECK (has_gap IN (0, 1)),
    aggregate_overflow INTEGER NOT NULL CHECK (aggregate_overflow IN (0, 1))
) STRICT;

CREATE TABLE _usage_report_token_coverage (
    coverage_state TEXT PRIMARY KEY NOT NULL,
    observation_count INTEGER NOT NULL
) STRICT;

CREATE TABLE _usage_report_coverage (
    coverage_state TEXT PRIMARY KEY NOT NULL,
    observation_count INTEGER NOT NULL
) STRICT;

CREATE TABLE _usage_report_spans (
    span_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL,
    activity_state TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER
) STRICT;

CREATE TRIGGER _usage_report_operation_insert AFTER INSERT ON operations
BEGIN
    INSERT INTO _usage_report_operations(
        operation_id, operation_kind, agent_id, started_at_ms,
        ended_at_ms, terminal_status, phase, activity,
        activity_state, attribution_provenance
    ) VALUES (
        NEW.id, NEW.operation_kind, NEW.agent_id, NEW.started_at_ms,
        NULL, NULL, NEW.phase, NEW.activity,
        NEW.activity_state, NEW.attribution_provenance
    );
END;

CREATE TRIGGER _usage_report_operation_terminal AFTER INSERT ON operation_events
WHEN NEW.terminal = 1
BEGIN
    UPDATE _usage_report_operations
    SET ended_at_ms = NEW.occurred_at_ms,
        terminal_status = NEW.event_kind
    WHERE operation_id = NEW.operation_id;
END;

CREATE TRIGGER _usage_report_classification AFTER INSERT ON classification_events
BEGIN
    UPDATE _usage_report_operations
    SET phase = NEW.phase,
        activity = NEW.activity,
        activity_state = NEW.activity_state,
        attribution_provenance = NEW.provenance
    WHERE operation_id = NEW.operation_id;
END;

CREATE TRIGGER _usage_report_token AFTER INSERT ON token_observations
WHEN NEW.category_path NOT GLOB 'attribution.items.*'
BEGIN
    INSERT INTO _usage_report_token_aggregates(
        category_path, repository_bucket, measurement_provenance,
        measured_tokens, unknown_observations, observation_count,
        has_gap, aggregate_overflow
    ) VALUES (
        NEW.category_path, NEW.repository_bucket,
        NEW.measurement_provenance, COALESCE(NEW.token_count, 0),
        NEW.token_count IS NULL, 1, NEW.coverage_state <> 'complete', 0
    ) ON CONFLICT(category_path, repository_bucket, measurement_provenance)
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
               > 9223372036854775807 - excluded.measured_tokens;

    INSERT INTO _usage_report_token_coverage(coverage_state, observation_count)
    VALUES (NEW.coverage_state, 1)
    ON CONFLICT(coverage_state) DO UPDATE SET
        observation_count = _usage_report_token_coverage.observation_count + 1;
END;

CREATE TRIGGER _usage_report_activity_token AFTER INSERT ON token_observations
WHEN NEW.category_path = 'total_tokens'
  AND NEW.measurement_provenance = 'provider_reported'
BEGIN
    INSERT INTO _usage_report_activity_tokens(
        operation_id, measured_tokens, unknown_observations,
        has_gap, aggregate_overflow
    ) VALUES (
        COALESCE(
            (SELECT operation_id FROM model_requests WHERE id = NEW.model_request_id),
            (SELECT operation_id FROM tool_invocations WHERE id = NEW.tool_invocation_id)
        ),
        COALESCE(NEW.token_count, 0), NEW.token_count IS NULL,
        NEW.coverage_state <> 'complete', 0
    ) ON CONFLICT(operation_id) DO UPDATE SET
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
               > 9223372036854775807 - excluded.measured_tokens;
END;

CREATE TRIGGER _usage_report_coverage_insert AFTER INSERT ON coverage_events
BEGIN
    INSERT INTO _usage_report_coverage(coverage_state, observation_count)
    VALUES (NEW.coverage_state, 1)
    ON CONFLICT(coverage_state) DO UPDATE SET
        observation_count = _usage_report_coverage.observation_count + 1;
END;

CREATE TRIGGER _usage_report_span_insert AFTER INSERT ON activity_spans
BEGIN
    INSERT INTO _usage_report_spans(
        span_id, operation_id, activity_state, started_at_ms, ended_at_ms
    ) VALUES (
        NEW.id, NEW.operation_id, NEW.activity_state, NEW.started_at_ms, NULL
    );
END;

CREATE TRIGGER _usage_report_span_end AFTER INSERT ON activity_span_events
WHEN NEW.event_kind = 'ended'
BEGIN
    UPDATE _usage_report_spans
    SET ended_at_ms = NEW.occurred_at_ms
    WHERE span_id = NEW.activity_span_id;
END;
"#;

const BACKFILL_CACHE_SQL: &str = r#"
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
  ON effective.operation_id = operation.id;

INSERT INTO _usage_report_token_aggregates(
    category_path, repository_bucket, measurement_provenance,
    measured_tokens, unknown_observations, observation_count,
    has_gap, aggregate_overflow
)
SELECT token.category_path, token.repository_bucket,
       token.measurement_provenance, COALESCE(token.token_count, 0),
       token.token_count IS NULL, 1, token.coverage_state <> 'complete', 0
FROM token_observations AS token
WHERE token.category_path NOT GLOB 'attribution.items.*'
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
           > 9223372036854775807 - excluded.measured_tokens;

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
WHERE token.category_path = 'total_tokens'
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
           > 9223372036854775807 - excluded.measured_tokens;

INSERT INTO _usage_report_token_coverage(coverage_state, observation_count)
SELECT coverage_state, COUNT(*)
FROM token_observations
WHERE category_path NOT GLOB 'attribution.items.*'
GROUP BY coverage_state;

INSERT INTO _usage_report_coverage(coverage_state, observation_count)
SELECT coverage_state, COUNT(*) FROM coverage_events GROUP BY coverage_state;

INSERT INTO _usage_report_spans(
    span_id, operation_id, activity_state, started_at_ms, ended_at_ms
)
SELECT span.id, span.operation_id, span.activity_state, span.started_at_ms,
       ended.occurred_at_ms
FROM activity_spans AS span
LEFT JOIN activity_span_events AS ended
  ON ended.activity_span_id = span.id AND ended.event_kind = 'ended';

INSERT INTO _usage_report_cache_meta(singleton, schema_version, ready)
VALUES (1, 1, 1);
"#;

pub(crate) async fn ensure(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    if is_ready(pool).await? {
        return Ok(());
    }
    let mut connection = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *connection)
        .await?;
    let result = rebuild_if_needed(&mut connection).await;
    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *connection).await?;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
            Err(error)
        }
    }
}

pub(crate) async fn is_ready(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?)",
    )
    .bind(CACHE_META_TABLE)
    .fetch_one(pool)
    .await?;
    if exists == 0 {
        return Ok(false);
    }
    let state = sqlx::query_as::<_, (i64, i64)>(
        "SELECT schema_version, ready FROM _usage_report_cache_meta WHERE singleton = 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(state == Some((REPORT_CACHE_SCHEMA_VERSION, 1)))
}

async fn rebuild_if_needed(connection: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let exists = sqlx::query_scalar::<_, i64>(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?)",
    )
    .bind(CACHE_META_TABLE)
    .fetch_one(&mut *connection)
    .await?;
    if exists != 0 {
        let state = sqlx::query_as::<_, (i64, i64)>(
            "SELECT schema_version, ready FROM _usage_report_cache_meta WHERE singleton = 1",
        )
        .fetch_optional(&mut *connection)
        .await?;
        if state == Some((REPORT_CACHE_SCHEMA_VERSION, 1)) {
            return Ok(());
        }
        if state.is_some_and(|(schema_version, _)| schema_version > REPORT_CACHE_SCHEMA_VERSION) {
            return Ok(());
        }
    }
    sqlx::raw_sql(RESET_CACHE_SQL)
        .execute(&mut *connection)
        .await?;
    sqlx::raw_sql(CREATE_CACHE_SQL)
        .execute(&mut *connection)
        .await?;
    sqlx::raw_sql(BACKFILL_CACHE_SQL)
        .execute(&mut *connection)
        .await?;
    Ok(())
}
