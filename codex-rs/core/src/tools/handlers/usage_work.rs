use crate::UsageWorkAction;
use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub struct UsageWorkHandler;

impl ToolExecutor<ToolInvocation> for UsageWorkHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("usage_work")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "usage_work".into(),
            description: "Declare the project, workstream and outcome for this thread only when its job changes. Use status to read the current revision, then set or clear with expected_revision. Keep the binding for the same outcome across operations, retries, compaction and account changes; do not call per operation. IDs are explicit declarations, never inferred from repository, task reads or activity. Replies identify the persisted revision and effective capture sequence; on unavailable/pending inspect status before retrying. Attribution does not mean an outcome was delivered. IDs: 1–128 ASCII letters, digits or -_.:; no prose.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("action".into(), JsonSchema::string_enum(vec![json!("set"), json!("clear"), json!("status")], Some("Set, clear, or inspect work for the current thread.".into()))),
                ("expected_revision".into(), JsonSchema::integer(Some("Required for set/clear. Revision from status or the previous receipt; zero before the first declaration. An identical retry is idempotent.".into()))),
                ("native_project_id".into(), JsonSchema::string(Some("Required for set. Content-free project ID.".into()))),
                ("workstream_id".into(), JsonSchema::string(Some("Optional for set. Content-free workstream ID.".into()))),
                ("outcome_id".into(), JsonSchema::string(Some("Optional for set. Content-free outcome ID.".into()))),
            ]), Some(vec!["action".into()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(error("usage_work requires a function payload"));
            };
            if arguments.len() > 1024 {
                return Err(error("usage_work declaration exceeds 1024 bytes"));
            }
            let action: UsageWorkAction = serde_json::from_str(arguments)
                .map_err(|_| error("usage_work arguments must match its action schema"))?;
            let receipt = crate::usage_capture::work_binding::apply_work(&invocation, action)
                .await.map_err(|message| error(&message))?;
            let encoded = serde_json::to_string(&receipt)
                .map_err(|_| error("usage_work receipt unavailable; inspect status"))?;
            Ok(boxed_tool_output(FunctionToolOutput::from_text(encoded, Some(true))))
        })
    }
}

impl CoreToolRuntime for UsageWorkHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

fn error(message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}
