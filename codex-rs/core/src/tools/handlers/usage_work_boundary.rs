use crate::UsageWorkBoundaryState;
use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::usage_capture::work_binding::apply_work_boundary;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

const TOOL_NAME: &str = "usage_work_boundary";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageWorkBoundaryArgs {
    state: Option<UsageWorkBoundaryState>,
}

pub struct UsageWorkBoundaryHandler;

impl ToolExecutor<ToolInvocation> for UsageWorkBoundaryHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.into(),
            description: "Resolve the current turn or job attribution boundary exactly once when no usage_work binding is available. Omit state to inspect the bounded request, then ask the user to choose continue, select, new, clear, or unknown. Use an explicit state after the user answers. Never call per operation, never infer identifiers, and keep the same boundary decision across retries and compaction. A bound or explicitly cleared boundary returns no request.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "state".into(),
                    JsonSchema::string_enum(
                        ["continue", "select", "new", "clear", "unknown"]
                            .into_iter()
                            .map(|value| json!(value))
                            .collect(),
                        Some("Optional explicit answer for this turn or job boundary.".into()),
                    ),
                )]),
                Some(Vec::new()),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(error("usage_work_boundary requires a function payload"));
            };
            if arguments.len() > 512 {
                return Err(error("usage_work_boundary declaration exceeds 512 bytes"));
            }
            let args: UsageWorkBoundaryArgs = serde_json::from_str(arguments)
                .map_err(|_| error("usage_work_boundary arguments must match its schema"))?;
            let status = apply_work_boundary(&invocation, args.state)
                .await
                .map_err(|message| error(&message))?;
            let encoded = serde_json::to_string(&status)
                .map_err(|_| error("usage_work_boundary response unavailable"))?;
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                encoded,
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for UsageWorkBoundaryHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

fn error(message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}

#[cfg(test)]
#[path = "usage_work_boundary_tests.rs"]
mod tests;
