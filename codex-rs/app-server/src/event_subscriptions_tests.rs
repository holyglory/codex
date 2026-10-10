use super::EventSubscriptionLifecycle;
use super::capacity_retry_delay_seconds;
use super::is_guardian_review_source;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::SystemClock;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeSink;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_protocol::ThreadId;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::AbsolutePathBuf;
use tempfile::tempdir;

#[derive(Clone)]
struct NoopWakeSink;

impl WakeSink for NoopWakeSink {
    async fn wake(&self, _wake: WakeBatch) -> Result<WakeDisposition, String> {
        Ok(WakeDisposition::DeferredUntilIdle)
    }
}

#[test]
fn capacity_retry_delay_is_thirty_to_under_ninety_seconds() {
    for _ in 0..1_000 {
        assert!((30..90).contains(&capacity_retry_delay_seconds()));
    }
}

#[tokio::test]
async fn terminal_capacity_error_registers_a_durable_retry_alarm() {
    let directory = tempdir().expect("temporary state directory");
    let home = AbsolutePathBuf::from_absolute_path(directory.path()).expect("absolute test home");
    let runtime = StateRuntime::init(SqliteConfig::new_for_testing(home), "test".to_string())
        .await
        .expect("state runtime");
    let service = EventSubscriptionService::spawn(
        runtime.event_subscriptions().clone(),
        NoopWakeSink,
        SystemClock,
    );
    let lifecycle =
        EventSubscriptionLifecycle::new(service.clone(), runtime.event_subscriptions().clone());
    let thread_id = ThreadId::new();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new(thread_id.to_string());
    let turn_store = ExtensionData::new("turn");
    <EventSubscriptionLifecycle as TurnLifecycleContributor>::on_turn_error(
        &lifecycle,
        TurnErrorInput {
            turn_id: "turn",
            error: CodexErrorInfo::ServerOverloaded,
            session_source: &SessionSource::Cli,
            retryable_before_response: true,
            session_store: &session_store,
            thread_store: &thread_store,
            turn_store: &turn_store,
        },
    )
    .await;

    let page = service
        .list(codex_event_subscriptions::ListSubscriptionsQuery {
            thread_id: Some(thread_id),
            offset: 0,
            limit: 100,
        })
        .await
        .expect("list retry alarm");
    assert_eq!(page.data.len(), 1);
    let deadline = page.data[0].next_heartbeat_at_ms.expect("retry deadline");
    let now = codex_core::project_automation_now_ms();
    assert!((now + 30_000..now + 90_000).contains(&deadline));
    runtime
        .event_subscriptions()
        .cancel_capacity_retry_subscriptions(thread_id)
        .await
        .expect("cancel retry alarm");
    <EventSubscriptionLifecycle as TurnLifecycleContributor>::on_turn_error(
        &lifecycle,
        TurnErrorInput {
            turn_id: "turn",
            error: CodexErrorInfo::ServerOverloaded,
            session_source: &SessionSource::Cli,
            retryable_before_response: false,
            session_store: &session_store,
            thread_store: &thread_store,
            turn_store: &turn_store,
        },
    )
    .await;
    let page = service
        .list(codex_event_subscriptions::ListSubscriptionsQuery {
            thread_id: Some(thread_id),
            offset: 0,
            limit: 100,
        })
        .await
        .expect("list non-retryable capacity alarm");
    assert!(page.data.is_empty());
    service.shutdown().await;
}

#[test]
fn guardian_review_sources_do_not_schedule_capacity_retries() {
    assert!(is_guardian_review_source(&SessionSource::Internal(
        InternalSessionSource::Guardian,
    )));
    assert!(is_guardian_review_source(&SessionSource::SubAgent(
        SubAgentSource::Other("guardian".to_string()),
    )));
    assert!(!is_guardian_review_source(&SessionSource::Cli));
}
