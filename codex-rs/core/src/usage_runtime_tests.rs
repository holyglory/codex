use super::*;
use codex_protocol::provider_usage::ProviderSourceEventKey;
use codex_usage::CanonicalRepositoryPath;
use codex_usage::RepositoryIdentityInput;
use codex_usage::RepositoryIdentityMaterial;
use codex_usage::UsageEventKind;
use codex_usage::UsageEventListQuery;
use codex_usage::UsageEventProvenance;
use codex_usage::UsagePageRequest;
use codex_usage::UsageSummaryScope;
use pretty_assertions::assert_eq;
use std::sync::Mutex as StdMutex;

fn context<'a>(
    thread_id: &'a str,
    parent_thread_id: Option<&'a str>,
    turn_id: &'a str,
    retry_slot: Arc<StdMutex<Option<OperationId>>>,
) -> ModelAttemptContext<'a> {
    let retry_of_operation_id = retry_slot.lock().expect("retry slot").take();
    ModelAttemptContext {
        thread_id,
        parent_thread_id,
        turn_id: Some(turn_id),
        delegated: parent_thread_id.is_some(),
        provider: ProviderKind::new("openai").expect("provider kind"),
        model: "test-model",
        transport: "responses_http",
        client_origin: if parent_thread_id.is_some() {
            "delegated"
        } else {
            "root"
        },
        account: AccountAttributionSnapshot::unknown(),
        repositories: Vec::new(),
        attempt_number: 1,
        retry_of_operation_id,
        retry_slot,
    }
}

fn tool_context<'a>(
    thread_id: &'a str,
    turn_id: &'a str,
    call_id: &'a str,
) -> ToolAttemptContext<'a> {
    ToolAttemptContext {
        thread_id,
        parent_thread_id: None,
        turn_id: Some(turn_id),
        delegated: false,
        call_id,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        descriptor: UsageToolDescriptor {
            kind: "builtin",
            safe_name: "test_tool",
            family: "testing",
            activity_control: false,
            activity_state: ActivityState::ToolActive,
        },
        account: AccountAttributionSnapshot::unknown(),
        repositories: Vec::new(),
    }
}

#[test]
fn content_shaped_model_and_provider_identifiers_become_bounded_opaque_labels() {
    let model = model_name("custom/gpt-image").expect("opaque model name");
    assert!(model.as_str().starts_with("opaque-"));
    assert!(!model.as_str().contains("custom"));
    assert_eq!(model, model_name("custom/gpt-image").expect("stable label"));

    let provider = provider_kind("Test Responses").expect("opaque provider kind");
    assert!(provider.as_str().starts_with("opaque-"));
    assert!(!provider.as_str().contains("Test"));
    assert_eq!(
        provider,
        provider_kind("Test Responses").expect("stable provider label")
    );
}

#[test]
fn logical_request_chains_do_not_link_ordinary_continuations() {
    let failed_operation = OperationId::new();
    let first_chain = UsageRequestChain::new();
    let first = first_chain.next_attempt().expect("first attempt");
    assert_eq!(first.attempt_number, 1);
    *first
        .retry_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(failed_operation);
    let retry = first_chain.next_attempt().expect("retry attempt");
    assert_eq!(retry.attempt_number, 2);
    assert_eq!(retry.retry_of_operation_id, Some(failed_operation));

    let continuation = UsageRequestChain::new()
        .next_attempt()
        .expect("continuation attempt");
    assert_eq!(continuation.attempt_number, 1);
    assert_eq!(continuation.retry_of_operation_id, None);
}

