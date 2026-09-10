use super::*;
use crate::*;
use pretty_assertions::assert_eq;
use std::time::Instant;

#[path = "performance_review_work_tests.rs"]
mod work;

struct Fixture {
    store: UsageStore,
    process: ProcessId,
    repository: RepositoryId,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = UsageStore::open(directory.path())
            .await
            .expect("usage store");
        let process = ProcessId::new();
        store
            .register_process(
                &process, /*os_pid*/ 1, /*started_at_ms*/ 1_000_000,
            )
            .await
            .expect("process");
        store
            .ensure_thread(&NewThread {
                id: ThreadId::new("spec").expect("thread"),
                parent_thread_id: None,
                source_kind: ThreadSourceKind::new("test").expect("source"),
                created_at_ms: 1_000_000,
            })
            .await
            .expect("thread");
        let repository = store
            .resolve_repository(
                &RepositoryIdentityInput::new(
                    CanonicalRepositoryPath::new("/project-alpha").expect("path"),
                ),
                &SafeRepositoryLabel::new("project-alpha").expect("label"),
                /*observed_at_ms*/ 1_000_000,
            )
            .await
            .expect("repository");
        Self {
            store,
            process,
            repository,
            _directory: directory,
        }
    }

    fn operation(&self, started_at_ms: i64) -> NewOperation {
        NewOperation {
            id: OperationId::new(),
            process_id: self.process,
            thread_id: Some(ThreadId::new("spec").expect("thread")),
            turn_id: None,
            agent_id: None,
            parent_operation_id: None,
            retry_of_operation_id: None,
            rework_of_operation_id: None,
            kind: OperationKind::LocalTool,
            started_at_ms,
            phase: Phase::Implementation,
            activity: Activity::Coding,
            activity_state: ActivityState::ToolActive,
            attribution_provenance: AttributionProvenance::AgentDeclared,
        }
    }

    async fn begin(&self, operation: &NewOperation) {
        self.store.begin_operation(operation).await.expect("begin");
        self.store
            .record_repository_attribution(&NewRepositoryAttribution {
                event_id: FactEventId::new(),
                operation_id: operation.id,
                repository_id: Some(self.repository.clone()),
                kind: RepositoryAttributionKind::Primary,
                provenance: RepositoryAttributionProvenance::RuntimeObserved,
                occurred_at_ms: operation.started_at_ms,
            })
            .await
            .expect("attribution");
    }

    async fn finish(&self, operation: &NewOperation, duration_ms: u64) {
        self.store
            .finish_operation(&TerminalOperation {
                operation_id: operation.id,
                status: TerminalStatus::Completed,
                occurred_at_ms: operation.started_at_ms
                    + i64::try_from(duration_ms).expect("duration"),
                duration_ns: duration_ms * 1_000_000,
                error_category: None,
            })
            .await
            .expect("finish");
    }

    async fn model(&self, operation: &NewOperation) -> ModelRequestId {
        self.begin(operation).await;
        let request = ModelRequestId::new();
        self.store
            .record_model_request(&NewModelRequest {
                id: request,
                operation_id: operation.id,
                provider_kind: ProviderKind::new("openai").expect("provider"),
                model: ModelName::new("fixture").expect("model"),
                transport_kind: TransportKind::new("http").expect("transport"),
                attempt_number: 1,
                account: AccountAttributionSnapshot::unknown(),
                client_origin: ClientOrigin::new("test").expect("client"),
            })
            .await
            .expect("request");
        request
    }

    async fn tool(
        &self,
        operation: &NewOperation,
        role: ToolExecutionRole,
        group: Option<ToolExecutionGroupId>,
        covering: Option<ModelRequestId>,
    ) -> ToolInvocationId {
        self.begin(operation).await;
        let tool = ToolInvocationId::new();
        self.store
            .record_tool_invocation(&NewToolInvocation {
                id: tool,
                operation_id: operation.id,
                operation_kind: operation.kind,
                tool_kind: ToolKind::new("function").expect("kind"),
                safe_tool_name: ToolName::new("exec").expect("tool"),
                operation_family: OperationFamily::new("test").expect("family"),
                observation_timing: ObservationTiming::new("runtime").expect("timing"),
                covering_model_request_id: covering,
                execution_group_id: group,
                execution_role: role,
            })
            .await
            .expect("tool");
        tool
    }

    async fn token(
        &self,
        source: TokenObservationSource,
        event: FactEventId,
        category: &str,
        count: u64,
    ) {
        self.store
            .record_token_observation(&NewTokenObservation {
                id: FactEventId::new(),
                source_event_id: event,
                source,
                category_path: TokenCategoryPath::new(category).expect("category"),
                token_count: Some(count),
                unit: TokenUnit::Tokens,
                measurement_provenance: MeasurementProvenance::ProviderReported,
                coverage_state: CoverageState::Complete,
                repository_bucket: RepositoryBucket::Single(self.repository.clone()),
                observed_at_ms: 1_000_020,
            })
            .await
            .expect("token fact");
    }

    async fn packet(&self) -> PerformanceReviewPacket {
        self.store
            .performance_review_packet(PerformanceReviewQuery::default())
            .await
            .expect("review packet")
    }
}

