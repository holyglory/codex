use super::*;
use codex_tools::JsonSchema;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn boundary_tool_is_optional_state_and_forbids_per_operation_guidance() {
    assert_eq!(
        UsageWorkBoundaryHandler.spec(),
        ToolSpec::Function(ResponsesApiTool {
            name: "usage_work_boundary".into(),
            description: "Resolve the current turn or job attribution boundary exactly once when no usage_work binding is available. Omit state to inspect the bounded request, then ask the user to choose continue, select, new, clear, or unknown. Use an explicit state after the user answers. Never call per operation, never infer identifiers, and keep the same boundary decision across retries and compaction. A bound or explicitly cleared boundary returns no request.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "state".into(),
                    JsonSchema::string_enum(
                        ["continue", "select", "new", "clear", "unknown"]
                            .into_iter()
                            .map(|value| serde_json::json!(value))
                            .collect(),
                        Some("Optional explicit answer for this turn or job boundary.".into()),
                    ),
                )]),
                Some(Vec::new()),
                Some(false.into()),
            ),
            output_schema: None,
        })
    );
}
