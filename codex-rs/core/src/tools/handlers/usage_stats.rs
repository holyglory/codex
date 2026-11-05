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
use codex_usage::TimeRange;
use codex_usage::UsageFilter;
use codex_usage::UsageStore;
use codex_utils_output_truncation::TruncationPolicy;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

const TOOL_NAME: &str = "usage_stats";
const MAX_OUTPUT_BYTES: usize = 16 * 1_024;

pub struct UsageStatsHandler;

#[derive(Clone, Copy, Deserialize)]
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

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UsageStatsScope {
    All,
    CurrentChat,
    CurrentRepository,
    Repository,
}

#[derive(Deserialize)]
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
}

impl UsageStatsContext {
    fn from_invocation(invocation: &ToolInvocation) -> Result<Self, FunctionCallError> {
        Ok(Self {
            codex_home: invocation.turn.config.codex_home.to_path_buf(),
            thread_id: invocation.session.thread_id().to_string(),
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
        usage_stats_spec()
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
            let mut store = UsageStore::open(&context.codex_home).map_err(|_| storage_error())?;
            let value = execute(&mut store, &context, args)?;
            Ok(boxed_tool_output(bounded_output(value)?))
        })
    }
}

fn bounded_output(value: Value) -> Result<UsageStatsOutput, FunctionCallError> {
    let encoded = serde_json::to_vec(&value).map_err(|_| storage_error())?;
    if encoded.len() > MAX_OUTPUT_BYTES {
        return Err(tool_error(
            "usage_stats result exceeds the safe response bound; narrow the scope or time range",
        ));
    }
    Ok(UsageStatsOutput(value))
}

