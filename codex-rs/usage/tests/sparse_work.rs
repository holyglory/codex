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
    fact.id = format!("{}:finish", fact.operation_id.as_deref().unwrap());
    fact.observed_at_ms = Some(at);
    fact.observed_at = at.div_euclid(1_000);
    fact.ended_at_ms = Some(at);
    fact.duration_ms = Some((at - fact.started_at_ms.unwrap()) as u64);
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

fn totals(report: &Value) -> Value {
    json!({"bound":report["workBindings"]["boundOperations"],
        "unknown":report["workBindings"]["unknownOperations"],
        "tokens":report["outcomes"]["totals"]["providerTotalTokens"]["measured"],
        "attributed":report["outcomes"]["attributed"]["providerTotalTokens"]["measured"],
        "unattributed":report["outcomes"]["unattributed"]["providerTotalTokens"]["measured"],
        "intervals":report["outcomes"]["attributed"]["elapsedExecutionMs"]})
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
        json!({"coverage":unavailable["workBindings"]["coverage"],"tokens":unavailable["outcomes"]["totals"]["providerTotalTokens"]["measured"]}),
        json!({"coverage":"unavailable","tokens":105})
    );
    Ok(())
}

#[test]
fn repeated_switches_aggregate_by_job_before_report_limits() -> codex_usage::Result<()> {
    let home = tempfile::tempdir()?;
    let switches = 1_025;
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
    let reader_start = Instant::now();
    let reader = UsageReader::open(home.path())?;
    let reader_time = reader_start.elapsed();
    let query_start = Instant::now();
    let report = reader.performance_review(TimeRange::all(), &UsageFilter::default())?;
    let query_time = query_start.elapsed();
    assert_eq!(
        json!({"bound":report["workBindings"]["boundOperations"],"rows":report["outcomes"]["totalRows"],
        "tokens":report["outcomes"]["attributed"]["providerTotalTokens"]["measured"]}),
        json!({"bound":switches,"rows":1,"tokens":switches})
    );
    assert_eq!(reader.work_binding_transitions().len(), switches as usize);
    eprintln!(
        "sparse-work-measurement {}",
        json!({"switches":switches,"operations":switches,
        "transitionMs":transition_time.as_millis(),"captureMs":capture_time.as_millis(),
        "readerOpenMs":reader_time.as_millis(),"reportMs":query_time.as_millis(),
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
    assert_eq!(reader.work_binding_transitions(), &[original.clone()]);
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
