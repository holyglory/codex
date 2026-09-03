use super::*;
use codex_app_server_protocol as protocol;

pub(super) fn local_aggregate(
    coverage: protocol::LocalUsageCoverage,
) -> protocol::LocalUsageAggregate {
    protocol::LocalUsageAggregate {
        input_tokens: Some(120),
        cached_input_tokens: Some(80),
        cache_write_input_tokens: None,
        output_tokens: Some(40),
        reasoning_output_tokens: Some(10),
        total_tokens: None,
        model_requests: 2,
        tool_calls: 3,
        duration_ms: 1_250,
        coverage,
    }
}

pub(super) fn local_thread_response(
    coverage: protocol::LocalUsageCoverage,
) -> protocol::LocalUsageThreadReadResponse {
    protocol::LocalUsageThreadReadResponse {
        thread: protocol::LocalUsageThread {
            thread_id: "private-thread-id".to_string(),
            repository_keys: vec!["private-repository-key".to_string()],
            account_id: None,
            started_at: 1,
            updated_at: 2,
            aggregate: local_aggregate(coverage),
        },
        token_categories: vec![protocol::LocalUsageTokenCategory {
            category_key: "input_tokens".to_string(),
            count: Some(120),
            provenance: protocol::LocalUsageProvenance::ProviderReported,
            coverage,
        }],
        report: local_report(coverage),
    }
}

fn all_report(coverage: protocol::LocalUsageCoverage) -> protocol::LocalUsageReport {
    let mut report = local_report(coverage);
    report.scope = protocol::LocalUsageReportScope {
        kind: "all".to_string(),
        id: None,
    };
    report
}

pub(super) fn local_report(coverage: protocol::LocalUsageCoverage) -> protocol::LocalUsageReport {
    let coverage_state = match coverage {
        protocol::LocalUsageCoverage::Complete => "complete",
        protocol::LocalUsageCoverage::Partial => "partial",
        protocol::LocalUsageCoverage::Unknown => "unknown",
    };
    let duration = protocol::LocalUsageReportDuration {
        measured_ns: 1_250_000_000,
        exact_ns: Some(1_250_000_000),
        unknown_intervals: 0,
    };
    protocol::LocalUsageReport {
        schema_version: 1,
        kind: "usageSummary".to_string(),
        database_schema_version: 3,
        taxonomy_version: 1,
        scope: protocol::LocalUsageReportScope {
            kind: "thread".to_string(),
            id: Some("private-thread-id".to_string()),
        },
        account: Some("primary".to_string()),
        time_range: Some(protocol::LocalUsageReportTimeRange {
            start_ms: 10,
            end_ms: 20,
        }),
        coverage: protocol::LocalUsageReportCoverage {
            state: coverage_state.to_string(),
            has_gaps: coverage != protocol::LocalUsageCoverage::Complete,
            events: vec![protocol::LocalUsageReportCoverageCount {
                state: "capture_started".to_string(),
                count: 2,
            }],
            token_observations: vec![protocol::LocalUsageReportCoverageCount {
                state: "complete".to_string(),
                count: 1,
            }],
        },
        counts: protocol::LocalUsageReportCounts {
            operations: 5,
            model_requests: 2,
            tools: 3,
        },
        provider_tokens: vec![protocol::LocalUsageReportTokenAggregate {
            category: "input_tokens".to_string(),
            repository_bucket: "private-repository-key".to_string(),
            repository_label: Some("CodexMulti".to_string()),
            measurement_provenance: "provider_reported".to_string(),
            measured_tokens: 120,
            exact_tokens: Some(120),
            unknown_observations: 0,
            observation_count: 1,
        }],
        provider_tokens_by_activity: vec![protocol::LocalUsageReportActivityTokenAggregate {
            phase: "implementation".to_string(),
            activity: "coding".to_string(),
            attribution_provenance: "agent_declared".to_string(),
            measured_tokens: 120,
            exact_tokens: Some(120),
            unknown_observations: 0,
        }],
        tools: protocol::LocalUsageReportToolMetrics {
            count: 3,
            duration,
            duration_basis: "summed_invocations".to_string(),
            outcomes: vec![protocol::LocalUsageReportToolOutcome {
                outcome: "completed".to_string(),
                count: 3,
            }],
        },
        time: protocol::LocalUsageReportTimeMetrics {
            request_to_delivery_wall: duration,
            execution_wall_union: duration,
            summed_per_agent_active: duration,
            phase_interval_unions: vec![protocol::LocalUsageReportNamedDuration {
                name: "implementation".to_string(),
                duration,
            }],
            activity_state_interval_unions: vec![protocol::LocalUsageReportNamedDuration {
                name: "model_active".to_string(),
                duration,
            }],
        },
        classifications: vec![protocol::LocalUsageReportClassificationCount {
            phase: "implementation".to_string(),
            activity: "coding".to_string(),
            provenance: "agent_declared".to_string(),
            count: 5,
        }],
        repository_participation: protocol::LocalUsageReportRepositoryParticipation {
            operation_count: 5,
            tool_count: 3,
            additive: false,
            label: "participation only".to_string(),
        },
        formulas: protocol::LocalUsageReportFormulas {
            wall_time: "wall formula".to_string(),
            tokens: "token formula".to_string(),
            concurrency: "concurrency formula".to_string(),
            repository: "repository formula".to_string(),
        },
    }
}

