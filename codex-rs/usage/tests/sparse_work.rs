use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use codex_usage::JournalReadLimits;
use codex_usage::TimeRange;
use codex_usage::UsageContextSize;
use codex_usage::UsageCostProvenance;
use codex_usage::UsageFilter;
use codex_usage::UsageOutcome;
use codex_usage::UsagePublisher;
use codex_usage::UsageReader;
use codex_usage::UsageRecord;
use codex_usage::WorkBindingUpdate;
use codex_usage::capture_record;
use codex_usage::capture_records;
use codex_usage::capture_work_binding_transition;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn assignment(sequence: u64, outcome: Option<&str>) -> WorkBindingUpdate {
    WorkBindingUpdate {
        version: 1,
        native_project_id: outcome.map(|_| "project".into()),
        thread_id: "thread".into(),
        workstream_id: outcome.map(|_| "stream".into()),
        outcome_id: outcome.map(str::to_owned),
        transition_sequence: sequence,
        source: "agent_declared".into(),
        authority: "agent_declared".into(),
    }
}

fn start(id: &str, at: i64) -> UsageRecord {
    let mut fact = UsageRecord::new_millis(format!("{id}:start"), at, 0, 0)
        .with_operation(id)
        .with_thread("thread");
    fact.event = Some("model_request_started".into());
    fact.started_at_ms = Some(at);
    fact.coverage = Some("capture_started".into());
    fact
}

#[expect(
    clippy::expect_used,
    reason = "fixture completions require the identity and timestamp supplied by start"
)]
fn finish(mut fact: UsageRecord, at: i64, tokens: u64) -> UsageRecord {
    fact.id = format!(
        "{}:finish",
        fact.operation_id.as_deref().expect("fixture operation ID")
    );
    fact.observed_at_ms = Some(at);
    fact.observed_at = at.div_euclid(1_000);
    fact.ended_at_ms = Some(at);
    fact.duration_ms = Some((at - fact.started_at_ms.expect("fixture start time")) as u64);
    fact.event = Some("model_request_finished".into());
    fact.outcome = Some(UsageOutcome::Completed);
    fact.coverage = Some("recorded".into());
    fact.provider_total_tokens = Some(tokens);
    fact
}

fn operation(home: &Path, id: &str, at: i64, tokens: u64) -> codex_usage::Result<()> {
    let begin = start(id, at);
    capture_records(home, vec![begin.clone(), finish(begin, at + 1, tokens)])?;
    Ok(())
}

#[test]
fn prepared_window_matches_raw_summary_across_corrections_and_scopes() -> codex_usage::Result<()> {
    let home = tempfile::tempdir()?;
    let root = home.path().join("usage-v2");
    let early = start("early", /*at*/ 100).with_repository("repo-a");
    let completed_before_window =
        start("completed-before-window", /*at*/ 100).with_repository("repo-a");
    let mut unknown_interval = completed_before_window.clone();
    unknown_interval.started_at_ms = None;
    let left = UsageRecord::new_millis(
        "left", /*observed_at_ms*/ 1_000, /*input_tokens*/ 10, /*output_tokens*/ 4,
    )
    .with_thread("thread")
    .with_repository("repo-a");
    let mut missing = UsageRecord::new_millis(
        "missing", /*observed_at_ms*/ 1_500, /*input_tokens*/ 3, /*output_tokens*/ 2,
    )
    .with_repository("repo-a");
    missing.provider_total_tokens = None;
    let other = UsageRecord::new_millis(
        "other", /*observed_at_ms*/ 1_500, /*input_tokens*/ 20, /*output_tokens*/ 6,
    )
    .with_thread("other-thread")
    .with_repository("repo-b");
    let right = UsageRecord::new_millis(
        "right", /*observed_at_ms*/ 2_000, /*input_tokens*/ 100,
        /*output_tokens*/ 30,
    )
    .with_thread("thread")
    .with_repository("repo-a");
    capture_records(
        home.path(),
        vec![
            early.clone(),
            finish(early, /*at*/ 200, /*tokens*/ 7),
            left.clone(),
            missing,
            other,
            right,
            unknown_interval,
        ],
    )?;
    UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    let mut duplicate = left;
    duplicate.input_tokens = 999;
    let mut late = UsageRecord::new_millis(
        "late", /*observed_at_ms*/ 1_750, /*input_tokens*/ 11, /*output_tokens*/ 5,
    )
    .with_thread("thread")
    .with_repository("repo-a");
    late.operation_id = Some("early".into());
    capture_records(
        home.path(),
        vec![
            duplicate,
            late,
            finish(completed_before_window, /*at*/ 200, /*tokens*/ 0),
        ],
    )?;
    let mut store = codex_usage::UsageStore::open(home.path())?;
    store.append_correction(codex_usage::UsageCorrection::new(
        "fix-left",
        "left",
        /*observed_at*/ 3,
        /*input_tokens_delta*/ -2,
        /*output_tokens_delta*/ 1,
        "fixture correction",
    ))?;
    let filters = [
        UsageFilter::default(),
        UsageFilter::default().for_repository("repo-a"),
        UsageFilter::default().for_thread("thread"),
        UsageFilter {
            repository_id: Some("repo-a".into()),
            thread_id: Some("thread".into()),
            ..Default::default()
        },
        UsageFilter {
            repository_id: Some("repo-b".into()),
            thread_ids: Some(["thread".to_owned(), "other-thread".to_owned()].into()),
            ..Default::default()
        },
    ];
    for published in [false, true] {
        if published {
            UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
                max_bytes: 1024 * 1024,
                max_entries: 64,
            })?;
        }
        let reader = UsageReader::open(home.path())?;
        for filter in &filters {
            for range in [
                TimeRange::all(),
                TimeRange::from_millis(/*start_ms*/ 1_000, /*end_ms*/ 2_000)?,
                TimeRange::from_millis(/*start_ms*/ 1_000, /*end_ms*/ 1_001)?,
                TimeRange::from_millis(/*start_ms*/ 2_000, /*end_ms*/ 2_001)?,
            ] {
                let window = reader.prepare_window(range, filter)?;
                assert_eq!(window.summary()?, reader.summary(range, filter)?);
            }
        }
        let range = TimeRange::from_millis(/*start_ms*/ 1_000, /*end_ms*/ 2_000)?;
        let filter = UsageFilter::default().for_repository("repo-a");
        let expected = reader.summary(range, &filter)?;
        assert_eq!(
            json!({"input":expected["inputTokens"],"output":expected["outputTokens"],"records":expected["records"]}),
            json!({"input":22,"output":12,"records":3})
        );
        let prepared = reader.prepare_window(range, &filter)?;
        let operations = prepared.operation_page(
            codex_usage::OperationSelection::All,
            /*after*/ None,
            /*limit*/ 50,
        )?;
        assert!(
            operations.data.is_empty(),
            "the tail completion must replace the older unknown interval before time filtering"
        );
        if published {
            let pinned = reader.prepare_window(range, &filter)?;
            capture_record(
                home.path(),
                UsageRecord::new_millis(
                    "after-pin",
                    /*observed_at_ms*/ 1_800,
                    /*input_tokens*/ 50,
                    /*output_tokens*/ 10,
                )
                .with_repository("repo-a"),
            )?;
            assert_eq!(pinned.summary()?, expected);
        }
    }
    Ok(())
}

