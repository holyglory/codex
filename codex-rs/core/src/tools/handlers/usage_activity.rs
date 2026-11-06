use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::usage_capture::Activity;
use crate::usage_capture::ActivityAction;
use crate::usage_capture::Persistence;
use crate::usage_capture::Phase;
use crate::usage_capture::Relation;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub struct UsageActivityHandler;

impl ToolExecutor<ToolInvocation> for UsageActivityHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("usage_activity")
    }

    fn spec(&self) -> ToolSpec {
        let enum_values = |values: &[&str], description: &str| {
            JsonSchema::string_enum(
                values.iter().map(|value| json!(value)).collect(),
                Some(description.to_string()),
            )
        };
        ToolSpec::Function(ResponsesApiTool {
            name: "usage_activity".to_string(),
            description: "Declare the category for upcoming work or append an enum-only correction to stored usage. Use set before the next model generation request and at every phase boundary; a replacement set ends the prior phase when it activates. Use heartbeat periodically while active and end when work stops without a replacement. Set relation=rework_previous only for an explicit redo of the prior model operation. Corrections use target_id plus phase/activity and never change current declarations. Declarations activate at the next generation request; warmups do not activate them. Replies distinguish recorded persistence from pending or omitted observations; an unresolved correction target remains pending. Do not include prose or task content.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("action".to_string(), enum_values(&["set", "heartbeat", "end", "correct_classification"], "Set, refresh, or end declared activity, or append a classification correction.")),
                ("phase".to_string(), enum_values(Phase::VALUES, "Required for set and correct_classification.")),
                ("activity".to_string(), enum_values(Activity::VALUES, "Required for set and correct_classification.")),
                ("relation".to_string(), enum_values(Relation::VALUES, "Optional for set; defaults to new_work.")),
                ("target_id".to_string(), JsonSchema::string(Some("Operation UUID or its started/finished event ID; required for correct_classification.".to_string()))),
            ]), Some(vec!["action".to_string()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(error("usage_activity requires a function payload"));
            };
            let raw: serde_json::Value = serde_json::from_str(arguments)
                .map_err(|_| error("usage_activity arguments must match its enum schema"))?;
            let object = raw
                .as_object()
                .ok_or_else(|| error("usage_activity arguments must match its enum schema"))?;
            let action_name = object
                .get("action")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| error("usage_activity arguments must match its enum schema"))?;
            let allowed = match action_name {
                "set" => ["action", "phase", "activity", "relation"].as_slice(),
                "heartbeat" | "end" => ["action"].as_slice(),
                "correct_classification" => ["action", "target_id", "phase", "activity"].as_slice(),
                _ => return Err(error("usage_activity arguments must match its enum schema")),
            };
            if object.keys().any(|key| !allowed.contains(&key.as_str())) {
                return Err(error("usage_activity arguments must match its enum schema"));
            }
            let action: ActivityAction = serde_json::from_value(raw)
                .map_err(|_| error("usage_activity arguments must match its enum schema"))?;
            let receipt = crate::usage_capture::apply_activity(&invocation, action)
                .await
                .map_err(error)?;
            let persisted = receipt.persistence == Persistence::Recorded;
            let encoded = serde_json::to_string(&receipt)
                .map_err(|_| error("usage activity receipt is unavailable"))?;
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                encoded,
                Some(persisted),
            )))
        })
    }
}

impl CoreToolRuntime for UsageActivityHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

fn error(message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.to_string())
}
