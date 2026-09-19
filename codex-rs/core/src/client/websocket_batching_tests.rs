use super::BATCH_BYTES;
use super::safe_batch_end;
use super::staging_can_fallback;
use codex_api::ApiError;
use codex_api::TransportError;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn custom_call(call_id: &str) -> ResponseItem {
    ResponseItem::CustomToolCall {
        id: None,
        status: Some("completed".into()),
        call_id: call_id.into(),
        name: "exec".into(),
        namespace: None,
        input: "{}".into(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn custom_output(call_id: &str) -> ResponseItem {
    ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: call_id.into(),
        name: Some("exec".into()),
        output: FunctionCallOutputPayload::from_text("ok".into()),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn staging_boundary_keeps_custom_tool_call_with_output() {
    let items = vec![
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        custom_call("call-1"),
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        custom_output("call-1"),
    ];
    let sizes = vec![BATCH_BYTES - 100, 50, 100, 50];

    let (end, bytes) = safe_batch_end(&items, &sizes, /*start*/ 0, /*base_bytes*/ 0);

    assert_eq!((end, bytes), (1, BATCH_BYTES - 100));

    let (end, bytes) = safe_batch_end(&items, &sizes, /*start*/ 1, /*base_bytes*/ 0);

    assert_eq!((end, bytes), (items.len(), 202));
}

#[test]
fn staging_falls_back_when_a_call_and_output_cannot_fit_together() {
    let items = vec![custom_call("call-1"), custom_output("call-1")];
    let sizes = vec![BATCH_BYTES - 10, 20];

    let (end, bytes) = safe_batch_end(&items, &sizes, /*start*/ 0, /*base_bytes*/ 0);

    assert_eq!((end, bytes), (0, 0));
}

#[test]
fn interleaved_calls_with_matching_ids_keep_independent_outputs() {
    let call = serde_json::from_value(serde_json::json!({
        "type":"function_call", "call_id":"shared", "name":"f", "arguments":"{}"
    }))
    .unwrap();
    let output = serde_json::from_value(serde_json::json!({
        "type":"function_call_output", "call_id":"shared", "output":"done"
    }))
    .unwrap();
    let items = vec![call, custom_call("shared"), custom_output("shared"), output];
    assert_eq!(
        safe_batch_end(
            &items,
            &[10, 10, 10, BATCH_BYTES],
            /*start*/ 0,
            /*base_bytes*/ 0
        ),
        (0, 0)
    );
    assert_eq!(
        safe_batch_end(
            &items,
            &[10, 10, 10, 10],
            /*start*/ 0,
            /*base_bytes*/ 0
        ),
        (4, 43)
    );
}

#[test]
fn staging_rejection_fallback_preserves_auth_and_capacity_errors() {
    let actual: Vec<_> = [400, 413, 401, 403, 429, 503]
        .into_iter()
        .map(|status| {
            staging_can_fallback(&ApiError::Transport(TransportError::Http {
                status: http::StatusCode::from_u16(status).unwrap(),
                url: None,
                headers: None,
                body: None,
            }))
        })
        .collect();
    assert_eq!(actual, vec![true, true, false, false, false, false]);
}
