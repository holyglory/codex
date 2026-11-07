use super::*;
use codex_usage::UsageRecord;
use codex_usage::UsageStore;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::tempdir;

fn args(action: UsageStatsAction) -> UsageStatsArgs {
    UsageStatsArgs {
        action,
        scope: Some(UsageStatsScope::All),
        repository: None,
        account: None,
        thread_id: None,
        root_thread_id: None,
        include_descendants: None,
        agent_id: None,
        detail: None,
        from_at_ms: None,
        to_at_ms: None,
        limit: Some(10),
        cursor_sort_value: None,
        cursor_id: None,
        outcome_cursor: None,
        outcome_limit: None,
    }
}

fn record(id: &str, observed_at: i64) -> UsageRecord {
    UsageRecord::new(id, observed_at, 10, 3)
        .with_thread("thread-1")
        .with_operation(format!("operation-{id}"))
        .with_repository("repo-1")
        .with_tool("shell")
        .with_activity("coding", "implementation")
}

#[test]
fn summary_respects_current_chat_and_account_filters() {
    let directory = tempdir().expect("temporary directory");
    let mut store = UsageStore::open(directory.path()).expect("open usage store");
    store.append_record(record("one", 10)).expect("append one");
    store
        .append_record(UsageRecord::new("other", 11, 100, 100).with_thread("thread-2"))
        .expect("append other");

    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".to_string(),
        cwd: directory.path().to_path_buf(),
    };
    let mut request = args(UsageStatsAction::Summary);
    request.scope = Some(UsageStatsScope::CurrentChat);
    let value = execute(
        &UsageReader::open(directory.path()).expect("reader"),
        &context,
        request,
    )
    .expect("summary");
    assert_eq!(value["records"], json!(1));
    assert_eq!(value["inputTokens"], json!(10));
    assert_eq!(value["scope"], json!("current_chat"));
}

#[test]
fn event_queries_return_metadata_and_a_cursor() {
    let directory = tempdir().expect("temporary directory");
    let mut store = UsageStore::open(directory.path()).expect("open usage store");
    store.append_record(record("one", 10)).expect("append one");
    store.append_record(record("two", 11)).expect("append two");
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".to_string(),
        cwd: directory.path().to_path_buf(),
    };
    let mut request = args(UsageStatsAction::Events);
    request.limit = Some(1);
    let value = execute(
        &UsageReader::open(directory.path()).expect("reader"),
        &context,
        request,
    )
    .expect("events");
    assert_eq!(value["kind"], json!("usageEvents"));
    assert_eq!(value["data"].as_array().expect("data").len(), 1);
    assert!(value["nextCursor"].is_object());
    assert!(value.get("snapshotGeneration").is_some());
}

