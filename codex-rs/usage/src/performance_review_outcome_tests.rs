use super::*;
use pretty_assertions::assert_eq;

fn binding(outcome: Option<&str>) -> OperationWorkContext {
    OperationWorkContext {
        native_project_id: Some("project-accounting".into()),
        workstream_id: Some("accounting".into()),
        outcome_id: outcome.map(str::to_string),
        experiment_ref: None,
    }
}

#[tokio::test]
async fn outcome_thread_scope_only_includes_descendants_when_requested() {
    let fixture = Fixture::new().await;
    fixture
        .store
        .ensure_thread(&NewThread {
            id: ThreadId::new("child").expect("child"),
            parent_thread_id: Some(ThreadId::new("spec").expect("parent")),
            source_kind: ThreadSourceKind::new("delegated").expect("source"),
            created_at_ms: 1_000_000,
        })
        .await
        .expect("child");
    for (thread, outcome) in [("spec", "outcome-a"), ("child", "outcome-b")] {
        let mut operation = fixture.operation(/*started_at_ms*/ 1_000_000);
        operation.thread_id = Some(ThreadId::new(thread).expect("thread"));
        operation.work_context = Some(binding(Some(outcome)));
        fixture.begin(&operation).await;
        fixture.finish(&operation, /*duration_ms*/ 10).await;
    }
    for (include_descendants, expected) in [
        (false, vec!["outcome-a"]),
        (true, vec!["outcome-a", "outcome-b"]),
    ] {
        let report = fixture
            .store
            .performance_review_packet(PerformanceReviewQuery {
                thread_id: Some(ThreadId::new("spec").expect("thread")),
                include_descendants,
                ..Default::default()
            })
            .await
            .expect("scoped report");
        assert_eq!(
            report
                .outcomes
                .rows
                .iter()
                .map(|row| row.outcome_id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[tokio::test]
async fn outcome_repository_scope_keeps_merged_identities() {
    let fixture = Fixture::new().await;
    let mut operation = fixture.operation(/*started_at_ms*/ 1_000_000);
    operation.work_context = Some(binding(Some("outcome-a")));
    fixture.begin(&operation).await;
    fixture.finish(&operation, /*duration_ms*/ 10).await;
    let target = fixture
        .store
        .resolve_repository(
            &RepositoryIdentityInput::new(
                CanonicalRepositoryPath::new("/canonical-project").expect("path"),
            ),
            &SafeRepositoryLabel::new("canonical-project").expect("label"),
            /*observed_at_ms*/ 1_000_010,
        )
        .await
        .expect("target");
    fixture
        .store
        .append_repository_merge(
            FactEventId::new(),
            &fixture.repository,
            &target,
            /*occurred_at_ms*/ 1_000_020,
        )
        .await
        .expect("merge");
    for id in [fixture.repository.clone(), target] {
        let packet = fixture
            .store
            .performance_review_packet(PerformanceReviewQuery {
                repository_id: Some(id),
                ..Default::default()
            })
            .await
            .expect("merged report");
        assert_eq!(packet.outcomes.rows[0].outcome_id, "outcome-a");
        assert_eq!(packet.outcomes.totals.operations, 1);
    }
}

async fn token_at(
    fixture: &Fixture,
    source: TokenObservationSource,
    event: FactEventId,
    count: u64,
    at: i64,
) {
    fixture
        .store
        .record_token_observation(&NewTokenObservation {
            id: FactEventId::new(),
            source_event_id: event,
            source,
            category_path: TokenCategoryPath::new("total_tokens").expect("category"),
            token_count: Some(count),
            unit: TokenUnit::Tokens,
            measurement_provenance: MeasurementProvenance::ProviderReported,
            coverage_state: CoverageState::Complete,
            repository_bucket: RepositoryBucket::Single(fixture.repository.clone()),
            observed_at_ms: at,
        })
        .await
        .expect("token");
}

#[tokio::test]
async fn outcomes_reconcile_deduplicated_tokens_and_overlapping_agent_time() {
    let fixture = Fixture::new().await;
    let agent = AgentId::new("agent").expect("agent");
    fixture
        .store
        .ensure_agent(&NewAgent {
            id: agent.clone(),
            thread_id: ThreadId::new("spec").expect("thread"),
            parent_agent_id: None,
            role_kind: AgentRoleKind::new("test").expect("role"),
            created_at_ms: 1_000_000,
        })
        .await
        .expect("agent");
    let event = FactEventId::new();
    let mut requests = Vec::new();
    for (index, (outcome, count)) in [
        (Some("outcome-a"), 100),
        (Some("outcome-b"), 50),
        (None, 7),
        (None, 3),
    ]
    .into_iter()
    .enumerate()
    {
        let mut operation = fixture.operation(1_000_000 + index as i64 * 50);
        operation.kind = OperationKind::ModelRequest;
        operation.activity_state = ActivityState::ModelActive;
        operation.agent_id = Some(agent.clone());
        operation.work_context = (index != 2).then(|| binding(outcome));
        let request = fixture.model(&operation).await;
        fixture.finish(&operation, /*duration_ms*/ 100).await;
        token_at(
            &fixture,
            TokenObservationSource::ModelRequest(request),
            event,
            count,
            operation.started_at_ms + 20,
        )
        .await;
        requests.push((operation, request));
    }
    let mut covered = fixture.operation(/*started_at_ms*/ 1_000_030);
    covered.kind = OperationKind::HostedTool;
    covered.work_context = Some(binding(Some("outcome-a")));
    covered.agent_id = Some(agent);
    let tool = fixture
        .tool(
            &covered,
            ToolExecutionRole::Standalone,
            /*group*/ None,
            Some(requests[0].1),
        )
        .await;
    fixture.finish(&covered, /*duration_ms*/ 0).await;
    token_at(
        &fixture,
        TokenObservationSource::ToolInvocation(tool),
        event,
        /*count*/ 100,
        /*at*/ 1_000_030,
    )
    .await;
    let span = ActivitySpanId::new();
    fixture
        .store
        .begin_activity_span(&NewActivitySpan {
            id: span,
            operation_id: requests[0].0.id,
            activity_state: ActivityState::ExternalWait,
            started_at_ms: 1_000_020,
        })
        .await
        .expect("wait");
    fixture
        .store
        .record_activity_span_event(&NewActivitySpanEvent {
            event_id: FactEventId::new(),
            activity_span_id: span,
            kind: ActivitySpanEventKind::Ended,
            occurred_at_ms: 1_000_040,
        })
        .await
        .expect("end wait");
    let report = fixture.packet().await.outcomes;
    assert_eq!(report.coverage, "partial");
    assert_eq!(
        (
            report.totals.provider_total_tokens.measured,
            report.attributed.provider_total_tokens.measured,
            report.unattributed.provider_total_tokens.measured
        ),
        (160, 150, 10)
    );
    assert_eq!(
        report.unattributed_reasons,
        std::collections::BTreeMap::from([
            ("legacy_operation".into(), 1),
            ("outcome_not_declared".into(), 1)
        ])
    );
    assert_eq!(
        report.totals.elapsed_execution_ms,
        OutcomeMeasurement {
            measured: 250,
            exact: Some(250),
            unknown: 0
        }
    );
    assert_eq!(
        report.totals.active_agent_ms,
        OutcomeMeasurement {
            measured: 230,
            exact: Some(230),
            unknown: 0
        }
    );
    assert_eq!(
        report.totals.recorded_wait_ms,
        OutcomeMeasurement {
            measured: 20,
            exact: Some(20),
            unknown: 0
        }
    );
    assert_eq!(
        report
            .rows
            .iter()
            .map(|row| (
                row.outcome_id.as_str(),
                row.effort.provider_total_tokens.measured,
                row.effort.active_agent_ms.measured
            ))
            .collect::<Vec<_>>(),
        vec![("outcome-a", 100, 80), ("outcome-b", 50, 100)]
    );
}

#[tokio::test]
async fn outcome_pages_keep_totals_and_coverage_on_the_original_snapshot() {
    let fixture = Fixture::new().await;
    for outcome in ["outcome-a", "outcome-b", "outcome-c"] {
        let mut operation = fixture.operation(/*started_at_ms*/ 1_000_000);
        operation.work_context = Some(binding(Some(outcome)));
        fixture.begin(&operation).await;
        fixture.finish(&operation, /*duration_ms*/ 100).await;
    }
    let query = PerformanceReviewQuery {
        outcome_limit: Some(1),
        ..Default::default()
    };
    let first = fixture
        .store
        .performance_review_packet(query.clone())
        .await
        .expect("first page");
    let mut later = fixture.operation(/*started_at_ms*/ 1_000_000);
    later.work_context = Some(binding(Some("outcome-d")));
    fixture.begin(&later).await;
    let mut cursor = first.outcomes.next_cursor.clone();
    let mut names = vec![first.outcomes.rows[0].outcome_id.clone()];
    while let Some(value) = cursor {
        let packet = fixture
            .store
            .performance_review_packet(PerformanceReviewQuery {
                outcome_cursor: Some(value),
                ..query.clone()
            })
            .await
            .expect("next page");
        assert_eq!(packet.outcomes.totals, first.outcomes.totals);
        assert_eq!(packet.operations, first.operations);
        names.extend(
            packet
                .outcomes
                .rows
                .iter()
                .map(|row| row.outcome_id.clone()),
        );
        cursor = packet.outcomes.next_cursor;
    }
    assert_eq!(names, vec!["outcome-a", "outcome-b", "outcome-c"]);
    assert_eq!(fixture.packet().await.outcomes.total_rows, 4);
    let changed_scope = PerformanceReviewQuery {
        outcome_cursor: first.outcomes.next_cursor,
        thread_id: Some(ThreadId::new("spec").expect("thread")),
        ..query
    };
    assert!(matches!(
        fixture.store.performance_review_packet(changed_scope).await,
        Err(UsageStoreError::InvalidReviewCursor)
    ));
}

#[tokio::test]
async fn outcome_window_keeps_late_measurements_but_excludes_completed_time() {
    let fixture = Fixture::new().await;
    let mut operation = fixture.operation(/*started_at_ms*/ 1_000_000);
    operation.kind = OperationKind::ModelRequest;
    operation.work_context = Some(binding(Some("outcome-a")));
    let request = fixture.model(&operation).await;
    fixture.finish(&operation, /*duration_ms*/ 5).await;
    token_at(
        &fixture,
        TokenObservationSource::ModelRequest(request),
        FactEventId::new(),
        /*count*/ 12,
        /*at*/ 1_000_020,
    )
    .await;
    let report = fixture
        .store
        .performance_review_packet(PerformanceReviewQuery {
            time_range: Some(
                UtcTimeRange::new(/*start_ms*/ 1_000_010, /*end_ms*/ 1_000_021).expect("window"),
            ),
            ..Default::default()
        })
        .await
        .expect("late usage")
        .outcomes;
    assert_eq!(
        report.rows[0].effort.provider_total_tokens,
        OutcomeMeasurement {
            measured: 12,
            exact: Some(12),
            unknown: 0
        }
    );
    assert_eq!(
        report.totals.elapsed_execution_ms,
        OutcomeMeasurement {
            measured: 0,
            exact: Some(0),
            unknown: 0
        }
    );
}
