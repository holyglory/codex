use crate::RepositoryId;
use crate::UsageStoreError;
use crate::WorktreeId;
use sqlx::Row;
use sqlx::SqliteConnection;

const MAX_WORKTREES: usize = 16_384;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeAggregate {
    pub id: WorktreeId,
    pub repository_id: RepositoryId,
    pub label: String,
    pub operation_count: u64,
    pub model_request_count: u64,
    pub tool_count: u64,
    pub total_tokens: Option<i64>,
    pub unknown_token_observations: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorktreeCoverage {
    pub assigned_operation_count: u64,
    pub unassigned_operation_count: u64,
    pub assigned_token_observation_count: u64,
    pub unassigned_token_observation_count: u64,
}

pub(super) async fn aggregates(
    connection: &mut SqliteConnection,
) -> Result<(Vec<WorktreeAggregate>, WorktreeCoverage), UsageStoreError> {
    let operation_rows = sqlx::query(
        r#"
        WITH assigned AS (
            SELECT DISTINCT selected.id AS operation_id, attribution.worktree_id
            FROM _usage_selected AS selected
            JOIN repository_attributions AS attribution
              ON attribution.operation_id = selected.id
             AND attribution.worktree_id IS NOT NULL
        )
        SELECT assigned.worktree_id, worktree.repository_id,
               worktree.safe_display_label, COUNT(*) AS operation_count,
               COALESCE(SUM(selected.operation_kind = 'model_request'), 0) AS model_count,
               COALESCE(SUM(selected.operation_kind IN ('local_tool', 'hosted_tool', 'activity_control')), 0) AS tool_count
        FROM assigned
        JOIN _usage_selected AS selected ON selected.id = assigned.operation_id
        JOIN worktrees AS worktree ON worktree.id = assigned.worktree_id
        GROUP BY assigned.worktree_id, worktree.repository_id, worktree.safe_display_label
        ORDER BY worktree.safe_display_label, assigned.worktree_id
        LIMIT ?
        "#,
    )
    .bind(i64::try_from(MAX_WORKTREES + 1).unwrap_or(i64::MAX))
    .fetch_all(&mut *connection)
    .await
    .map_err(database_error)?;
    if operation_rows.len() > MAX_WORKTREES {
        return Err(UsageStoreError::ReportTooLarge);
    }

    let token_rows = sqlx::query(
        r#"
        WITH assigned AS (
            SELECT DISTINCT selected.id AS operation_id, attribution.worktree_id
            FROM _usage_selected AS selected
            JOIN repository_attributions AS attribution
              ON attribution.operation_id = selected.id
             AND attribution.worktree_id IS NOT NULL
        ), token_rows AS (
            SELECT assigned.worktree_id, token.category_path,
                   token.measurement_provenance, token.token_count,
                   token.coverage_state
            FROM assigned
            JOIN model_requests AS request ON request.operation_id = assigned.operation_id
            JOIN token_observations AS token ON token.model_request_id = request.id
            WHERE token.category_path = 'total_tokens'
              AND token.measurement_provenance = 'provider_reported'
            UNION ALL
            SELECT assigned.worktree_id, token.category_path,
                   token.measurement_provenance, token.token_count,
                   token.coverage_state
            FROM assigned
            JOIN tool_invocations AS tool ON tool.operation_id = assigned.operation_id
            JOIN token_observations AS token ON token.tool_invocation_id = tool.id
            WHERE token.category_path = 'total_tokens'
              AND token.measurement_provenance = 'provider_reported'
        )
        SELECT worktree_id, COALESCE(SUM(token_count), 0) AS measured_tokens,
               COALESCE(SUM(token_count IS NULL), 0) AS unknown_observations
        FROM token_rows
        GROUP BY worktree_id
        "#,
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(database_error)?;

    let mut aggregates = Vec::with_capacity(operation_rows.len());
    for row in operation_rows {
        let id = WorktreeId::new(row.get::<String, _>("worktree_id"))
            .map_err(|_| UsageStoreError::InvalidFact)?;
        let token = token_rows
            .iter()
            .find(|token| token.get::<String, _>("worktree_id") == id.as_str());
        aggregates.push(WorktreeAggregate {
            id,
            repository_id: RepositoryId::new(row.get::<String, _>("repository_id"))
                .map_err(|_| UsageStoreError::InvalidFact)?,
            label: row.get("safe_display_label"),
            operation_count: count(row.get("operation_count"))?,
            model_request_count: count(row.get("model_count"))?,
            tool_count: count(row.get("tool_count"))?,
            total_tokens: token.and_then(|token| token.get("measured_tokens")),
            unknown_token_observations: token
                .map(|token| count(token.get("unknown_observations")))
                .transpose()?
                .unwrap_or(0),
        });
    }

    let coverage_row = sqlx::query(
        r#"
        WITH assigned AS (
            SELECT DISTINCT selected.id AS operation_id
            FROM _usage_selected AS selected
            JOIN repository_attributions AS attribution
              ON attribution.operation_id = selected.id
             AND attribution.worktree_id IS NOT NULL
        ), token_rows AS (
            SELECT DISTINCT operation_id, token_id FROM (
            SELECT selected.id AS operation_id, token.id AS token_id
            FROM _usage_selected AS selected
            JOIN model_requests AS request ON request.operation_id = selected.id
            JOIN token_observations AS token ON token.model_request_id = request.id
            WHERE token.category_path = 'total_tokens'
            UNION ALL
            SELECT selected.id AS operation_id, token.id AS token_id
            FROM _usage_selected AS selected
            JOIN tool_invocations AS tool ON tool.operation_id = selected.id
            JOIN token_observations AS token ON token.tool_invocation_id = tool.id
            WHERE token.category_path = 'total_tokens'
            )
        )
        SELECT
            COALESCE(SUM(EXISTS(SELECT 1 FROM assigned WHERE assigned.operation_id = selected.id)), 0) AS assigned_operations,
            COALESCE(SUM(NOT EXISTS(SELECT 1 FROM assigned WHERE assigned.operation_id = selected.id)), 0) AS unassigned_operations,
            COUNT(DISTINCT CASE WHEN EXISTS(SELECT 1 FROM assigned WHERE assigned.operation_id = token_rows.operation_id) THEN token_rows.token_id END) AS assigned_tokens,
            COUNT(DISTINCT CASE WHEN NOT EXISTS(SELECT 1 FROM assigned WHERE assigned.operation_id = token_rows.operation_id) THEN token_rows.token_id END) AS unassigned_tokens
        FROM _usage_selected AS selected
        LEFT JOIN token_rows ON token_rows.operation_id = selected.id
        "#,
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(database_error)?;

    // The LEFT JOIN above repeats operation rows for each token. Read operation
    // counts independently so token coverage cannot distort them.
    let operation_coverage = sqlx::query(
        r#"
        WITH assigned AS (
            SELECT DISTINCT selected.id AS operation_id
            FROM _usage_selected AS selected
            JOIN repository_attributions AS attribution
              ON attribution.operation_id = selected.id
             AND attribution.worktree_id IS NOT NULL
        )
        SELECT COALESCE(SUM(EXISTS(SELECT 1 FROM assigned WHERE assigned.operation_id = selected.id)), 0) AS assigned_operations,
               COALESCE(SUM(NOT EXISTS(SELECT 1 FROM assigned WHERE assigned.operation_id = selected.id)), 0) AS unassigned_operations
        FROM _usage_selected AS selected
        "#,
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(database_error)?;
    let coverage = WorktreeCoverage {
        assigned_operation_count: count(operation_coverage.get("assigned_operations"))?,
        unassigned_operation_count: count(operation_coverage.get("unassigned_operations"))?,
        assigned_token_observation_count: count(coverage_row.get("assigned_tokens"))?,
        unassigned_token_observation_count: count(coverage_row.get("unassigned_tokens"))?,
    };
    Ok((aggregates, coverage))
}

fn count(value: i64) -> Result<u64, UsageStoreError> {
    u64::try_from(value).map_err(|_| UsageStoreError::DatabaseValueOutOfRange)
}

fn database_error(error: sqlx::Error) -> UsageStoreError {
    UsageStoreError::Database(error)
}