#[test]
fn event_cursor_continuation_does_not_skip_the_next_record() {
    let directory = tempdir().expect("temporary directory");
    let mut store = UsageStore::open(directory.path()).expect("open usage store");
    let mut first_record = record("z-first", 10);
    first_record.observed_at_ms = Some(10_100);
    first_record.started_at = Some(10);
    first_record.started_at_ms = Some(10_010);
    first_record.ended_at = Some(10);
    first_record.ended_at_ms = Some(10_090);
    let mut second_record = record("a-second", 10);
    second_record.observed_at_ms = Some(10_900);
    store.append_record(first_record).expect("append one");
    store.append_record(second_record).expect("append two");
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".to_string(),
        cwd: directory.path().to_path_buf(),
    };
    let mut first_request = args(UsageStatsAction::Events);
    first_request.limit = Some(1);
    first_request.from_at_ms = Some(10_100);
    first_request.to_at_ms = Some(10_901);
    let first = execute(
        &UsageReader::open(directory.path()).expect("reader"),
        &context,
        first_request,
    )
    .expect("first page");
    let cursor = first["nextCursor"].as_object().expect("next cursor");
    assert_eq!(first["data"][0]["observedAtMs"], json!(10_100));
    assert_eq!(
        (
            &first["data"][0]["startedAtMs"],
            &first["data"][0]["endedAtMs"]
        ),
        (&json!(10_010), &json!(10_090))
    );
    assert_eq!(cursor["sortValue"], json!(10_100));
    let mut second_request = args(UsageStatsAction::Events);
    second_request.limit = Some(1);
    second_request.from_at_ms = Some(10_100);
    second_request.to_at_ms = Some(10_901);
    second_request.cursor_sort_value = cursor["sortValue"].as_i64();
    second_request.cursor_id = cursor["id"].as_str().map(str::to_string);
    let second = execute(
        &UsageReader::open(directory.path()).expect("reader"),
        &context,
        second_request,
    )
    .expect("second page");
    assert_eq!(second["data"][0]["id"], json!("a-second"));
    assert_eq!(second["data"][0]["observedAtMs"], json!(10_900));
    assert!(second["nextCursor"].is_null());
    let reader = UsageReader::open(directory.path()).expect("bounded reader");
    let mut bounded = args(UsageStatsAction::Events);
    bounded.from_at_ms = Some(10_100);
    bounded.to_at_ms = Some(10_900);
    let included = execute(&reader, &context, bounded).expect("half-open subsecond events");
    assert_eq!(
        included["data"]
            .as_array()
            .expect("events")
            .iter()
            .map(|event| event["id"].as_str())
            .collect::<Vec<_>>(),
        vec![Some("z-first")]
    );
    let repositories =
        execute(&reader, &context, args(UsageStatsAction::Repositories)).expect("repository times");
    assert_eq!(
        (
            &repositories["data"][0]["createdAtMs"],
            &repositories["data"][0]["updatedAtMs"]
        ),
        (&json!(10_100), &json!(10_900))
    );
    let mut request = args(UsageStatsAction::Events);
    request.limit = Some(1);
    let pinned = snapshots::execute_request(&context, request).expect("pinned first page");
    store
        .append_record(record("late-between", 10))
        .expect("new arrival");
    store
        .materialize_snapshot(codex_usage::Freshness::Fresh)
        .expect("publish changed snapshot");
    let mut request = args(UsageStatsAction::Events);
    request.limit = Some(1);
    request.cursor_id = pinned["nextCursor"]["id"].as_str().map(str::to_string);
    request.cursor_sort_value = pinned["nextCursor"]["sortValue"].as_i64();
    let continuation = snapshots::execute_request(&context, request).expect("pinned continuation");
    assert_eq!(continuation["data"][0]["id"], json!("a-second"));
    assert!(continuation["nextCursor"].is_null());
    assert_eq!(continuation["journalWatermark"], pinned["journalWatermark"]);
    assert_eq!(
        continuation["snapshotGeneration"],
        pinned["snapshotGeneration"]
    );
    for (id, span, event, at, ended, heartbeats) in [
        (
            "z-span-start",
            "span-a",
            "activity_span_started",
            12_000,
            None,
            0,
        ),
        (
            "a-span-finish",
            "span-a",
            "activity_span_finished",
            12_000,
            Some(12_000),
            1,
        ),
        (
            "other-span-finish",
            "span-b",
            "activity_span_finished",
            13_000,
            Some(13_000),
            2,
        ),
    ] {
        let mut span = UsageRecord::new_millis(id, at, 0, 0).with_operation(span);
        span.event = Some(event.to_string());
        span.started_at_ms = Some(12_000);
        span.ended_at_ms = ended;
        span.heartbeat_count = Some(heartbeats);
        store.append_record(span).expect("capture span lifecycle");
    }
    store
        .materialize_snapshot(codex_usage::Freshness::Fresh)
        .expect("publish span states");
    let reader = UsageReader::open(directory.path()).expect("span snapshot reader");
    let mut request = args(UsageStatsAction::Details);
    request.detail = Some("activity_spans".to_string());
    request.limit = Some(1);
    let first = execute(&reader, &context, request).expect("first span");
    assert_eq!(
        first["data"],
        json!([{"type":"activity_span", "id":"span-a",
        "agentId":null,"processId":null,"threadId":null,"phase":null,"activity":null,"provenance":null,
        "reworkOfOperationId":null,"startedAtMs":12_000,"endedAtMs":12_000,"heartbeatCount":1}])
    );
    let mut continuation = args(UsageStatsAction::Details);
    continuation.detail = Some("activity_spans".to_string());
    continuation.limit = Some(1);
    continuation.cursor_sort_value = first["nextCursor"]["sortValue"].as_i64();
    continuation.cursor_id = first["nextCursor"]["id"].as_str().map(str::to_string);
    let second = execute(&reader, &context, continuation).expect("second span");
    assert_eq!(second["data"][0]["id"], json!("span-b"));
    assert!(
        second["nextCursor"].is_null(),
        "older snapshots must not reappear after the cursor"
    );
}

