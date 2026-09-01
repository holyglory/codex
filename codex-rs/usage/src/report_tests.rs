use super::*;
use crate::StructuredUsageSummary;
use crate::facts::*;
use crate::repository::*;
use crate::types::*;
use pretty_assertions::assert_eq;

fn identity(value: &str) -> RepositoryIdentityInput {
    RepositoryIdentityInput::new(CanonicalRepositoryPath::new(value).expect("canonical path"))
}

fn label(value: &str) -> SafeRepositoryLabel {
    SafeRepositoryLabel::new(value).expect("repository label")
}

fn operation(process_id: ProcessId, thread_id: &str, kind: OperationKind) -> NewOperation {
    NewOperation {
        id: OperationId::new(),
        process_id,
        thread_id: Some(ThreadId::new(thread_id).expect("thread id")),
        turn_id: None,
        agent_id: None,
        parent_operation_id: None,
        retry_of_operation_id: None,
        rework_of_operation_id: None,
        kind,
        started_at_ms: 1_000,
        phase: Phase::Implementation,
        activity: Activity::Coding,
        activity_state: if matches!(kind, OperationKind::ModelRequest) {
            ActivityState::ModelActive
        } else {
            ActivityState::ToolActive
        },
        attribution_provenance: AttributionProvenance::AgentDeclared,
    }
}

async fn insert_thread(store: &UsageStore, thread_id: &str) {
    sqlx::query(
        "INSERT INTO threads(id, parent_thread_id, source_kind, created_at_ms) VALUES (?, NULL, 'cli', 1)",
    )
    .bind(thread_id)
    .execute(&store.pool)
    .await
    .expect("insert thread");
}

async fn record_request(store: &UsageStore, operation: &NewOperation) -> ModelRequestId {
    store
        .begin_operation(operation)
        .await
        .expect("begin model operation");
    let id = ModelRequestId::new();
    let fact = NewModelRequest {
        id,
        operation_id: operation.id,
        provider_kind: ProviderKind::new("openai").expect("provider"),
        model: ModelName::new("test-model").expect("model"),
        transport_kind: TransportKind::new("sse").expect("transport"),
        attempt_number: 1,
        account: AccountAttributionSnapshot::unknown(),
        client_origin: ClientOrigin::new("test").expect("client origin"),
    };
    store
        .record_model_request(&fact)
        .await
        .expect("record model request");
    store
        .record_model_request(&fact)
        .await
        .expect("replay model request");
    let mut conflict = fact.clone();
    conflict.model = ModelName::new("other-model").expect("model");
    assert!(matches!(
        store.record_model_request(&conflict).await,
        Err(UsageStoreError::FactConflict)
    ));
    id
}

async fn attribute(
    store: &UsageStore,
    operation_id: OperationId,
    repository_id: Option<RepositoryId>,
    kind: RepositoryAttributionKind,
) {
    let fact = NewRepositoryAttribution {
        event_id: FactEventId::new(),
        operation_id,
        repository_id,
        kind,
        provenance: RepositoryAttributionProvenance::RuntimeObserved,
        occurred_at_ms: 1_001,
    };
    store
        .record_repository_attribution(&fact)
        .await
        .expect("record repository attribution");
    store
        .record_repository_attribution(&fact)
        .await
        .expect("replay repository attribution");
    let mut conflict = fact.clone();
    conflict.occurred_at_ms += 1;
    assert!(matches!(
        store.record_repository_attribution(&conflict).await,
        Err(UsageStoreError::FactConflict)
    ));
}