#[test]
fn unrelated_scope_review_ignores_corrupt_history_and_keeps_scoped_errors()
-> codex_usage::Result<()> {
    let home = tempfile::tempdir()?;
    let root = home.path().join("usage-v2");
    capture_work_binding_transition(home.path(), assignment(/*sequence*/ 1, Some("a")))?;
    operation(home.path(), "known", /*at*/ 1_000, /*tokens*/ 7)?;
    UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    operation(
        home.path(),
        "pending",
        /*at*/ 2_000,
        /*tokens*/ 13,
    )?;
    // Move the first scope's assignment into an immutable historical page.
    // All controls use the real capture API; no runtime history is rewritten.
    for sequence in 2..=257 {
        capture_work_binding_transition(home.path(), assignment(sequence, Some("a")))?;
    }
    let store = root.join("work-binding-store");
    let active: Value = serde_json::from_slice(&std::fs::read(store.join("active.json"))?)?;
    let historical = root.join("work-bindings.jsonl");
    assert_ne!(active["file"], "work-bindings.jsonl");
    std::fs::write(&historical, b"{corrupt-history}\n")?;
    let begin = start("unrelated", /*at*/ 3_000).with_thread("unrelated-thread");
    capture_records(
        home.path(),
        vec![begin.clone(), finish(begin, /*at*/ 3_001, /*tokens*/ 11)],
    )?;
    let reader = UsageReader::open(home.path())?;
    let unrelated = reader.performance_review(
        TimeRange::all(),
        &UsageFilter::default().for_thread("unrelated-thread"),
    )?;
    assert_eq!(
        json!({"coverage":unrelated["workBindings"]["coverage"],
        "metadata":unrelated["workBindings"]["metadataCoverage"],
        "operations":unrelated["counts"]["operations"],
        "tokens":unrelated["outcomes"]["totals"]["providerTotalTokens"]["measured"]}),
        json!({"coverage":"unattributed","metadata":null,"operations":1,"tokens":11})
    );
    // The same damaged page is required by this scope's unassigned tail.
    // Its error remains visible while the earlier canonical assignment survives.
    let requested = reader.performance_review(
        TimeRange::all(),
        &UsageFilter::default().for_thread("thread"),
    )?;
    assert_eq!(
        json!({"coverage":requested["workBindings"]["coverage"],
        "metadata":requested["workBindings"]["metadataCoverage"],
        "known":requested["outcomes"]["attributed"]["providerTotalTokens"]["measured"],
        "unknown":requested["outcomes"]["unattributed"]["providerTotalTokens"]["measured"]}),
        json!({"coverage":"partial","metadata":"unavailable","known":7,"unknown":13})
    );
    std::fs::write(store.join("active.json"), b"{corrupt-active}\n")?;
    let unavailable = UsageReader::open(home.path())?.performance_review(
        TimeRange::all(),
        &UsageFilter::default().for_thread("unrelated-thread"),
    )?;
    assert_eq!(
        json!({"coverage":unavailable["workBindings"]["coverage"],
        "metadata":unavailable["workBindings"]["metadataCoverage"],
        "tokens":unavailable["outcomes"]["totals"]["providerTotalTokens"]["measured"]}),
        json!({"coverage":"unavailable","metadata":"unavailable","tokens":11})
    );
    Ok(())
}

#[test]
fn finite_raw_totals_prune_history_and_preserve_exact_row_filters() -> codex_usage::Result<()> {
    let home = tempfile::tempdir()?;
    let root = home.path().join("usage-v2");
    const HOUR: i64 = 3_600_000;
    let history = (0..6)
        .map(|hour| {
            UsageRecord::new_millis(
                format!("old-{hour}"),
                hour * HOUR + 1,
                /*input_tokens*/ 1,
                /*output_tokens*/ 0,
            )
            .with_repository("repo-a")
        })
        .collect();
    capture_records(home.path(), history)?;
    UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    let legacy = UsageRecord::new(
        "legacy",
        8 * 3_600 + 1,
        /*input_tokens*/ 3,
        /*output_tokens*/ 1,
    )
    .with_thread("thread")
    .with_repository("repo-a");
    let records = vec![
        legacy,
        UsageRecord::new_millis(
            "before",
            8 * HOUR + 999,
            /*input_tokens*/ 99,
            /*output_tokens*/ 99,
        )
        .with_thread("thread")
        .with_repository("repo-a"),
        UsageRecord::new_millis(
            "exact",
            8 * HOUR + 1_999,
            /*input_tokens*/ 10,
            /*output_tokens*/ 2,
        )
        .with_thread("thread")
        .with_repository("repo-a"),
        UsageRecord::new_millis(
            "after",
            8 * HOUR + 2_000,
            /*input_tokens*/ 100,
            /*output_tokens*/ 100,
        )
        .with_thread("thread")
        .with_repository("repo-a"),
        UsageRecord::new_millis(
            "other-repo",
            8 * HOUR + 1_500,
            /*input_tokens*/ 50,
            /*output_tokens*/ 3,
        )
        .with_thread("thread")
        .with_repository("repo-b"),
        UsageRecord::new_millis(
            "other-thread",
            8 * HOUR + 1_500,
            /*input_tokens*/ 60,
            /*output_tokens*/ 4,
        )
        .with_thread("other")
        .with_repository("repo-a"),
    ];
    capture_records(home.path(), records)?;
    UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    let mut store = codex_usage::UsageStore::open(home.path())?;
    store.append_correction(codex_usage::UsageCorrection::new(
        "old-fix",
        "old-0",
        9 * 3_600,
        /*input_tokens_delta*/ 7,
        /*output_tokens_delta*/ 0,
        "old observation",
    ))?;
    store.append_correction(codex_usage::UsageCorrection::new(
        "fix",
        "legacy",
        9 * 3_600,
        /*input_tokens_delta*/ 2,
        /*output_tokens_delta*/ -1,
        "outside observation window",
    ))?;
    UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    let reader = UsageReader::open(home.path())?;
    assert!(reader.manifest().membership_files.len() > 1);
    let range = TimeRange::from_millis(8 * HOUR + 1_000, 8 * HOUR + 2_000)?;
    let filter = UsageFilter {
        repository_id: Some("repo-a".into()),
        thread_id: Some("thread".into()),
        ..Default::default()
    };
    let totals = reader.totals(range, &filter)?;
    assert_eq!(
        totals,
        codex_usage::UsageTotals {
            input_tokens: 15,
            output_tokens: 2,
            records: 2,
            ..Default::default()
        }
    );
    assert_eq!(
        reader.prepare_window(range, &filter)?.summary()?,
        reader.summary(range, &filter)?
    );
    Ok(())
}