fn execute(
    store: &mut UsageStore,
    context: &UsageStatsContext,
    args: UsageStatsArgs,
) -> Result<Value, FunctionCallError> {
    let start = args.from_at_ms.map_or(i64::MIN, |value| value / 1_000);
    let end = args.to_at_ms.map_or(i64::MAX, |value| value / 1_000);
    let range =
        TimeRange::new(start, end).map_err(|_| tool_error("usage_stats time range is invalid"))?;
    // Only summary has a current-chat default. List/detail actions are global
    // unless they carry an explicit thread, repository, or account filter.
    let scope = if matches!(args.action, UsageStatsAction::Summary) {
        args.scope.unwrap_or(UsageStatsScope::CurrentChat)
    } else {
        args.scope.unwrap_or(UsageStatsScope::All)
    };
    let mut filter = UsageFilter::default();
    if let Some(account) = args.account.as_deref() {
        filter = filter.for_account(account);
    }
    match scope {
        UsageStatsScope::All | UsageStatsScope::CurrentRepository => {}
        UsageStatsScope::CurrentChat => {
            filter = filter.for_thread(context.thread_id.clone());
        }
        UsageStatsScope::Repository => {
            let repository = args
                .repository
                .as_deref()
                .ok_or_else(|| tool_error("repository is required for repository scope"))?;
            filter = filter.for_repository(repository);
        }
    }
    if let Some(repository) = args.repository.as_deref() {
        filter = filter.for_repository(repository);
    }
    if let Some(thread_id) = args.thread_id.as_deref() {
        filter = filter.for_thread(if thread_id == "current" {
            context.thread_id.clone()
        } else {
            thread_id.to_string()
        });
    }
    let records = matches!(args.action, UsageStatsAction::Summary)
        .then(Vec::new)
        .unwrap_or_else(|| store.query(range, &filter));
    let totals = store.totals(range, &filter);
    let limit = usize::try_from(args.limit.unwrap_or(50))
        .map_err(|_| tool_error("limit must be between 1 and 50"))?;
    if !(1..=50).contains(&limit) {
        return Err(tool_error("limit must be between 1 and 50"));
    }
    let metadata = snapshot_metadata(store);

    match args.action {
        UsageStatsAction::Summary => Ok(with_metadata(
            json!({
                "schemaVersion": 1,
                "kind": "usageSummary",
                "inputTokens": totals.input_tokens,
                "cachedInputTokens": totals.cached_input_tokens,
                "outputTokens": totals.output_tokens,
                "reasoningOutputTokens": totals.reasoning_output_tokens,
                "records": totals.records,
                "scope": scope_name(scope),
            }),
            &metadata,
        )),
        UsageStatsAction::Events => {
            let (page, cursor) = page_records(&records, limit, &args);
            let data = page.into_iter().map(event_json).collect::<Vec<_>>();
            Ok(with_metadata(
                json!({
                    "schemaVersion": 1,
                    "kind": "usageEvents",
                    "data": data,
                    "nextCursor": cursor,
                }),
                &metadata,
            ))
        }
        UsageStatsAction::Details => {
            let (page, cursor) = page_records(&records, limit, &args);
            let data = page.into_iter().map(detail_json).collect::<Vec<_>>();
            Ok(with_metadata(
                json!({
                    "schemaVersion": 1,
                    "kind": "usageDetails",
                    "detailKind": args.detail.as_deref().unwrap_or("tokens"),
                    "data": data,
                    "nextCursor": cursor,
                }),
                &metadata,
            ))
        }
        UsageStatsAction::Repositories => Ok(with_metadata(
            json!({
                "schemaVersion": 1,
                "kind": "usageRepositories",
                "data": repository_rows(&records, limit),
                "nextCursor": Value::Null,
            }),
            &metadata,
        )),
        UsageStatsAction::Tools => {
            let tool_records = records
                .iter()
                .filter(|record| record.tool.is_some())
                .cloned()
                .collect::<Vec<_>>();
            let (page, cursor) = page_records(&tool_records, limit, &args);
            Ok(with_metadata(
                json!({
                    "schemaVersion": 1,
                    "kind": "usageTools",
                    "data": page.into_iter().map(tool_json).collect::<Vec<_>>(),
                    "nextCursor": cursor,
                }),
                &metadata,
            ))
        }
        UsageStatsAction::Activities => {
            let activity_records = records
                .iter()
                .filter(|record| record.activity.is_some())
                .cloned()
                .collect::<Vec<_>>();
            let (page, cursor) = page_records(&activity_records, limit, &args);
            Ok(with_metadata(
                json!({
                    "schemaVersion": 1,
                    "kind": "usageActivities",
                    "data": page.into_iter().map(activity_json).collect::<Vec<_>>(),
                    "nextCursor": cursor,
                }),
                &metadata,
            ))
        }
        UsageStatsAction::TaskTreeSummary => {
            let root = args.root_thread_id.as_deref().unwrap_or(&context.thread_id);
            let mut groups = BTreeMap::<String, Vec<&codex_usage::UsageRecord>>::new();
            for record in &records {
                if let Some(thread_id) = record.thread_id.as_deref()
                    && (root == "current" && thread_id == context.thread_id
                        || root != "current" && thread_id == root)
                {
                    groups
                        .entry(thread_id.to_string())
                        .or_default()
                        .push(record);
                }
            }
            let data = groups
                .into_iter()
                .map(|(thread_id, records)| {
                    let totals = records.iter().fold(
                        codex_usage::UsageTotals::default(),
                        |mut totals, record| {
                            totals.add_assign(&record.totals());
                            totals
                        },
                    );
                    json!({"threadId": thread_id, "records": totals.records, "inputTokens": totals.input_tokens, "outputTokens": totals.output_tokens})
                })
                .collect::<Vec<_>>();
            Ok(with_metadata(
                json!({
                    "schemaVersion": 1,
                    "kind": "taskTreeSummary",
                    "rootThreadId": root,
                    "data": data,
                }),
                &metadata,
            ))
        }
        UsageStatsAction::PerformanceReview => {
            let (page, cursor) = page_records(&records, limit, &args);
            let data = page.into_iter().map(review_json).collect::<Vec<_>>();
            Ok(with_metadata(
                json!({
                    "schemaVersion": 1,
                    "kind": "performanceReview",
                    "data": data,
                    "nextCursor": cursor,
                    "includeDescendants": args.include_descendants.unwrap_or(false),
                "agentId": args.agent_id,
                "outcomeCursor": args.outcome_cursor,
                "outcomeLimit": args.outcome_limit.unwrap_or(8),
                }),
                &metadata,
            ))
        }
    }
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

fn page_records<'a>(
    records: &'a [codex_usage::UsageRecord],
    limit: usize,
    args: &UsageStatsArgs,
) -> (Vec<&'a codex_usage::UsageRecord>, Value) {
    let after = args.cursor_sort_value.zip(args.cursor_id.as_deref());
    let mut filtered = records.iter().filter(|record| {
        after.is_none_or(|(timestamp, id)| {
            let current = (record.observed_at.saturating_mul(1_000), record.id.as_str());
            current > (timestamp, id)
        })
    });
    let mut page = Vec::with_capacity(limit);
    for _ in 0..limit {
        let Some(record) = filtered.next() else {
            return (page, Value::Null);
        };
        page.push(record);
    }
    let next = filtered.next().map(|record| {
        json!({
            "sortValue": record.observed_at.saturating_mul(1_000),
            "id": record.id,
        })
    });
    (page, next.map_or(Value::Null, |value| value))
}