#[cfg(unix)]
#[tokio::test]
async fn provider_usage_is_deduplicated_and_reported_content_free() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let attempt = runtime
        .begin_model_attempt(context(
            "11111111-1111-7111-8111-111111111111",
            /*parent_thread_id*/ None,
            "turn-one",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("attempt");
    let usage = ProviderUsage::from_json_value(&serde_json::json!({
        "input_tokens": 10,
        "total_tokens": 10
    }));
    let key = ProviderSourceEventKey::from_provider_response_id("private-response-id")
        .expect("source key");
    attempt
        .record_provider_usage_parts(Some(key.as_bytes()), &usage)
        .await;
    attempt
        .record_provider_usage_parts(Some(key.as_bytes()), &usage)
        .await;
    attempt
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    let store = runtime.store.get().expect("usage store");
    let summary = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("summary");
    assert_eq!(summary.operation_count, 1);
    assert_eq!(summary.tokens.len(), 2);
    assert_eq!(
        summary
            .tokens
            .iter()
            .map(|tokens| tokens.observation_count)
            .sum::<u64>(),
        2
    );
    assert_eq!(summary.coverage.overall_state, "unknown");
}

#[cfg(unix)]
#[tokio::test]
async fn provider_tokens_use_the_hashed_repository_bucket() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let origin = "https://example.test/owner/repository.git";
    let mut model = context(
        "88888888-8888-7888-8888-888888888888",
        /*parent_thread_id*/ None,
        "repository-turn",
        Arc::new(StdMutex::new(None)),
    );
    model.repositories = vec![RepositoryCandidate::new(
        "/workspace/repository",
        Some(origin.to_string()),
        "repository",
    )];
    let attempt = runtime
        .begin_model_attempt(model)
        .await
        .expect("model attempt");
    let usage = ProviderUsage::from_json_value(&serde_json::json!({
        "input_tokens": 4,
        "output_tokens": 2,
        "total_tokens": 6
    }));
    attempt
        .record_provider_usage_parts(/*source_event_key*/ None, &usage)
        .await;
    attempt
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    let store = runtime.store.get().expect("usage store");
    let identity = RepositoryIdentityInput::new(
        CanonicalRepositoryPath::new("/workspace/repository").expect("workspace"),
    )
    .with_origin(RepositoryIdentityMaterial::new(origin).expect("origin"));
    let repository_id = store
        .repository_id_for_identity(&identity)
        .expect("repository id");
    let summary = store
        .usage_summary(UsageSummaryScope::Repository(repository_id.clone()))
        .await
        .expect("repository summary");
    assert_eq!(summary.operation_count, 1);
    assert_eq!(summary.tokens.len(), 3);
    assert!(
        summary
            .tokens
            .iter()
            .all(|tokens| tokens.repository_bucket == repository_id.as_str())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn multi_repository_tokens_are_not_duplicated_into_each_repository() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let origins = [
        "https://example.test/owner/one.git",
        "https://example.test/owner/two.git",
    ];
    let mut model = context(
        "89898989-8989-7989-8989-898989898989",
        /*parent_thread_id*/ None,
        "multi-repository-turn",
        Arc::new(StdMutex::new(None)),
    );
    model.repositories = origins
        .iter()
        .enumerate()
        .map(|(index, origin)| {
            RepositoryCandidate::new(
                format!("/workspace/repository-{index}"),
                Some((*origin).to_string()),
                format!("repository-{index}"),
            )
        })
        .collect();
    let attempt = runtime
        .begin_model_attempt(model)
        .await
        .expect("model attempt");
    attempt
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({"input_tokens": 7})),
        )
        .await;
    attempt
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let store = runtime.store.get().expect("usage store");
    let all = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("all summary");
    assert_eq!(all.tokens.len(), 1);
    assert_eq!(all.tokens[0].repository_bucket, "multi_repo");
    for (index, origin) in origins.iter().enumerate() {
        let identity = RepositoryIdentityInput::new(
            CanonicalRepositoryPath::new(format!("/workspace/repository-{index}"))
                .expect("workspace"),
        )
        .with_origin(RepositoryIdentityMaterial::new(*origin).expect("origin"));
        let repository_id = store
            .repository_id_for_identity(&identity)
            .expect("repository id");
        let repository = store
            .usage_summary(UsageSummaryScope::Repository(repository_id))
            .await
            .expect("repository summary");
        assert!(repository.tokens.is_empty());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn delegated_attempts_inherit_parent_activity_until_the_child_overrides_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let parent_thread_id = "22222222-2222-7222-8222-222222222222";
    let child_thread_id = "33333333-3333-7333-8333-333333333333";
    let retry_slot = Arc::new(StdMutex::new(None));
    runtime
        .begin_model_attempt(context(
            parent_thread_id,
            /*parent_thread_id*/ None,
            "root-seed-turn",
            Arc::clone(&retry_slot),
        ))
        .await
        .expect("seed parent thread")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    runtime
        .stage_activity(
            parent_thread_id,
            Phase::Implementation,
            Activity::Coding,
            UsageActivityRelation::NewWork,
        )
        .await
        .expect("stage parent activity");
    let child = runtime
        .begin_model_attempt(context(
            child_thread_id,
            Some(parent_thread_id),
            "child-turn",
            Arc::clone(&retry_slot),
        ))
        .await
        .expect("child attempt");
    child
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({"total_tokens": 30})),
        )
        .await;
    child
        .finish(TerminalStatus::Cancelled, Some(ErrorCategory::Cancelled))
        .await;
    runtime
        .begin_model_attempt(context(
            parent_thread_id,
            /*parent_thread_id*/ None,
            "root-followup-turn",
            Arc::clone(&retry_slot),
        ))
        .await
        .expect("classified parent continuation without tokens")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    runtime
        .stage_activity(
            child_thread_id,
            Phase::Testing,
            Activity::IntegrationTesting,
            UsageActivityRelation::NewWork,
        )
        .await
        .expect("stage child activity");
    let child_override = runtime
        .begin_model_attempt(context(
            child_thread_id,
            Some(parent_thread_id),
            "child-next-turn",
            Arc::clone(&retry_slot),
        ))
        .await
        .expect("child override attempt");
    child_override
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({"total_tokens": 40})),
        )
        .await;
    child_override
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    runtime.end_activity(child_thread_id).await;
    let child_after_end = runtime
        .begin_model_attempt(context(
            child_thread_id,
            Some(parent_thread_id),
            "child-after-end",
            retry_slot,
        ))
        .await
        .expect("child activity ended");
    child_after_end
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({"total_tokens": 5})),
        )
        .await;
    child_after_end
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let store = runtime.store.get().expect("usage store");
    let summary = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("summary");
    assert_eq!(summary.operation_count, 5);
    assert_eq!(
        summary
            .provider_tokens_by_activity
            .iter()
            .map(|tokens| (tokens.activity.as_str(), tokens.measured_tokens))
            .collect::<Vec<_>>(),
        vec![("coding", 30), ("integration_testing", 40), ("unknown", 5),]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn delegated_model_tool_and_continuation_start_without_parent_usage_facts() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let child_thread_id = "44444444-4444-7444-8444-444444444444";
    let parent_thread_id = "22222222-2222-7222-8222-222222222222";
    let turn_id = "child-turn";
    let child = runtime
        .begin_model_attempt(context(
            child_thread_id,
            Some(parent_thread_id),
            turn_id,
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("child attempt without parent usage");

    child
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    let mut tool_context = tool_context(child_thread_id, turn_id, "delegated-tool");
    tool_context.parent_thread_id = Some(parent_thread_id);
    tool_context.delegated = true;
    let tool = runtime
        .begin_tool_attempt(tool_context)
        .await
        .expect("delegated tool attempt without parent usage");
    tool.finish(TerminalStatus::Completed, /*error*/ None).await;

    let continuation = runtime
        .begin_model_attempt(context(
            child_thread_id,
            Some(parent_thread_id),
            turn_id,
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("delegated continuation without parent usage");
    continuation
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    assert_eq!(
        runtime
            .store
            .get()
            .expect("usage store")
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("summary")
            .operation_count,
        3
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failed_and_incomplete_attempts_link_the_next_retry() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let retry_slot = Arc::new(StdMutex::new(None));
    let failed = runtime
        .begin_model_attempt(context(
            "55555555-5555-7555-8555-555555555555",
            /*parent_thread_id*/ None,
            "retry-turn",
            Arc::clone(&retry_slot),
        ))
        .await
        .expect("failed attempt");
    let failed_id = failed.operation_id;
    failed.finish_provider(ProviderResponseStatus::Failed).await;

    let retry_context = context(
        "55555555-5555-7555-8555-555555555555",
        /*parent_thread_id*/ None,
        "retry-turn",
        Arc::clone(&retry_slot),
    );
    assert_eq!(retry_context.retry_of_operation_id, Some(failed_id));
    let incomplete = runtime
        .begin_model_attempt(retry_context)
        .await
        .expect("incomplete attempt");
    let incomplete_id = incomplete.operation_id;
    incomplete
        .finish_provider(ProviderResponseStatus::Incomplete)
        .await;

    let final_context = context(
        "55555555-5555-7555-8555-555555555555",
        /*parent_thread_id*/ None,
        "retry-turn",
        Arc::clone(&retry_slot),
    );
    assert_eq!(final_context.retry_of_operation_id, Some(incomplete_id));
    runtime
        .begin_model_attempt(final_context)
        .await
        .expect("completed attempt")
        .finish_provider(ProviderResponseStatus::Completed)
        .await;
    assert_eq!(*retry_slot.lock().expect("retry slot"), None);
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_attempts_replay_shared_lifecycle_facts() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let first = runtime.begin_model_attempt(context(
        "66666666-6666-7666-8666-666666666666",
        /*parent_thread_id*/ None,
        "concurrent-turn",
        Arc::new(StdMutex::new(None)),
    ));
    let second = runtime.begin_model_attempt(context(
        "66666666-6666-7666-8666-666666666666",
        /*parent_thread_id*/ None,
        "concurrent-turn",
        Arc::new(StdMutex::new(None)),
    ));
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first attempt");
    let second = second.expect("second attempt");
    tokio::join!(
        first.finish(TerminalStatus::Completed, /*error*/ None),
        second.finish(TerminalStatus::Completed, /*error*/ None),
    );

    let store = runtime.store.get().expect("usage store");
    assert_eq!(
        store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("summary")
            .operation_count,
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tools_continue_when_a_stable_turn_changes_account_snapshots() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let account = AccountAttributionSnapshot::new(
        Some(AccountProfileRef::new("acct_primary").expect("account")),
        Some(AccountAuthMode::Chatgpt),
    );
    let mut model = context(
        "77777777-7777-7777-8777-777777777777",
        /*parent_thread_id*/ None,
        "account-turn",
        Arc::new(StdMutex::new(None)),
    );
    model.account = account.clone();
    runtime
        .begin_model_attempt(model)
        .await
        .expect("model attempt")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    let mut tool = tool_context(
        "77777777-7777-7777-8777-777777777777",
        "account-turn",
        "tool-same-account",
    );
    tool.account = account;
    runtime
        .begin_tool_attempt(tool)
        .await
        .expect("tool account continuity")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    let mut conflicting = tool_context(
        "77777777-7777-7777-8777-777777777777",
        "account-turn",
        "tool-other-account",
    );
    conflicting.account = AccountAttributionSnapshot::new(
        Some(AccountProfileRef::new("acct_other").expect("account")),
        Some(AccountAuthMode::ApiKey),
    );
    runtime
        .begin_tool_attempt(conflicting)
        .await
        .expect("tool after account failover")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    let store = runtime.store.get().expect("usage store");
    assert_eq!(
        store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("summary")
            .operation_count,
        3
    );
}

#[cfg(unix)]
#[tokio::test]
async fn resumed_runtime_reuses_immutable_lifecycle_timestamps() {
    let home = tempfile::tempdir().expect("tempdir");
    let first_runtime = UsageRuntime::new(home.path().to_path_buf());
    first_runtime
        .begin_model_attempt(context(
            "88888888-8888-7888-8888-888888888888",
            /*parent_thread_id*/ None,
            "resumed-turn",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("first process attempt")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;

    let resumed_runtime = UsageRuntime::new(home.path().to_path_buf());
    resumed_runtime
        .begin_model_attempt(context(
            "88888888-8888-7888-8888-888888888888",
            /*parent_thread_id*/ None,
            "resumed-turn",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("resumed process attempt")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    assert_eq!(
        resumed_runtime
            .store
            .get()
            .expect("usage store")
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("summary")
            .operation_count,
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_tool_records_provider_tokens_approval_and_timeout() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let thread_id = "99999999-9999-7999-8999-999999999999";
    let turn_id = "tool-turn";
    runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            turn_id,
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("model attempt")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let tool = runtime
        .begin_tool_attempt(tool_context(thread_id, turn_id, "tool-call"))
        .await
        .expect("tool attempt");
    let approval_wait = runtime
        .begin_active_tool_wait(
            thread_id,
            Some(turn_id),
            "tool-call",
            ActivityState::BlockedWait,
        )
        .await
        .expect("approval wait")
        .expect("active tool");
    approval_wait.heartbeat().await;
    approval_wait.finish().await;
    approval_wait.heartbeat().await;
    assert!(
        !runtime.faulted.load(Ordering::Acquire),
        "a late wait heartbeat must not follow the terminal event"
    );
    runtime
        .record_active_tool_approval(
            thread_id,
            Some(turn_id),
            "tool-call",
            codex_usage::ApprovalOutcome::Approved,
            codex_usage::ApprovalProvenance::User,
        )
        .await;
    tool.record_provider_usage(&ProviderUsage::from_json_value(&serde_json::json!({
        "input_tokens": 3,
        "total_tokens": 3
    })))
    .await;
    tool.finish(TerminalStatus::TimedOut, Some(ErrorCategory::Timeout))
        .await;

    let summary = runtime
        .store
        .get()
        .expect("usage store")
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("summary");
    assert_eq!(summary.tool_count, 1);
    assert_eq!(summary.tools.outcomes[0].outcome, "timed_out");
    assert_eq!(summary.tokens.len(), 2);
    assert!(
        summary
            .timing
            .activity_state_interval_unions
            .iter()
            .any(|duration| duration.name == "blocked_wait")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn staged_activity_crosses_turns_without_changing_or_inventing_token_totals() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let thread_id = "aaaaaaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa";
    let turn_id = "activity-turn";
    runtime
        .stage_activity(
            thread_id,
            Phase::Implementation,
            Activity::Coding,
            UsageActivityRelation::NewWork,
        )
        .await
        .expect("stage activity");
    let same_turn = runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            turn_id,
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("same-turn classified model");
    same_turn
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({"total_tokens": 11})),
        )
        .await;
    same_turn
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let next_turn_id = "activity-next-turn";
    let next_turn = runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            next_turn_id,
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("next-turn classified model");
    next_turn
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({"total_tokens": 13})),
        )
        .await;
    next_turn
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    runtime.heartbeat_activity(thread_id).await;
    runtime.heartbeat_activity(thread_id).await;
    runtime
        .begin_tool_attempt(tool_context(thread_id, next_turn_id, "classified-tool"))
        .await
        .expect("classified tool")
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    runtime.end_activity(thread_id).await;
    let unknown = runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            "activity-after-end",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("unclassified model");
    unknown
        .record_provider_usage_parts(
            /*source_event_key*/ None,
            &ProviderUsage::from_json_value(&serde_json::json!({"total_tokens": 17})),
        )
        .await;
    unknown
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let summary = runtime
        .store
        .get()
        .expect("usage store")
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("summary");
    assert_eq!(
        summary
            .classifications
            .iter()
            .find(|classification| classification.activity == "coding")
            .map(|classification| classification.count),
        Some(3)
    );
    assert_eq!(summary.tokens.len(), 1);
    assert_eq!(summary.tokens[0].category_path, "total_tokens");
    assert_eq!(summary.tokens[0].measured_tokens, 41);
    assert_eq!(summary.tokens[0].exact_tokens, Some(41));
    assert_eq!(
        summary
            .provider_tokens_by_activity
            .iter()
            .map(|tokens| tokens.measured_tokens)
            .sum::<i64>(),
        41
    );
    assert!(
        summary
            .provider_tokens_by_activity
            .iter()
            .any(|tokens| tokens.activity == "coding" && tokens.measured_tokens == 24)
    );
    assert!(
        summary
            .provider_tokens_by_activity
            .iter()
            .any(|tokens| tokens.activity == "unknown" && tokens.measured_tokens == 17)
    );
    assert!(summary.coverage.has_gaps);
    let heartbeats = runtime
        .store
        .get()
        .expect("usage store")
        .list_events(&UsageEventListQuery {
            page: UsagePageRequest {
                cursor: None,
                limit: 20,
            },
            time_range: None,
            thread_id: Some(ThreadId::new(thread_id).expect("thread")),
            repository_id: None,
            kind: Some(UsageEventKind::ActivityChanged),
        })
        .await
        .expect("activity events");
    assert_eq!(
        heartbeats
            .data
            .iter()
            .filter(|event| event.provenance == UsageEventProvenance::AgentDeclared)
            .count(),
        2
    );
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_rework_links_the_next_model_only() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let thread_id = "cccccccc-cccc-7ccc-8ccc-cccccccccccc";
    let turn_id = "rework-turn";
    let first = runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            turn_id,
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("first model");
    let first_id = first.operation_id;
    first
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    runtime
        .stage_activity(
            thread_id,
            Phase::Implementation,
            Activity::Coding,
            UsageActivityRelation::ReworkPrevious,
        )
        .await
        .expect("explicit rework");
    let rework = runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            "rework-next-turn",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("rework model");
    let rework_id = rework.operation_id;
    rework
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let links = runtime
        .store
        .get()
        .expect("usage store")
        .operation_links(rework_id)
        .await
        .expect("links")
        .expect("operation");
    assert_eq!(links.retry_of_operation_id, None);
    assert_eq!(links.rework_of_operation_id, Some(first_id));
    let continuation = runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            "rework-later-turn",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("continuation model");
    let continuation_id = continuation.operation_id;
    continuation
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    assert_eq!(
        runtime
            .store
            .get()
            .expect("usage store")
            .operation_links(continuation_id)
            .await
            .expect("links")
            .expect("operation")
            .rework_of_operation_id,
        None
    );

    assert_eq!(
        runtime
            .stage_activity(
                "dddddddd-dddd-7ddd-8ddd-dddddddddddd",
                Phase::Implementation,
                Activity::Coding,
                UsageActivityRelation::ReworkPrevious,
            )
            .await,
        Err(tool::MissingReworkTarget)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn usage_activity_boundary_is_overhead_when_pure_and_mixed_otherwise() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let thread_id = "bbbbbbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb";
    let pure = runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            "boundary-turn",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("pure boundary");
    pure.observe_response_item(&function_call("usage_activity", "usage-call"))
        .await;
    pure.finish(TerminalStatus::Completed, /*error*/ None).await;
    let mixed = runtime
        .begin_model_attempt(context(
            thread_id,
            /*parent_thread_id*/ None,
            "boundary-turn",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("mixed boundary");
    mixed
        .observe_response_item(&function_call("usage_activity", "usage-call-2"))
        .await;
    mixed
        .observe_response_item(&function_call("update_plan", "other-call"))
        .await;
    mixed
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let summary = runtime
        .store
        .get()
        .expect("usage store")
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("summary");
    assert!(
        summary
            .classifications
            .iter()
            .any(|classification| classification.activity == "accounting_overhead")
    );
    assert!(
        summary
            .classifications
            .iter()
            .any(|classification| classification.activity == "mixed")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hosted_tool_observation_is_linked_once_without_fake_prestart() {
    let home = tempfile::tempdir().expect("tempdir");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let attempt = runtime
        .begin_model_attempt(context(
            "cccccccc-cccc-7ccc-8ccc-cccccccccccc",
            /*parent_thread_id*/ None,
            "hosted-turn",
            Arc::new(StdMutex::new(None)),
        ))
        .await
        .expect("model attempt");
    let item = ResponseItem::WebSearchCall {
        id: Some(codex_protocol::ResponseItemId::from_server(
            "ws-stable".to_string(),
        )),
        status: Some("completed".to_string()),
        action: None,
        internal_chat_message_metadata_passthrough: None,
    };
    attempt.observe_response_item(&item).await;
    attempt.observe_response_item(&item).await;
    attempt
        .finish(TerminalStatus::Completed, /*error*/ None)
        .await;
    let summary = runtime
        .store
        .get()
        .expect("usage store")
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("summary");
    assert_eq!(summary.tool_count, 1);
    assert_eq!(summary.tools.duration.measured_ns, 0);
    assert_eq!(summary.coverage.overall_state, "unknown");
}

fn function_call(name: &str, call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: name.to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unavailable_store_returns_only_the_safe_blocking_error() {
    let home = tempfile::tempdir().expect("tempdir");
    let codex_home = home.path().join("not-a-directory");
    std::fs::write(&codex_home, b"fixture").expect("home fixture");
    let runtime = UsageRuntime::new(codex_home);
    let result = runtime
        .begin_model_attempt(context(
            "77777777-7777-7777-8777-777777777777",
            /*parent_thread_id*/ None,
            "blocked-turn",
            Arc::new(StdMutex::new(None)),
        ))
        .await;
    let error = match result {
        Ok(_) => panic!("unavailable store must block"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        format!("Fatal error: {SAFE_UNAVAILABLE}")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn corrupt_database_blocks_requests_without_replacing_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let usage_dir = home.path().join("usage");
    std::fs::create_dir_all(&usage_dir).expect("usage dir");
    let database_path = usage_dir.join("usage.sqlite3");
    let corrupt = b"not a sqlite database";
    std::fs::write(&database_path, corrupt).expect("corrupt fixture");
    let runtime = UsageRuntime::new(home.path().to_path_buf());
    let retry_slot = Arc::new(StdMutex::new(None));
    let result = runtime
        .begin_model_attempt(context(
            "44444444-4444-7444-8444-444444444444",
            /*parent_thread_id*/ None,
            "blocked-turn",
            Arc::clone(&retry_slot),
        ))
        .await;
    let error = match result {
        Ok(_) => panic!("corrupt database must block"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        format!("Fatal error: {SAFE_UNAVAILABLE}")
    );
    assert!(
        runtime
            .begin_model_attempt(context(
                "44444444-4444-7444-8444-444444444444",
                /*parent_thread_id*/ None,
                "blocked-turn",
                retry_slot,
            ))
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(database_path).expect("database bytes"),
        corrupt
    );
}
