use super::*;
use pretty_assertions::assert_eq;

fn args() -> UsageStatsArgs {
    UsageStatsArgs {
        action: UsageStatsAction::Repositories,
        scope: None,
        repository: None,
        account: None,
        thread_id: None,
        root_thread_id: None,
        include_descendants: None,
        agent_id: None,
        detail: None,
        from_at_ms: None,
        to_at_ms: None,
        limit: None,
        cursor_sort_value: None,
        cursor_id: None,
    }
}

#[test]
fn usage_stats_spec_is_visible_and_content_free() {
    let spec = usage_stats_spec();
    assert_eq!(spec.name(), TOOL_NAME);
    let encoded = serde_json::to_string(&spec).expect("serialize spec");
    assert!(encoded.contains("token observation"));
    assert!(encoded.contains("never returns prompts"));
    assert!(
        encoded.len() <= 6_000,
        "usage_stats spec is {} bytes",
        encoded.len()
    );
    let tokens = codex_utils_output_truncation::approx_token_count(&encoded);
    assert!(tokens <= 1_000, "usage_stats spec is ~{tokens} tokens");
}

#[test]
fn detail_pages_are_bounded_and_cursor_fields_are_paired() {
    assert_eq!(
        query::page(&args()).expect("default page").limit,
        DEFAULT_PAGE_LIMIT
    );
    let mut invalid_limit = args();
    invalid_limit.limit = Some(MAX_PAGE_LIMIT + 1);
    assert!(query::page(&invalid_limit).is_err());

    let mut partial_cursor = args();
    partial_cursor.cursor_sort_value = Some(1);
    assert!(query::page(&partial_cursor).is_err());

    let mut complete_cursor = args();
    complete_cursor.cursor_sort_value = Some(1);
    complete_cursor.cursor_id = Some("safe-id".to_string());
    let page = query::page(&complete_cursor).expect("complete cursor");
    assert_eq!(page.cursor.expect("cursor").id(), "safe-id");
}

#[test]
fn tool_output_rejects_more_than_the_context_safe_bound() {
    assert!(bounded_output(json!({ "data": "x".repeat(MAX_OUTPUT_BYTES) })).is_err());
    let output = bounded_output(json!({ "kind": "usageClassificationCorrected" }))
        .expect("bounded mutation output");
    assert_eq!(output.log_output(), "content-free local usage operation");
}

