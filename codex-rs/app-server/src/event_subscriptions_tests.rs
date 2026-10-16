use super::EventSubscriptionLifecycle;
use super::capacity_retry_delay_seconds;
use codex_event_subscriptions::CapacityRetryContext;
use codex_event_subscriptions::EventSubscriptionService;
use codex_event_subscriptions::SystemClock;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WakeSink;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::CodexErrorInfo;
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
    turn_store.insert(CapacityRetryContext {
        retryable_before_response: true,
        guardian_review: false,
    });
    let before_registration = codex_core::project_automation_now_ms();
    <EventSubscriptionLifecycle as TurnLifecycleContributor>::on_turn_error(
        &lifecycle,
        TurnErrorInput {
            turn_id: "turn",
            error: CodexErrorInfo::ServerOverloaded,
            error_details: &CodexErrorDetails::ServerOverloaded,
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
    let after_registration = codex_core::project_automation_now_ms();
    assert!((before_registration + 30_000..after_registration + 90_000).contains(&deadline));
    service.shutdown().await;
}

#[tokio::test]
async fn non_retryable_capacity_error_does_not_schedule_alarm() {
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
    turn_store.insert(CapacityRetryContext {
        retryable_before_response: false,
        guardian_review: false,
    });
    <EventSubscriptionLifecycle as TurnLifecycleContributor>::on_turn_error(
        &lifecycle,
        TurnErrorInput {
            turn_id: "turn",
            error: CodexErrorInfo::ServerOverloaded,
            error_details: &CodexErrorDetails::ServerOverloaded,
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
        .expect("list retry alarms");
    assert!(page.data.is_empty());
    service.shutdown().await;
}

#[tokio::test]
async fn guardian_capacity_error_does_not_schedule_alarm() {
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
    turn_store.insert(CapacityRetryContext {
        retryable_before_response: true,
        guardian_review: true,
    });
    <EventSubscriptionLifecycle as TurnLifecycleContributor>::on_turn_error(
        &lifecycle,
        TurnErrorInput {
            turn_id: "turn",
            error: CodexErrorInfo::ServerOverloaded,
            error_details: &CodexErrorDetails::ServerOverloaded,
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
        .expect("list retry alarms");
    assert!(page.data.is_empty());
    service.shutdown().await;
}
