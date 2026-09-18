//! Upload large context through generation-disabled Responses requests. Each
//! original input item stays intact; only the transport upload is divided.

use super::ModelClientSession;
use codex_api::ApiError;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponseEvent;
use codex_api::ResponsesWsRequest;
use futures::StreamExt;
use serde::Serialize;
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

impl ModelClientSession {
    #[tracing::instrument(skip_all)]
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
            let mut batch_bytes =
                encoded_size(&ResponsesWsRequest::ResponseCreate(request.clone()))?;
            let mut end = start;
            while let Some(item_bytes) = sizes.get(end) {
                let next_bytes = batch_bytes.saturating_add(*item_bytes + usize::from(end > start));
                if next_bytes > BATCH_BYTES {
                    break;
                }
                batch_bytes = next_bytes;
                end += 1;
            }
            if end == start {
                tracing::warn!(target: "codex.network_diagnostics",
                    event = "websocket_batching_fallback", kind = "indivisible_request",
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
                Err(_) => return Ok(StagingOutcome::FallbackToHttp),
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
                    | Some(Err(_))
                    | None => {
                        tracing::warn!(target: "codex.network_diagnostics",
                            event = "websocket_batching_fallback", kind = "staging_failed",
                            batch_index, request_bytes = batch_bytes);
                        return Ok(StagingOutcome::FallbackToHttp);
                    }
                }
            };
            request.previous_response_id = Some(response_id);
            start = end;
        }
    }
}