#[tokio::test]
async fn factual_ownership_deduplicates_covered_tokens_and_typed_tool_groups() {
    let fixture = Fixture::new().await;
    let mut model = fixture.operation(/*started_at_ms*/ 1_000_000);
    model.kind = OperationKind::ModelRequest;
    model.activity_state = ActivityState::ModelActive;
    let request = fixture.model(&model).await;
    fixture
        .store
        .begin_operation(&model)
        .await
        .expect("idempotent capture");
    fixture.finish(&model, /*duration_ms*/ 100).await;
    let event = FactEventId::new();
    for (category, count) in [
        ("total_tokens", 100),
        ("input_tokens", 80),
        ("input_tokens.cached_tokens", 60),
        ("output_tokens", 20),
        ("output_tokens.reasoning_tokens", 5),
    ] {
        fixture
            .token(
                TokenObservationSource::ModelRequest(request),
                event,
                category,
                count,
            )
            .await;
    }
    let mut hosted = fixture.operation(/*started_at_ms*/ 1_000_010);
    hosted.kind = OperationKind::HostedTool;
    let tool = fixture
        .tool(
            &hosted,
            ToolExecutionRole::Standalone,
            /*group*/ None,
            Some(request),
        )
        .await;
    fixture.finish(&hosted, /*duration_ms*/ 50).await;
    fixture
        .token(
            TokenObservationSource::ToolInvocation(tool),
            event,
            "total_tokens",
            /*count*/ 100,
        )
        .await;
    let group = ToolExecutionGroupId::from_stable_key(b"execution-group");
    for (role, duration_ms) in [
        (ToolExecutionRole::Wrapper, 100),
        (ToolExecutionRole::Nested, 80),
    ] {
        let operation = fixture.operation(/*started_at_ms*/ 1_000_200);
        fixture
            .tool(&operation, role, Some(group), /*covering*/ None)
            .await;
        fixture.finish(&operation, duration_ms).await;
    }
    let packet = fixture.packet().await;
    assert_eq!(
        packet
            .tokens
            .iter()
            .map(|value| (
                value.category.as_str(),
                value.measured_tokens,
                value.exact_tokens
            ))
            .collect::<Vec<_>>(),
        vec![
            ("total_tokens", 100, Some(100)),
            ("input_tokens", 80, Some(80)),
            ("input_tokens.cached_tokens", 60, Some(60)),
            ("output_tokens", 20, Some(20)),
            ("output_tokens.reasoning_tokens", 5, Some(5))
        ]
    );
    assert_eq!(
        (
            packet.coverage.raw_operations,
            packet.coverage.deduplicated_operations,
            packet.coverage.raw_token_observations,
            packet.coverage.deduplicated_token_observations,
            packet.coverage.model_requests_without_provider_total
        ),
        (4, 3, 6, 5, 0)
    );
    assert_eq!(
        packet.operations,
        vec![
            ReviewCategory {
                category: "hosted_tool".to_string(),
                count: 1,
                measured_interval_sum_ms: 50,
                unknown_intervals: 0
            },
            ReviewCategory {
                category: "local_tool".to_string(),
                count: 1,
                measured_interval_sum_ms: 80,
                unknown_intervals: 0
            },
            ReviewCategory {
                category: "model_request".to_string(),
                count: 1,
                measured_interval_sum_ms: 100,
                unknown_intervals: 0
            },
        ]
    );
    assert!(packet.candidates.is_empty());
    assert_eq!(fixture.packet().await, packet);
    sqlx::query("UPDATE _usage_report_cache_meta SET ready = 0")
        .execute(&fixture.store.pool)
        .await
        .expect("disable disposable derived cache");
    assert_eq!(fixture.packet().await, packet);
}