#[test]
fn dimension_actions_expose_recorded_rows() {
    let directory = tempdir().expect("temporary directory");
    let root = directory.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    let journal = codex_usage::UsageJournal::new(root.join("events.jsonl"));
    let mut facts = Vec::new();
    for (id, kind, start, end, tool) in [
        ("tool-before", "tool", 1_000, 1_100, Some("shell")),
        ("tool-one", "tool", 1_010, 1_510, Some("shell")),
        ("model-one", "model_request", 1_110, 1_310, None),
    ] {
        let mut started = UsageRecord::new_millis(format!("{id}-start"), start, 0, 0)
            .with_operation(id)
            .with_thread("thread-1")
            .with_repository("repo-1")
            .with_activity("coding", "implementation");
        started.event = Some(format!("{kind}_started"));
        started.started_at_ms = Some(start);
        started.tool = tool.map(str::to_string);
        started.classification = Some("agent_declared".to_string());
        let mut finished = started.clone();
        finished.id = format!("{id}-finish");
        finished.observed_at_ms = Some(end);
        finished.event = Some(format!("{kind}_finished"));
        finished.ended_at_ms = Some(end);
        finished.duration_ms = Some((end - start) as u64);
        finished.outcome = Some(codex_usage::UsageOutcome::Completed);
        finished.coverage = Some("recorded".to_string());
        if tool.is_none() {
            finished.response_id = Some("response-one".to_string());
        }
        facts.extend([started, finished]);
    }
    let mut token = UsageRecord::new_millis("provider-one", 1_350, 10, 3)
        .with_operation("response-one")
        .with_thread("thread-1")
        .with_repository("repo-1");
    token.response_id = Some("response-one".to_string());
    token.event = Some("token_observation".to_string());
    token.coverage = Some("recorded".to_string());
    token.provider_total_tokens = Some(13);
    token.cached_input_tokens = 4;
    token.reasoning_output_tokens = 1;
    facts.push(token);
    journal
        .append_batch(
            facts
                .into_iter()
                .map(codex_usage::JournalPayload::Record)
                .collect(),
        )
        .expect("capture facts");
    let mut publisher = codex_usage::UsagePublisher::open_at(&root).expect("publisher");
    publisher
        .publish_batch(codex_usage::JournalReadLimits {
            max_bytes: 1024 * 1024,
            max_entries: 100,
        })
        .expect("publish");
    let pinned = UsageReader::open(directory.path()).expect("pinned reader");
    let mut correction = UsageRecord::new_millis("classification-late", 10_000, 0, 0)
        .with_operation("model-one")
        .with_activity("unit_testing", "testing");
    correction.event = Some("classification_correction".to_string());
    correction.classification = Some("agent_corrected".to_string());
    journal.append_record(correction).expect("late correction");
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".to_string(),
        cwd: directory.path().to_path_buf(),
    };
    let review = |reader: &UsageReader| {
        let mut request = args(UsageStatsAction::PerformanceReview);
        request.repository = Some("repo-1".to_string());
        request.from_at_ms = Some(1_100);
        request.to_at_ms = Some(1_400);
        execute(reader, &context, request).expect("review query")
    };
    let before = review(&pinned);
    let reader = UsageReader::open(directory.path()).expect("latest reader");
    let after = review(&reader);
    assert_eq!(
        bounded_output(after.clone())
            .expect("small complete report fits")
            .0,
        after
    );
    assert_eq!(
        after["operations"],
        json!([
            {"category":"model_request","count":1,"measuredIntervalSumMs":200,"unknownIntervals":0},
            {"category":"tool","count":1,"measuredIntervalSumMs":300,"unknownIntervals":0},
        ])
    );
    assert_eq!(
        before["tokensByActivity"],
        json!([{"phase":"implementation","activity":"coding","provenance":"agent_declared","measuredTokens":13,"unknownObservations":0}])
    );
    assert_eq!(
        after["tokensByActivity"],
        json!([{"phase":"testing","activity":"unit_testing","provenance":"agent_corrected","measuredTokens":13,"unknownObservations":0}])
    );
    assert_eq!(after["coverage"]["deduplicatedOperations"], json!(2));
    assert_eq!(after["coverage"]["deduplicatedTokenObservations"], json!(1));
    for (action, expected) in [
        (UsageStatsAction::Repositories, 1),
        (UsageStatsAction::Tools, 2),
        (UsageStatsAction::Activities, 3),
    ] {
        let value = execute(&reader, &context, args(action)).expect("dimension query");
        assert_eq!(value["data"].as_array().expect("data").len(), expected);
        assert!(value.get("freshness").is_some());
    }
    let mut request = args(UsageStatsAction::Details);
    request.detail = Some("tokens".to_string());
    let details = execute(&reader, &context, request).expect("token detail");
    assert_eq!(details["data"].as_array().expect("tokens").len(), 1);
    assert_eq!(details["data"][0]["providerTotalTokens"], json!(13));
    publisher
        .publish_batch(codex_usage::JournalReadLimits {
            max_bytes: 1024 * 1024,
            max_entries: 100,
        })
        .expect("publish correction");
    let published = review(&UsageReader::open(directory.path()).expect("reopened"));
    assert_eq!(published["tokensByActivity"], after["tokensByActivity"]);
    assert_eq!(
        review(&pinned)["tokensByActivity"],
        before["tokensByActivity"]
    );
}