#[test]
fn all_scope_provider_tokens_aggregate_repository_buckets_without_losing_categories() {
    let tokens = vec![
        codex_usage::StructuredTokenAggregate {
            category: "input_tokens".to_string(),
            repository_bucket: "repository-one".to_string(),
            measurement_provenance: "provider_reported".to_string(),
            measured_tokens: 10,
            exact_tokens: Some(10),
            unknown_observations: 0,
            observation_count: 1,
        },
        codex_usage::StructuredTokenAggregate {
            category: "input_tokens".to_string(),
            repository_bucket: "repository-two".to_string(),
            measurement_provenance: "provider_reported".to_string(),
            measured_tokens: 20,
            exact_tokens: None,
            unknown_observations: 1,
            observation_count: 2,
        },
        codex_usage::StructuredTokenAggregate {
            category: "output_tokens".to_string(),
            repository_bucket: "repository-one".to_string(),
            measurement_provenance: "provider_reported".to_string(),
            measured_tokens: 3,
            exact_tokens: Some(3),
            unknown_observations: 0,
            observation_count: 1,
        },
    ];

    assert_eq!(
        query::aggregate_all_provider_tokens(tokens).expect("aggregate provider tokens"),
        vec![
            codex_usage::StructuredTokenAggregate {
                category: "input_tokens".to_string(),
                repository_bucket: "all".to_string(),
                measurement_provenance: "provider_reported".to_string(),
                measured_tokens: 30,
                exact_tokens: None,
                unknown_observations: 1,
                observation_count: 3,
            },
            codex_usage::StructuredTokenAggregate {
                category: "output_tokens".to_string(),
                repository_bucket: "all".to_string(),
                measurement_provenance: "provider_reported".to_string(),
                measured_tokens: 3,
                exact_tokens: Some(3),
                unknown_observations: 0,
                observation_count: 1,
            },
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn isolated_handler_query_resolves_current_repository_and_account_alias() {
    use codex_account_registry::AccountMetadata;
    use codex_account_registry::AccountRegistry;
    use codex_account_registry::RegistryStore;
    use codex_protocol::auth::AuthMode;
    use codex_usage::AccountAttributionSnapshot;
    use codex_usage::AccountAuthMode;
    use codex_usage::AccountProfileRef;
    use codex_usage::Activity;
    use codex_usage::ActivityState;
    use codex_usage::AttributionProvenance;
    use codex_usage::CanonicalRepositoryPath;
    use codex_usage::NewOperation;
    use codex_usage::NewRepositoryAttribution;
    use codex_usage::NewThread;
    use codex_usage::NewTurn;
    use codex_usage::OperationId;
    use codex_usage::OperationKind;
    use codex_usage::Phase;
    use codex_usage::ProcessId;
    use codex_usage::RepositoryAttributionKind;
    use codex_usage::RepositoryAttributionProvenance;
    use codex_usage::RepositoryIdentityInput;
    use codex_usage::SafeRepositoryLabel;
    use codex_usage::ThreadSourceKind;
    use codex_usage::TurnId;

    let home = tempfile::tempdir().expect("home");
    let checkout = tempfile::tempdir().expect("checkout");
    std::fs::create_dir(checkout.path().join(".git")).expect("git directory");
    let mut account = AccountMetadata::new(
        "primary".parse().expect("alias"),
        AuthMode::Chatgpt,
        chrono::Utc::now(),
    );
    account.enabled = true;
    let account_id = account.id.clone();
    let mut registry = AccountRegistry::default();
    registry.add_account(account).expect("add account");
    registry.default_account_id = Some(account_id.clone());
    RegistryStore::new(home.path())
        .create(&registry)
        .expect("registry");

    let store = UsageStore::open(home.path()).await.expect("usage store");
    let workspace = std::fs::canonicalize(checkout.path()).expect("canonical checkout");
    let repository = store
        .resolve_repository(
            &RepositoryIdentityInput::new(
                CanonicalRepositoryPath::new(workspace.to_string_lossy()).expect("repository path"),
            ),
            &SafeRepositoryLabel::new("checkout").expect("label"),
            /*observed_at_ms*/ 1,
        )
        .await
        .expect("repository");
    let thread_id = ThreadId::new("thread-current").expect("thread");
    let turn_id = TurnId::new("turn-current").expect("turn");
    let account_ref = AccountProfileRef::new(account_id.as_str()).expect("account ref");
    store
        .ensure_thread(&NewThread {
            id: thread_id.clone(),
            parent_thread_id: None,
            source_kind: ThreadSourceKind::new("cli").expect("source"),
            created_at_ms: 2,
        })
        .await
        .expect("thread");
    store
        .ensure_turn(&NewTurn {
            id: turn_id.clone(),
            thread_id: thread_id.clone(),
            account: AccountAttributionSnapshot::new(
                Some(account_ref),
                Some(AccountAuthMode::Chatgpt),
            ),
            created_at_ms: 3,
        })
        .await
        .expect("turn");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 4)
        .await
        .expect("process");
    let operation_id = OperationId::new();
    store
        .begin_operation(&NewOperation {
            id: operation_id,
            process_id,
            thread_id: Some(thread_id.clone()),
            turn_id: Some(turn_id),
            agent_id: None,
            parent_operation_id: None,
            retry_of_operation_id: None,
            rework_of_operation_id: None,
            kind: OperationKind::ModelRequest,
            started_at_ms: 5,
            phase: Phase::Implementation,
            activity: Activity::Coding,
            activity_state: ActivityState::ModelActive,
            attribution_provenance: AttributionProvenance::AgentDeclared,
        })
        .await
        .expect("operation");
    store
        .record_repository_attribution(&NewRepositoryAttribution {
            event_id: codex_usage::FactEventId::new(),
            operation_id,
            repository_id: Some(repository.clone()),
            kind: RepositoryAttributionKind::Primary,
            provenance: RepositoryAttributionProvenance::RuntimeObserved,
            occurred_at_ms: 6,
        })
        .await
        .expect("repository attribution");

    let value = query::execute(
        &store,
        &UsageStatsContext {
            codex_home: home.path().to_path_buf(),
            thread_id,
            cwd: Some(workspace),
        },
        UsageStatsArgs {
            action: UsageStatsAction::Summary,
            scope: Some(UsageStatsScope::CurrentRepository),
            repository: None,
            account: Some("primary".to_string()),
            thread_id: None,
            root_thread_id: None,
            include_descendants: None,
            agent_id: None,
            detail: None,
            from_at_ms: None,
            to_at_ms: None,
            limit: None,
            cursor_sort_value: None,
            cursor_id: None,
        },
    )
    .await
    .expect("usage query");
    assert_eq!(value["scope"]["id"], repository.as_str());
    assert_eq!(value["account"], "primary");
    assert_eq!(value["counts"]["operations"], 1);
    assert_eq!(value["reportingOperationInProgress"], true);
    assert!(!value.to_string().contains(account_id.as_str()));
    assert!(
        !value
            .to_string()
            .contains(checkout.path().to_string_lossy().as_ref())
    );
}
