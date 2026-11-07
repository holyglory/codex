use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_usage::RecordCursor;
use codex_usage::RecordKind;
use codex_usage::TimeRange;
use codex_usage::UsageFilter;
use codex_usage::UsageReader;
use codex_utils_output_truncation::TruncationPolicy;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

mod outcomes;
mod presentation;
mod snapshots;
mod spec;

const TOOL_NAME: &str = "usage_stats";
const MAX_OUTPUT_BYTES: usize = 16 * 1_024;

pub struct UsageStatsHandler;

#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum UsageStatsAction {
    Summary,
    TaskTreeSummary,
    PerformanceReview,
    Repositories,
    Tools,
    Activities,
    Events,
    Details,
}

#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum UsageStatsScope {
    All,
    CurrentChat,
    CurrentRepository,
    Repository,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct UsageStatsArgs {
    action: UsageStatsAction,
    scope: Option<UsageStatsScope>,
    repository: Option<String>,
    account: Option<String>,
    thread_id: Option<String>,
    root_thread_id: Option<String>,
    include_descendants: Option<bool>,
    agent_id: Option<String>,
    detail: Option<String>,
    from_at_ms: Option<i64>,
    to_at_ms: Option<i64>,
    limit: Option<u32>,
    cursor_sort_value: Option<i64>,
    cursor_id: Option<String>,
    outcome_cursor: Option<String>,
    outcome_limit: Option<u32>,
}

struct UsageStatsContext {
    codex_home: PathBuf,
    thread_id: String,
    cwd: PathBuf,
}

impl UsageStatsContext {
    fn from_invocation(invocation: &ToolInvocation) -> Result<Self, FunctionCallError> {
        Ok(Self {
            codex_home: invocation.turn.config.codex_home.to_path_buf(),
            thread_id: invocation.session.thread_id().to_string(),
            cwd: invocation.turn.config.cwd.to_path_buf(),
        })
    }
}

struct UsageStatsOutput(Value);

impl ToolOutput for UsageStatsOutput {
    fn log_output(&self) -> String {
        "content-free local usage operation".to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn fallback_token_limit_override(&self) -> Option<usize> {
        // The handler already bounds complete JSON; generic middle truncation would
        // corrupt both the report and its pagination cursor.
        Some(TruncationPolicy::Bytes(MAX_OUTPUT_BYTES).token_budget())
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        FunctionToolOutput::from_text(self.0.to_string(), Some(true))
            .to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        self.0.clone()
    }
}

impl ToolExecutor<ToolInvocation> for UsageStatsHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        spec::usage_stats_spec()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(tool_error("usage_stats requires a function payload"));
            };
            let args: UsageStatsArgs = serde_json::from_str(arguments)
                .map_err(|_| tool_error("usage_stats arguments are invalid"))?;
            let context = UsageStatsContext::from_invocation(&invocation)?;
            let value =
                tokio::task::spawn_blocking(move || snapshots::execute_request(&context, args))
                    .await
                    .map_err(|_| storage_error())??;
            Ok(boxed_tool_output(bounded_output(value)?))
        })
    }
}

fn bounded_output(mut value: Value) -> Result<UsageStatsOutput, FunctionCallError> {
    while serde_json::to_vec(&value)
        .map_err(|_| storage_error())?
        .len()
        > MAX_OUTPUT_BYTES
    {
        if !presentation::coalesce_operation_facet(&mut value)? {
            return Err(tool_error(
                "usage_stats result exceeds the safe response bound; narrow the scope or time range",
            ));
        }
    }
    Ok(UsageStatsOutput(value))
}