fn token(
    request_id: ModelRequestId,
    repository_bucket: RepositoryBucket,
    count: Option<u64>,
    coverage_state: CoverageState,
) -> NewTokenObservation {
    NewTokenObservation {
        id: FactEventId::new(),
        source_event_id: FactEventId::new(),
        source: TokenObservationSource::ModelRequest(request_id),
        category_path: TokenCategoryPath::new("input_tokens").expect("token category"),
        token_count: count,
        unit: TokenUnit::Tokens,
        measurement_provenance: MeasurementProvenance::ProviderReported,
        coverage_state,
        repository_bucket,
        observed_at_ms: 1_002,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn summaries_reconcile_without_duplicating_multi_repo_or_unknown_usage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    insert_thread(&store, "thread-one").await;
    insert_thread(&store, "thread-two").await;
    let repository_one = store
        .resolve_repository(
            &identity("/repo-one"),
            &label("repo-one"),
            /*observed_at_ms*/ 1,
        )
        .await
        .expect("repository one");
    let repository_two = store
        .resolve_repository(
            &identity("/repo-two"),
            &label("repo-two"),
            /*observed_at_ms*/ 1,
        )
        .await
        .expect("repository two");

    let single_operation = operation(process_id, "thread-one", OperationKind::ModelRequest);
    let single_request = record_request(&store, &single_operation).await;
    attribute(
        &store,
        single_operation.id,
        Some(repository_one.clone()),
        RepositoryAttributionKind::Primary,
    )
    .await;
    let single_token = token(
        single_request,
        RepositoryBucket::Single(repository_one.clone()),
        Some(10),
        CoverageState::Complete,
    );
    store
        .record_token_observation(&single_token)
        .await
        .expect("single repository token");
    store
        .record_token_observation(&single_token)
        .await
        .expect("deduplicated replay");
    let mut conflicting = single_token.clone();
    conflicting.id = FactEventId::new();
    conflicting.token_count = Some(11);
    assert!(matches!(
        store.record_token_observation(&conflicting).await,
        Err(UsageStoreError::FactConflict)
    ));

    let multi_operation = operation(process_id, "thread-two", OperationKind::ModelRequest);
    let multi_request = record_request(&store, &multi_operation).await;
    attribute(
        &store,
        multi_operation.id,
        Some(repository_one.clone()),
        RepositoryAttributionKind::ObservedCwd,
    )
    .await;
    attribute(
        &store,
        multi_operation.id,
        Some(repository_two.clone()),
        RepositoryAttributionKind::FileChange,
    )
    .await;
    store
        .record_token_observation(&token(
            multi_request,
            RepositoryBucket::MultiRepo,
            Some(20),
            CoverageState::Complete,
        ))
        .await
        .expect("multi repository token");

    let unknown_operation = operation(process_id, "thread-one", OperationKind::ModelRequest);
    let unknown_request = record_request(&store, &unknown_operation).await;
    attribute(
        &store,
        unknown_operation.id,
        /*repository_id*/ None,
        RepositoryAttributionKind::Unknown,
    )
    .await;
    store
        .record_token_observation(&token(
            unknown_request,
            RepositoryBucket::Unknown,
            /*count*/ None,
            CoverageState::Unknown,
        ))
        .await
        .expect("unknown token count");

    let tool_operation = operation(process_id, "thread-one", OperationKind::LocalTool);
    store
        .begin_operation(&tool_operation)
        .await
        .expect("begin tool operation");
    let tool_fact = NewToolInvocation {
        id: ToolInvocationId::new(),
        operation_id: tool_operation.id,
        operation_kind: OperationKind::LocalTool,
        tool_kind: ToolKind::new("builtin").expect("tool kind"),
        safe_tool_name: ToolName::new("apply_patch").expect("tool name"),
        operation_family: OperationFamily::new("filesystem").expect("operation family"),
        observation_timing: ObservationTiming::new("runtime").expect("timing"),
        covering_model_request_id: None,
    };
    store
        .record_tool_invocation(&tool_fact)
        .await
        .expect("record tool invocation");
    store
        .record_tool_invocation(&tool_fact)
        .await
        .expect("replay tool invocation");
    let mut tool_conflict = tool_fact.clone();
    tool_conflict.safe_tool_name = ToolName::new("other_tool").expect("tool name");
    assert!(matches!(
        store.record_tool_invocation(&tool_conflict).await,
        Err(UsageStoreError::FactConflict)
    ));
    attribute(
        &store,
        tool_operation.id,
        Some(repository_one.clone()),
        RepositoryAttributionKind::ObservedCwd,
    )
    .await;
    let original_classification = NewClassificationEvent {
        event_id: FactEventId::new(),
        operation_id: tool_operation.id,
        phase: Phase::Implementation,
        activity: Activity::Coding,
        activity_state: ActivityState::ToolActive,
        provenance: AttributionProvenance::AgentDeclared,
        supersedes_event_id: None,
        occurred_at_ms: 1_003,
    };
    store
        .record_classification(&original_classification)
        .await
        .expect("record classification");
    store
        .record_classification(&original_classification)
        .await
        .expect("replay classification");
    let mut classification_conflict = original_classification.clone();
    classification_conflict.activity = Activity::ReviewFeedback;
    assert!(matches!(
        store.record_classification(&classification_conflict).await,
        Err(UsageStoreError::FactConflict)
    ));
    store
        .record_classification(&NewClassificationEvent {
            event_id: FactEventId::new(),
            operation_id: tool_operation.id,
            phase: Phase::Reporting,
            activity: Activity::ReviewFeedback,
            activity_state: ActivityState::ToolActive,
            provenance: AttributionProvenance::UserCorrected,
            supersedes_event_id: Some(original_classification.event_id),
            occurred_at_ms: 1_004,
        })
        .await
        .expect("record correction");
    assert!(matches!(
        store
            .record_classification(&NewClassificationEvent {
                event_id: FactEventId::new(),
                operation_id: single_operation.id,
                phase: Phase::Reporting,
                activity: Activity::ReviewFeedback,
                activity_state: ActivityState::ModelActive,
                provenance: AttributionProvenance::UserCorrected,
                supersedes_event_id: Some(original_classification.event_id),
                occurred_at_ms: 1_005,
            })
            .await,
        Err(UsageStoreError::Database(_))
    ));
    for (operation_id, state) in [
        (single_operation.id, CoverageState::Complete),
        (multi_operation.id, CoverageState::Complete),
        (unknown_operation.id, CoverageState::Unknown),
        (tool_operation.id, CoverageState::Complete),
    ] {
        let fact = NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(operation_id),
            scope_kind: CoverageScopeKind::new("operation").expect("coverage scope"),
            state,
            reason_code: None,
            occurred_at_ms: 1_005,
        };
        store.record_coverage(&fact).await.expect("record coverage");
        store.record_coverage(&fact).await.expect("replay coverage");
        let mut conflict = fact.clone();
        conflict.occurred_at_ms += 1;
        assert!(matches!(
            store.record_coverage(&conflict).await,
            Err(UsageStoreError::FactConflict)
        ));
    }

    let all = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("all summary");
    let thread_one = store
        .usage_summary(UsageSummaryScope::Thread(
            ThreadId::new("thread-one").expect("thread id"),
        ))
        .await
        .expect("thread one summary");
    let thread_two = store
        .usage_summary(UsageSummaryScope::Thread(
            ThreadId::new("thread-two").expect("thread id"),
        ))
        .await
        .expect("thread two summary");
    let repository_one_summary = store
        .usage_summary(UsageSummaryScope::Repository(repository_one))
        .await
        .expect("repository one summary");
    let repository_two_summary = store
        .usage_summary(UsageSummaryScope::Repository(repository_two.clone()))
        .await
        .expect("repository two summary");

    let measured = |summary: &UsageSummary| {
        summary
            .tokens
            .iter()
            .map(|aggregate| aggregate.measured_tokens)
            .sum::<i64>()
    };
    assert_eq!(measured(&all), 30);
    assert_eq!(
        measured(&all),
        measured(&thread_one) + measured(&thread_two)
    );
    assert_eq!(measured(&repository_one_summary), 10);
    assert_eq!(measured(&repository_two_summary), 0);
    store
        .append_repository_merge(
            FactEventId::new(),
            match &repository_one_summary.scope {
                UsageSummaryScope::Repository(repository_id) => repository_id,
                _ => panic!("repository scope"),
            },
            &repository_two,
            /*occurred_at_ms*/ 2_000,
        )
        .await
        .expect("merge source history");
    let merged_summary = store
        .usage_summary(UsageSummaryScope::Repository(repository_two))
        .await
        .expect("merged repository summary");
    assert_eq!(measured(&merged_summary), 10);
    assert_eq!(all.tokens.len(), 3);
    assert_eq!(
        1,
        all.tokens
            .iter()
            .filter(|aggregate| aggregate.repository_bucket == "multi_repo")
            .count()
    );
    let unknown = all
        .tokens
        .iter()
        .find(|aggregate| aggregate.repository_bucket == "unknown")
        .expect("unknown aggregate");
    assert_eq!(unknown.exact_tokens, None);
    assert_eq!(unknown.unknown_observations, 1);
    assert!(all.coverage.has_gaps);
    assert!(!thread_two.coverage.has_gaps);
    assert_eq!(all.operation_count, 4);
    assert_eq!(all.tool_count, 1);
    assert_eq!(all.database_schema_version, 4);
    assert_eq!(all.taxonomy_version, TAXONOMY_VERSION);
    assert_eq!(repository_one_summary.tool_count, 1);
    assert_eq!(all.classifications.len(), 2);
    let correction = all
        .classifications
        .iter()
        .find(|classification| classification.activity == "review_feedback")
        .expect("effective correction");
    assert_eq!(correction.provenance, "user_corrected");
    let structured = StructuredUsageSummary::new(&all, Some("primary".to_string()));
    assert_eq!(structured.database_schema_version, 4);
    assert_eq!(structured.taxonomy_version, TAXONOMY_VERSION);
    assert_eq!(structured.account.as_deref(), Some("primary"));
    assert_eq!(structured.provider_tokens.len(), all.tokens.len());
    assert_eq!(
        structured.provider_tokens_by_activity.len(),
        all.provider_tokens_by_activity.len()
    );
    assert_eq!(structured.classifications.len(), all.classifications.len());

    sqlx::query("DELETE FROM _usage_report_cache_meta")
        .execute(&store.pool)
        .await
        .expect("invalidate only the derived report cache");
    assert_eq!(
        store
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("canonical all summary"),
        all
    );
    sqlx::raw_sql(
        r#"
        DELETE FROM _usage_report_token_aggregates;
        DELETE FROM _usage_report_activity_tokens;
        DELETE FROM _usage_report_token_coverage;
        "#,
    )
    .execute(&store.pool)
    .await
    .expect("damage only the invalidated derived report cache");
    store.pool.close().await;
    let reopened = UsageStore::open(temp.path())
        .await
        .expect("reopen and rebuild report cache");
    assert_eq!(
        reopened
            .usage_summary(UsageSummaryScope::All)
            .await
            .expect("rebuilt all summary"),
        all
    );
}

