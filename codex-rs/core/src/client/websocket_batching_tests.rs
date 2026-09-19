use super::safe_batch_end;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;

const BATCH_BYTES: usize = 4 * 1024 * 1024;

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

    let (end, bytes) = safe_batch_end(&items, &sizes, 0, 0);

    assert_eq!(end, 1);
    assert_eq!(bytes, BATCH_BYTES - 100);

    let (end, bytes) = safe_batch_end(&items, &sizes, 1, 0);

    assert_eq!(end, items.len());
    assert_eq!(bytes, 202);
}

#[test]
fn staging_falls_back_when_a_call_and_output_cannot_fit_together() {
    let items = vec![custom_call("call-1"), custom_output("call-1")];
    let sizes = vec![BATCH_BYTES - 10, 20];

    let (end, bytes) = safe_batch_end(&items, &sizes, 0, 0);

    assert_eq!(end, 0);
    assert_eq!(bytes, 0);
}
