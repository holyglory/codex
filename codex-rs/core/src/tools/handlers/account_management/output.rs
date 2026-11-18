use super::MAX_TOOL_OUTPUT_BYTES;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_protocol::models::ResponseInputItem;
use codex_utils_output_truncation::TruncationPolicy;
use serde_json::Value;

pub(super) struct AccountManagementOutput(pub(super) FunctionToolOutput);

impl ToolOutput for AccountManagementOutput {
    fn log_output(&self) -> String {
        "content-free local account management".to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn fallback_token_limit_override(&self) -> Option<usize> {
        // Serialization already bounds complete JSON, including its pagination offset.
        Some(TruncationPolicy::Bytes(MAX_TOOL_OUTPUT_BYTES).token_budget())
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.0.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.0.code_mode_result(payload)
    }
}
