use super::*;
use pretty_assertions::assert_eq;
use std::sync::Mutex as StdMutex;

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
        .await
        .expect("first attempt");
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
        .await
        .expect("recovered attempt");
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
        .await
        .expect("late terminal must not relatch the runtime");
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
async fn invalid_metadata_blocks_only_the_invalid_attempt() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let invalid_model = "x".repeat(513);
    let result = runtime
        .begin_model_attempt(context(
            "44444444-4444-7444-8444-444444444444",
            "invalid-turn",
            &invalid_model,
        ))
        .await;
    let error = match result {
        Ok(_) => panic!("invalid model metadata must remain blocked"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        format!("Fatal error: {SAFE_UNAVAILABLE}")
    );
    assert!(!runtime.faulted.load(Ordering::Acquire));

    let valid = runtime
        .begin_model_attempt(context(
            "55555555-5555-7555-8555-555555555555",
            "valid-turn",
            "test-model",
        ))
        .await
        .expect("valid attempt after invalid metadata");
    valid
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
}
