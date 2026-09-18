use super::*;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

#[tokio::test]
async fn close_diagnostics_preserve_code_and_redact_reason() {
    let (tx_command, mut commands) = mpsc::channel(/*buffer*/ 4);
    let (messages, rx_message) = mpsc::unbounded_channel();
    messages
        .send(Ok(Message::Close(Some(CloseFrame {
            code: CloseCode::Policy,
            reason: "policy violation token=synthetic-secret".into(),
        }))))
        .unwrap();
    drop(messages);
    let pump_task = tokio::spawn(async move {
        while let Some(WsCommand::Send { tx_result, .. }) = commands.recv().await {
            let _ = tx_result.send(Ok(()));
        }
    });
    let mut stream = WsStream {
        tx_command,
        rx_message,
        pump_task,
    };
    let (events, _receiver) = mpsc::channel(/*buffer*/ 4);
    let context = ResponsesWebsocketTimingLogContext {
        model: "test-model".to_string(),
        session_id: None,
        thread_id: None,
        turn_id: None,
        traceparent: None,
        previous_response_id: None,
        request_start_ms: None,
        warmup: false,
        connection_reused: false,
    };
    let error = run_websocket_response_stream(
        &mut stream,
        events,
        "{}".to_string(),
        Duration::from_secs(/*secs*/ 1),
        /*telemetry*/ None,
        /*turn_state*/ None,
        &context,
    )
    .await
    .unwrap_err();
    insta::assert_snapshot!(error.to_string(), @r#"stream error: websocket closed by server before response.completed (code: 1008, reason: "policy violation token=[REDACTED_SECRET]")"#);
}
