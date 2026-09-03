use super::tool::ToolAttemptContext;
use super::tool::UsageToolDescriptor;
use super::*;
use codex_usage::ToolExecutionRole;
use pretty_assertions::assert_eq;
use std::sync::Mutex as StdMutex;
use tracing_test::internal::MockWriter;

fn context<'a>(thread_id: &'a str, turn_id: &'a str, model: &'a str) -> ModelAttemptContext<'a> {
    ModelAttemptContext {
        thread_id,
        parent_thread_id: None,
        turn_id: Some(turn_id),
        delegated: false,
        provider: match ProviderKind::new("openai") {
            Ok(provider) => provider,
            Err(_) => panic!("provider kind should be valid"),
        },
        model,
        transport: "responses_http",
        client_origin: "root",
        account: AccountAttributionSnapshot::unknown(),
        repositories: Vec::new(),
        attempt_number: 1,
        retry_of_operation_id: None,
        retry_slot: Arc::new(StdMutex::new(None)),
        context_estimate: None,
    }
}

#[tokio::test]
async fn recoverable_failure_does_not_poison_later_model_attempts() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let first = runtime
        .begin_model_attempt(context(
            "11111111-1111-7111-8111-111111111111",
            "first-turn",
            "test-model",
        ))
        .await;
    runtime.latch_write_failure(
        "fixture",
        Some(first.operation_id),
        UsageStoreError::Filesystem(std::io::Error::other("temporary fixture failure")),
    );

    let second = runtime
        .begin_model_attempt(context(
            "22222222-2222-7222-8222-222222222222",
            "second-turn",
            "test-model",
        ))
        .await;
    first
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    second
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let third = runtime
        .begin_model_attempt(context(
            "33333333-3333-7333-8333-333333333333",
            "third-turn",
            "test-model",
        ))
        .await;
    third
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    assert_eq!(
        runtime
            .store
            .get()
            .expect("usage store")
            .doctor()
            .await
            .expect("doctor")
            .incomplete_operations,
        0
    );
    assert!(!runtime.faulted.load(Ordering::Acquire));
}

#[tokio::test]
async fn unavailable_accounting_buffers_logs_and_replays_model_usage() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    runtime.store().await.expect("usage store");
    runtime.faulted.store(true, Ordering::Release);
    runtime
        .fault_recovery_allowed
        .store(false, Ordering::Release);
    let buffer: &'static StdMutex<Vec<u8>> = Box::leak(Box::new(StdMutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let buffered = runtime
        .begin_model_attempt(context(
            "44444444-4444-7444-8444-444444444444",
            "buffered-turn",
            "test-model",
        ))
        .await;
    assert!(!buffered.durable);
    buffered
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({
                "input_tokens": 4,
                "output_tokens": 3,
                "total_tokens": 7
            })),
        )
        .await;
    buffered
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    assert_eq!(runtime.pending_usage.lock().await.len(), 1);

    runtime.faulted.store(false, Ordering::Release);
    runtime
        .fault_recovery_allowed
        .store(true, Ordering::Release);

    let valid = runtime
        .begin_model_attempt(context(
            "55555555-5555-7555-8555-555555555555",
            "valid-turn",
            "test-model",
        ))
        .await;
    valid
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    assert!(runtime.pending_usage.lock().await.is_empty());

    let summary = runtime
        .store
        .get()
        .expect("usage store")
        .usage_summary(codex_usage::UsageSummaryScope::All)
        .await
        .expect("usage summary");
    assert_eq!(summary.model_request_count, 2);
    assert_eq!(
        summary
            .tokens
            .iter()
            .find(|tokens| tokens.category_path == "total_tokens")
            .map(|tokens| (tokens.measured_tokens, tokens.repository_bucket.as_str())),
        Some((7, "unknown"))
    );
    let logs = String::from_utf8(
        buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .expect("usage log should be utf-8");
    assert!(logs.contains("work will continue with a pending in-memory record"));
    assert!(logs.contains("usage accounting retry deferred; work will continue"));
}

