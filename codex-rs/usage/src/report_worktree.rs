use crate::RepositoryId;
use crate::UsageStoreError;
use crate::UtcTimeRange;
use crate::WorktreeId;
use sqlx::Row;
use sqlx::SqliteConnection;

const MAX_WORKTREES: usize = 16_384;

pub(super) async fn global_aggregates(
    connection: &mut SqliteConnection,
) -> Result<(Vec<WorktreeAggregate>, WorktreeCoverage), UsageStoreError> {
    let has_worktree_assignment: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_attributions WHERE worktree_id IS NOT NULL)",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(database_error)?;
    if has_worktree_assignment == 0 {
        let unassigned_operations: i64 = sqlx::query_scalar(
            "SELECT operations FROM _usage_report_global_counts WHERE singleton=1",
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(database_error)?;
        let unassigned_tokens: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(observation_count),0) FROM _usage_report_token_aggregates",
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(database_error)?;
        return Ok((
            Vec::new(),
            WorktreeCoverage {
                assigned_operation_count: 0,
                unassigned_operation_count: count(unassigned_operations)?,
                assigned_token_observation_count: 0,
                unassigned_token_observation_count: count(unassigned_tokens)?,
            },
        ));
    }
    aggregates(connection, /*time_range*/ None).await
}

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
    time_range: Option<UtcTimeRange>,
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
        SELECT worktree_id,
               COALESCE(SUM(token_count) FILTER (
                   WHERE category_path = 'total_tokens'
                     AND measurement_provenance = 'provider_reported'
               ), 0) AS measured_tokens,
               COALESCE(SUM(token_count IS NULL) FILTER (
                   WHERE category_path = 'total_tokens'
                     AND measurement_provenance = 'provider_reported'
               ), 0) AS unknown_observations
        FROM _usage_worktree_tokens
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

    let mut coverage_query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "WITH all_tokens AS (SELECT DISTINCT selected.id AS operation_id, token.id AS token_id FROM _usage_selected AS selected JOIN model_requests AS request ON request.operation_id = selected.id JOIN token_observations AS token ON token.model_request_id = request.id WHERE token.category_path NOT GLOB 'attribution.items.*'",
    );
    if let Some(range) = time_range {
        coverage_query
            .push(" AND token.observed_at_ms >= ")
            .push_bind(range.start_ms())
            .push(" AND token.observed_at_ms < ")
            .push_bind(range.end_ms());
    }
    coverage_query.push(" UNION SELECT DISTINCT selected.id, token.id FROM _usage_selected AS selected JOIN tool_invocations AS tool ON tool.operation_id = selected.id JOIN token_observations AS token ON token.tool_invocation_id = tool.id WHERE token.category_path NOT GLOB 'attribution.items.*'");
    if let Some(range) = time_range {
        coverage_query
            .push(" AND token.observed_at_ms >= ")
            .push_bind(range.start_ms())
            .push(" AND token.observed_at_ms < ")
            .push_bind(range.end_ms());
    }
    coverage_query.push(") SELECT COUNT(DISTINCT CASE WHEN EXISTS (SELECT 1 FROM repository_attributions AS attribution WHERE attribution.operation_id = all_tokens.operation_id AND attribution.worktree_id IS NOT NULL) THEN all_tokens.token_id END) AS assigned_tokens, COUNT(DISTINCT CASE WHEN NOT EXISTS (SELECT 1 FROM repository_attributions AS attribution WHERE attribution.operation_id = all_tokens.operation_id AND attribution.worktree_id IS NOT NULL) THEN all_tokens.token_id END) AS unassigned_tokens FROM all_tokens");
    let coverage_row = coverage_query
        .build()
        .fetch_one(&mut *connection)
        .await
        .map_err(database_error)?;

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
