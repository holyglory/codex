//! Upload large context through generation-disabled Responses requests. Each
//! original input item stays intact; only the transport upload is divided.

use super::ModelClientSession;
use codex_api::ApiError;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponseEvent;
use codex_api::ResponsesWsRequest;
use codex_api::TransportError;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use serde::Serialize;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;

// A client batching target, not an assumed provider limit. Indivisible input
// items and providers that reject staging use the existing HTTP transport.
const BATCH_BYTES: usize = 4 * 1024 * 1024;

pub(super) enum StagingOutcome {
    Unchanged,
    Staged,
    FallbackToHttp,
}

#[derive(Default)]
struct ByteCount(usize);

impl io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_size(value: &impl Serialize) -> Result<usize, ApiError> {
    let mut size = ByteCount::default();
    serde_json::to_writer(&mut size, value)
        .map_err(|err| ApiError::Stream(format!("failed to measure websocket request: {err}")))?;
    Ok(size.0)
}

/// Finds the largest request prefix that both fits the transport target and leaves
/// no client-side tool call waiting for an output. The Responses API validates each
/// staged prefix independently, so a boundary between a call and its output is not
/// a valid context boundary even though both items are valid in the full history.
fn safe_batch_end(
    input: &[ResponseItem],
    sizes: &[usize],
    start: usize,
    base_bytes: usize,
) -> (usize, usize) {
    let mut pending_calls = HashSet::new();
    let mut end = start;
    let mut batch_bytes = base_bytes;
    let mut safe_end = start;
    let mut safe_bytes = base_bytes;
    while let Some(item_bytes) = sizes.get(end) {
        let next_bytes = batch_bytes.saturating_add(*item_bytes + usize::from(end > start));
        if next_bytes > BATCH_BYTES {
            break;
        }
        let call = match &input[end] {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => Some(("function", call_id.as_str(), true)),
            ResponseItem::CustomToolCall { call_id, .. } => {
                Some(("custom", call_id.as_str(), true))
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            } => Some(("search", call_id.as_str(), true)),
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } => Some(("function", call_id.as_str(), false)),
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                Some(("custom", call_id.as_str(), false))
            }
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } => Some(("search", call_id.as_str(), false)),
            ResponseItem::LocalShellCall { call_id: None, .. }
            | ResponseItem::ToolSearchCall { call_id: None, .. }
            | ResponseItem::FunctionCallOutput { call_id: None, .. }
            | ResponseItem::AdditionalTools { .. }
            | ResponseItem::Message { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::ToolSearchOutput { call_id: None, .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => None,
        };
        if let Some((kind, call_id, opens)) = call {
            if opens {
                pending_calls.insert((kind, call_id));
            } else {
                pending_calls.remove(&(kind, call_id));
            }
        }
        batch_bytes = next_bytes;
        end += 1;
        if pending_calls.is_empty() {
            safe_end = end;
            safe_bytes = batch_bytes;
        }
    }
    (safe_end, safe_bytes)
}

fn staging_can_fallback(error: &ApiError) -> bool {
    matches!(error, ApiError::Stream(_) | ApiError::InvalidRequest { .. })
        || matches!(error, ApiError::Transport(TransportError::Http { status, .. })
            if matches!(status.as_u16(), 400 | 413))
}

