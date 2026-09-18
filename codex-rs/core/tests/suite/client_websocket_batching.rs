use super::*;
use core_test_support::responses::WebSocketRequest;
use pretty_assertions::assert_eq;

fn large_input() -> Vec<ResponseItem> {
    (0..3)
        .map(|index| message_item(&format!("{index} 🦀 \\\"\n{}", "x".repeat(3 * 1024 * 1024))))
        .collect()
}

fn replies(count: usize) -> Vec<Vec<serde_json::Value>> {
    (0..count)
        .map(|index| {
            let id = format!("resp-{index}");
            vec![ev_response_created(&id), ev_completed(&id)]
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_preserves_context_window_errors() {
    skip_if_no_network!();
    let server = start_websocket_server(vec![vec![vec![json!({
        "type": "response.failed",
        "response": {"id": "failed-stage", "error": {
            "code": "context_length_exceeded", "message": "context exceeded"
        }}
    })]]])
    .await;
    let harness = websocket_harness(&server).await;
    let result = harness
        .client
        .new_session()
        .stream(
            &prompt_with_input(large_input()),
            &harness.model_info,
            &harness.session_telemetry,
            harness.effort.clone(),
            harness.summary,
            /*service_tier*/ None,
            &turn_metadata(&harness, /*turn_id*/ None),
            &InferenceTraceContext::disabled(),
        )
        .await;
    assert!(matches!(
        result,
        Err(codex_protocol::error::CodexErr::ContextWindowExceeded)
    ));
    assert_eq!(server.single_connection().len(), 1);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_context_is_uploaded_once_before_generation_and_remains_incremental() {
    skip_if_no_network!();
    let server = start_websocket_server(vec![replies(/*count*/ 5)]).await;
    let harness = websocket_harness(&server).await;
    let mut session = harness.client.new_session();
    let first = message_item("earlier context");
    stream_until_complete(
        &mut session,
        &harness,
        &prompt_with_input(vec![first.clone()]),
    )
    .await;

    let uploaded = large_input();
    let mut input = vec![first];
    input.extend(uploaded.clone());
    let mut prompt = prompt_with_input(input);
    stream_until_complete(&mut session, &harness, &prompt).await;
    let next = message_item("continue after the large upload");
    prompt.input.push(next.clone());
    stream_until_complete(&mut session, &harness, &prompt).await;

    let requests = server.single_connection();
    assert_eq!(requests.len(), 5);
    let batches: Vec<_> = requests[1..4]
        .iter()
        .map(WebSocketRequest::body_json)
        .collect();
    let reconstructed: Vec<_> = batches
        .iter()
        .flat_map(|body| body["input"].as_array().unwrap().clone())
        .collect();
    assert_eq!(
        reconstructed,
        serde_json::to_value(&uploaded)
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    );
    for (index, body) in batches.iter().enumerate() {
        assert!(serde_json::to_vec(body).unwrap().len() <= 4 * 1024 * 1024);
        assert_eq!(body["previous_response_id"], format!("resp-{index}"));
        assert_eq!(body.get("generate"), (index < 2).then_some(&json!(false)));
    }
    let next_request = requests[4].body_json();
    assert_eq!(next_request["previous_response_id"], "resp-3");
    assert_eq!(next_request["input"], json!([next]));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_prewarm_preserves_all_context_for_one_generated_response() {
    skip_if_no_network!();
    let server = start_websocket_server(vec![replies(/*count*/ 4)]).await;
    let harness = websocket_harness(&server).await;
    let mut session = harness.client.new_session();
    let prompt = prompt_with_input(large_input());
    session
        .prewarm_websocket(
            &prompt,
            &harness.model_info,
            &harness.session_telemetry,
            harness.effort.clone(),
            harness.summary,
            /*service_tier*/ None,
            &prewarm_metadata(&harness, /*turn_id*/ None),
        )
        .await
        .unwrap();
    stream_until_complete(&mut session, &harness, &prompt).await;
    let requests = server.single_connection();
    assert_eq!(requests.len(), 4);
    let mut reconstructed = Vec::new();
    for request in &requests[..3] {
        let body = request.body_json();
        assert_eq!(body["generate"], false);
        assert!(serde_json::to_vec(&body).unwrap().len() <= 4 * 1024 * 1024);
        reconstructed.extend(body["input"].as_array().unwrap().clone());
    }
    assert_eq!(json!(reconstructed), json!(prompt.input));
    let final_request = requests[3].body_json();
    assert_eq!(final_request["input"], json!([]));
    assert_eq!(final_request["previous_response_id"], "resp-2");
    assert!(!final_request.as_object().unwrap().contains_key("generate"));
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_upload_does_not_generate_or_reuse_partial_context() {
    skip_if_no_network!();
    let server = start_websocket_server_with_headers(vec![
        WebSocketConnectionConfig {
            requests: vec![vec![]],
            response_headers: vec![],
            accept_delay: None,
            close_after_requests: false,
        },
        WebSocketConnectionConfig {
            requests: replies(/*count*/ 3),
            response_headers: vec![],
            accept_delay: None,
            close_after_requests: true,
        },
    ])
    .await;
    let harness = websocket_harness(&server).await;
    let mut session = harness.client.new_session();
    let prompt = prompt_with_input(large_input());
    let metadata = turn_metadata(&harness, /*turn_id*/ None);
    let trace = InferenceTraceContext::disabled();
    let mut pending = Box::pin(session.stream(
        &prompt,
        &harness.model_info,
        &harness.session_telemetry,
        harness.effort.clone(),
        harness.summary,
        /*service_tier*/ None,
        &metadata,
        &trace,
    ));
    tokio::select! {
        _ = server.wait_for_request(/*connection_index*/ 0, /*request_index*/ 0) => {}
        _ = &mut pending => panic!("staging returned before its acknowledgement"),
    }
    drop(pending);
    stream_until_complete(&mut session, &harness, &prompt).await;
    let connections = server.connections();
    assert_eq!(
        connections.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(connections[0][0].body_json()["generate"], false);
    let restarted = connections[1][0].body_json();
    assert_eq!(restarted["input"], json!([prompt.input[0]]));
    assert!(restarted.get("previous_response_id").is_none());
    server.shutdown().await;
}