fn totals(report: &Value) -> Value {
    json!({"bound":report["workBindings"]["boundOperations"],
        "unknown":report["workBindings"]["unknownOperations"],
        "tokens":report["outcomes"]["totals"]["providerTotalTokens"]["measured"],
        "attributed":report["outcomes"]["attributed"]["providerTotalTokens"]["measured"],
        "unattributed":report["outcomes"]["unattributed"]["providerTotalTokens"]["measured"],
        "intervals":report["outcomes"]["attributed"]["elapsedExecutionMs"]})
}

fn measure_reports(
    home: &Path,
    baseline: &Path,
    expected: u64,
    state: &str,
) -> codex_usage::Result<Value> {
    let started = Instant::now();
    let reader = UsageReader::open(home)?;
    let reader_time = started.elapsed();
    let baseline_reader = UsageReader::open(baseline)?;
    let started = Instant::now();
    let unbound = baseline_reader.performance_review(TimeRange::all(), &UsageFilter::default())?;
    let baseline_time = started.elapsed();
    eprintln!(
        "sparse-work-phase {}",
        json!({"state":state,"phase":"baselineReport","ms":baseline_time.as_millis()})
    );
    let started = Instant::now();
    let _ = reader.work_binding_transitions();
    let binding_decode_time = started.elapsed();
    let started = Instant::now();
    let sparse = reader.bound_work_effort(
        TimeRange::all(),
        &UsageFilter::default(),
        &Default::default(),
    )?;
    let sparse_time = started.elapsed();
    eprintln!(
        "sparse-work-phase {}",
        json!({"state":state,"phase":"sparseEffort","ms":sparse_time.as_millis()})
    );
    let started = Instant::now();
    let report = reader.performance_review(TimeRange::all(), &UsageFilter::default())?;
    let report_time = started.elapsed();
    eprintln!(
        "sparse-work-phase {}",
        json!({"state":state,"phase":"boundReport","ms":report_time.as_millis()})
    );
    assert_eq!(
        report["tokens"], unbound["tokens"],
        "identical facts retain identical token totals"
    );
    assert_eq!(report["counts"], unbound["counts"]);
    assert_eq!(
        json!({"bound":report["workBindings"]["boundOperations"],"rows":report["outcomes"]["totalRows"],
        "tokens":report["outcomes"]["attributed"]["providerTotalTokens"]["measured"]}),
        json!({"bound":expected,"rows":1,"tokens":expected})
    );
    assert_eq!(
        sparse.iter().map(|row| row.effort.operations).sum::<u64>(),
        expected
    );
    Ok(
        json!({"readerOpenMs":reader_time.as_millis(),"baselineReportMs":baseline_time.as_millis(),
        "bindingDecodeMs":binding_decode_time.as_millis(),"sparseEffortMs":sparse_time.as_millis(),
        "boundReportMs":report_time.as_millis()}),
    )
}