fn execute(
    store: &UsageReader,
    context: &UsageStatsContext,
    args: UsageStatsArgs,
) -> Result<Value, FunctionCallError> {
    let start = args.from_at_ms.unwrap_or(i64::MIN);
    let end = args.to_at_ms.unwrap_or(i64::MAX);
    let range = TimeRange::from_millis(start, end)
        .map_err(|_| tool_error("usage_stats time range is invalid"))?;
    // Only summary has a current-chat default. List/detail actions are global
    // unless they carry an explicit thread, repository, or account filter.
    let scope = if matches!(args.action, UsageStatsAction::Summary) {
        args.scope.unwrap_or(UsageStatsScope::CurrentChat)
    } else {
        args.scope.unwrap_or(UsageStatsScope::All)
    };
    let mut filter = UsageFilter::default();
    if let Some(account) = args.account.as_deref() {
        let id = if let Ok(id) = account.parse::<codex_account_registry::AccountId>() {
            id.to_string()
        } else {
            let registry = codex_account_registry::RegistryStore::new(&context.codex_home)
                .read()
                .map_err(|_| tool_error("account registry is unavailable"))?;
            registry
                .accounts
                .iter()
                .find(|profile| profile.alias.as_str() == account)
                .map(|profile| profile.id.to_string())
                .ok_or_else(|| tool_error("unknown account alias"))?
        };
        filter = filter.for_account(id);
    }
    let current_repository = || {
        crate::usage_identity::repository_id(&context.cwd)
            .ok_or_else(|| tool_error("current directory is not a recognized Git repository"))
    };
    match scope {
        UsageStatsScope::All => {}
        UsageStatsScope::CurrentRepository => {
            filter = filter.for_repository(current_repository()?);
        }
        UsageStatsScope::CurrentChat => {
            filter = filter.for_thread(context.thread_id.clone());
        }
        UsageStatsScope::Repository => {
            let repository = args
                .repository
                .as_deref()
                .ok_or_else(|| tool_error("repository is required for repository scope"))?;
            filter = filter.for_repository(if repository == "current" {
                current_repository()?
            } else {
                repository.to_string()
            });
        }
    }
    if let Some(repository) = args.repository.as_deref() {
        filter = filter.for_repository(if repository == "current" {
            current_repository()?
        } else {
            repository.to_string()
        });
    }
    if let Some(thread_id) = args.thread_id.as_deref() {
        filter = filter.for_thread(if thread_id == "current" {
            context.thread_id.clone()
        } else {
            thread_id.to_string()
        });
    }
    let limit = usize::try_from(args.limit.unwrap_or(50))
        .map_err(|_| tool_error("limit must be between 1 and 50"))?;
    if !(1..=50).contains(&limit) {
        return Err(tool_error("limit must be between 1 and 50"));
    }
    filter = presentation::agent_filter(filter, args.agent_id.as_deref())?;
    let metadata = snapshot_metadata(store, range);
    let value = match args.action {
        UsageStatsAction::Summary => {
            let mut summary = store.summary(range, &filter).map_err(|_| storage_error())?;
            summary["scope"] = json!(scope_name(scope));
            summary
        }
        UsageStatsAction::Repositories => {
            let mut rows = store
                .repositories(range, &filter, args.cursor_id.as_deref(), limit)
                .map_err(|_| storage_error())?;
            let has_more = rows.len() > limit;
            rows.truncate(limit);
            let cursor = if has_more {
                rows.last().map(|row| json!({"id": row.id, "sortValue": 0}))
            } else {
                None
            };
            let data = rows.into_iter().map(|row| json!({
                "repositoryId": row.id,
                "createdAtMs": row.first_observed_at_ms,
                "updatedAtMs": row.last_observed_at_ms,
                "records": row.totals.records,
                "inputTokens": row.totals.input_tokens, "outputTokens": row.totals.output_tokens,
            })).collect::<Vec<_>>();
            json!({"schemaVersion": 1, "kind": "usageRepositories", "data": data, "nextCursor": cursor})
        }
        UsageStatsAction::TaskTreeSummary => {
            let root = args
                .root_thread_id
                .as_deref()
                .filter(|id| *id != "current")
                .unwrap_or(&context.thread_id);
            let include_descendants = args.include_descendants.unwrap_or(true);
            let scope = if include_descendants {
                codex_usage::ThreadLineageScope::Descendants
            } else {
                codex_usage::ThreadLineageScope::RootOnly
            };
            let threads = store
                .thread_scope(root, scope)
                .map_err(|_| storage_error())?;
            let tree_filter = presentation::agent_filter(
                filter.for_threads(threads.iter().cloned()),
                args.agent_id.as_deref(),
            )?;
            let threads = tree_filter.thread_ids.as_ref().ok_or_else(storage_error)?;
            let totals = store
                .totals(range, &tree_filter)
                .map_err(|_| storage_error())?;
            let after = args.cursor_id.as_deref();
            let mut page = threads
                .iter()
                .filter(|id| after.is_none_or(|after| id.as_str() > after))
                .take(limit + 1)
                .cloned()
                .collect::<Vec<_>>();
            let has_more = page.len() > limit;
            page.truncate(limit);
            let cursor = if has_more {
                page.last().map(|id| json!({"id": id, "sortValue": 0}))
            } else {
                None
            };
            json!({
                "schemaVersion": 1, "kind": "taskTreeSummary", "rootThreadId": root,
                "includeDescendants": include_descendants, "counts": {"threads": threads.len()},
                "threadIds": page, "nextCursor": cursor,
                "data": [{"threadId": root, "records": totals.records,
                    "inputTokens": totals.input_tokens, "outputTokens": totals.output_tokens}],
            })
        }
        UsageStatsAction::PerformanceReview => {
            let outcome_limit = args.outcome_limit.unwrap_or(8);
            if !(1..=50).contains(&outcome_limit) {
                return Err(tool_error("outcome_limit must be between 1 and 50"));
            }
            if args.include_descendants.unwrap_or(false)
                && let Some(root) = filter.thread_id.as_deref()
            {
                let threads = store
                    .thread_scope(root, codex_usage::ThreadLineageScope::Descendants)
                    .map_err(|_| storage_error())?;
                filter = presentation::agent_filter(
                    filter.for_threads(threads),
                    args.agent_id.as_deref(),
                )?;
            }
            let mut report = store
                .performance_review(range, &filter)
                .map_err(|_| storage_error())?;
            report["includeDescendants"] = json!(args.include_descendants.unwrap_or(false));
            report
        }
        UsageStatsAction::Events
        | UsageStatsAction::Details
        | UsageStatsAction::Tools
        | UsageStatsAction::Activities => {
            presentation::record_page(store, range, &filter, &args, limit)?
        }
    };
    Ok(with_metadata(value, &metadata))
}