#[test]
fn local_usage_report_renders_every_advanced_dimension_with_safe_labels() {
    let items = super::super::local_usage_report::report_items(
        &local_report(protocol::LocalUsageCoverage::Partial),
        "current chat",
        LocalUsageQuery::All,
    );
    let rendered = items
        .iter()
        .flat_map(|item| std::iter::once(item.name.as_str()).chain(item.description.as_deref()))
        .collect::<Vec<_>>()
        .join("\n");

    for expected in [
        "database schema",
        "Coverage events",
        "Token observations",
        "provider_reported",
        "Counts",
        "operations",
        "Provider tokens by activity",
        "Tools",
        "count 3",
        "Request-to-delivery wall",
        "Phase interval unions",
        "Activity-state interval unions",
        "Classifications",
        "Repository participation",
        "Formula · wall time",
        "Formula · tokens",
        "Formula · concurrency",
        "Formula · repository",
        "CodexMulti",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected}: {rendered}"
        );
    }
    assert!(!rendered.contains("private-thread-id"));
    assert!(!rendered.contains("private-repository-key"));
}

#[tokio::test]
async fn local_usage_all_opens_full_installation_report_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_local_usage_supported(/*supported*/ true);
    chat.open_local_usage_command("all");
    let Ok(AppEvent::RefreshLocalUsage { request_id, query }) = rx.try_recv() else {
        panic!("expected local usage request");
    };
    assert_eq!(query, LocalUsageQuery::All);
    chat.finish_local_usage(
        request_id,
        query,
        Ok(LocalUsageResponse::Summary(
            protocol::LocalUsageSummaryResponse {
                aggregate: local_aggregate(protocol::LocalUsageCoverage::Complete),
                token_categories: Vec::new(),
                report: all_report(protocol::LocalUsageCoverage::Complete),
                generated_at: 30,
            },
        )),
    );

    assert_chatwidget_snapshot!(
        "local_usage_all_full_report",
        render_bottom_popup(&chat, /*width*/ 72)
    );
    assert_chatwidget_snapshot!(
        "local_usage_all_full_report_narrow",
        render_bottom_popup(&chat, /*width*/ 38)
    );
    for _ in 0..7 {
        chat.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    assert_chatwidget_snapshot!(
        "local_usage_all_provider_tokens_wide",
        render_bottom_popup(&chat, /*width*/ 72)
    );
    assert_chatwidget_snapshot!(
        "local_usage_all_provider_tokens_narrow",
        render_bottom_popup(&chat, /*width*/ 38)
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_chatwidget_snapshot!(
        "local_usage_all_formulas_wide",
        render_bottom_popup(&chat, /*width*/ 72)
    );
    assert_chatwidget_snapshot!(
        "local_usage_all_formulas_narrow",
        render_bottom_popup(&chat, /*width*/ 38)
    );
}

#[tokio::test]
async fn local_usage_events_are_content_free_and_paginated() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_local_usage_supported(/*supported*/ true);
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.open_local_usage_command("events");
    let Ok(AppEvent::RefreshLocalUsage { request_id, query }) = rx.try_recv() else {
        panic!("expected local usage request");
    };
    let original_query = query.clone();
    finish_events(&mut chat, request_id, query);
    let rendered = render_bottom_popup(&chat, /*width*/ 72);
    assert!(rendered.contains("coverage gap"));
    assert!(rendered.contains("runtime observed"));
    assert!(!rendered.contains("private-event-id"));
    assert!(!rendered.contains("private-thread-id"));
    assert!(!rendered.contains("private-repository-key"));

    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::ShowLocalUsageFactDetail { title, .. }) if title == "coverage gap"
    );

    chat.open_local_usage(original_query);
    let Ok(AppEvent::RefreshLocalUsage { request_id, query }) = rx.try_recv() else {
        panic!("expected local usage request");
    };
    finish_events(&mut chat, request_id, query);
    chat.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::OpenLocalUsage {
            query: LocalUsageQuery::Events { cursor: Some(cursor), .. }
        }) if cursor == "opaque-cursor"
    );
}

