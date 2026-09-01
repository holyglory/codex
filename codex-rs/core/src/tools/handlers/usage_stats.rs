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
use codex_usage::ThreadId;
use codex_usage::UsageDetailKind;
use codex_usage::UsageStore;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

mod query;
mod query_lists;
mod repository;

const TOOL_NAME: &str = "usage_stats";
const DEFAULT_PAGE_LIMIT: u32 = 10;
const MAX_PAGE_LIMIT: u32 = 50;
const MAX_OUTPUT_BYTES: usize = 16 * 1_024;

pub struct UsageStatsHandler;

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UsageStatsAction {
    Summary,
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
    agent_id: Option<String>,
    detail: Option<String>,
    from_at_ms: Option<i64>,
    to_at_ms: Option<i64>,
    limit: Option<u32>,
    cursor_sort_value: Option<i64>,
    cursor_id: Option<String>,
}

struct UsageStatsContext {
    codex_home: PathBuf,
    thread_id: ThreadId,
    cwd: Option<PathBuf>,
}

impl UsageStatsContext {
    fn from_invocation(invocation: &ToolInvocation) -> Result<Self, FunctionCallError> {
        Ok(Self {
            codex_home: invocation.turn.config.codex_home.to_path_buf(),
            thread_id: ThreadId::new(invocation.session.thread_id().to_string())
                .map_err(|_| storage_error())?,
            cwd: invocation
                .step_context
                .environments
                .primary()
                .map(|environment| environment.cwd().to_path_buf()),
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
            let store = UsageStore::open(&context.codex_home)
                .await
                .map_err(|_| storage_error())?;
            let value = query::execute(&store, &context, args).await?;
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

impl CoreToolRuntime for UsageStatsHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }

    fn bypasses_tool_hooks(&self) -> bool {
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
                Some("Summary scope; defaults to current_chat.".to_string()),
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
                "Thread id or current for detail filtering.".to_string(),
            )),
        ),
        (
            "agent_id".to_string(),
            JsonSchema::string(Some("Agent id for activity filtering.".to_string())),
        ),
        (
            "detail".to_string(),
            JsonSchema::string_enum(
                UsageDetailKind::ALL
                    .into_iter()
                    .map(|kind| serde_json::Value::from(kind.as_str()))
                    .collect(),
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
            JsonSchema::number(Some("Detail page size from 1 through 50.".to_string())),
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
        description: "Read the private, content-free local usage collector. Summary preserves every aggregate dimension. Details paginate every approved entity, model/tool attempt, token observation, approval, attribution, classification, coverage, wait, lifecycle, repository-evidence, and taxonomy field while omitting OS PIDs. This tool never returns prompts, output, source, commands, payloads, credentials, email, raw paths/remotes, or service/workspace identifiers. Use usage_activity correct_classification for an append-only enum correction."
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
