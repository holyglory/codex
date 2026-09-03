use super::UsageStatsArgs;
use super::UsageStatsContext;
use super::query::cursor;
use super::query::optional_thread;
use super::query::page;
use super::repository::optional_repository;
use super::storage_error;
use crate::function_tool::FunctionCallError;
use codex_usage::AgentId;
use codex_usage::RepositoryId;
use codex_usage::ThreadId;
use codex_usage::UsageActivityListQuery;
use codex_usage::UsageEventListQuery;
use codex_usage::UsageStore;
use codex_usage::UsageToolListQuery;
use codex_usage::UtcTimeRange;
use serde_json::Value;
use serde_json::json;

pub(super) async fn repositories(
    store: &UsageStore,
    args: &UsageStatsArgs,
) -> Result<Value, FunctionCallError> {
    let page = store
        .list_repositories(&page(args)?)
        .await
        .map_err(|_| storage_error())?;
    Ok(json!({
        "schemaVersion": 1,
        "kind": "usageRepositories",
        "data": page.data.iter().map(|record| json!({
            "id": record.id.as_str(),
            "label": record.label,
            "createdAtMs": record.created_at_ms,
            "updatedAtMs": record.updated_at_ms,
        })).collect::<Vec<_>>(),
        "nextCursor": page.next_cursor.as_ref().map(cursor),
    }))
}

pub(super) async fn tools(
    store: &UsageStore,
    context: &UsageStatsContext,
    args: &UsageStatsArgs,
    time_range: Option<UtcTimeRange>,
) -> Result<Value, FunctionCallError> {
    let page = store
        .list_tools(&UsageToolListQuery {
            page: page(args)?,
            time_range,
            thread_id: optional_thread(context, args.thread_id.as_deref())?,
            repository_id: optional_repository(store, context, args.repository.as_deref()).await?,
        })
        .await
        .map_err(|_| storage_error())?;
    Ok(json!({
        "schemaVersion": 1,
        "kind": "usageTools",
        "data": page.data.iter().map(|record| json!({
            "id": record.id.as_string(),
            "threadId": record.thread_id.as_str(),
            "repositoryId": record.repository_id.as_ref().map(RepositoryId::as_str),
            "tool": record.tool_name.as_str(),
            "family": record.operation_family.as_str(),
            "startedAtMs": record.started_at_ms,
            "completedAtMs": record.completed_at_ms,
            "status": record.status.map(codex_usage::TerminalStatus::as_str),
            "provenance": record.provenance.as_str(),
        })).collect::<Vec<_>>(),
        "nextCursor": page.next_cursor.as_ref().map(cursor),
    }))
}

pub(super) async fn activities(
    store: &UsageStore,
    context: &UsageStatsContext,
    args: &UsageStatsArgs,
    time_range: Option<UtcTimeRange>,
) -> Result<Value, FunctionCallError> {
    let page = store
        .list_activities(&UsageActivityListQuery {
            page: page(args)?,
            time_range,
            thread_id: optional_thread(context, args.thread_id.as_deref())?,
            agent_id: args
                .agent_id
                .as_deref()
                .map(AgentId::new)
                .transpose()
                .map_err(|_| super::tool_error("agent_id is invalid"))?,
        })
        .await
        .map_err(|_| storage_error())?;
    Ok(json!({
        "schemaVersion": 1,
        "kind": "usageActivities",
        "data": page.data.iter().map(|record| json!({
            "id": record.id.as_string(),
            "threadId": record.thread_id.as_str(),
            "agentId": record.agent_id.as_str(),
            "phase": record.phase.as_str(),
            "activity": record.activity.as_str(),
            "state": record.state.as_str(),
            "startedAtMs": record.started_at_ms,
            "endedAtMs": record.ended_at_ms,
            "provenance": record.provenance.as_str(),
        })).collect::<Vec<_>>(),
        "nextCursor": page.next_cursor.as_ref().map(cursor),
    }))
}

pub(super) async fn events(
    store: &UsageStore,
    context: &UsageStatsContext,
    args: &UsageStatsArgs,
    time_range: Option<UtcTimeRange>,
) -> Result<Value, FunctionCallError> {
    let page = store
        .list_events(&UsageEventListQuery {
            page: page(args)?,
            time_range,
            thread_id: optional_thread(context, args.thread_id.as_deref())?,
            repository_id: optional_repository(store, context, args.repository.as_deref()).await?,
            kind: None,
        })
        .await
        .map_err(|_| storage_error())?;
    Ok(json!({
        "schemaVersion": 1,
        "kind": "usageEvents",
        "data": page.data.iter().map(|record| json!({
            "id": record.id.as_string(),
            "threadId": record.thread_id.as_ref().map(ThreadId::as_str),
            "repositoryId": record.repository_id.as_ref().map(RepositoryId::as_str),
            "occurredAtMs": record.occurred_at_ms,
            "event": record.kind.as_str(),
            "provenance": record.provenance.as_str(),
            "coverage": record.coverage.as_str(),
        })).collect::<Vec<_>>(),
        "nextCursor": page.next_cursor.as_ref().map(cursor),
    }))
}