fn finish_events(chat: &mut ChatWidget, request_id: u64, query: LocalUsageQuery) {
    chat.finish_local_usage(
        request_id,
        query,
        Ok(LocalUsageResponse::Events(
            protocol::LocalUsageEventListResponse {
                data: vec![protocol::LocalUsageEvent {
                    event_id: "private-event-id".to_string(),
                    thread_id: Some("private-thread-id".to_string()),
                    repository_key: Some("private-repository-key".to_string()),
                    occurred_at: 10,
                    kind: protocol::LocalUsageEventKind::CoverageGap,
                    provenance: protocol::LocalUsageProvenance::RuntimeObserved,
                    coverage: protocol::LocalUsageCoverage::Partial,
                }],
                next_cursor: Some("opaque-cursor".to_string()),
            },
        )),
    );
}

#[tokio::test]
async fn local_usage_tools_drilldown_and_error_retry_are_functional() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.set_local_usage_supported(/*supported*/ true);
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.open_local_usage_command("tools");
    let Ok(AppEvent::RefreshLocalUsage { request_id, query }) = rx.try_recv() else {
        panic!("expected local usage request");
    };
    chat.finish_local_usage(
        request_id,
        query.clone(),
        Ok(LocalUsageResponse::Tools(
            protocol::LocalUsageToolListResponse {
                data: vec![protocol::LocalUsageTool {
                    tool_call_id: "private-call-id".to_string(),
                    thread_id: "private-thread-id".to_string(),
                    repository_key: None,
                    tool_name: "shell".to_string(),
                    operation_family: "execution".to_string(),
                    started_at: 1,
                    completed_at: Some(2),
                    status: protocol::LocalUsageToolStatus::Completed,
                    provenance: protocol::LocalUsageProvenance::RuntimeObserved,
                }],
                next_cursor: Some("opaque-cursor".to_string()),
            },
        )),
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(rx.try_recv(), Ok(AppEvent::ShowLocalUsageToolDetail { .. }));

    chat.open_local_usage(query.clone());
    let Ok(AppEvent::RefreshLocalUsage { request_id, .. }) = rx.try_recv() else {
        panic!("expected retryable request");
    };
    chat.finish_local_usage(request_id, query.clone(), Err("private error".to_string()));
    let rendered = render_bottom_popup(&chat, /*width*/ 64);
    assert!(!rendered.contains("private error"));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_matches!(rx.try_recv(), Ok(AppEvent::OpenLocalUsage { query: retried }) if retried == query);
}
