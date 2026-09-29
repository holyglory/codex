//! Disk-backed selection keeps raw histories out of the report reader's heap.
use crate::UsageSummaryQuery;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};
use sqlx::types::Json;
use std::collections::HashSet;

pub(super) async fn select(
    connection: &mut SqliteConnection,
    query: &UsageSummaryQuery,
    family: Option<&HashSet<String>>,
    cache: super::ReportSource,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("DROP TABLE IF EXISTS temp._usage_selected; DROP TABLE IF EXISTS temp._usage_tokens; DROP TABLE IF EXISTS temp._usage_spans; DROP TABLE IF EXISTS temp._usage_intervals; DROP TABLE IF EXISTS temp._usage_active;")
        .execute(&mut *connection).await?;
    if cache == super::ReportSource::CachedAll {
        sqlx::query("CREATE TEMP TABLE _usage_selected AS SELECT operation_id AS id, operation_kind, agent_id, started_at_ms, ended_at_ms, terminal_status, phase, activity, activity_state, attribution_provenance AS provenance FROM _usage_report_operations")
            .execute(&mut *connection).await?;
        sqlx::query("CREATE UNIQUE INDEX temp._usage_selected_id ON _usage_selected(id)").execute(&mut *connection).await?;
        return Ok(());
    }
    let mut sql = QueryBuilder::<Sqlite>::new(r#"
        CREATE TEMP TABLE _usage_selected AS
        SELECT operation.id, operation.operation_kind, operation.agent_id,
               operation.started_at_ms, terminal.occurred_at_ms AS ended_at_ms,
               terminal.event_kind AS terminal_status,
               COALESCE(classification.phase, operation.phase) AS phase,
               COALESCE(classification.activity, operation.activity) AS activity,
               COALESCE(classification.activity_state, operation.activity_state) AS activity_state,
               COALESCE(classification.provenance, operation.attribution_provenance) AS provenance
        FROM operations AS operation
        LEFT JOIN operation_events AS terminal
          ON terminal.operation_id = operation.id AND terminal.terminal = 1
        LEFT JOIN effective_classification_events AS classification
          ON classification.operation_id = operation.id
    "#);
    if query.account_profile_ref.is_some() {
        sql.push(r#"
            LEFT JOIN model_requests AS request ON request.operation_id = operation.id
            LEFT JOIN model_requests AS parent_request ON parent_request.operation_id = operation.parent_operation_id
            LEFT JOIN turns AS turn ON turn.id = operation.turn_id
        "#);
    }
    sql.push(" WHERE 1 = 1");
    if let Some(thread) = &query.thread_id {
        sql.push(" AND operation.thread_id = ").push_bind(thread.as_str());
    }
    if let Some(family) = family {
        sql.push(" AND operation.id IN (SELECT operation_id FROM repository_attributions WHERE repository_id IN (SELECT value FROM json_each(")
            .push_bind(Json(family.iter().collect::<Vec<_>>())).push(")))");
    }
    if let Some(account) = &query.account_profile_ref {
        sql.push(" AND COALESCE(request.account_profile_ref, parent_request.account_profile_ref, turn.account_profile_ref) = ")
            .push_bind(account.as_str());
    }
    if let Some(range) = query.time_range {
        sql.push(" AND operation.started_at_ms < ").push_bind(range.end_ms());
        sql.push(" AND (terminal.occurred_at_ms IS NULL OR terminal.occurred_at_ms > ")
            .push_bind(range.start_ms()).push(")");
    }
    sql.build().execute(&mut *connection).await?;
    sqlx::query("CREATE UNIQUE INDEX temp._usage_selected_id ON _usage_selected(id)")
        .execute(&mut *connection).await?;
    let mut tokens = QueryBuilder::<Sqlite>::new("CREATE TEMP TABLE _usage_tokens AS WITH owned(operation_id, category_path, repository_bucket, measurement_provenance, coverage_state, measured_tokens, unknown_observations, observation_count, aggregate_overflow) AS (");
    if cache == super::ReportSource::CachedScoped {
        tokens.push("SELECT hours.operation_id, hours.category_path, hours.repository_bucket, hours.measurement_provenance, hours.coverage_state, hours.measured_tokens, hours.unknown_observations, hours.observation_count, hours.aggregate_overflow FROM _usage_report_token_hours AS hours JOIN _usage_selected AS selected ON selected.id = hours.operation_id WHERE 1 = 1");
        if let Some(range) = query.time_range {
            tokens.push(" AND hours.hour_index > ").push_bind(range.start_ms().div_euclid(3_600_000));
            tokens.push(" AND hours.hour_index < ").push_bind(range.end_ms().div_euclid(3_600_000));
        }
        if query.time_range.is_some() { tokens.push(" UNION ALL "); }
    }
    if cache == super::ReportSource::Canonical || query.time_range.is_some() {
        tokens.push(r#"
            SELECT token.operation_id, token.category_path, token.repository_bucket, token.measurement_provenance,
                   token.coverage_state, COALESCE(token.token_count, 0), token.token_count IS NULL, 1, 0
            FROM (
                SELECT token.*, request.operation_id FROM _usage_selected AS selected
                JOIN model_requests AS request ON request.operation_id = selected.id
                JOIN token_observations AS token ON token.model_request_id = request.id
                UNION ALL
                SELECT token.*, tool.operation_id FROM _usage_selected AS selected
                JOIN tool_invocations AS tool ON tool.operation_id = selected.id
                JOIN token_observations AS token ON token.tool_invocation_id = tool.id
            ) AS token WHERE token.category_path NOT GLOB 'attribution.items.*'
        "#);
        if let Some(range) = query.time_range {
            tokens.push(" AND token.observed_at_ms >= ").push_bind(range.start_ms());
            tokens.push(" AND token.observed_at_ms < ").push_bind(range.end_ms());
            if cache == super::ReportSource::CachedScoped {
                tokens.push(" AND (token.observed_at_ms / 3600000 - (token.observed_at_ms % 3600000 < 0) = ")
                    .push_bind(range.start_ms().div_euclid(3_600_000))
                    .push(" OR token.observed_at_ms / 3600000 - (token.observed_at_ms % 3600000 < 0) = ")
                    .push_bind(range.end_ms().div_euclid(3_600_000)).push(")");
            }
        }
    }
    tokens.push(r#"
        ) SELECT token.category_path, token.repository_bucket, token.measurement_provenance,
                 selected.phase, selected.activity, selected.provenance, token.coverage_state,
                 SUM(token.measured_tokens) AS measured_tokens, SUM(token.unknown_observations) AS unknown_observations,
                 SUM(token.observation_count) AS observation_count, MAX(token.aggregate_overflow) AS aggregate_overflow
          FROM owned AS token JOIN _usage_selected AS selected ON selected.id = token.operation_id WHERE 1 = 1
    "#);
    if let Some(family) = family {
        tokens.push(" AND token.repository_bucket IN (SELECT value FROM json_each(")
            .push_bind(Json(family.iter().collect::<Vec<_>>())).push("))");
    }
    tokens.push(" GROUP BY token.category_path, token.repository_bucket, token.measurement_provenance, selected.phase, selected.activity, selected.provenance, token.coverage_state");
    tokens.build().execute(&mut *connection).await?;
    Ok(())
}