#[test]
fn performance_review_facets_keep_all_measurements_within_the_response_bound() {
    let directory = tempdir().expect("temporary directory");
    let root = directory.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    let journal = codex_usage::UsageJournal::new(root.join("events.jsonl"));
    let count = 192_u64;
    let mut facts = Vec::new();
    let mut interval_sum = 0;
    let mut unknown_intervals = 0;
    for index in 0..count {
        let at = 1_000 + index as i64 * 10;
        let duration = index % 7 + 1;
        let tool = format!(
            "tool-{index:03}-{}",
            "t".repeat(1_000 + (index % 5) as usize * 137)
        );
        let activity = format!(
            "activity-{index:03}-{}",
            "a".repeat(200 + (index % 3) as usize * 71)
        );
        let mut started = UsageRecord::new_millis(
            format!("tool-{index}:start"),
            at,
            /*input_tokens*/ 0,
            /*output_tokens*/ 0,
        )
        .with_operation(format!("tool-{index}"))
        .with_thread("thread-1")
        .with_repository("repo-1")
        .with_tool(tool)
        .with_activity(activity, "implementation");
        started.event = Some("tool_started".into());
        started.started_at_ms = Some(at);
        started.classification = Some("agent_declared".into());
        let mut finished = started.clone();
        finished.id = format!("tool-{index}:finish");
        finished.observed_at_ms = Some(at + duration as i64);
        finished.event = Some("tool_finished".into());
        finished.ended_at_ms = finished.observed_at_ms;
        finished.outcome = Some(codex_usage::UsageOutcome::Completed);
        finished.coverage = Some("recorded".into());
        if index % 11 == 0 {
            finished.started_at_ms = None;
            unknown_intervals += 1;
        } else {
            finished.duration_ms = Some(duration);
            interval_sum += duration;
            facts.push(codex_usage::JournalPayload::Record(started));
        }
        facts.push(codex_usage::JournalPayload::Record(finished));
    }
    journal
        .append_batch(facts)
        .expect("capture many distinct zero-token tool groups");
    codex_usage::UsagePublisher::open_at(&root)
        .expect("publisher")
        .publish_batch(codex_usage::JournalReadLimits {
            max_bytes: 16 * 1024 * 1024,
            max_entries: (2 * count) as usize,
        })
        .expect("publish actual lifecycle facts");
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "thread-1".into(),
        cwd: directory.path().to_path_buf(),
    };
    let mut request = args(UsageStatsAction::PerformanceReview);
    request.repository = Some("repo-1".into());
    let report = snapshots::execute_request(&context, request)
        .expect("more than128 operation groups remain reportable");
    let expected = [count, interval_sum, unknown_intervals, count];
    let measurements = |rows: &Value| {
        rows.as_array()
            .expect("facet rows")
            .iter()
            .fold([0_u64; 4], |mut total, row| {
                for (index, key) in [
                    "count",
                    "measuredIntervalSumMs",
                    "unknownIntervals",
                    "mergedGroups",
                ]
                .into_iter()
                .enumerate()
                {
                    total[index] += row[key].as_u64().expect("exact facet measurement");
                }
                total
            })
    };
    for facet in ["tools", "operationsByActivity"] {
        assert_eq!(measurements(&report[facet]), expected);
        assert!(
            report[facet]
                .as_array()
                .expect("facet")
                .iter()
                .any(|row| row["remainder"] == true
                    && row["mergedGroups"].as_u64().expect("merged groups") > 1)
        );
    }
    assert!(serde_json::to_vec(&report).expect("original JSON").len() > MAX_OUTPUT_BYTES);
    let bounded = bounded_output(report.clone())
        .expect("complete bounded response")
        .0;
    assert!(serde_json::to_vec(&bounded).expect("bounded JSON").len() <= MAX_OUTPUT_BYTES);
    assert_eq!(bounded["counts"]["operations"], json!(count));
    assert_eq!(bounded["counts"]["interval_sum_ms"], json!(interval_sum));
    assert_eq!(
        bounded["counts"]["unknown_intervals"],
        json!(unknown_intervals)
    );
    let mut original_totals = report.clone();
    let mut bounded_totals = bounded.clone();
    let mut reduced = false;
    for facet in ["tools", "operationsByActivity"] {
        assert_eq!(measurements(&bounded[facet]), expected);
        let rows = bounded[facet].as_array().expect("bounded facet");
        let remainder = rows
            .iter()
            .find(|row| row["remainder"] == true)
            .expect("explicit complete remainder");
        assert!(remainder["mergedGroups"].as_u64().expect("merged groups") > 1);
        reduced |= rows.len() < report[facet].as_array().expect("original facet").len();
        original_totals
            .as_object_mut()
            .expect("report")
            .remove(facet);
        bounded_totals
            .as_object_mut()
            .expect("report")
            .remove(facet);
    }
    assert!(
        reduced,
        "variable labels require additional coalescing at the final boundary"
    );
    assert_eq!(bounded_totals, original_totals);
    let mut oversized_base = report;
    oversized_base["interpretation"] = json!("x".repeat(MAX_OUTPUT_BYTES));
    assert!(
        bounded_output(oversized_base).is_err(),
        "existing oversized report fields still fail"
    );
}

