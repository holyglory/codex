use crate::context_manager::estimate_item_token_count;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesApiTools;
use codex_api::TextControls;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModelContextEstimate {
    pub(crate) policy_tokens: u64,
    pub(crate) conversation_tokens: u64,
    pub(crate) tool_output_tokens: u64,
}

#[derive(Clone, Copy)]
enum RequestContextSource {
    Policy,
    Conversation,
    ToolOutput,
}

pub(crate) fn model_context_estimate(
    request: &ResponsesApiRequest,
) -> Option<ModelContextEstimate> {
    model_context_estimate_parts(
        &request.instructions,
        request.tools.as_ref(),
        request.text.as_ref(),
        &request.input,
    )
}

pub(crate) fn model_context_estimate_parts(
    instructions: &str,
    tools: Option<&ResponsesApiTools>,
    text: Option<&TextControls>,
    input: &[ResponseItem],
) -> Option<ModelContextEstimate> {
    let mut estimate = ModelContextEstimate {
        policy_tokens: estimated_text_tokens(instructions)?,
        conversation_tokens: 0,
        tool_output_tokens: 0,
    };
    if let Some(tools) = tools {
        let encoded = serde_json::to_string(tools).ok()?;
        estimate.policy_tokens = estimate
            .policy_tokens
            .checked_add(estimated_text_tokens(&encoded)?)?;
    }
    if let Some(text) = text {
        let encoded = serde_json::to_string(text).ok()?;
        estimate.policy_tokens = estimate
            .policy_tokens
            .checked_add(estimated_text_tokens(&encoded)?)?;
    }
    for item in input {
        let tokens = u64::try_from(estimate_item_token_count(item).max(0)).ok()?;
        let total = match request_context_source(item) {
            RequestContextSource::Policy => &mut estimate.policy_tokens,
            RequestContextSource::Conversation => &mut estimate.conversation_tokens,
            RequestContextSource::ToolOutput => &mut estimate.tool_output_tokens,
        };
        *total = total.checked_add(tokens)?;
    }
    Some(estimate)
}

fn estimated_text_tokens(value: &str) -> Option<u64> {
    u64::try_from(approx_token_count(value)).ok()
}

fn request_context_source(item: &ResponseItem) -> RequestContextSource {
    match item {
        ResponseItem::AdditionalTools { .. } => RequestContextSource::Policy,
        ResponseItem::Message { role, .. } if matches!(role.as_str(), "developer" | "system") => {
            RequestContextSource::Policy
        }
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. } => RequestContextSource::ToolOutput,
        ResponseItem::Message { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => RequestContextSource::Conversation,
    }
}
