use super::OperationLifecycle;
use super::ReportSelection;
use super::UsageSummaryScope;
use super::UtcTimeRange;
use crate::AccountProfileRef;
use crate::UsageStore;
use crate::UsageStoreError;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::sqlite::SqliteRow;
use sqlx::types::Json;
use std::collections::HashSet;

impl UsageStore {
    pub(crate) async fn build_report_selection(
        &self,
        scope: UsageSummaryScope,
        time_range: Option<UtcTimeRange>,
        repository_family: Option<&HashSet<String>>,
        account_profile_ref: Option<&AccountProfileRef>,
    ) -> Result<ReportSelection, UsageStoreError> {
        let mut query = QueryBuilder::<Sqlite>::new(
            r#"
            SELECT operation.id, operation.operation_kind, operation.agent_id,
                   operation.started_at_ms, terminal.occurred_at_ms, terminal.event_kind,
                   COALESCE(classification.phase, operation.phase) AS phase,
                   COALESCE(classification.activity, operation.activity) AS activity,
                   COALESCE(classification.activity_state, operation.activity_state) AS activity_state,
                   COALESCE(classification.provenance, operation.attribution_provenance) AS provenance
            FROM operations AS operation
            LEFT JOIN operation_events AS terminal
              ON terminal.operation_id = operation.id AND terminal.terminal = 1
            LEFT JOIN effective_classification_events AS classification
              ON classification.operation_id = operation.id
            "#,
        );
        if account_profile_ref.is_some() {
            query.push(
                r#"
                LEFT JOIN model_requests AS request ON request.operation_id = operation.id
                LEFT JOIN model_requests AS parent_request
                  ON parent_request.operation_id = operation.parent_operation_id
                LEFT JOIN turns AS turn ON turn.id = operation.turn_id
                "#,
            );
        }
        query.push(" WHERE 1 = 1");
        if let UsageSummaryScope::Thread(thread) = &scope {
            query.push(" AND operation.thread_id = ").push_bind(thread.as_str());
        }
        if let Some(family) = repository_family {
            query
                .push(" AND operation.id IN (SELECT operation_id FROM repository_attributions WHERE repository_id IN (SELECT value FROM json_each(")
                .push_bind(Json(family.iter().collect::<Vec<_>>()))
                .push(")))");
        }
        if let Some(account) = account_profile_ref {
            query
                .push(" AND COALESCE(request.account_profile_ref, parent_request.account_profile_ref, turn.account_profile_ref) = ")
                .push_bind(account.as_str());
        }
        if let Some(range) = time_range {
            query.push(" AND operation.started_at_ms < ").push_bind(range.end_ms());
            query
                .push(" AND (terminal.occurred_at_ms IS NULL OR terminal.occurred_at_ms > ")
                .push_bind(range.start_ms())
                .push(")");
        }
        let rows = query.build().fetch_all(&self.pool).await.map_err(UsageStoreError::Database)?;
        let operations = rows.into_iter().map(|row| OperationLifecycle {
            id: row.get("id"),
            kind: row.get("operation_kind"),
            started_at_ms: row.get("started_at_ms"),
            ended_at_ms: row.get("occurred_at_ms"),
            terminal_status: row.get("event_kind"),
            agent_id: row.get("agent_id"),
            phase: row.get("phase"),
            activity: row.get("activity"),
            activity_state: row.get("activity_state"),
            attribution_provenance: row.get("provenance"),
        }).collect();
        Ok(ReportSelection::new(scope, time_range, operations, /*uses_report_cache*/ false))
    }

