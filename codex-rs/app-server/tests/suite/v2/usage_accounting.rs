use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_usage::UsageStore;
use codex_usage::UsageSummaryScope;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn turn_continues_while_usage_store_is_unavailable_and_replays_after_recovery() -> Result<()>
{
    let server = create_mock_responses_server_sequence_unchecked(vec![
        create_final_assistant_message_sse_response("continued during outage")?,
        create_final_assistant_message_sse_response("continued after recovery")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(codex_home.path())?;
    std::fs::write(
        codex_home.path().join("usage"),
        b"blocks the usage directory",
    )?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?
        .thread;

    let first_request = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "continue despite accounting outage".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app_server.read_response(first_request).await?;
    let first: TurnCompletedNotification =
        timeout(TIMEOUT, app_server.read_notification("turn/completed")).await??;
    assert_eq!(first.turn.status, TurnStatus::Completed);
    assert!(matches!(
        &first.turn.items[..],
        [ThreadItem::AgentMessage { text, .. }] if text == "continued during outage"
    ));

    std::fs::remove_file(codex_home.path().join("usage"))?;
    let second_request = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![UserInput::Text {
                text: "continue after accounting recovery".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let _: TurnStartResponse = app_server.read_response(second_request).await?;
    let second: TurnCompletedNotification =
        timeout(TIMEOUT, app_server.read_notification("turn/completed")).await??;
    assert_eq!(second.turn.status, TurnStatus::Completed);
    assert!(matches!(
        &second.turn.items[..],
        [ThreadItem::AgentMessage { text, .. }] if text == "continued after recovery"
    ));

    let store = UsageStore::open(codex_home.path()).await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let summary = store.usage_summary(UsageSummaryScope::All).await?;
        if summary.model_request_count == 2 {
            break;
        }
        if Instant::now() >= deadline {
            assert_eq!(summary.model_request_count, 2);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    app_server.shutdown_gracefully().await?;
    Ok(())
}
