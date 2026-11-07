//! Verifies observed Responses API usage is durably recorded and reported from published snapshots.

use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_history::RolloutItem;
use codex_protocol::SessionId;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::TokenUsageRecord;
use codex_usage::Freshness;
use codex_usage::TimeRange;
use codex_usage::USAGE_DIRECTORY_NAME;
use codex_usage::UsageFilter;
use codex_usage::UsageJournal;
use codex_usage::UsageReader;
use codex_usage::UsageTotals;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn token_usage_records(path: &std::path::Path) -> Vec<TokenUsageRecord> {
    std::fs::read_to_string(path)
        .expect("read rollout")
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|line| match line.item {
            RolloutItem::TokenUsageRecord(record) => Some(record),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observed_response_usage_accumulates_per_turn_and_thread() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let plan_args = json!({
        "plan": [{
            "step": "keep sampling",
            "status": "in_progress"
        }]
    })
    .to_string();
    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("response-a"),
                ev_function_call("call-a", "update_plan", &plan_args),
                ev_completed_with_tokens("response-a", /*total_tokens*/ 120),
            ]),
            sse(vec![
                ev_response_created("response-b"),
                ev_assistant_message("message-b", "done"),
                ev_completed_with_tokens("response-b", /*total_tokens*/ 80),
            ]),
            sse(vec![
                ev_response_created("response-c"),
                ev_assistant_message("message-c", "next"),
                ev_completed_with_tokens("response-c", /*total_tokens*/ 30),
            ]),
            sse(vec![
                ev_response_created("response-without-usage"),
                ev_assistant_message("message-d", "no usage"),
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "response-without-usage"
                    }
                }),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.update_plan_enabled = true)
        .build_with_auto_env(&server)
        .await?;
    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let home = test.home.clone();

    test.submit_turn("first").await?;
    test.codex.shutdown_and_wait().await?;

    let resumed = test_codex()
        .resume(&server, home, rollout_path.clone())
        .await?;
    for prompt in ["second", "third"] {
        resumed.submit_turn(prompt).await?;
    }
    resumed.codex.shutdown_and_wait().await?;

    let records = token_usage_records(&rollout_path);
    assert_eq!(records.len(), 3);
    assert_eq!(
        records
            .iter()
            .map(|record| {
                (
                    record.response_id.as_str(),
                    record.turn_token_usage.total_tokens,
                    record.thread_token_usage.total_tokens,
                )
            })
            .collect::<Vec<_>>(),
        vec![
            ("response-a", 120, 120),
            ("response-b", 200, 200),
            ("response-c", 30, 230),
        ]
    );
    assert_eq!(records[0].turn_id, records[1].turn_id);
    assert_ne!(records[1].turn_id, records[2].turn_id);
    assert!(records.iter().all(|record| {
        record.session_id == SessionId::from(record.thread_id)
            && record.root_turn_id == record.turn_id
    }));

    let facts = UsageJournal::new(
        resumed
            .home
            .path()
            .join(USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()?;
    assert!(facts.iter().all(|fact| fact.observed_at_ms.is_some()
        && (fact.started_at.is_none() || fact.started_at_ms.is_some())
        && (fact.ended_at.is_none() || fact.ended_at_ms.is_some())));
    let requests = facts
        .iter()
        .filter(|fact| fact.event.as_deref() == Some("model_request_finished"))
        .collect::<Vec<_>>();
    assert_eq!(
        requests
            .iter()
            .map(|fact| (
                fact.response_id.as_deref(),
                fact.outcome,
                fact.duration_ms.is_some(),
                fact.coverage.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                Some("response-a"),
                Some(codex_usage::UsageOutcome::Completed),
                true,
                Some("recorded")
            ),
            (
                Some("response-b"),
                Some(codex_usage::UsageOutcome::Completed),
                true,
                Some("recorded")
            ),
            (
                Some("response-c"),
                Some(codex_usage::UsageOutcome::Completed),
                true,
                Some("recorded")
            ),
            (
                Some("response-without-usage"),
                Some(codex_usage::UsageOutcome::Completed),
                true,
                Some("recorded")
            ),
        ]
    );
    for finished in requests {
        let started = facts
            .iter()
            .find(|fact| {
                fact.event.as_deref() == Some("model_request_started")
                    && fact.operation_id == finished.operation_id
            })
            .expect("request start fact");
        assert_eq!(
            (
                &started.thread_id,
                &started.turn_id,
                started.coverage.as_deref()
            ),
            (
                &finished.thread_id,
                &finished.turn_id,
                Some("capture_started")
            )
        );
    }
    let tool = facts
        .iter()
        .filter(|fact| fact.tool.as_deref() == Some("update_plan"))
        .collect::<Vec<_>>();
    assert_eq!(
        tool.iter()
            .map(|fact| (
                fact.event.as_deref(),
                fact.outcome,
                fact.classification.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some("tool_started"), None, Some("runtime_observed")),
            (
                Some("tool_finished"),
                Some(codex_usage::UsageOutcome::Completed),
                Some("runtime_observed")
            ),
        ]
    );
    assert_eq!(
        (&tool[0].operation_id, &tool[0].turn_id),
        (&tool[1].operation_id, &tool[1].turn_id)
    );
    assert!(
        facts
            .iter()
            .filter(|fact| fact.event.as_deref() != Some("token_observation"))
            .all(|fact| {
                (
                    fact.input_tokens,
                    fact.cached_input_tokens,
                    fact.output_tokens,
                    fact.reasoning_output_tokens,
                ) == (0, 0, 0, 0)
                    && fact.activity.is_none()
                    && fact.phase.is_none()
            })
    );
    assert!(!serde_json::to_string(&facts)?.contains("keep sampling"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_stats_reports_automatically_published_response_usage_without_catalog_opt_in()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    const MODEL: &str = "local-usage-report-model";
    const RETIRED: &[u8] = b"retired usage database must stay unused";
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("usage-observed"),
                ev_assistant_message("usage-message", "observation captured"),
                ev_completed_with_tokens("usage-observed", /*total_tokens*/ 120),
            ]),
            sse(vec![
                ev_response_created("usage-report"),
                ev_function_call(
                    "usage-call",
                    "usage_stats",
                    &json!({"action": "summary", "scope": "current_chat"}).to_string(),
                ),
                ev_function_call(
                    "usage-review-call",
                    "usage_stats",
                    &json!({"action":"performance_review", "thread_id":"current", "outcome_limit":1})
                        .to_string(),
                ),
                json!({"type": "response.completed", "response": {"id": "usage-report"}}),
            ]),
            sse(vec![
                ev_response_created("usage-final"),
                ev_assistant_message("usage-final-message", "usage reported"),
                json!({"type": "response.completed", "response": {"id": "usage-final"}}),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_model_info_override(MODEL, |model| {
            model.use_responses_lite = false;
            model.tool_mode = Some(ToolMode::Direct);
            model.experimental_supported_tools.clear();
        })
        .with_config(|config| {
            let retired = config.codex_home.join("usage");
            std::fs::create_dir_all(&retired).expect("legacy usage directory");
            std::fs::write(retired.join("usage.sqlite3"), RETIRED).expect("legacy database canary");
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Record one response.").await?;
    let body = mock.requests()[0].body_json();
    assert!(
        body["tools"]
            .as_array()
            .expect("model tools")
            .iter()
            .any(|tool| { tool["type"] == "function" && tool["name"] == "usage_stats" })
    );

    let root = test.home.path().join(USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let expected_totals = UsageTotals {
        input_tokens: 120,
        cached_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
        records: 1,
    };
    assert_eq!(
        journal
            .records()?
            .iter()
            .filter(|record| record.event.as_deref() == Some("token_observation"))
            .map(codex_usage::UsageRecord::totals)
            .collect::<Vec<_>>(),
        vec![expected_totals.clone()]
    );
    let captured = journal.checkpoint()?.sequence;
    let mut latest = (0, 0);
    let published = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let reader = UsageReader::open(test.home.path())?;
            let manifest = reader.manifest();
            latest = (manifest.snapshot_generation, manifest.watermark);
            if manifest.snapshot_generation > 0 && manifest.watermark >= captured {
                return Ok::<_, codex_usage::StorageError>(reader);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .with_context(|| format!("automatic publication did not reach {captured}; latest generation/watermark: {latest:?}"))??;
    assert_eq!(published.freshness(), Freshness::Fresh);
    assert_eq!(published.pending_entries(), 0);
    assert!(!published.manifest().segment_files.is_empty());
    let current_thread = test.session_configured.session_id.to_string();
    let totals_through = |watermark| -> Result<(UsageTotals, UsageTotals)> {
        let mut by_id = std::collections::BTreeMap::new();
        for entry in journal.read_after(/*sequence*/ 0)? {
            if entry.sequence > watermark {
                break;
            }
            if let codex_usage::JournalPayload::Record(record) = entry.payload {
                by_id.entry(record.id.clone()).or_insert(record);
            }
        }
        let mut all = UsageTotals::default();
        let mut chat = UsageTotals::default();
        for record in by_id.values() {
            all.add_assign(&record.totals());
            if record.thread_id.as_deref() == Some(current_thread.as_str()) {
                chat.add_assign(&record.totals());
            }
        }
        Ok((all, chat))
    };
    assert_eq!(
        published.totals(TimeRange::all(), &UsageFilter::default())?,
        totals_through(published.journal_watermark())?.0
    );

    test.submit_turn("Report this chat's usage.").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let (content, _) = requests[2]
        .function_call_output_content_and_success("usage-call")
        .expect("exposed usage_stats output");
    let report: Value = serde_json::from_str(&content.expect("usage report JSON"))?;
    let content = requests[2]
        .function_call_output_text("usage-review-call")
        .expect("performance review JSON output");
    let review: Value = serde_json::from_str(&content)
        .context("performance review must be report JSON, not a tool error")?;
    assert_eq!(
        (&review["schemaVersion"], &review["kind"]),
        (&json!(1), &json!("performanceReview"))
    );
    assert_eq!(
        (
            &review["outcomes"]["coverage"],
            &review["outcomes"]["rows"],
            &review["outcomes"]["totalRows"],
            &review["outcomes"]["nextCursor"]
        ),
        (&json!("unobserved"), &json!([]), &json!(0), &Value::Null)
    );
    let report_watermark = report["journalWatermark"]
        .as_u64()
        .expect("report watermark");
    assert!(report_watermark >= captured);
    assert!(report_watermark <= journal.checkpoint()?.sequence);
    let generation = report["snapshotGeneration"]
        .as_u64()
        .expect("snapshot generation");
    assert!(generation >= published.manifest().snapshot_generation);
    let watermark_at_ms = report["watermarkAtMs"]
        .as_i64()
        .expect("snapshot timestamp");
    assert!(watermark_at_ms >= published.manifest().watermark_at_ms);
    let (_, observed) = totals_through(report_watermark)?;
    let mut token_report = report.clone();
    for field in ["counts", "tools", "coverage", "formulas"] {
        token_report
            .as_object_mut()
            .expect("report object")
            .remove(field);
    }
    let mut operations = std::collections::BTreeMap::new();
    for entry in journal.read_after(/*sequence*/ 0)? {
        if entry.sequence > report_watermark {
            break;
        }
        if let codex_usage::JournalPayload::Record(fact) = entry.payload
            && let Some(id) = &fact.operation_id
            && matches!(
                fact.event.as_deref(),
                Some(
                    "model_request_started"
                        | "model_request_finished"
                        | "model_warmup_started"
                        | "model_warmup_finished"
                        | "tool_started"
                        | "tool_finished"
                )
            )
        {
            let previous = operations.get(id);
            if previous.is_none_or(|old: &codex_usage::UsageRecord| old.outcome.is_none()) {
                operations.insert(id.clone(), fact);
            }
        }
    }
    let count = |predicate: fn(&codex_usage::UsageRecord) -> bool| {
        operations.values().filter(|fact| predicate(fact)).count()
    };
    assert_eq!(
        report["counts"],
        json!({
            "operations":operations.len(),
            "modelRequests":count(|fact| fact.event.as_deref().is_some_and(|event| event.starts_with("model_request_"))),
            "tools":count(|fact| fact.event.as_deref().is_some_and(|event| event.starts_with("tool_"))),
            "warmups":count(|fact| fact.event.as_deref().is_some_and(|event| event.starts_with("model_warmup_"))),
            "unfinished":count(|fact| fact.outcome.is_none()),
            "unknownIntervals":count(|fact| fact.effective_started_at_ms().is_none() || fact.effective_ended_at_ms().is_none()),
            "unclassified":count(|fact| fact.phase.is_none() || fact.activity.is_none()),
            "missingRepository":count(|fact| fact.repository_id.is_none()),
        })
    );
    assert!(
        report["counts"]["modelRequests"]
            .as_u64()
            .expect("model requests")
            >= 1
    );
    assert_eq!(
        token_report,
        json!({
            "schemaVersion": 1,
            "kind": "usageSummary",
            "inputTokens": 120,
            "cachedInputTokens": 0,
            "outputTokens": 0,
            "reasoningOutputTokens": 0,
            "records": observed.records,
            "scope": "current_chat",
            "snapshotGeneration": generation,
            "watermarkAtMs": watermark_at_ms,
            "journalWatermark": report_watermark,
            "pendingEntries": 0,
            "freshness": "fresh",
            "pendingCorrections": 0,
            "retention": {
                "rolledUpObservations": 0,
                "unappliedCorrections": 0,
                "unappliedObservations": 0,
                "retiredReviewOperations": 0,
                "retiredReviewContributions": 0,
                "totalsCoverage": "recorded",
                "historicalWindowCoverage": "rawObservations",
            },
        })
    );
    test.codex.shutdown_and_wait().await?;
    let retired = test.home.path().join("usage/usage.sqlite3");
    assert_eq!(std::fs::read(&retired)?, RETIRED);
    for sidecar in [
        "usage.sqlite3-wal",
        "usage.sqlite3-shm",
        "usage.sqlite3-journal",
    ] {
        assert!(!retired.with_file_name(sidecar).exists());
    }
    Ok(())
}