fn record_fields(record: &codex_usage::UsageRecord) -> Value {
    json!({
        "id": record.id,
        "observedAtMs": record.observed_at.saturating_mul(1_000),
        "accountId": record.account_id,
        "operationId": record.operation_id,
        "threadId": record.thread_id,
        "repositoryId": record.repository_id,
        "model": record.model,
        "tool": record.tool,
        "activity": record.activity,
        "phase": record.phase,
        "classification": record.classification,
        "event": record.event,
        "coverage": record.coverage,
        "startedAtMs": record.started_at.map(|value| value.saturating_mul(1_000)),
        "endedAtMs": record.ended_at.map(|value| value.saturating_mul(1_000)),
        "inputTokens": record.input_tokens,
        "cachedInputTokens": record.cached_input_tokens,
        "outputTokens": record.output_tokens,
        "reasoningOutputTokens": record.reasoning_output_tokens,
    })
}

fn event_json(record: &codex_usage::UsageRecord) -> Value {
    record_fields(record)
}

fn detail_json(record: &codex_usage::UsageRecord) -> Value {
    record_fields(record)
}

fn tool_json(record: &codex_usage::UsageRecord) -> Value {
    record_fields(record)
}

fn activity_json(record: &codex_usage::UsageRecord) -> Value {
    record_fields(record)
}

fn review_json(record: &codex_usage::UsageRecord) -> Value {
    record_fields(record)
}

fn repository_rows(records: &[codex_usage::UsageRecord], limit: usize) -> Vec<Value> {
    let mut groups = BTreeMap::<String, (i64, i64, codex_usage::UsageTotals)>::new();
    for record in records {
        let Some(repository) = record.repository_id.as_deref() else {
            continue;
        };
        let entry = groups.entry(repository.to_string()).or_insert_with(|| {
            (
                record.observed_at,
                record.observed_at,
                codex_usage::UsageTotals::default(),
            )
        });
        entry.0 = entry.0.min(record.observed_at);
        entry.1 = entry.1.max(record.observed_at);
        entry.2.add_assign(&record.totals());
    }
    groups
        .into_iter()
        .take(limit)
        .map(|(id, (created, updated, totals))| {
            json!({
                "id": id,
                "label": id,
                "createdAtMs": created.saturating_mul(1_000),
                "updatedAtMs": updated.saturating_mul(1_000),
                "records": totals.records,
                "inputTokens": totals.input_tokens,
                "outputTokens": totals.output_tokens,
            })
        })
        .collect()
}