#[test]
fn captured_starts_pin_assignments_across_switches_publication_and_late_retries()
-> codex_usage::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("new-home");
    let a = capture_work_binding_transition(&home, assignment(1, Some("a")))?;
    assert_eq!(a.effective_capture_sequence, 1);
    let begin = start("a-operation", 1_000);
    capture_record(&home, begin.clone())?;
    UsagePublisher::open_at(home.join("usage-v2"))?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    let pinned = UsageReader::open(&home)?;
    capture_work_binding_transition(&home, assignment(2, Some("b")))?;
    capture_record(&home, finish(begin, 2_000, 10))?;
    operation(&home, "b-operation", 3_000, 20)?;
    let mut retry = start("late-retry", 3_100);
    retry.retry_of_operation_id = Some("a-operation".into());
    capture_records(&home, vec![retry.clone(), finish(retry, 3_101, 5)])?;
    // A terminal with an old start timestamp is not an observed start fact.
    capture_record(&home, finish(start("terminal-only", 3_200), 3_201, 30))?;
    capture_work_binding_transition(&home, assignment(3, None))?;
    operation(&home, "cleared", 3_500, 40)?;
    assert_eq!(
        pinned.work_binding_transitions().len(),
        1,
        "reader must pin the lazy transition prefix"
    );
    let report =
        UsageReader::open(&home)?.performance_review(TimeRange::all(), &UsageFilter::default())?;
    assert_eq!(
        totals(&report),
        json!({"bound":3,"unknown":2,"tokens":105,"attributed":35,"unattributed":70,
        "intervals":{"measured":1_002,"exact":1_002,"unknown":0}})
    );
    let rows = report["outcomes"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["outcomeId"].as_str().unwrap(),
                row["effort"]["providerTotalTokens"]["measured"]
                    .as_u64()
                    .unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(rows, BTreeMap::from([("a", 15), ("b", 20)]));
    // A fresh unavailable binding source must not disable unrelated measurements.
    std::fs::write(home.join("usage-v2/work-bindings.jsonl"), b"{broken}\n")?;
    let reader = UsageReader::open(&home)?;
    assert_eq!(
        reader.summary(TimeRange::all(), &UsageFilter::default())?["counts"]["operations"],
        5
    );
    let unavailable = reader.performance_review(TimeRange::all(), &UsageFilter::default())?;
    assert_eq!(
        json!({"coverage":unavailable["workBindings"]["coverage"],"tokens":unavailable["outcomes"]["totals"]["providerTotalTokens"]["measured"],
            "historical":unavailable["outcomes"]["attributed"]["providerTotalTokens"]["measured"]}),
        json!({"coverage":"partial","tokens":105,"historical":15})
    );
    // Concurrent jobs share elapsed time, and unattributed work can overlap
    // attributed work. Every selected set must compute its own union.
    let concurrent = directory.path().join("concurrent");
    for (thread, outcome, stream) in [("first", "a", "alpha"), ("second", "b", "beta")] {
        let mut binding = assignment(/*sequence*/ 1, Some(outcome));
        binding.thread_id = thread.into();
        binding.workstream_id = Some(stream.into());
        capture_work_binding_transition(&concurrent, binding)?;
    }
    let mut records = Vec::new();
    for (id, thread, agent, begin, end) in [
        ("overlap-a", "first", "agent-a", 1_000, 2_000),
        ("overlap-b", "second", "agent-b", 1_500, 2_500),
        ("unbound-overlap", "unbound", "agent-b", 1_800, 2_200),
    ] {
        let mut begin = start(id, begin).with_thread(thread);
        begin.agent_id = Some(agent.into());
        records.extend([begin.clone(), finish(begin, end, /*tokens*/ 1)]);
    }
    for (id, thread, begin, end) in [
        ("wait-a1", "first", 1_200, 1_700),
        ("wait-a2", "first", 1_300, 1_800),
        ("wait-b", "second", 1_400, 1_900),
        ("wait-unbound", "unbound", 1_600, 2_000),
    ] {
        let mut wait = UsageRecord::new_millis(
            format!("{id}:start"),
            begin,
            /*input_tokens*/ 0,
            /*output_tokens*/ 0,
        )
        .with_operation(id)
        .with_thread(thread);
        wait.event = Some("retry_wait_started".into());
        wait.started_at_ms = Some(begin);
        let mut ended = wait.clone();
        ended.id = format!("{id}:finish");
        ended.event = Some("retry_wait_finished".into());
        ended.observed_at_ms = Some(end);
        ended.observed_at = end.div_euclid(1_000);
        ended.ended_at_ms = Some(end);
        ended.duration_ms = Some((end - begin) as u64);
        ended.outcome = Some(UsageOutcome::Completed);
        records.extend([wait, ended]);
    }
    capture_records(&concurrent, records)?;
    let reader = UsageReader::open(&concurrent)?;
    let filter = UsageFilter::default();
    let window = reader.prepare_window(TimeRange::from_millis(1_100, 2_300)?, &filter)?;
    let report = window.performance_review()?;
    let measured = |effort: &Value| {
        json!([
            effort["elapsedExecutionMs"],
            effort["activeAgentMs"],
            effort["recordedWaitMs"]
        ])
    };
    let exact = |amount| json!({"measured":amount,"exact":amount,"unknown":0});
    assert_eq!(
        json!({"total":measured(&report["outcomes"]["totals"]),
            "attributed":measured(&report["outcomes"]["attributed"]),
            "unattributed":measured(&report["outcomes"]["unattributed"]),
            "rows":report["outcomes"]["rows"].as_array().unwrap().iter().map(|row|
                (row["outcomeId"].as_str().unwrap(),measured(&row["effort"]))).collect::<BTreeMap<_,_>>()}),
        json!({"total":[exact(1_200),exact(1_700),exact(800)],
            "attributed":[exact(1_200),exact(1_700),exact(700)],
            "unattributed":[exact(400),exact(400),exact(400)],
            "rows":{"a":[exact(900),exact(900),exact(600)],"b":[exact(800),exact(800),exact(500)]}})
    );
    let mut scoped = json!({"totals":{},"attributed":{},"unattributed":{},"rows":[{
        "nativeProjectId":"project","workstreamId":"alpha","outcomeId":"a","effort":{}}]});
    let overrides = BTreeMap::from([(
        "overlap-b".into(),
        codex_usage::OutcomeTimingContext {
            native_project_id: Some("project".into()),
            workstream_id: Some("alpha".into()),
            outcome_id: Some("a".into()),
        },
    )]);
    window
        .outcome_timings(&overrides, Some("alpha"))?
        .apply(&mut scoped);
    assert_eq!(
        json!({"total":measured(&scoped["totals"]),"attributed":measured(&scoped["attributed"]),
            "unattributed":measured(&scoped["unattributed"]),"row":measured(&scoped["rows"][0]["effort"])}),
        json!({"total":[exact(1_200),exact(1_700),exact(800)],
            "attributed":[exact(1_200),exact(1_700),exact(600)],
            "unattributed":[exact(400),exact(400),exact(400)],"row":[exact(1_200),exact(1_700),exact(600)]})
    );
    let mut default = report["outcomes"].clone();
    window
        .outcome_timings(&BTreeMap::new(), /*workstream*/ None)?
        .apply(&mut default);
    assert_eq!(default, report["outcomes"]);
    let mut late = start("late-concurrent", /*at*/ 1_200).with_thread("late-thread");
    late.agent_id = Some("late-agent".into());
    capture_record(&concurrent, finish(late, /*at*/ 2_200, /*tokens*/ 1))?;
    UsagePublisher::open_at(concurrent.join("usage-v2"))?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    let fresh = UsageReader::open(&concurrent)?
        .performance_review(TimeRange::from_millis(1_100, 2_300)?, &filter)?;
    assert_eq!(fresh["outcomes"]["totals"]["activeAgentMs"], exact(2_700));
    window
        .outcome_timings(&BTreeMap::new(), /*workstream*/ None)?
        .apply(&mut default);
    assert_eq!(
        default, report["outcomes"],
        "the old query keeps its pinned prefix"
    );
    Ok(())
}

fn switch_fixture(switches: u64) -> codex_usage::Result<(tempfile::TempDir, Duration, Duration)> {
    let home = tempfile::tempdir()?;
    let mut transition_time = Duration::ZERO;
    let mut capture_time = Duration::ZERO;
    for sequence in 1..=switches {
        let transition_start = Instant::now();
        capture_work_binding_transition(home.path(), assignment(sequence, Some("same-job")))?;
        transition_time += transition_start.elapsed();
        let capture_start = Instant::now();
        operation(
            home.path(),
            &format!("operation-{sequence}"),
            sequence as i64,
            1,
        )?;
        capture_time += capture_start.elapsed();
    }
    eprintln!(
        "sparse-work-phase {}",
        json!({"phase":"captureComplete","switches":switches,
        "transitionMs":transition_time.as_millis(),"captureMs":capture_time.as_millis()})
    );
    Ok((home, transition_time, capture_time))
}

#[test]
fn repeated_switches_aggregate_by_job_before_report_limits() -> codex_usage::Result<()> {
    let switches = 1_025;
    let (home, transition_time, capture_time) = switch_fixture(switches)?;
    let started = Instant::now();
    let reader = UsageReader::open(home.path())?;
    let report = reader.performance_review(TimeRange::all(), &UsageFilter::default())?;
    assert_eq!(
        json!({"bound":report["workBindings"]["boundOperations"],"rows":report["outcomes"]["totalRows"],
        "tokens":report["outcomes"]["attributed"]["providerTotalTokens"]["measured"]}),
        json!({"bound":switches,"rows":1,"tokens":switches})
    );
    eprintln!(
        "sparse-work-measurement {}",
        json!({"switches":switches,"operations":switches,
        "transitionMs":transition_time.as_millis(),"captureMs":capture_time.as_millis(),"reportMs":started.elapsed().as_millis()})
    );
    Ok(())
}