#[tokio::test]
async fn retries_and_rework_are_signals_but_manual_waits_and_deliberate_validation_are_not() {
    let fixture = Fixture::new().await;
    let original = fixture.operation(/*started_at_ms*/ 1_000_000);
    fixture
        .tool(
            &original,
            ToolExecutionRole::Standalone,
            /*group*/ None,
            /*covering*/ None,
        )
        .await;
    fixture.finish(&original, /*duration_ms*/ 20).await;
    let mut linked_ids = Vec::new();
    for offset in 1..=7 {
        let mut operation = fixture.operation(1_000_000 + offset * 100);
        operation.retry_of_operation_id = Some(original.id);
        match offset {
            1 => {
                operation.rework_of_operation_id = Some(original.id);
            }
            2 => operation.activity_state = ActivityState::UserWait,
            3 => {
                operation.phase = Phase::Testing;
                operation.activity = Activity::IntegrationTesting;
            }
            4 => operation.activity = Activity::VerificationReview,
            5 => operation.activity_state = ActivityState::ExternalWait,
            6 => operation.activity_state = ActivityState::BlockedWait,
            7 => {
                operation.retry_of_operation_id = None;
                operation.parent_operation_id = Some(original.id);
            }
            _ => unreachable!("fixture cases"),
        }
        fixture
            .tool(
                &operation,
                ToolExecutionRole::Standalone,
                /*group*/ None,
                /*covering*/ None,
            )
            .await;
        fixture.finish(&operation, /*duration_ms*/ 50).await;
        linked_ids.push(operation.id.as_string());
    }
    let packet = fixture.packet().await;
    assert_eq!(
        packet.candidates,
        vec![ReviewCandidate {
            signal: "linked_retry_and_rework",
            measured_interval_ms: Some(50),
            critical_path: "unknown",
            avoidability: "unknown",
            evidence: ReviewLink {
                operation_id: linked_ids[0].clone(),
                started_at_ms: 1_000_100,
                retry_of_operation_id: Some(original.id.as_string()),
                rework_of_operation_id: Some(original.id.as_string())
            },
        }]
    );
    assert_eq!(
        (
            packet.links.retry_operations,
            packet.links.rework_operations,
            packet.links.omitted_operations
        ),
        (6, 1, 1)
    );
    assert_eq!(
        packet
            .waits
            .iter()
            .map(|value| (
                value.category.as_str(),
                value.count,
                value.measured_interval_sum_ms
            ))
            .collect::<Vec<_>>(),
        vec![
            ("blocked_wait", 1, 50),
            ("external_wait", 1, 50),
            ("user_wait", 1, 50)
        ]
    );
}

