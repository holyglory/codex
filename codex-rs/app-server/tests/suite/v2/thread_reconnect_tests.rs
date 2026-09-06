use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadTurnsListParams;
use codex_app_server_protocol::ThreadTurnsListResponse;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

const READ_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 20);

async fn connect(codex_home: &Path) -> Result<TestAppServer> {
    TestAppServer::builder()
        .with_codex_home(codex_home)
        .without_managed_config()
        .build_initialized()
        .await
}

fn input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }]
}

async fn loaded_threads(app: &mut TestAppServer) -> Result<Vec<String>> {
    let response: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: ThreadLoadedListParams::default(),
        })
        .await?;
    Ok(response.data)
}

async fn model_requests(server: &MockServer) -> Result<Vec<Value>> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .map(|request| request.body_json().map_err(Into::into))
        .collect()
}

#[tokio::test]
async fn stale_steer_after_restart_restores_task_without_submitting_input() -> Result<()> {
    let home = TempDir::new()?;
    let server = create_mock_responses_server_sequence(vec![
        create_final_assistant_message_sse_response("first reply")?,
        create_final_assistant_message_sse_response("second reply")?,
    ])
    .await;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = connect(home.path()).await?;
    let thread = app.start_thread(ThreadStartParams::default()).await?.thread;
    let first = timeout(
        READ_TIMEOUT,
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: input("first message"),
            ..Default::default()
        }),
    )
    .await??;
    timeout(READ_TIMEOUT, app.shutdown_gracefully()).await??;

    let mut app = connect(home.path()).await?;
    assert_eq!(loaded_threads(&mut app).await?, Vec::<String>::new());
    let request = app
        .send_turn_steer_request(TurnSteerParams {
            thread_id: thread.id.clone(),
            client_user_message_id: Some("reconnected-message".to_string()),
            input: input("second message"),
            responsesapi_client_metadata: None,
            additional_context: None,
            expected_turn_id: first.turn.id.clone(),
        })
        .await?;
    let error = timeout(
        READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(request)),
    )
    .await??;
    assert_eq!(
        error.error,
        JSONRPCErrorError {
            code: -32600,
            message: "no active turn to steer".to_string(),
            data: None,
        }
    );
    assert_eq!(loaded_threads(&mut app).await?, vec![thread.id.clone()]);
    assert_eq!(model_requests(&server).await?.len(), 1);

    // The client can safely retry the same message as a new turn, on the same connection.
    let second = timeout(
        READ_TIMEOUT,
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: Some("reconnected-message".to_string()),
            input: input("second message"),
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(
        (second.thread_id, second.turn.status),
        (thread.id, TurnStatus::Completed)
    );
    assert_ne!(first.turn.id, second.turn.id);
    assert_eq!(model_requests(&server).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn direct_input_survives_repeated_restarts_with_history_and_live_responses() -> Result<()> {
    for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        let home = TempDir::new()?;
        let server = create_mock_responses_server_sequence(vec![
            create_final_assistant_message_sse_response("reply one")?,
            create_final_assistant_message_sse_response("reply two")?,
            create_final_assistant_message_sse_response("reply three")?,
        ])
        .await;
        MockResponsesConfig::new(&server.uri()).write(home.path())?;
        let mut app = connect(home.path()).await?;
        let thread = app
            .start_thread(ThreadStartParams {
                history_mode: Some(history_mode),
                ..Default::default()
            })
            .await?
            .thread;
        let mut expected_turns = Vec::new();
        for message in ["message one", "message two", "message three"] {
            let completed = timeout(
                READ_TIMEOUT,
                app.start_turn_and_wait_for_completion(TurnStartParams {
                    thread_id: thread.id.clone(),
                    input: input(message),
                    ..Default::default()
                }),
            )
            .await??;
            expected_turns.push((completed.turn.id, TurnStatus::Completed));
            let history: ThreadTurnsListResponse = app
                .request(|request_id| ClientRequest::ThreadTurnsList {
                    request_id,
                    params: ThreadTurnsListParams {
                        thread_id: thread.id.clone(),
                        cursor: None,
                        limit: Some(10),
                        sort_direction: Some(SortDirection::Asc),
                        items_view: Some(TurnItemsView::Summary),
                    },
                })
                .await?;
            assert_eq!(
                history
                    .data
                    .into_iter()
                    .map(|turn| (turn.id, turn.status))
                    .collect::<Vec<_>>(),
                expected_turns
            );
            timeout(READ_TIMEOUT, app.shutdown_gracefully()).await??;
            app = connect(home.path()).await?;
            assert_eq!(loaded_threads(&mut app).await?, Vec::<String>::new());
        }
        let requests = model_requests(&server).await?;
        assert_eq!(requests.len(), 3);
        let last_input = requests[2]["input"].as_array().expect("model input");
        for text in ["message one", "message two", "message three"] {
            let count = last_input
                .iter()
                .filter(|item| {
                    item["role"] == "user"
                        && item["content"]
                            .as_array()
                            .is_some_and(|content| content.iter().any(|part| part["text"] == text))
                })
                .count();
            assert_eq!(count, 1, "history must contain each message exactly once");
        }
    }
    Ok(())
}

#[tokio::test]
async fn input_recovery_does_not_create_missing_tasks() -> Result<()> {
    let home = TempDir::new()?;
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = connect(home.path()).await?;
    for thread_id in ["00000000-0000-4000-8000-000000000001", "invalid-task-id"] {
        let request = app
            .send_turn_start_request(TurnStartParams {
                thread_id: thread_id.to_string(),
                input: input("must not run"),
                ..Default::default()
            })
            .await?;
        let error = timeout(
            READ_TIMEOUT,
            app.read_stream_until_error_message(RequestId::Integer(request)),
        )
        .await??;
        assert_eq!(error.error.code, -32600);
        if thread_id != "invalid-task-id" {
            let expected = JSONRPCErrorError {
                code: -32600,
                message: format!("thread not found: {thread_id}"),
                data: None,
            };
            assert_eq!(error.error, expected);
            let request = app
                .send_turn_steer_request(TurnSteerParams {
                    thread_id: thread_id.to_string(),
                    input: input("must not run"),
                    client_user_message_id: None,
                    responsesapi_client_metadata: None,
                    additional_context: None,
                    expected_turn_id: "missing-turn".to_string(),
                })
                .await?;
            let error = timeout(
                READ_TIMEOUT,
                app.read_stream_until_error_message(RequestId::Integer(request)),
            )
            .await??;
            assert_eq!(error.error, expected);
        }
        assert_eq!(loaded_threads(&mut app).await?, Vec::<String>::new());
    }
    assert_eq!(model_requests(&server).await?, Vec::<Value>::new());
    Ok(())
}

#[tokio::test]
async fn input_recovery_preserves_archive_and_exclusive_writer_rules() -> Result<()> {
    let home = TempDir::new()?;
    let server =
        create_mock_responses_server_sequence(vec![create_final_assistant_message_sse_response(
            "saved reply",
        )?])
        .await;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut owner = connect(home.path()).await?;
    let thread = owner
        .start_thread(ThreadStartParams {
            history_mode: Some(ThreadHistoryMode::Paginated),
            ..Default::default()
        })
        .await?
        .thread;
    timeout(
        READ_TIMEOUT,
        owner.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: input("saved message"),
            ..Default::default()
        }),
    )
    .await??;
    let mut other = connect(home.path()).await?;
    let request = other
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: input("must not steal writer"),
            ..Default::default()
        })
        .await?;
    let error = timeout(
        READ_TIMEOUT,
        other.read_stream_until_error_message(RequestId::Integer(request)),
    )
    .await??;
    assert_eq!(error.error.code, -32600);
    assert!(
        error.error.message.contains("already") || error.error.message.contains("another"),
        "{}",
        error.error.message
    );
    assert_eq!(loaded_threads(&mut other).await?, Vec::<String>::new());
    timeout(READ_TIMEOUT, other.shutdown_gracefully()).await??;

    let _: ThreadArchiveResponse = owner
        .request(|request_id| ClientRequest::ThreadArchive {
            request_id,
            params: ThreadArchiveParams {
                thread_id: thread.id.clone(),
            },
        })
        .await?;
    let request = owner
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: input("must not unarchive"),
            ..Default::default()
        })
        .await?;
    let error = timeout(
        READ_TIMEOUT,
        owner.read_stream_until_error_message(RequestId::Integer(request)),
    )
    .await??;
    assert_eq!(error.error.code, -32600);
    assert!(
        error.error.message.contains("archived"),
        "{}",
        error.error.message
    );
    assert_eq!(loaded_threads(&mut owner).await?, Vec::<String>::new());
    assert_eq!(model_requests(&server).await?.len(), 1);
    Ok(())
}