    pub(crate) async fn selected_token_rows(
        &self,
        selection: &ReportSelection,
        repository_family: Option<&HashSet<String>>,
    ) -> Result<Vec<SqliteRow>, UsageStoreError> {
        // Start with selected owners so repository queries use the owner indexes instead
        // of scanning every token ever recorded. Each token has exactly one owner kind.
        let mut query = QueryBuilder::<Sqlite>::new("WITH selected AS (SELECT value AS id FROM json_each(");
        query.push_bind(selection.operation_ids()).push(")), tokens AS (");
        query.push(
            r#"
            SELECT token.*, request.operation_id
            FROM selected JOIN model_requests AS request ON request.operation_id = selected.id
            JOIN token_observations AS token ON token.model_request_id = request.id
            UNION ALL
            SELECT token.*, tool.operation_id
            FROM selected JOIN tool_invocations AS tool ON tool.operation_id = selected.id
            JOIN token_observations AS token ON token.tool_invocation_id = tool.id
            ) SELECT * FROM tokens AS token WHERE token.category_path NOT GLOB 'attribution.items.*'
            "#,
        );
        if let Some(range) = selection.time_range {
            query.push(" AND token.observed_at_ms >= ").push_bind(range.start_ms());
            query.push(" AND token.observed_at_ms < ").push_bind(range.end_ms());
        }
        if let Some(family) = repository_family {
            query.push(" AND token.repository_bucket IN (SELECT value FROM json_each(")
                .push_bind(Json(family.iter().collect::<Vec<_>>())).push("))");
        }
        query.build().fetch_all(&self.pool).await.map_err(UsageStoreError::Database)
    }

    pub(crate) async fn selected_coverage_rows(
        &self,
        selection: &ReportSelection,
        include_global: bool,
    ) -> Result<Vec<SqliteRow>, UsageStoreError> {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT coverage_state, COUNT(*) AS observation_count FROM coverage_events WHERE (operation_id IN (SELECT value FROM json_each(",
        );
        query.push_bind(selection.operation_ids()).push("))");
        if include_global {
            query.push(" OR operation_id IS NULL");
        }
        query.push(")");
        if let Some(range) = selection.time_range {
            query.push(" AND occurred_at_ms >= ").push_bind(range.start_ms());
            query.push(" AND occurred_at_ms < ").push_bind(range.end_ms());
        }
        query.push(" GROUP BY coverage_state");
        query.build().fetch_all(&self.pool).await.map_err(UsageStoreError::Database)
    }

    pub(crate) async fn unresolved_report_coverage(
        &self,
        selection: &ReportSelection,
        include_global: bool,
    ) -> Result<bool, UsageStoreError> {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT EXISTS(SELECT 1 FROM coverage_events AS coverage WHERE (operation_id IN (SELECT value FROM json_each(",
        );
        query.push_bind(selection.operation_ids()).push("))");
        if include_global {
            query.push(" OR operation_id IS NULL");
        }
        query.push(") AND coverage_state <> 'complete'");
        if let Some(range) = selection.time_range {
            query.push(" AND occurred_at_ms >= ").push_bind(range.start_ms());
            query.push(" AND occurred_at_ms < ").push_bind(range.end_ms());
        }
        // Lifecycle start events and the legacy conservative partial marker do not
        // erase a terminal receipt with captured usage. Explicit unknown/error coverage
        // and completed model attempts without usage facts remain visible.
        query.push(r#" AND NOT (
            scope_kind IN ('model_attempt', 'tool_attempt')
            AND coverage_state IN ('capture_started', 'partial') AND reason_code IS NULL
            AND EXISTS (SELECT 1 FROM operation_events AS terminal
                        WHERE terminal.operation_id = coverage.operation_id AND terminal.terminal = 1)
            AND (scope_kind = 'tool_attempt' OR EXISTS (
                SELECT 1 FROM model_requests AS request
                JOIN token_observations AS token ON token.model_request_id = request.id
                WHERE request.operation_id = coverage.operation_id
                  AND token.measurement_provenance = 'provider_reported'
                  AND token.token_count IS NOT NULL AND token.coverage_state = 'complete'
                  AND token.category_path NOT GLOB 'attribution.items.*'
        "#);
        if let Some(range) = selection.time_range {
            query.push(" AND token.observed_at_ms >= ").push_bind(range.start_ms());
            query.push(" AND token.observed_at_ms < ").push_bind(range.end_ms());
        }
        query.push(r#" )) ) AND NOT EXISTS (
            SELECT 1 FROM coverage_events AS later
            WHERE later.operation_id = coverage.operation_id AND later.scope_kind = coverage.scope_kind
              AND (later.occurred_at_ms > coverage.occurred_at_ms
                   OR (later.occurred_at_ms = coverage.occurred_at_ms AND later.event_id > coverage.event_id))
        "#);
        if let Some(range) = selection.time_range {
            query.push(" AND later.occurred_at_ms < ").push_bind(range.end_ms());
        }
        query.push("))");
        query.build_query_scalar().fetch_one(&self.pool).await.map_err(UsageStoreError::Database)
    }
}