#[cfg(unix)]
#[tokio::test]
async fn empty_scope_is_explicitly_unobserved() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let summary = store
        .usage_summary(UsageSummaryScope::Thread(
            ThreadId::new("missing-thread").expect("thread id"),
        ))
        .await
        .expect("empty summary");
    assert_eq!(summary.coverage.overall_state, "unobserved");
    assert!(summary.coverage.has_gaps);
    assert!(summary.tokens.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn summaries_ignore_legacy_opaque_per_item_token_categories() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    insert_thread(&store, "legacy-item-thread").await;
    let request_operation = operation(
        process_id,
        "legacy-item-thread",
        OperationKind::ModelRequest,
    );
    let request_id = record_request(&store, &request_operation).await;
    store
        .record_token_observation(&token(
            request_id,
            RepositoryBucket::Unknown,
            Some(10),
            CoverageState::Complete,
        ))
        .await
        .expect("aggregate token observation");
    let mut per_item = token(
        request_id,
        RepositoryBucket::Unknown,
        Some(10),
        CoverageState::Partial,
    );
    per_item.category_path =
        TokenCategoryPath::new("attribution.items.msg_opaque_provider_id.input_tokens")
            .expect("legacy per-item category");
    store
        .record_token_observation(&per_item)
        .await
        .expect("legacy per-item token observation");

    let summary = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("usage summary");

    assert_eq!(
        summary.tokens,
        vec![TokenAggregate {
            category_path: "input_tokens".to_string(),
            repository_bucket: "unknown".to_string(),
            measurement_provenance: "provider_reported".to_string(),
            measured_tokens: 10,
            exact_tokens: Some(10),
            unknown_observations: 0,
            observation_count: 1,
        }]
    );
    assert_eq!(
        summary.coverage.token_observation_counts,
        vec![CoverageCount {
            state: "complete".to_string(),
            count: 1,
        }]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn checked_token_aggregation_reports_overflow() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("open store");
    let process_id = ProcessId::new();
    store
        .register_process(&process_id, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("register process");
    insert_thread(&store, "overflow-thread").await;
    let request_operation = operation(process_id, "overflow-thread", OperationKind::ModelRequest);
    let request_id = record_request(&store, &request_operation).await;
    for _ in 0..2 {
        store
            .record_token_observation(&token(
                request_id,
                RepositoryBucket::Unknown,
                Some(i64::MAX as u64),
                CoverageState::Complete,
            ))
            .await
            .expect("large token observation");
    }
    assert!(matches!(
        store.usage_summary(UsageSummaryScope::All).await,
        Err(UsageStoreError::AggregateOverflow)
    ));
}