#[test]
fn task_tree_summary_combines_published_and_recent_descendants() {
    let directory = tempdir().expect("directory");
    let root = directory.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    let journal = codex_usage::UsageJournal::new(root.join("events.jsonl"));
    let mut child =
        UsageRecord::new("child", 11, 20, 3).with_thread("00000000-0000-0000-0000-000000000002");
    child.parent_thread_id = Some("00000000-0000-0000-0000-000000000001".to_string());
    journal
        .append_batch(vec![
            codex_usage::JournalPayload::Record(
                record("parent", 10).with_thread("00000000-0000-0000-0000-000000000001"),
            ),
            codex_usage::JournalPayload::Record(child),
            codex_usage::JournalPayload::Record(
                UsageRecord::new("unrelated", 12, 100, 100).with_thread("unrelated"),
            ),
        ])
        .expect("capture");
    codex_usage::UsagePublisher::open_at(&root)
        .expect("publisher")
        .publish_batch(codex_usage::JournalReadLimits {
            max_bytes: 64 * 1024,
            max_entries: 100,
        })
        .expect("publication");
    let mut recent = UsageRecord::new("grandchild", 13, 30, 3)
        .with_thread("00000000-0000-0000-0000-000000000003");
    recent.parent_thread_id = Some("00000000-0000-0000-0000-000000000002".to_string());
    journal.append_record(recent).expect("recent lineage");
    let reader = UsageReader::open(directory.path()).expect("reader");
    let context = UsageStatsContext {
        codex_home: directory.path().to_path_buf(),
        thread_id: "00000000-0000-0000-0000-000000000001".to_string(),
        cwd: directory.path().to_path_buf(),
    };
    let value = execute(&reader, &context, args(UsageStatsAction::TaskTreeSummary)).expect("tree");
    assert_eq!(
        value["data"],
        json!([{"threadId": "00000000-0000-0000-0000-000000000001", "records": 3, "inputTokens": 60, "outputTokens": 9}])
    );
    assert_eq!(
        value["threadIds"],
        json!([
            "00000000-0000-0000-0000-000000000001",
            "00000000-0000-0000-0000-000000000002",
            "00000000-0000-0000-0000-000000000003"
        ])
    );
    let mut root_only = args(UsageStatsAction::TaskTreeSummary);
    root_only.include_descendants = Some(false);
    let value = execute(&reader, &context, root_only).expect("root only");
    assert_eq!(
        value["data"],
        json!([{"threadId": "00000000-0000-0000-0000-000000000001", "records": 1, "inputTokens": 10, "outputTokens": 3}])
    );
    let mut request = args(UsageStatsAction::TaskTreeSummary);
    request.agent_id = Some("00000000-0000-0000-0000-000000000002".to_string());
    let narrowed = execute(&reader, &context, request).expect("agent in descendant set");
    assert_eq!(narrowed["data"][0]["inputTokens"], json!(20));
    assert_eq!(
        narrowed["threadIds"],
        json!(["00000000-0000-0000-0000-000000000002"])
    );
    assert_eq!(narrowed["counts"]["threads"], json!(1));
    let mut root_only = args(UsageStatsAction::TaskTreeSummary);
    root_only.include_descendants = Some(false);
    root_only.agent_id = Some("00000000-0000-0000-0000-000000000002".to_string());
    let narrowed =
        execute(&reader, &context, root_only).expect("agent cannot widen root-only scope");
    assert_eq!(narrowed["threadIds"], json!([]));
    assert_eq!(narrowed["counts"]["threads"], json!(0));
    assert_eq!(narrowed["data"][0]["records"], json!(0));
    let mut missing_repository = args(UsageStatsAction::TaskTreeSummary);
    missing_repository.agent_id = Some("00000000-0000-0000-0000-000000000002".to_string());
    missing_repository.repository = Some("another-repository".to_string());
    let narrowed = execute(&reader, &context, missing_repository).expect("repository intersection");
    assert_eq!(narrowed["data"][0]["records"], json!(0));
}