fn scope_name(scope: UsageStatsScope) -> &'static str {
    match scope {
        UsageStatsScope::All => "all",
        UsageStatsScope::CurrentChat => "current_chat",
        UsageStatsScope::CurrentRepository => "current_repository",
        UsageStatsScope::Repository => "repository",
    }
}

fn with_metadata(mut value: Value, metadata: &serde_json::Map<String, Value>) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.extend(metadata.clone());
    }
    value
}

fn record_fields(record: &codex_usage::UsageRecord) -> Value {
    json!({
        "id": record.id,
        "observedAtMs": record.effective_observed_at_ms(),
        "accountId": record.account_id,
        "operationId": record.operation_id,
        "threadId": record.thread_id,
        "parentThreadId": record.parent_thread_id,
        "turnId": record.turn_id,
        "responseId": record.response_id,
        "outcome": record.outcome,
        "durationMs": record.duration_ms,
        "attemptNumber": record.attempt_number,
        "providerTotalTokens": record.provider_total_tokens,
        "cacheWriteInputTokens": record.cache_write_input_tokens,
        "missingObservations": record.missing_observations,
        "reworkOfOperationId": record.rework_of_operation_id,
        "retryOfOperationId": record.retry_of_operation_id,
        "processId": record.process_id,
        "agentId": record.agent_id,
        "approvalDecision": record.approval_decision,
        "approvalSource": record.approval_source,
        "taxonomyVersion": record.taxonomy_version,
        "heartbeatCount": record.heartbeat_count,
        "repositoryId": record.repository_id,
        "model": record.model,
        "tool": record.tool,
        "activity": record.activity,
        "phase": record.phase,
        "classification": record.classification,
        "event": record.event,
        "coverage": record.coverage,
        "startedAtMs": record.effective_started_at_ms(),
        "endedAtMs": record.effective_ended_at_ms(),
        "inputTokens": record.input_tokens,
        "cachedInputTokens": record.cached_input_tokens,
        "outputTokens": record.output_tokens,
        "reasoningOutputTokens": record.reasoning_output_tokens,
    })
}

fn snapshot_metadata(store: &UsageReader, range: TimeRange) -> serde_json::Map<String, Value> {
    let manifest = store.manifest();
    serde_json::Map::from_iter([
        (
            "snapshotGeneration".to_string(),
            json!(manifest.snapshot_generation),
        ),
        ("watermarkAtMs".to_string(), json!(manifest.watermark_at_ms)),
        (
            "retention".to_string(),
            json!({
                "rolledUpObservations": manifest.retired_observations,
                "unappliedCorrections": manifest.unapplied_retention_corrections,
                "unappliedObservations": manifest.unapplied_retention_records,
                "retiredReviewOperations": manifest.retired_review_operations,
                "retiredReviewContributions": manifest.retired_review_contributions,
                "totalsCoverage": if manifest.unapplied_retention_corrections != 0
                    || manifest.unapplied_retention_records != 0
                    || (range != TimeRange::all() && manifest.retired_observations != 0) {
                    "partial_retention"
                } else { "recorded" },
                "historicalWindowCoverage": if manifest.retired_observations == 0 {
                    "rawObservations"
                } else { "wholeAggregateBuckets" },
            }),
        ),
        ("pendingEntries".to_string(), json!(store.pending_entries())),
        (
            "journalWatermark".to_string(),
            json!(store.journal_watermark()),
        ),
        (
            "freshness".to_string(),
            serde_json::to_value(store.freshness()).unwrap_or_else(|_| json!("unknown")),
        ),
        (
            "pendingCorrections".to_string(),
            json!(manifest.pending_corrections.len()),
        ),
    ])
}

impl CoreToolRuntime for UsageStatsHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

fn tool_error(message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.to_string())
}

fn storage_error() -> FunctionCallError {
    tool_error("local usage storage is unavailable")
}

#[cfg(test)]
#[path = "usage_stats_tests.rs"]
mod tests;
