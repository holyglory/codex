use super::*;
use pretty_assertions::assert_eq;

#[cfg(unix)]
#[tokio::test]
async fn completed_model_without_usage_keeps_a_gap_until_facts_arrive() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store
        .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("process");
    insert_thread(&store, "complete").await;
    let mut requests = Vec::new();
    for _ in 0..2 {
        let op = operation(process, "complete", OperationKind::ModelRequest);
        requests.push(record_request(&store, &op).await);
        store
            .record_coverage(&NewCoverageEvent {
                event_id: FactEventId::new(),
                operation_id: Some(op.id),
                scope_kind: CoverageScopeKind::new("model_attempt").expect("scope"),
                state: CoverageState::Partial,
                reason_code: None,
                occurred_at_ms: 1_100,
            })
            .await
            .expect("capture");
        store
            .finish_operation(&TerminalOperation {
                operation_id: op.id,
                status: TerminalStatus::Completed,
                occurred_at_ms: 1_100,
                duration_ns: 100_000_000,
                error_category: None,
            })
            .await
            .expect("terminal");
    }
    store
        .record_token_observation(&token(
            requests[0],
            RepositoryBucket::Unknown,
            Some(25),
            CoverageState::Complete,
        ))
        .await
        .expect("first usage");
    let missing = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("missing summary");
    store
        .record_token_observation(&token(
            requests[1],
            RepositoryBucket::Unknown,
            Some(25),
            CoverageState::Complete,
        ))
        .await
        .expect("late usage");
    let complete = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("complete summary");
    assert_eq!(
        (missing.coverage.has_gaps, complete.coverage.has_gaps),
        (true, false)
    );
    assert_eq!(
        (
            missing.tokens[0].measured_tokens,
            complete.tokens[0].measured_tokens
        ),
        (25, 50)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn scoped_reports_preserve_results_when_unrelated_history_grows() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store
        .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("process");
    insert_thread(&store, "selected").await;
    insert_thread(&store, "unrelated").await;
    let repository = store
        .resolve_repository(
            &identity("/selected"),
            &label("selected"),
            /*observed_at_ms*/ 1,
        )
        .await
        .expect("repository");
    let selected = operation(process, "selected", OperationKind::ModelRequest);
    let request = record_request(&store, &selected).await;
    attribute(
        &store,
        selected.id,
        Some(repository.clone()),
        RepositoryAttributionKind::Primary,
    )
    .await;
    store
        .record_token_observation(&token(
            request,
            RepositoryBucket::Single(repository.clone()),
            Some(25),
            CoverageState::Complete,
        ))
        .await
        .expect("tokens");
    let queries = [
        UsageSummaryQuery {
            worktree_ids: None,
            include_unassigned_worktrees: true,
            thread_id: Some(ThreadId::new("selected").expect("thread")),
            repository_id: None,
            account_profile_ref: None,
            time_range: Some(
                UtcTimeRange::new(/*start_ms*/ 1_000, /*end_ms*/ 2_000).expect("range"),
            ),
        },
        UsageSummaryQuery {
            worktree_ids: None,
            include_unassigned_worktrees: true,
            thread_id: None,
            repository_id: Some(repository.clone()),
            account_profile_ref: None,
            time_range: Some(
                UtcTimeRange::new(/*start_ms*/ 1_000, /*end_ms*/ 2_000).expect("range"),
            ),
        },
    ];
    let mut expected = Vec::new();
    for query in &queries {
        expected.push(
            store
                .usage_summary_query(query.clone())
                .await
                .expect("baseline"),
        );
    }
    for _ in 0..100 {
        let unrelated = operation(process, "unrelated", OperationKind::ModelRequest);
        let request = record_request(&store, &unrelated).await;
        store
            .record_token_observation(&token(
                request,
                RepositoryBucket::Unknown,
                Some(1_000),
                CoverageState::Partial,
            ))
            .await
            .expect("unrelated tokens");
    }
    let mut outside_window = token(
        request,
        RepositoryBucket::Single(repository),
        Some(100),
        CoverageState::Partial,
    );
    outside_window.observed_at_ms = 2_000;
    store
        .record_token_observation(&outside_window)
        .await
        .expect("later observation");
    let mut actual = Vec::new();
    for query in queries {
        actual.push(
            store
                .usage_summary_query(query)
                .await
                .expect("scoped summary"),
        );
    }
    assert_eq!(actual, expected);
}