#[tokio::test]
async fn replay_after_partial_durable_write_is_idempotent() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let attempt = runtime
        .begin_model_attempt(context(
            "77777777-7777-7777-8777-777777777777",
            "partially-durable-turn",
            "test-model",
        ))
        .await;
    assert!(attempt.durable);
    attempt
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({
                "input_tokens": 5,
                "output_tokens": 4,
                "total_tokens": 9
            })),
        )
        .await;
    let finish = attempt
        .pending
        .finish(
            TerminalStatus::Completed,
            /*error*/ None,
            1,
            /*saw_provider_usage*/ true,
            /*saw_usage_activity*/ false,
            /*saw_mixed_activity_output*/ false,
        )
        .expect("first terminal");
    attempt.finished.store(true, Ordering::Release);
    runtime
        .store()
        .await
        .expect("usage store")
        .finish_operation(&finish.terminal)
        .await
        .expect("partial durable terminal");
    runtime
        .enqueue_pending(buffer::PendingUsageRecord::Model(Arc::clone(
            &attempt.pending,
        )))
        .await;

    runtime.flush_pending_usage().await;

    assert!(runtime.pending_usage.lock().await.is_empty());
    let summary = runtime
        .store()
        .await
        .expect("usage store")
        .usage_summary(codex_usage::UsageSummaryScope::All)
        .await
        .expect("usage summary");
    assert_eq!(summary.model_request_count, 1);
    assert_eq!(
        summary
            .tokens
            .iter()
            .find(|tokens| tokens.category_path == "total_tokens")
            .map(|tokens| (tokens.measured_tokens, tokens.observation_count)),
        Some((9, 1))
    );
}

#[tokio::test]
async fn delegated_buffer_replays_parent_identity_and_restores_durable_capture() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let parent_thread_id = "88888888-8888-7888-8888-888888888888";
    runtime
        .begin_model_attempt(context(parent_thread_id, "parent-turn", "test-model"))
        .await
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    runtime.faulted.store(true, Ordering::Release);
    runtime
        .fault_recovery_allowed
        .store(false, Ordering::Release);

    let child_thread_id = "99999999-9999-7999-8999-999999999999";
    let mut child_context = context(child_thread_id, "buffered-child-turn", "test-model");
    child_context.parent_thread_id = Some(parent_thread_id);
    child_context.delegated = true;
    let buffered = runtime.begin_model_attempt(child_context).await;
    assert!(!buffered.durable);
    buffered
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    runtime.faulted.store(false, Ordering::Release);
    runtime
        .fault_recovery_allowed
        .store(true, Ordering::Release);
    runtime.flush_pending_usage().await;
    assert!(runtime.pending_usage.lock().await.is_empty());

    let mut resumed_context = context(child_thread_id, "resumed-child-turn", "test-model");
    resumed_context.parent_thread_id = Some(parent_thread_id);
    resumed_context.delegated = true;
    let resumed = runtime.begin_model_attempt(resumed_context).await;
    assert!(resumed.durable);
    resumed
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
}

#[tokio::test]
async fn pending_usage_cache_stays_bounded_during_persistent_failure() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    runtime.faulted.store(true, Ordering::Release);
    runtime
        .fault_recovery_allowed
        .store(false, Ordering::Release);

    for index in 0..=buffer::PENDING_USAGE_CAPACITY {
        let thread_id = format!("00000000-0000-7000-8000-{index:012}");
        let turn_id = format!("turn-{index}");
        runtime
            .begin_model_attempt(context(&thread_id, &turn_id, "test-model"))
            .await
            .finish(TerminalStatus::Completed, /*error*/ None)
            .await;
    }

    assert_eq!(
        runtime.pending_usage.lock().await.len(),
        buffer::PENDING_USAGE_CAPACITY
    );
}

#[tokio::test]
async fn unavailable_accounting_never_blocks_tool_execution_and_replays_terminal_record() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    runtime.store().await.expect("usage store");
    runtime.faulted.store(true, Ordering::Release);
    runtime
        .fault_recovery_allowed
        .store(false, Ordering::Release);
    let attempt = runtime
        .begin_tool_attempt(ToolAttemptContext {
            thread_id: "66666666-6666-7666-8666-666666666666",
            parent_thread_id: None,
            turn_id: Some("tool-turn"),
            delegated: false,
            call_id: "tool-call",
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            descriptor: UsageToolDescriptor {
                kind: "local",
                safe_name: "test_tool",
                family: "testing",
                activity_control: false,
                activity_state: ActivityState::ToolActive,
            },
            execution_group_id: None,
            execution_role: ToolExecutionRole::Standalone,
            account: AccountAttributionSnapshot::unknown(),
            repositories: Vec::new(),
        })
        .await;
    assert!(!attempt.durable);
    attempt
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    assert_eq!(runtime.pending_usage.lock().await.len(), 1);

    runtime.faulted.store(false, Ordering::Release);
    runtime
        .fault_recovery_allowed
        .store(true, Ordering::Release);
    runtime.flush_pending_usage().await;
    assert!(runtime.pending_usage.lock().await.is_empty());
    assert_eq!(
        runtime
            .store
            .get()
            .expect("usage store")
            .usage_summary(codex_usage::UsageSummaryScope::All)
            .await
            .expect("usage summary")
            .tool_count,
        1
    );
}