fn snapshot_metadata(store: &UsageStore) -> serde_json::Map<String, Value> {
    let manifest = store.manifest();
    serde_json::Map::from_iter([
        (
            "snapshotGeneration".to_string(),
            json!(manifest.snapshot_generation),
        ),
        ("watermarkAtMs".to_string(), json!(manifest.watermark_at_ms)),
        (
            "freshness".to_string(),
            serde_json::to_value(manifest.freshness).unwrap_or_else(|_| json!("unknown")),
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

fn usage_stats_spec() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "action".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("summary"),
                    json!("task_tree_summary"),
                    json!("performance_review"),
                    json!("repositories"),
                    json!("tools"),
                    json!("activities"),
                    json!("events"),
                    json!("details"),
                ],
                Some("Report or paged detail to read.".to_string()),
            ),
        ),
        (
            "scope".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("all"),
                    json!("current_chat"),
                    json!("current_repository"),
                    json!("repository"),
                ],
                Some("For summary only; defaults to current_chat.".to_string()),
            ),
        ),
        (
            "repository".to_string(),
            JsonSchema::string(Some(
                "Repository id, unique safe label, or current.".to_string(),
            )),
        ),
        (
            "account".to_string(),
            JsonSchema::string(Some(
                "Safe local account alias or local account reference for summary filtering."
                    .to_string(),
            )),
        ),
        (
            "thread_id".to_string(),
            JsonSchema::string(Some(
                "Thread id or current for details or performance_review.".to_string(),
            )),
        ),
        (
            "root_thread_id".to_string(),
            JsonSchema::string(Some(
                "Root task for task_tree_summary; defaults to current.".to_string(),
            )),
        ),
        (
            "include_descendants".to_string(),
            JsonSchema::boolean(Some(
                "Include recorded descendants: task_tree_summary defaults to true; thread-scoped performance_review defaults to false. Set true for daily task reviews that include delegated work."
                    .to_string(),
            )),
        ),
        (
            "outcome_cursor".to_string(),
            JsonSchema::string(Some("For performance_review, continue its fixed outcome snapshot; expired cursors require a new first page.".to_string())),
        ),
        (
            "outcome_limit".to_string(),
            JsonSchema::number(Some("For performance_review, outcome rows per page, 1–50 (default 8), subject to the response byte limit.".to_string())),
        ),
        (
            "agent_id".to_string(),
            JsonSchema::string(Some("Agent id for activity filtering.".to_string())),
        ),
        (
            "detail".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("operations"),
                    json!("model_requests"),
                    json!("tokens"),
                    json!("tools"),
                    json!("events"),
                ],
                Some("Record family for details.".to_string()),
            ),
        ),
        (
            "from_at_ms".to_string(),
            JsonSchema::number(Some("Inclusive UTC Unix milliseconds.".to_string())),
        ),
        (
            "to_at_ms".to_string(),
            JsonSchema::number(Some("Exclusive UTC Unix milliseconds.".to_string())),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some(
                "Detail/summary page size, 1–50. Summary totals repeat across pages.".to_string(),
            )),
        ),
        (
            "cursor_sort_value".to_string(),
            JsonSchema::number(Some("Sort value from the returned nextCursor.".to_string())),
        ),
        (
            "cursor_id".to_string(),
            JsonSchema::string(Some("Identifier from the returned nextCursor.".to_string())),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: TOOL_NAME.to_string(),
        description: "Read the private, content-free local usage collector. Summaries retain totals and paginate detail when needed; task_tree_summary includes recorded descendants. Details paginate every approved entity, model/tool attempt, token observation, approval, attribution, classification, coverage, wait, lifecycle, repository-evidence, and taxonomy field while omitting OS PIDs. This tool never returns prompts, output, source, commands, payloads, credentials, email, raw paths/remotes, or service/workspace identifiers. Use usage_activity correct_classification for an append-only enum correction."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["action".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
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
