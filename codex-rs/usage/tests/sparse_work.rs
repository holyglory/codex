use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use codex_usage::JournalReadLimits;
use codex_usage::TimeRange;
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
    capture_records(home.path(), vec![duplicate, late])?;
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
        "intervals":{"measured":1002,"exact":null,"unknown":0}})
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
                "activeAgentMs":{"measured":0,"exact":null,"unknown":2},
                "elapsedExecutionMs":{"measured":2,"exact":null,"unknown":0},
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