#[test]
#[ignore = "comparative phase diagnostics; run with the explicit benchmark profile"]
fn sparse_report_phase_measurement() -> codex_usage::Result<()> {
    let switches = 1_025;
    let (home, transition_time, capture_time) = switch_fixture(switches)?;
    let baseline = tempfile::tempdir()?;
    std::fs::create_dir_all(baseline.path().join("usage-v2"))?;
    std::fs::copy(
        home.path().join("usage-v2/events.jsonl"),
        baseline.path().join("usage-v2/events.jsonl"),
    )?;
    let tail = measure_reports(home.path(), baseline.path(), switches, "unpublished")?;
    for path in [home.path(), baseline.path()] {
        UsagePublisher::open_at(path.join("usage-v2"))?.publish_batch(JournalReadLimits {
            max_bytes: 16 * 1024 * 1024,
            max_entries: 16_384,
        })?;
    }
    let published = measure_reports(home.path(), baseline.path(), switches, "published")?;
    assert_eq!(
        UsageReader::open(home.path())?
            .work_binding_transitions()
            .len(),
        switches as usize
    );
    eprintln!(
        "sparse-work-measurement {}",
        json!({"switches":switches,"operations":switches,
        "transitionMs":transition_time.as_millis(),"captureMs":capture_time.as_millis(),
        "unpublished":tail,"published":published,
        "transitionBytes":std::fs::metadata(home.path().join("usage-v2/work-bindings.jsonl"))?.len()})
    );
    Ok(())
}

#[test]
fn replacement_with_identical_size_and_mtime_never_reuses_another_file() -> codex_usage::Result<()>
{
    let home = tempfile::tempdir()?;
    let original = capture_work_binding_transition(home.path(), assignment(1, Some("a")))?;
    let reader = UsageReader::open(home.path())?;
    assert_eq!(
        reader.work_binding_transitions(),
        std::slice::from_ref(&original)
    );
    let path = home.path().join("usage-v2/work-bindings.jsonl");
    let metadata = std::fs::metadata(&path)?;
    let mut replaced = original.clone();
    replaced.update.outcome_id = Some("b".into());
    let mut bytes = serde_json::to_vec(&replaced)?;
    bytes.push(b'\n');
    assert_eq!(bytes.len() as u64, metadata.len());
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, bytes)?;
    std::fs::File::options()
        .write(true)
        .open(&replacement)?
        .set_times(std::fs::FileTimes::new().set_modified(metadata.modified()?))?;
    std::fs::rename(replacement, &path)?;
    assert_eq!(std::fs::metadata(&path)?.modified()?, metadata.modified()?);
    let fresh = UsageReader::open(home.path())?;
    assert_eq!(reader.work_binding_transitions(), &[original]);
    assert_eq!(fresh.work_binding_transitions(), &[replaced]);
    Ok(())
}

#[test]
fn cross_partition_retry_keeps_the_original_assignment() -> codex_usage::Result<()> {
    let home = tempfile::tempdir()?;
    capture_work_binding_transition(home.path(), assignment(/*sequence*/ 1, Some("a")))?;
    operation(home.path(), "root", /*at*/ 1000, /*tokens*/ 10)?;
    let fillers = (0..16_382)
        .map(|index| {
            UsageRecord::new_millis(
                format!("filler-{index}"),
                /*observed_at_ms*/ 1000,
                /*input_tokens*/ 0,
                /*output_tokens*/ 0,
            )
        })
        .collect();
    capture_records(home.path(), fillers)?;
    capture_work_binding_transition(home.path(), assignment(/*sequence*/ 2, Some("b")))?;
    let mut retry = start("retry", /*at*/ 2000);
    retry.retry_of_operation_id = Some("root".into());
    capture_records(
        home.path(),
        vec![retry.clone(), finish(retry, /*at*/ 2001, /*tokens*/ 20)],
    )?;
    let root = home.path().join("usage-v2");
    let end = codex_usage::UsageJournal::new(root.join("events.jsonl")).last_sequence()?;
    let manifest = UsagePublisher::open_at(&root)?.publish_batch(JournalReadLimits {
        max_bytes: 64 * 1024 * 1024,
        max_entries: 20_000,
    })?;
    assert_eq!(
        manifest.watermark, end,
        "one publication must consume both partitions"
    );
    let descriptor: Value = serde_json::from_slice(&std::fs::read(
        root.join("segments")
            .join(manifest.review_projection_file.unwrap()),
    )?)?;
    assert_eq!(descriptor["partitions"].as_array().unwrap().len(), 2);
    let report = UsageReader::open(home.path())?
        .performance_review(TimeRange::all(), &UsageFilter::default())?;
    assert_eq!(
        json!({"bound":report["workBindings"]["boundOperations"],"rows":report["outcomes"]["rows"]}),
        json!({"bound":2,"rows":[{"nativeProjectId":"project","outcomeId":"a","workstreamId":"stream","title":null,
            "effort":{"operations":2,"retryOperations":1,"reworkOperations":0,
                "providerTotalTokens":{"measured":30,"exact":null,"unknown":0},
                "activeAgentMs":{"measured":2,"exact":2,"unknown":0},
                "elapsedExecutionMs":{"measured":2,"exact":2,"unknown":0},
                "recordedWaitMs":{"measured":0,"exact":null,"unknown":0}}}]})
    );
    Ok(())
}

#[test]
fn exceptional_producer_binds_exact_token_identity_without_a_lifecycle() -> codex_usage::Result<()>
{
    let home = tempfile::tempdir()?;
    let mut token = UsageRecord::new_millis(
        "external-token",
        /*observed_at_ms*/ 1000,
        /*input_tokens*/ 7,
        /*output_tokens*/ 0,
    )
    .with_operation("external-operation")
    .with_thread("thread");
    token.event = Some("token_observation".into());
    token.provider_total_tokens = Some(7);
    capture_record(home.path(), token)?;
    let reader = UsageReader::open(home.path())?;
    let evidence = reader.bound_operation_effort(
        TimeRange::all(),
        &UsageFilter::default(),
        &std::collections::BTreeSet::from(["external-operation".to_owned()]),
    )?;
    assert_eq!(
        serde_json::to_value(evidence)?,
        json!([{"operation_id":"external-operation","operations":0,
        "retry_operations":0,"rework_operations":0,"provider_total_tokens":7,"token_observations":1,
        "unknown_token_observations":0,"execution_interval_sum_ms":0,"unknown_execution_intervals":0,"recorded_wait_ms":0}])
    );
    Ok(())
}