#[tokio::test]
async fn missing_coverage_is_not_fabricated_as_zero_cost_or_complete_history() {
    let fixture = Fixture::new().await;
    let mut model = fixture.operation(/*started_at_ms*/ 1_000_000);
    model.kind = OperationKind::ModelRequest;
    model.activity_state = ActivityState::ModelActive;
    model.attribution_provenance = AttributionProvenance::Unknown;
    fixture.begin(&model).await;
    fixture
        .store
        .record_coverage(&NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(model.id),
            scope_kind: CoverageScopeKind::new("operation").expect("scope"),
            state: CoverageState::Unavailable,
            reason_code: None,
            occurred_at_ms: 1_000_010,
        })
        .await
        .expect("coverage gap");
    fixture
        .store
        .record_coverage(&NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: None,
            scope_kind: CoverageScopeKind::new("process").expect("scope"),
            state: CoverageState::Unknown,
            reason_code: None,
            occurred_at_ms: 1_000_010,
        })
        .await
        .expect("unattributed collection gap");
    let packet = fixture.packet().await;
    assert_eq!(
        (
            packet.coverage.unknown_operation_intervals,
            packet.coverage.unclassified_operations,
            packet.coverage.model_requests_without_provider_total,
            packet.coverage.unattributed_coverage_events_in_window
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(
        packet.coverage.coverage_events,
        vec![ReviewCoverageCount {
            state: "unavailable".to_string(),
            count: 1
        }]
    );
    assert_eq!(packet.coverage.collection_completeness, "unknown");
    assert_eq!(packet.coverage.repeated_input_comparison, "not_collected");
    assert!(packet.tokens.is_empty());
    assert!(packet.candidates.is_empty());
    let empty = fixture
        .store
        .performance_review_packet(PerformanceReviewQuery {
            thread_id: Some(ThreadId::new("unobserved").expect("thread")),
            ..Default::default()
        })
        .await
        .expect("no observations");
    assert_eq!(
        (
            empty.coverage.raw_operations,
            empty.coverage.collection_completeness
        ),
        (0, "unknown")
    );
}

#[tokio::test]
async fn filters_clip_intervals_and_keep_observation_time_distinct_from_operation_start() {
    let fixture = Fixture::new().await;
    let mut model = fixture.operation(/*started_at_ms*/ 1_000_000);
    model.kind = OperationKind::ModelRequest;
    let request = fixture.model(&model).await;
    fixture.finish(&model, /*duration_ms*/ 10).await;
    fixture
        .token(
            TokenObservationSource::ModelRequest(request),
            FactEventId::new(),
            "total_tokens",
            /*count*/ 40,
        )
        .await;
    let query = PerformanceReviewQuery {
        repository_id: Some(fixture.repository.clone()),
        thread_id: Some(ThreadId::new("spec").expect("thread")),
        time_range: Some(
            UtcTimeRange::new(/*start_ms*/ 1_000_010, /*end_ms*/ 1_000_021).expect("window"),
        ),
    };
    let packet = fixture
        .store
        .performance_review_packet(query)
        .await
        .expect("filtered packet");
    assert!(packet.operations.is_empty());
    assert_eq!(packet.tokens[0].exact_tokens, Some(40));
    let query = PerformanceReviewQuery {
        time_range: Some(
            UtcTimeRange::new(/*start_ms*/ 1_000_005, /*end_ms*/ 1_000_020).expect("window"),
        ),
        ..Default::default()
    };
    let packet = fixture
        .store
        .performance_review_packet(query)
        .await
        .expect("clipped packet");
    assert_eq!(packet.operations[0].measured_interval_sum_ms, 5);
    assert!(packet.tokens.is_empty());
    assert_eq!(packet.coverage.model_requests_without_provider_total, 1);
}

#[tokio::test]
async fn category_and_link_fanout_returns_bounded_deterministic_evidence_not_truncated_json() {
    let fixture = Fixture::new().await;
    let mut model = fixture.operation(/*started_at_ms*/ 1_000_000);
    model.kind = OperationKind::ModelRequest;
    let request = fixture.model(&model).await;
    fixture.finish(&model, /*duration_ms*/ 10).await;
    for index in 0..40 {
        let category = format!(
            "{}.{}.category_{index:02}_{}_tokens",
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(40)
        );
        fixture
            .token(
                TokenObservationSource::ModelRequest(request),
                FactEventId::new(),
                &category,
                i64::MAX as u64,
            )
            .await;
    }
    for offset in 1..=20 {
        let mut retry = fixture.operation(1_000_000 + offset * 100);
        retry.retry_of_operation_id = Some(model.id);
        fixture.begin(&retry).await;
        fixture.finish(&retry, /*duration_ms*/ 40).await;
    }
    let started = Instant::now();
    let packet = fixture.packet().await;
    let encoded = serde_json::to_vec(&packet).expect("JSON");
    eprintln!(
        "performance_review measurements: bytes={}, elapsed_us={}, operations=21, token_categories=40",
        encoded.len(),
        started.elapsed().as_micros()
    );
    assert!(
        encoded.len() <= 12 * 1024,
        "packet is {} bytes",
        encoded.len()
    );
    assert!(encoded.len() <= 16 * 1024);
    assert_eq!(
        (
            packet.tokens.len(),
            packet.coverage.omitted_token_categories,
            packet.links.sample.len(),
            packet.links.omitted_operations,
            packet.candidates.len(),
            packet.coverage.omitted_candidates
        ),
        (12, 28, 5, 15, 5, 15)
    );
    assert_eq!(fixture.packet().await, packet);
    assert_eq!(
        packet
            .candidates
            .iter()
            .map(|candidate| candidate.evidence.started_at_ms)
            .collect::<Vec<_>>(),
        vec![1_000_100, 1_000_200, 1_000_300, 1_000_400, 1_000_500]
    );
}

#[tokio::test]
async fn conflicting_and_unknown_facts_never_inflate_or_fabricate_provider_totals() {
    let fixture = Fixture::new().await;
    let mut model = fixture.operation(/*started_at_ms*/ 1_000_000);
    model.kind = OperationKind::ModelRequest;
    let request = fixture.model(&model).await;
    let event = FactEventId::new();
    fixture
        .token(
            TokenObservationSource::ModelRequest(request),
            event,
            "total_tokens",
            /*count*/ 100,
        )
        .await;
    let mut hosted = fixture.operation(/*started_at_ms*/ 1_000_010);
    hosted.kind = OperationKind::HostedTool;
    let tool = fixture
        .tool(
            &hosted,
            ToolExecutionRole::Standalone,
            /*group*/ None,
            Some(request),
        )
        .await;
    fixture
        .token(
            TokenObservationSource::ToolInvocation(tool),
            event,
            "total_tokens",
            /*count*/ 200,
        )
        .await;
    let mut unknown = fixture.operation(/*started_at_ms*/ 1_000_030);
    unknown.kind = OperationKind::ModelRequest;
    let request = fixture.model(&unknown).await;
    fixture
        .store
        .record_token_observation(&NewTokenObservation {
            id: FactEventId::new(),
            source_event_id: FactEventId::new(),
            source: TokenObservationSource::ModelRequest(request),
            category_path: TokenCategoryPath::new("total_tokens").expect("category"),
            token_count: None,
            unit: TokenUnit::Tokens,
            measurement_provenance: MeasurementProvenance::ProviderReported,
            coverage_state: CoverageState::Unknown,
            repository_bucket: RepositoryBucket::Unknown,
            observed_at_ms: 1_000_040,
        })
        .await
        .expect("unknown fact");
    let packet = fixture.packet().await;
    assert_eq!(
        packet.tokens,
        vec![ReviewTokens {
            category: "total_tokens".to_string(),
            provenance: "provider_reported".to_string(),
            measured_tokens: 0,
            exact_tokens: None,
            observations: 2,
            unknown_observations: 1
        }]
    );
    assert_eq!(
        (
            packet.coverage.conflicting_token_observations,
            packet.coverage.unknown_token_observations,
            packet.coverage.model_requests_without_provider_total
        ),
        (1, 1, 2)
    );
    assert!(packet.candidates.is_empty());
}

#[tokio::test]
async fn wait_spans_preserve_missing_ends_and_do_not_double_count_wait_operations() {
    let fixture = Fixture::new().await;
    let operation = fixture.operation(/*started_at_ms*/ 1_000_000);
    fixture.begin(&operation).await;
    fixture.finish(&operation, /*duration_ms*/ 100).await;
    for (started_at_ms, ended_at_ms) in [(1_000_010, Some(1_000_030)), (1_000_040, None)] {
        let span = ActivitySpanId::new();
        fixture
            .store
            .begin_activity_span(&NewActivitySpan {
                id: span,
                operation_id: operation.id,
                activity_state: ActivityState::UserWait,
                started_at_ms,
            })
            .await
            .expect("wait span");
        if let Some(occurred_at_ms) = ended_at_ms {
            fixture
                .store
                .record_activity_span_event(&NewActivitySpanEvent {
                    event_id: FactEventId::new(),
                    activity_span_id: span,
                    kind: ActivitySpanEventKind::Ended,
                    occurred_at_ms,
                })
                .await
                .expect("wait ended");
        }
    }
    let packet = fixture.packet().await;
    assert_eq!(
        packet.waits,
        vec![ReviewCategory {
            category: "user_wait".to_string(),
            count: 2,
            measured_interval_sum_ms: 20,
            unknown_intervals: 1
        }]
    );
    fixture
        .store
        .record_classification(&NewClassificationEvent {
            event_id: FactEventId::new(),
            operation_id: operation.id,
            phase: Phase::Implementation,
            activity: Activity::Coding,
            activity_state: ActivityState::UserWait,
            provenance: AttributionProvenance::UserCorrected,
            supersedes_event_id: None,
            occurred_at_ms: 1_000_200,
        })
        .await
        .expect("correct activity");
    assert_eq!(
        fixture.packet().await.waits,
        vec![ReviewCategory {
            category: "user_wait".to_string(),
            count: 1,
            measured_interval_sum_ms: 100,
            unknown_intervals: 0
        }]
    );
}

#[tokio::test]
async fn incremental_classification_cache_removes_corrected_validation_candidates() {
    let fixture = Fixture::new().await;
    let original = fixture.operation(/*started_at_ms*/ 1_000_000);
    fixture.begin(&original).await;
    let mut retry = fixture.operation(/*started_at_ms*/ 1_000_200);
    retry.retry_of_operation_id = Some(original.id);
    fixture.begin(&retry).await;
    fixture.finish(&retry, /*duration_ms*/ 100).await;
    assert_eq!(fixture.packet().await.candidates.len(), 1);
    fixture
        .store
        .record_classification(&NewClassificationEvent {
            event_id: FactEventId::new(),
            operation_id: retry.id,
            phase: Phase::Testing,
            activity: Activity::VerificationReview,
            activity_state: ActivityState::ToolActive,
            provenance: AttributionProvenance::UserCorrected,
            supersedes_event_id: None,
            occurred_at_ms: 1_000_400,
        })
        .await
        .expect("correct deliberate validation");
    let packet = fixture.packet().await;
    assert!(packet.candidates.is_empty());
    assert_eq!(packet.links.retry_operations, 1);
    sqlx::query("UPDATE _usage_report_cache_meta SET ready = 0")
        .execute(&fixture.store.pool)
        .await
        .expect("disable disposable derived cache");
    assert_eq!(fixture.packet().await, packet);
}