impl ModelClientSession {
    #[tracing::instrument(skip_all, fields(
        transport = "websocket",
        thread_id = %self.client.state.thread_id,
        turn_id = request.client_metadata.as_ref().and_then(|metadata| metadata.get("turn_id")).map(String::as_str),
        model = request.model,
    ))]
    pub(super) async fn stage_large_websocket_request(
        &mut self,
        request: &mut ResponseCreateWsRequest<'_>,
    ) -> Result<StagingOutcome, ApiError> {
        let request_bytes = encoded_size(&ResponsesWsRequest::ResponseCreate(request.clone()))?;
        if request_bytes <= BATCH_BYTES {
            return Ok(StagingOutcome::Unchanged);
        }

        tracing::info!(target: "codex.network_diagnostics",
            event = "websocket_batching", request_bytes, batch_limit_bytes = BATCH_BYTES,
            input_items = request.input.len());
        let input = request.input;
        let generate = request.generate;
        let sizes = input
            .iter()
            .map(encoded_size)
            .collect::<Result<Vec<_>, _>>()?;
        let mut start = 0;
        let mut batch_index = 0;
        request.generate = Some(false);
        loop {
            request.input = &[];
            let base_bytes = encoded_size(&ResponsesWsRequest::ResponseCreate(request.clone()))?;
            let (end, batch_bytes) = safe_batch_end(input, &sizes, start, base_bytes);
            if end == start {
                let kind = if sizes
                    .get(start)
                    .is_some_and(|item_bytes| base_bytes.saturating_add(*item_bytes) <= BATCH_BYTES)
                {
                    "unsafe_tool_call_boundary"
                } else {
                    "indivisible_request"
                };
                tracing::warn!(target: "codex.network_diagnostics",
                    event = "websocket_batching_fallback", kind,
                    request_bytes, batch_limit_bytes = BATCH_BYTES);
                return Ok(StagingOutcome::FallbackToHttp);
            }
            request.input = &input[start..end];
            if end == input.len() {
                request.generate = generate;
                return Ok(StagingOutcome::Staged);
            }

            // A cancelled/failed partial upload must not leave a reusable local
            // baseline pointing at an evicted server-side response. The caller
            // records the full request again after final dispatch succeeds.
            self.websocket_session.last_request = None;
            self.websocket_session.last_response_rx = None;
            self.websocket_session.last_response_from_untraced_warmup = false;
            let Some(connection) = self.websocket_session.connection.as_ref() else {
                return Ok(StagingOutcome::FallbackToHttp);
            };
            batch_index += 1;
            tracing::info!(target: "codex.network_diagnostics",
                event = "websocket_context_batch", batch_index, request_bytes = batch_bytes,
                input_items = end - start, warmup = true);
            let mut stream = match connection
                .stream_request(
                    ResponsesWsRequest::ResponseCreate(request.clone()),
                    self.websocket_session.connection_reused(),
                    Some(Arc::clone(&self.turn_state)),
                )
                .await
            {
                Ok(stream) => stream,
                Err(err) if staging_can_fallback(&err) => {
                    return Ok(StagingOutcome::FallbackToHttp);
                }
                Err(err) => return Err(err),
            };
            let response_id = loop {
                match stream.next().await {
                    Some(Ok(ResponseEvent::Completed { response_id, .. }))
                        if !response_id.is_empty() =>
                    {
                        break response_id;
                    }
                    Some(Ok(
                        ResponseEvent::Created { .. }
                        | ResponseEvent::SafetyBuffering(_)
                        | ResponseEvent::ServerModel(_)
                        | ResponseEvent::ModelVerifications(_)
                        | ResponseEvent::TurnModerationMetadata(_)
                        | ResponseEvent::ServerReasoningIncluded(_)
                        | ResponseEvent::ProviderUsage(_)
                        | ResponseEvent::RateLimits(_)
                        | ResponseEvent::ModelsEtag(_),
                    )) => {}
                    Some(Ok(
                        ResponseEvent::Completed { .. }
                        | ResponseEvent::OutputItemDone(_)
                        | ResponseEvent::OutputItemAdded(_)
                        | ResponseEvent::OutputTextDelta(_)
                        | ResponseEvent::ToolCallInputDelta { .. }
                        | ResponseEvent::ReasoningSummaryDelta { .. }
                        | ResponseEvent::ReasoningSummaryDone { .. }
                        | ResponseEvent::ReasoningContentDelta { .. }
                        | ResponseEvent::ReasoningSummaryPartAdded { .. },
                    ))
                    | None => {
                        tracing::warn!(target: "codex.network_diagnostics",
                            event = "websocket_batching_fallback", kind = "staging_failed",
                            batch_index, request_bytes = batch_bytes);
                        return Ok(StagingOutcome::FallbackToHttp);
                    }
                    Some(Err(err)) if staging_can_fallback(&err) => {
                        tracing::warn!(target: "codex.network_diagnostics",
                            event = "websocket_batching_fallback", kind = "staging_rejected",
                            batch_index, request_bytes = batch_bytes);
                        return Ok(StagingOutcome::FallbackToHttp);
                    }
                    Some(Err(err)) => return Err(err),
                }
            };
            request.previous_response_id = Some(response_id);
            self.websocket_session
                .set_connection_reused(/*connection_reused*/ true);
            start = end;
        }
    }
}

#[cfg(test)]
#[path = "websocket_batching_tests.rs"]
mod tests;