#[test]
fn clear_and_conflicting_revisions_have_explicit_unattributed_coverage() -> codex_usage::Result<()>
{
    let home = tempfile::tempdir()?;
    let first = capture_work_binding_transition(home.path(), assignment(1, Some("a")))?;
    assert_eq!(
        capture_work_binding_transition(home.path(), first.update.clone())?,
        first
    );
    assert!(capture_work_binding_transition(home.path(), assignment(1, Some("conflict"))).is_err());
    capture_work_binding_transition(home.path(), assignment(2, None))?;
    operation(home.path(), "cleared", 1_000, 7)?;
    let report = UsageReader::open(home.path())?
        .performance_review(TimeRange::all(), &UsageFilter::default())?;
    assert_eq!(
        json!({"basis":report["workBindings"]["basis"],"bound":report["workBindings"]["boundOperations"],
        "coverage":report["outcomes"]["coverage"],"tokens":report["outcomes"]["unattributed"]["providerTotalTokens"]["measured"]}),
        json!({"basis":"explicit_sparse_transitions","bound":0,"coverage":"unattributed","tokens":7})
    );
    Ok(())
}

#[test]
fn report_preserves_context_cost_provenance_and_union_intervals() -> codex_usage::Result<()> {
    let started = Instant::now();
    let mut previous = started;
    let mut mark = |stage: &str| {
        let now = Instant::now();
        eprintln!(
            "usage-metric-phase {}",
            json!({"stage":stage,"phaseMs":now.duration_since(previous).as_millis(),
                "totalMs":now.duration_since(started).as_millis()})
        );
        previous = now;
    };
    mark("fixture_started");
    let home = tempfile::tempdir()?;
    let mut first = start("first", 1_000);
    first.agent_id = Some("agent".into());
    let mut first_finished = finish(first, 2_000, 20);
    first_finished.agent_id = Some("agent".into());
    first_finished.context_size = Some(UsageContextSize {
        prompt_tokens: Some(10),
        context_tokens: Some(12),
        cache_tokens: Some(2),
        compaction_count: Some(1),
        context_window_tokens: Some(16),
        window_class: Some("short".into()),
    });
    first_finished.cost_provenance = Some(UsageCostProvenance {
        model: Some("gpt-6.1-sol".into()),
        provider: Some("openai".into()),
        service_tier: Some("standard".into()),
        context_class: Some("short".into()),
        region: Some("us".into()),
        rate_card: Some("openai-standard".into()),
        rate_card_version: Some("2026-11-15".into()),
        pricing_basis: Some("api_equivalent".into()),
        estimated_api_equivalent_cost_micros: Some(7),
    });
    first_finished.actual_billing_micros = Some(11);
    let mut second = start("second", 1_500);
    second.agent_id = Some("agent".into());
    let mut second_finished = finish(second, 2_500, 20);
    second_finished.agent_id = Some("agent".into());
    second_finished.context_size = first_finished.context_size.clone();
    second_finished.cost_provenance = first_finished.cost_provenance.clone();
    second_finished.actual_billing_micros = Some(13);
    let mut wait = UsageRecord::new_millis("wait:start", 1_200, 0, 0).with_operation("wait");
    wait.event = Some("retry_wait_started".into());
    wait.started_at_ms = Some(1_200);
    let mut wait_finished = wait.clone();
    wait_finished.id = "wait:finish".into();
    wait_finished.event = Some("retry_wait_finished".into());
    wait_finished.observed_at_ms = Some(1_700);
    wait_finished.ended_at_ms = Some(1_700);
    wait_finished.duration_ms = Some(500);
    wait_finished.outcome = Some(UsageOutcome::Completed);
    let metric_template = first_finished.clone();
    mark("initial_capture_started");
    capture_records(
        home.path(),
        vec![first_finished, second_finished, wait, wait_finished],
    )?;
    mark("initial_capture_finished");
    mark("initial_review_started");
    let report = UsageReader::open(home.path())?
        .performance_review(TimeRange::from_millis(0, 3_000)?, &UsageFilter::default())?;
    mark("initial_review_finished");
    assert_eq!(report["timing"]["elapsedMs"]["measured"], 1_500);
    assert_eq!(report["timing"]["wallTimeMs"]["measured"], 1_500);
    assert_eq!(report["timing"]["activeMs"]["measured"], 1_500);
    assert_eq!(report["timing"]["waitMs"]["measured"], 500);
    assert_eq!(report["lifecycle"]["firstStartedAtMs"], 1_000);
    assert_eq!(report["lifecycle"]["lastEndedAtMs"], 2_500);
    assert_eq!(report["contextSize"]["promptTokens"]["measured"], 20);
    assert_eq!(
        report["cost"]["estimatedApiEquivalentCostMicros"]["measured"],
        14
    );
    assert_eq!(report["cost"]["status"], "partial");
    assert_eq!(report["cost"]["basis"], "api_equivalent");
    assert_eq!(report["cost"]["currency"], "USD");
    assert_eq!(report["cost"]["pricedObservations"], 2);
    assert_eq!(report["cost"]["actualBillingMicros"]["measured"], 24);
    assert_eq!(
        report["cost"]["unavailableReasons"],
        json!({"component_cost_breakdown_unavailable": 2})
    );
    // Exceed the old raw-row limit with real operations and observations. The
    // detail groups also exceed their separate cap without truncating totals.
    let additional = 16_385_u64;
    mark("scale_fixture_build_started");
    // Owners repeat every two facts, detail every 33, and intervals every three.
    // Clone the 66 complete variants instead of allocating replacements twice.
    let variants = (0..66)
        .map(|index| {
            let mut record = metric_template.clone();
            record.id.clear();
            record.operation_id = None;
            record.agent_id = Some(
                if index % 2 == 0 {
                    "agent"
                } else {
                    "other-agent"
                }
                .into(),
            );
            if index % 3 != 2 {
                let interval_start = if index % 3 == 0 { 1_100 } else { 1_300 };
                record.started_at_ms = Some(interval_start);
                record.ended_at_ms = Some(interval_start + 100);
                record.observed_at_ms = record.ended_at_ms;
                record.observed_at = (interval_start + 100).div_euclid(1_000);
                record.duration_ms = Some(100);
            }
            record.input_tokens = 3;
            record.cached_input_tokens = 2;
            record.cache_write_input_tokens = Some(1);
            record.output_tokens = 4;
            record.reasoning_output_tokens = 1;
            record.context_size.as_mut().unwrap().window_class =
                Some(format!("window-{:02}", index % 33));
            record.cost_provenance.as_mut().unwrap().rate_card =
                Some(format!("metric-{:02}", index % 33));
            record
        })
        .collect::<Vec<_>>();
    let mut records = Vec::with_capacity(additional as usize + 5);
    for index in 0..additional {
        let mut record = variants[index as usize % variants.len()].clone();
        record.id = format!("metric-{index}:finish");
        record.operation_id = Some(format!("metric-{index}"));
        records.push(record);
    }
    records.push(start("unfinished-metric", /*at*/ 1_300));
    let mut missing_start = finish(
        start("missing-start", /*at*/ 1_400),
        /*at*/ 1_600,
        /*tokens*/ 2,
    );
    missing_start.started_at_ms = None;
    missing_start.duration_ms = None;
    records.push(missing_start);
    let mut missing_owner = metric_template;
    missing_owner.id = "missing-owner:finish".into();
    missing_owner.operation_id = Some("missing-owner".into());
    missing_owner.agent_id = None;
    missing_owner.thread_id = None;
    missing_owner.context_size = None;
    missing_owner.cost_provenance = None;
    missing_owner.actual_billing_micros = None;
    records.push(missing_owner);
    let mut unfinished_wait = UsageRecord::new_millis(
        "unfinished-wait:start",
        /*observed_at_ms*/ 1_400,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation("unfinished-wait");
    unfinished_wait.event = Some("retry_wait_started".into());
    unfinished_wait.started_at_ms = Some(1_400);
    records.push(unfinished_wait);
    let mut provider_only = UsageRecord::new_millis(
        "provider-only",
        /*observed_at_ms*/ 1_800,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    );
    provider_only.provider_total_tokens = Some(5);
    provider_only.actual_billing_micros = Some(17);
    records.push(provider_only);
    mark("scale_fixture_build_finished");
    mark("scale_capture_started");
    capture_records(home.path(), records)?;
    mark("scale_capture_finished");
    mark("scale_publication_started");
    UsagePublisher::open_at(home.path().join("usage-v2"))?.publish_batch(JournalReadLimits {
        max_bytes: 64 * 1024 * 1024,
        max_entries: 20_000,
    })?;
    mark("scale_publication_finished");
    mark("published_reader_open_started");
    let published_reader = UsageReader::open(home.path())?;
    mark("published_reader_open_finished");
    mark("published_review_started");
    let published = published_reader
        .performance_review(TimeRange::from_millis(0, 3_000)?, &UsageFilter::default())?;
    mark("published_review_finished");
    assert_eq!(
        json!({"active":published["timing"]["activeMs"],
            "elapsed":published["timing"]["elapsedMs"],"wait":published["timing"]["waitMs"],
            "unknown":published["timing"]["unknownReasons"],
            "first":published["lifecycle"]["firstStartedAtMs"],
            "last":published["lifecycle"]["lastEndedAtMs"],
            "operations":published["counts"]["operations"],
            "observations":published["coverage"]["rawTokenObservations"]}),
        json!({"active":{"measured":2_500,"exact":null,"unknown":3},
            "elapsed":{"measured":1_500,"exact":null,"unknown":3},
            "wait":{"measured":500,"exact":null,"unknown":1},
            "unknown":{"model_request:missing_end":1,"model_request:missing_start":1,
                "model_request:missing_active_owner":1,"wait:missing_end":1},
            "first":1_000,"last":2_500,"operations":additional+5,"observations":additional+5})
    );
    assert_eq!(
        json!({"context":published["contextSize"],"cost":{
            "estimate":published["cost"]["estimatedApiEquivalentCostMicros"],
            "actual":published["cost"]["actualBillingMicros"],
            "priced":published["cost"]["pricedObservations"],
            "input":published["cost"]["inputTokens"],"cached":published["cost"]["cachedInputTokens"],
            "write":published["cost"]["cacheWriteTokens"],"output":published["cost"]["outputTokens"],
            "reasoning":published["cost"]["reasoningTokens"],
            "provider":published["cost"]["providerTotalTokens"]}}),
        json!({"context":{"coverage":"partial","observations":additional+5,
            "promptTokens":{"measured":10*(additional+2),"exact":null,"unknown":3},
            "contextTokens":{"measured":12*(additional+2),"exact":null,"unknown":3},
            "contextWindowTokens":{"measured":16*(additional+2),"exact":null,"unknown":3},
            "cacheTokens":{"measured":2*(additional+2),"exact":null,"unknown":3},
            "compactionCount":{"measured":additional+2,"exact":null,"unknown":3},
            "windowClasses":std::iter::once(json!({"class":"short","observations":2}))
                .chain((0..31).map(|index| json!({"class":format!("window-{index:02}"),
                    "observations":if index < 17 { 497 } else { 496 }}))).collect::<Vec<_>>(),
            "windowClassesOmitted":2,"windowClassObservationsOmitted":992,"unknownWindowClass":3},
            "cost":{"estimate":{"measured":7*(additional+2),"exact":null,"unknown":3},
                "actual":{"measured":11*additional+41,"exact":null,"unknown":2},
                "priced":additional+2,"input":3*additional,"cached":2*additional,
                "write":additional,"output":4*additional,"reasoning":additional,
                "provider":20*additional+67}})
    );
    assert_eq!(
        json!({"provenanceRows":published["cost"]["provenance"].as_array().unwrap().len(),
            "provenanceOmitted":published["cost"]["provenanceOmitted"],
            "provenanceObservationsOmitted":published["cost"]["provenanceObservationsOmitted"],
            "rateCardRows":published["cost"]["rateCardRefs"].as_array().unwrap().len(),
            "rateCardsOmitted":published["cost"]["rateCardRefsOmitted"],
            "rateCardObservationsOmitted":published["cost"]["rateCardObservationsOmitted"]}),
        json!({"provenanceRows":32,"provenanceOmitted":2,"provenanceObservationsOmitted":498,
            "rateCardRows":32,"rateCardsOmitted":2,"rateCardObservationsOmitted":498})
    );
    mark("clipped_review_started");
    let clipped = published_reader.performance_review(
        TimeRange::from_millis(1_200, 2_200)?,
        &UsageFilter::default(),
    )?;
    mark("clipped_review_finished");
    assert_eq!(
        json!({"active":clipped["timing"]["activeMs"]["measured"],
            "elapsed":clipped["timing"]["elapsedMs"]["measured"],
            "wait":clipped["timing"]["waitMs"]["measured"]}),
        json!({"active":1_800,"elapsed":1_000,"wait":500})
    );
    mark("reopened_reader_open_started");
    let reopened_reader = UsageReader::open(home.path())?;
    mark("reopened_reader_open_finished");
    mark("reopened_review_started");
    let reopened = reopened_reader
        .performance_review(TimeRange::from_millis(0, 3_000)?, &UsageFilter::default())?;
    mark("reopened_review_finished");
    assert_eq!(reopened["timing"], published["timing"]);
    assert_eq!(reopened["contextSize"], published["contextSize"]);
    assert_eq!(reopened["cost"], published["cost"]);
    // A trusted zero estimate is available even without actual invoice data.
    let mut zero = finish(
        start("zero-cost", /*at*/ 4_000),
        /*at*/ 4_000,
        /*tokens*/ 1,
    );
    zero.thread_id = Some("zero-metric-thread".into());
    zero.context_size = Some(UsageContextSize {
        prompt_tokens: Some(1),
        context_tokens: Some(1),
        cache_tokens: Some(0),
        compaction_count: Some(0),
        context_window_tokens: Some(16),
        window_class: Some("short".into()),
    });
    zero.cost_provenance = Some(UsageCostProvenance {
        rate_card: Some("trusted-zero-rate".into()),
        rate_card_version: Some("1".into()),
        pricing_basis: Some("api_equivalent".into()),
        estimated_api_equivalent_cost_micros: Some(0),
        ..Default::default()
    });
    let mut billed_zero = zero.clone();
    billed_zero.id = "zero-billing:finish".into();
    billed_zero.operation_id = Some("zero-billing".into());
    billed_zero.thread_id = Some("zero-billing-thread".into());
    billed_zero.actual_billing_micros = Some(0);
    mark("zero_capture_started");
    capture_records(home.path(), vec![zero, billed_zero])?;
    mark("zero_capture_finished");
    mark("zero_reopen_and_review_started");
    let restarted_reader = UsageReader::open(home.path())?;
    let restarted = restarted_reader.performance_review(
        TimeRange::from_millis(4_000, 5_000)?,
        &UsageFilter {
            thread_id: Some("zero-metric-thread".into()),
            ..Default::default()
        },
    )?;
    mark("zero_reopen_and_review_finished");
    assert_eq!(
        json!({"status":restarted["cost"]["status"],
        "estimate":restarted["cost"]["estimatedUsdMicros"],
        "unknown":restarted["cost"]["unknownObservations"],
        "estimatedMeasurement":restarted["cost"]["estimatedApiEquivalentCostMicros"],
        "actualMeasurement":restarted["cost"]["actualBillingMicros"],
        "cache":restarted["contextSize"]["cacheTokens"],
        "compaction":restarted["contextSize"]["compactionCount"],
        "timing":restarted["timing"],
        "lifecycle":restarted["lifecycle"],
        "breakdown":restarted["cost"]["inputUsdMicros"]}),
        json!({"status":"partial","estimate":0,"unknown":0,
            "estimatedMeasurement":{"measured":0,"exact":0,"unknown":0},
            "actualMeasurement":{"measured":0,"exact":null,"unknown":1},
            "cache":{"measured":0,"exact":0,"unknown":0},
            "compaction":{"measured":0,"exact":0,"unknown":0},
            "timing":{"activeMs":{"measured":0,"exact":0,"unknown":0},
                "elapsedMs":{"measured":0,"exact":0,"unknown":0},
                "wallTimeMs":{"measured":0,"exact":0,"unknown":0},
                "waitMs":{"measured":0,"exact":null,"unknown":0},
                "unknownReasons":{},"coverage":"recorded"},
            "lifecycle":{"firstStartedAtMs":4_000,"lastEndedAtMs":4_000,
                "wallTimeMs":{"measured":0,"exact":0,"unknown":0},
                "unknownReasons":{},"coverage":"recorded"},"breakdown":null})
    );
    mark("zero_billing_review_started");
    let billed = restarted_reader.performance_review(
        TimeRange::from_millis(4_000, 5_000)?,
        &UsageFilter {
            thread_id: Some("zero-billing-thread".into()),
            ..Default::default()
        },
    )?;
    mark("zero_billing_review_finished");
    assert_eq!(
        json!({"estimate":billed["cost"]["estimatedApiEquivalentCostMicros"],
            "actual":billed["cost"]["actualBillingMicros"]}),
        json!({"estimate":{"measured":0,"exact":0,"unknown":0},
            "actual":{"measured":0,"exact":0,"unknown":0}})
    );
    mark("empty_review_started");
    let empty = restarted_reader.performance_review(
        TimeRange::all(),
        &UsageFilter {
            thread_id: Some("empty-metric-selection".into()),
            ..Default::default()
        },
    )?;
    mark("empty_review_finished");
    assert_eq!(
        json!({"timing":empty["timing"],"lifecycle":empty["lifecycle"],
            "context":empty["contextSize"],"cost":{
                "status":empty["cost"]["status"],"estimate":empty["cost"]["estimatedUsdMicros"],
                "estimatedMeasurement":empty["cost"]["estimatedApiEquivalentCostMicros"],
                "actualMeasurement":empty["cost"]["actualBillingMicros"],
                "reasons":empty["cost"]["unavailableReasons"]}}),
        json!({"timing":{"activeMs":{"measured":0,"exact":null,"unknown":0},
                "elapsedMs":{"measured":0,"exact":null,"unknown":0},
                "wallTimeMs":{"measured":0,"exact":null,"unknown":0},
                "waitMs":{"measured":0,"exact":null,"unknown":0},
                "unknownReasons":{},"coverage":"unobserved"},
            "lifecycle":{"firstStartedAtMs":null,"lastEndedAtMs":null,
                "wallTimeMs":{"measured":0,"exact":null,"unknown":0},
                "unknownReasons":{},"coverage":"unobserved"},
            "context":{"coverage":"unobserved","observations":0,
                "promptTokens":{"measured":0,"exact":null,"unknown":0},
                "contextTokens":{"measured":0,"exact":null,"unknown":0},
                "contextWindowTokens":{"measured":0,"exact":null,"unknown":0},
                "cacheTokens":{"measured":0,"exact":null,"unknown":0},
                "compactionCount":{"measured":0,"exact":null,"unknown":0},
                "windowClasses":[],"windowClassesOmitted":0,
                "windowClassObservationsOmitted":0,"unknownWindowClass":0},
            "cost":{"status":"unavailable","estimate":null,
                "estimatedMeasurement":{"measured":0,"exact":null,"unknown":0},
                "actualMeasurement":{"measured":0,"exact":null,"unknown":0},
                "reasons":{"no_token_observations":1}}})
    );
    let overflowing = tempfile::tempdir()?;
    let estimates = (0..3)
        .map(|index| {
            let mut fact = finish(
                start(&format!("overflow-{index}"), /*at*/ 1_000),
                /*at*/ 2_000,
                /*tokens*/ 1,
            );
            fact.cost_provenance = Some(UsageCostProvenance {
                rate_card: Some("trusted-overflow-fixture".into()),
                rate_card_version: Some("1".into()),
                pricing_basis: Some("api_equivalent".into()),
                estimated_api_equivalent_cost_micros: Some(1_u64 << 63),
                ..Default::default()
            });
            fact
        })
        .collect();
    capture_records(overflowing.path(), estimates)?;
    let overflow_error = UsageReader::open(overflowing.path())?
        .performance_review(TimeRange::all(), &UsageFilter::default())
        .expect_err("an overflowing sum must not become exact u64::MAX");
    assert!(overflow_error.to_string().contains("out of range"));
    Ok(())
}