#[cfg(unix)]
#[tokio::test]
async fn ready_cache_preserves_one_sided_window_bounds() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store
        .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("process");
    insert_thread(&store, "bounds").await;
    let mut old = operation(process, "bounds", OperationKind::ModelRequest);
    old.started_at_ms = 1_000;
    let old_request = record_request(&store, &old).await;
    let mut new = operation(process, "bounds", OperationKind::ModelRequest);
    new.started_at_ms = 2_000;
    let new_request = record_request(&store, &new).await;
    for (operation, ended_at_ms) in [(&old, 1_100), (&new, 2_100)] {
        store
            .finish_operation(&TerminalOperation {
                operation_id: operation.id,
                status: TerminalStatus::Completed,
                occurred_at_ms: ended_at_ms,
                duration_ns: 100_000_000,
                error_category: None,
            })
            .await
            .expect("terminal");
    }
    for (request, observed_at_ms, count) in [(old_request, 1_002, 11), (new_request, 2_002, 13)] {
        let mut fact = token(
            request,
            RepositoryBucket::Unknown,
            Some(count),
            CoverageState::Complete,
        );
        fact.observed_at_ms = observed_at_ms;
        store
            .record_token_observation(&fact)
            .await
            .expect("token fact");
    }
    crate::report_cache::ensure(&store.pool, &store.report_refresh.reader)
        .await
        .expect("ready cache");
    store.report_refresh.cancel();
    let query = |time_range| UsageSummaryQuery {
        worktree_ids: None,
        include_unassigned_worktrees: true,
        thread_id: Some(ThreadId::new("bounds").expect("thread")),
        repository_id: None,
        account_profile_ref: None,
        time_range,
    };
    let from_only = store
        .usage_summary_query(query(Some(
            UtcTimeRange::new(1_500, i64::MAX).expect("from"),
        )))
        .await
        .expect("from-only summary");
    let from_expected = store
        .usage_summary_query(query(Some(
            UtcTimeRange::new(1_500, 2_500).expect("from range"),
        )))
        .await
        .expect("from expected");
    let to_only = store
        .usage_summary_query(query(Some(UtcTimeRange::new(i64::MIN, 1_500).expect("to"))))
        .await
        .expect("to-only summary");
    let to_expected = store
        .usage_summary_query(query(Some(
            UtcTimeRange::new(500, 1_500).expect("to range"),
        )))
        .await
        .expect("to expected");
    for (actual, expected, tokens) in [
        (&from_only, &from_expected, 13),
        (&to_only, &to_expected, 11),
    ] {
        assert_eq!(actual.operation_count, expected.operation_count);
        assert_eq!(actual.model_request_count, expected.model_request_count);
        assert_eq!(actual.coverage.event_counts, expected.coverage.event_counts);
        assert_eq!(actual.tokens, expected.tokens);
        assert_eq!(actual.tokens[0].measured_tokens, tokens);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn completed_capture_keeps_history_without_reporting_the_start_as_a_gap() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = UsageStore::open(temp.path()).await.expect("store");
    let process = ProcessId::new();
    store
        .register_process(&process, /*os_pid*/ 42, /*started_at_ms*/ 900)
        .await
        .expect("process");
    insert_thread(&store, "complete").await;
    let op = operation(process, "complete", OperationKind::ModelRequest);
    let request = record_request(&store, &op).await;
    store
        .record_token_observation(&token(
            request,
            RepositoryBucket::Unknown,
            Some(25),
            CoverageState::Complete,
        ))
        .await
        .expect("tokens");
    for (state, time) in [
        (CoverageState::CaptureStarted, 1_000),
        (CoverageState::Partial, 1_100),
    ] {
        store
            .record_coverage(&NewCoverageEvent {
                event_id: FactEventId::new(),
                operation_id: Some(op.id),
                scope_kind: CoverageScopeKind::new("model_attempt").expect("scope"),
                state,
                reason_code: None,
                occurred_at_ms: time,
            })
            .await
            .expect("capture");
    }
    let pending = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("pending summary");
    assert!(pending.coverage.has_gaps);
    store
        .finish_operation(&TerminalOperation {
            operation_id: op.id,
            status: TerminalStatus::Completed,
            occurred_at_ms: 1_100,
            duration_ns: 100_000_000,
            error_category: None,
        })
        .await
        .expect("terminal");
    let complete = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("complete summary");
    assert_eq!(
        complete.coverage,
        CoverageSummary {
            overall_state: "complete".into(),
            event_counts: vec![
                CoverageCount {
                    state: "capture_started".into(),
                    count: 1
                },
                CoverageCount {
                    state: "partial".into(),
                    count: 1
                }
            ],
            token_observation_counts: vec![CoverageCount {
                state: "complete".into(),
                count: 1
            }],
            has_gaps: false,
            unfinished_operations: 0
        }
    );
    let structured = StructuredUsageSummary::new(&complete, /*account*/ None);
    assert_eq!(structured.coverage.dimensions.recorded_tokens, "complete");
    assert_eq!(structured.coverage.dimensions.timing_unknown_intervals, 0);
    store
        .record_coverage(&NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(op.id),
            scope_kind: CoverageScopeKind::new("model_attempt").expect("scope"),
            state: CoverageState::Unknown,
            reason_code: None,
            occurred_at_ms: 1_200,
        })
        .await
        .expect("missing evidence");
    let missing = store
        .usage_summary(UsageSummaryScope::All)
        .await
        .expect("missing summary");
    assert!(missing.coverage.has_gaps);
    assert_eq!(missing.tokens, complete.tokens);
}
