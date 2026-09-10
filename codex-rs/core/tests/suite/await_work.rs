use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::project_automation_now_ms;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::SystemClock;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeSink;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::timeout;

#[derive(Clone)]
struct DeferredSink;

impl WakeSink for DeferredSink {
    async fn wake(&self, _wake: WakeBatch) -> Result<WakeDisposition, String> {
        Ok(WakeDisposition::DeferredUntilIdle)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_work_attention_has_no_sequence_but_real_completion_retains_it() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| config.local_control_tools_enabled = true)
        .build_with_auto_env(&server)
        .await?;
    let state = test.codex.state_db().context("persistent test state")?;
    let store = state.event_subscriptions();
    let service = EventSubscriptionService::spawn(store.clone(), DeferredSink, SystemClock);
    let labels = BTreeMap::from([
        ("repository_id".into(), "repo".into()),
        ("run_id".into(), "run".into()),
    ]);
    for kind in ["source.unavailable", "source.cursor_stale", "test.finished"] {
        let response = mount_function_call_agent_response(
            &server,
            kind,
            &json!({
                "source":"devcoordinator", "event_types":["test.finished"], "labels":labels,
                "after_sequence":41, "deadline_at_ms":project_automation_now_ms()+60_000,
            })
            .to_string(),
            "await_work",
        )
        .await;
        test.codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Wait for the exact job.".into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        let subscription = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(subscription) =
                    store.coordinator_subscriptions().await?.into_iter().next()
                {
                    break Ok::<_, anyhow::Error>(subscription);
                }
                store.await_source_change().await;
            }
        })
        .await??;
        service
            .publish(PublishedEvent {
                id: kind.into(),
                source: "devcoordinator".into(),
                event_type: kind.into(),
                cursor: SourceCursor {
                    sequence: 42,
                    value: None,
                },
                labels: labels.clone(),
                occurred_at_ms: 1_000,
            })
            .await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let request = response.completion.single_request();
        let output: Value = serde_json::from_str(
            &request
                .function_call_output_text(kind)
                .context("native wait output")?,
        )?;
        let mut expected_event =
            json!({"source":"devcoordinator","type":kind,"occurredAtMs":1_000,"coalescedCount":1});
        let status = if kind == "test.finished" {
            expected_event["sequence"] = json!(42);
            "event_received"
        } else {
            "attention_required"
        };
        assert_eq!(
            output,
            json!({"subscriptionId":subscription.id,"status":status,"event":expected_event})
        );
        assert!(store.coordinator_subscriptions().await?.is_empty());
    }
    service.shutdown().await;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
