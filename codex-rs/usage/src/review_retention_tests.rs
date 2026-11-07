use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::JournalPayload;
use crate::JournalReadLimits;
use crate::RetentionPolicy;
use crate::TimeRange;
use crate::UsageCorrection;
use crate::UsageFilter;
use crate::UsageOutcome;
use crate::UsagePublisher;
use crate::UsageReader;
use crate::UsageRecord;
use crate::review_projection::load_review_operations;
use crate::review_state::ReviewClassification;
use crate::review_state::ReviewContribution;

const DAY: i64 = 86_400_000;

fn publish(root: &Path) -> UsageManifest {
    UsagePublisher::open_at(root)
        .expect("publisher")
        .publish_batch(JournalReadLimits {
            max_bytes: 16 * 1024 * 1024,
            max_entries: REVIEW_PARTITION_SIZE as usize,
        })
        .expect("publish real journal")
}

fn token(id: &str, at: i64, thread: &str, input: u64) -> UsageRecord {
    let mut record = UsageRecord::new_millis(id, at, input, /*output_tokens*/ 1)
        .with_thread(thread)
        .with_repository("repository-a")
        .with_model("model-a");
    record.event = Some("token_observation".into());
    record.provider_total_tokens = Some(input + 1);
    record.coverage = Some("recorded".into());
    record
}

fn started(id: &str, at: i64, thread: &str) -> UsageRecord {
    let mut record = token(&format!("{id}:started"), at, thread, /*input*/ 0).with_operation(id);
    record.output_tokens = 0;
    record.provider_total_tokens = None;
    record.event = Some("tool_started".into());
    record.started_at_ms = Some(at);
    record.tool = Some("shell".into());
    record
}

fn finished(start: &UsageRecord) -> UsageRecord {
    let mut record = start.clone();
    record.id = record.id.replace(":started", ":finished");
    record.event = Some("tool_finished".into());
    record.ended_at_ms = record.started_at_ms.map(|at| at + 1_000);
    record.observed_at_ms = record.ended_at_ms;
    record.observed_at = record.ended_at_ms.expect("terminal time") / 1_000;
    record.duration_ms = Some(1_000);
    record.outcome = Some(UsageOutcome::Completed);
    record
}

fn descriptor(root: &Path) -> crate::review_projection::ReviewProjectionDescriptor {
    let manifest = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("manifest");
    load_review_projection(root, &manifest)
        .expect("projection")
        .expect("published projection")
}

#[test]
fn advances_review_cutoff_without_a_projection() {
    let directory = tempdir().expect("directory");
    let mut manifest = UsageManifest::default();
    assert!(
        !retire(directory.path(), &mut manifest, 123, 456, "cutoff-test").expect("cutoff metadata")
    );
    assert_eq!(manifest.review_retention_cutoff_at_ms, Some(123));
}

#[test]
fn mixed_detail_retires_without_losing_live_membership_or_resumed_chat_totals() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let old = started("old", DAY, "active");
    let idle = started("idle", 9 * DAY, "idle");
    let recent = started("recent", 11 * DAY, "active");
    let open = started("open", DAY, "active");
    let mut retired_classification =
        token("class-old", 11 * DAY, "active", /*input*/ 0).with_operation("old");
    retired_classification.event = Some("classification_correction".into());
    retired_classification.activity = Some("diagnosis".into());
    retired_classification.output_tokens = 0;
    retired_classification.provider_total_tokens = None;
    let mut live_classification = retired_classification.clone();
    live_classification.id = "class-recent".into();
    live_classification.operation_id = Some("recent".into());
    live_classification.observed_at_ms = Some(DAY);
    live_classification.observed_at = DAY / 1_000;
    UsageJournal::new(root.join("events.jsonl"))
        .append_batch(
            [
                old.clone(),
                finished(&old),
                idle.clone(),
                finished(&idle),
                recent.clone(),
                finished(&recent),
                open,
                token("old-token", DAY, "active", /*input*/ 10),
                token("idle-token-a", 9 * DAY, "idle", /*input*/ 20),
                token(
                    "idle-token-b",
                    9 * DAY + 3_600_000,
                    "idle",
                    /*input*/ 30,
                ),
                token("recent-token", 11 * DAY, "active", /*input*/ 40),
                retired_classification,
                live_classification,
            ]
            .into_iter()
            .map(JournalPayload::Record)
            .collect(),
        )
        .expect("mixed captures");
    let initial = publish(&root);
    let pinned = UsageReader::open_at(&root).expect("pin original detail");
    let filter = UsageFilter::default().for_repository("repository-a");
    let expected = pinned
        .totals(TimeRange::all(), &filter)
        .expect("original totals");
    let expected_report = pinned
        .performance_review(TimeRange::all(), &filter)
        .expect("original operation statistics");
    let policy = RetentionPolicy {
        raw_retention_days: 7,
        inactive_chat_days: 2,
    };
    let mut maintenance = Default::default();
    for _ in 0..4 {
        crate::retention::maintain(&root, policy, 12 * DAY, &mut maintenance)
            .expect("background retention");
    }
    let retained = descriptor(&root);
    let connection = Connection::open_in_memory().expect("query connection");
    let operations = load_review_operations(
        &root,
        &retained,
        &BTreeSet::from([
            "old".to_string(),
            "idle".to_string(),
            "recent".to_string(),
            "open".to_string(),
        ]),
        &connection,
    )
    .expect("rewritten operation lookup");
    assert_eq!(
        operations
            .iter()
            .map(|row| row.operation_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["open", "recent"])
    );
    assert_eq!(
        (
            retained.retired_operation_rows,
            retained.partitions[0].operation_count,
            retained.partitions[0].open_operations
        ),
        (2, 2, 1)
    );
    let range = TimeRange::new(11 * DAY / 1_000, 12 * DAY / 1_000).expect("recent interval");
    let files = crate::review_index::select_review_files(&root, &retained, range, &filter)
        .expect("fresh membership routing");
    assert_eq!(files, vec![retained.partitions[0].file.clone()]);
    let classification_source =
        parquet_source(&root, &retained.classification_files).expect("live classification");
    let classes = query_rows::<ReviewClassification>(
        &connection,
        &format!("SELECT * FROM {classification_source}"),
        /*max_rows*/ 10,
    )
    .expect("classification targets");
    assert_eq!(
        classes
            .iter()
            .map(|row| row.operation_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recent"]
    );
    let contribution_source = parquet_source(
        &root,
        &retained
            .contribution_fragments
            .iter()
            .map(|row| row.file.clone())
            .collect::<Vec<_>>(),
    )
    .expect("recent detail");
    let rows = query_rows::<ReviewContribution>(
        &connection,
        &format!("SELECT * FROM {contribution_source}"),
        /*max_rows*/ 20,
    )
    .expect("retained contributions");
    assert!(
        rows.iter()
            .all(|row| row.observed_at_ms >= 5 * DAY && row.thread_id.as_deref() != Some("idle"))
    );
    let restarted = UsageReader::open_at(&root).expect("restart retained reader");
    assert_eq!(
        restarted
            .totals(TimeRange::all(), &filter)
            .expect("summary totals"),
        expected
    );
    assert_eq!(
        pinned
            .totals(TimeRange::all(), &filter)
            .expect("pinned totals"),
        expected
    );
    assert_eq!(
        pinned.manifest().review_projection_file,
        initial.review_projection_file
    );
    let report = restarted
        .performance_review(TimeRange::all(), &filter)
        .expect("weighted old and current contributions");
    for section in [
        "counts",
        "operations",
        "operationsByActivity",
        "tools",
        "tokens",
    ] {
        assert_eq!(report[section], expected_report[section], "{section}");
    }
    assert_eq!(
        (
            &report["coverage"]["rawTokenObservations"],
            &report["coverage"]["collectionCompleteness"]
        ),
        (&serde_json::json!(4), &serde_json::json!("partial"))
    );
    let rollup_source =
        parquet_source(&root, &retained.contribution_rollup_files).expect("weighted rollups");
    let idle_rows = query_rows::<ReviewContribution>(
        &connection,
        &format!(
            "SELECT * FROM {rollup_source} WHERE thread_id = 'idle' AND event = 'token_observation'"
        ),
        /*max_rows*/ 10,
    )
    .expect("inactive lifetime rows");
    assert_eq!(idle_rows.len(), 1);
    assert_eq!(
        (idle_rows[0].observation_count, idle_rows[0].input_tokens),
        (Some(2), 50)
    );
    assert!(idle_rows[0].record_id.starts_with("review-lifetime:"));
    UsageJournal::new(root.join("events.jsonl"))
        .append_record(token("resumed", 12 * DAY + 1, "idle", /*input*/ 7))
        .expect("resume chat");
    publish(&root);
    crate::retention::maintain(&root, policy, 12 * DAY + 1, &mut maintenance)
        .expect("retain resumed totals");
    let resumed = UsageReader::open_at(&root).expect("resumed reader");
    assert_eq!(
        resumed
            .totals(TimeRange::all(), &UsageFilter::default().for_thread("idle"))
            .expect("old plus new usage")
            .input_tokens,
        57
    );
    let after_resume = descriptor(&root);
    let source = parquet_source(&root, &after_resume.contribution_rollup_files)
        .expect("retained lifetime state");
    let rows = query_rows::<ReviewContribution>(
        &connection,
        &format!("SELECT * FROM {source} WHERE thread_id = 'idle' AND event = 'token_observation'"),
        /*max_rows*/ 10,
    )
    .expect("no hourly resurrection");
    assert_eq!(rows, idle_rows);
    // The same explicit ID may reappear after its earlier canonical row retired.
    // Raw observation IDs stay distinct and the original reader remains pinned.
    let mut reappeared = started("old", 12 * DAY + 2, "active");
    reappeared.id = "old:resumed:started".into();
    UsageJournal::new(root.join("events.jsonl")).append_record(reappeared.clone()).expect("new incarnation");
    let latest = publish(&root);
    let reappeared_descriptor = descriptor(&root);
    let ids = BTreeSet::from(["old".to_string()]);
    assert_eq!(load_review_operations(&root, &reappeared_descriptor, &ids, &connection).expect("new live incarnation"),
        vec![crate::ReviewOperation::from_fact(latest.watermark, &reappeared).expect("new first sequence")]);
    let original = load_review_projection(&root, &initial).expect("pinned metadata").expect("original projection");
    let old_rows = load_review_operations(&root, &original, &ids, &connection).expect("pinned old incarnation");
    assert_eq!((old_rows[0].first_sequence, old_rows[0].terminal_observed), (1, true));
    assert_eq!(crate::review_counts::read_review_counts(&root, &reappeared_descriptor,
        &crate::review_counts::ReviewCountScope::Global).expect("all-time count unchanged by index").operations, 5);

}

#[test]
fn signed_tokens_and_inherited_activity_survive_retirement_restart_and_chat_resume() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut model = started("model", DAY + 1_000, "idle").with_activity("coding", "implementation");
    model.event = Some("model_request_started".into());
    model.response_id = Some("provider-response".into());
    model.classification = Some("agent_declared".into());
    let mut model_end = finished(&model);
    model_end.event = Some("model_request_finished".into());
    let mut tool = started("tool", DAY + 2_000, "idle").with_activity("coding", "implementation");
    tool.classification = Some("agent_declared".into());
    tool.rework_of_operation_id = Some("earlier-operation".into());
    let mut observed = token("corrected-token", DAY + 1_500, "idle", /*input*/ 5)
        .with_operation("provider-response");
    observed.response_id = Some("provider-response".into());
    observed.output_tokens = 2;
    observed.cached_input_tokens = 3;
    observed.reasoning_output_tokens = 1;
    observed.provider_total_tokens = Some(7);
    journal
        .append_batch(
            [model, model_end, tool.clone(), finished(&tool), observed]
                .into_iter()
                .map(JournalPayload::Record)
                .collect(),
        )
        .expect("capture completed operations and provider usage");
    publish(&root);
    maintain(&root, 5 * DAY, /*inactive_cutoff*/ 0)
        .expect("review waits for raw observation retirement");
    let guarded = descriptor(&root);
    assert_eq!(guarded.retired_contribution_rows, 0);
    assert!(guarded.contribution_rollup_files.is_empty());
    assert!(!guarded.contribution_fragments.is_empty());
    let mut correction = UsageCorrection::new(
        "signed-adjustment",
        "corrected-token",
        2 * DAY / 1_000,
        /*input_tokens_delta*/ -9,
        /*output_tokens_delta*/ -4,
        "provider adjustment",
    );
    correction.cached_input_tokens_delta = -5;
    correction.reasoning_output_tokens_delta = -3;
    let mut classification = token("taxonomy", 2 * DAY, "idle", /*input*/ 0)
        .with_operation("model")
        .with_activity("integration_testing", "testing");
    classification.output_tokens = 0;
    classification.provider_total_tokens = None;
    classification.event = Some("classification_correction".into());
    classification.classification = Some("agent_corrected".into());
    journal
        .append_batch(vec![
            JournalPayload::Correction(correction),
            JournalPayload::Record(classification),
        ])
        .expect("late token and taxonomy corrections");
    publish(&root);
    let filter = UsageFilter::default()
        .for_repository("repository-a")
        .for_thread("idle");
    let before = UsageReader::open_at(&root).expect("before reader");
    let expected = before
        .performance_review(TimeRange::all(), &filter)
        .expect("original report");
    let expected_summary = before
        .summary(TimeRange::all(), &filter)
        .expect("original summary");
    let policy = RetentionPolicy::default();
    let mut maintenance = Default::default();
    for _ in 0..6 {
        crate::retention::maintain(&root, policy, 12 * DAY, &mut maintenance)
            .expect("retire original detail");
    }
    let retained = descriptor(&root);
    assert!(retained.partitions.is_empty());
    assert!(retained.contribution_fragments.is_empty());
    assert!(!retained.operation_rollup_files.is_empty());
    let after = UsageReader::open_at(&root).expect("restart retained reader");
    let actual = after
        .performance_review(TimeRange::all(), &filter)
        .expect("retained report");
    for section in [
        "counts",
        "operations",
        "operationsByActivity",
        "tools",
        "tokens",
        "tokensByActivity",
    ] {
        assert_eq!(actual[section], expected[section], "{section}");
    }
    assert_eq!(
        actual["coverage"]["retainedAggregateOperations"],
        serde_json::json!(2)
    );
    assert_eq!(
        actual["counts"]["interval_sum_ms"],
        serde_json::json!(2_000)
    );
    assert_eq!(actual["links"]["reworkOperations"], serde_json::json!(1));
    assert_eq!(actual["links"]["omittedOperations"], serde_json::json!(1));
    let summary = after
        .summary(TimeRange::all(), &filter)
        .expect("intersected retained totals");
    for section in [
        "inputTokens",
        "cachedInputTokens",
        "outputTokens",
        "reasoningOutputTokens",
        "counts",
    ] {
        assert_eq!(summary[section], expected_summary[section], "{section}");
    }
    assert_eq!(
        (
            summary["inputTokens"].clone(),
            summary["cachedInputTokens"].clone(),
            summary["outputTokens"].clone(),
            summary["reasoningOutputTokens"].clone()
        ),
        (
            serde_json::json!(-4),
            serde_json::json!(-2),
            serde_json::json!(-2),
            serde_json::json!(-2)
        )
    );
    assert!(
        after
            .operation_page(
                TimeRange::all(),
                &filter,
                crate::OperationSelection::All,
                /*after*/ None,
                /*limit*/ 20
            )
            .expect("retained operation page")
            .data
            .is_empty()
    );
    let connection = Connection::open_in_memory().expect("inspect aggregates");
    let source = parquet_source(&root, &retained.operation_rollup_files).expect("aggregate files");
    let rows = query_rows::<serde_json::Value>(
        &connection,
        &format!("SELECT * FROM {source}"),
        /*max_rows*/ 10,
    )
    .expect("id-free grouped statistics");
    assert!(rows.iter().all(|row| row.get("operation_id").is_none()
        && row.get("response_id").is_none()
        && row["bucket_at_ms"].is_null()));
    let finite = after
        .performance_review(
            TimeRange::from_millis(DAY, 3 * DAY).expect("old interval"),
            &filter,
        )
        .expect("truthful discarded interval");
    assert_eq!(
        finite["coverage"]["operationStatistics"],
        serde_json::json!("partial_retention")
    );
    let resumed =
        started("resumed-tool", 12 * DAY + 1_000, "idle").with_activity("coding", "implementation");
    journal
        .append_batch(vec![
            JournalPayload::Record(resumed.clone()),
            JournalPayload::Record(finished(&resumed)),
        ])
        .expect("resume original chat");
    publish(&root);
    crate::retention::maintain(&root, policy, 12 * DAY + 2_000, &mut maintenance)
        .expect("maintain resumed chat");
    let resumed = UsageReader::open_at(&root)
        .expect("restart resumed reader")
        .performance_review(TimeRange::all(), &filter)
        .expect("old plus new statistics");
    assert_eq!(resumed["counts"]["operations"], serde_json::json!(3));
    assert_eq!(
        resumed["counts"]["interval_sum_ms"],
        serde_json::json!(3_000)
    );
    assert_eq!(resumed["tokens"], actual["tokens"]);
    assert_eq!(resumed["tokensByActivity"], actual["tokensByActivity"]);
}

#[test]
fn scan_revisits_old_fragments_while_publication_keeps_adding_work() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    for (id, at) in [("first", DAY), ("second", 2 * DAY)] {
        journal
            .append_record(token(id, at, "active", /*input*/ 1))
            .expect("capture");
        publish(&root);
    }
    assert!(!maintain(&root, /*cutoff*/ 0, /*inactive_cutoff*/ 0).expect("initial scan"));
    assert_eq!(descriptor(&root).retention_scan_remaining, 1);
    let mut maintenance = Default::default();
    for id in ["third", "fourth"] {
        journal
            .append_record(token(id, 12 * DAY, "active", /*input*/ 1))
            .expect("ongoing capture");
        publish(&root);
        crate::retention::maintain(
            &root,
            RetentionPolicy {
                raw_retention_days: 7,
                inactive_chat_days: 30,
            },
            12 * DAY,
            &mut maintenance,
        )
        .expect("resume frozen scan after raw retirement");
    }
    let retained = descriptor(&root);
    assert_eq!(retained.retired_contribution_rows, 2);
    assert_eq!(retained.contribution_fragments.len(), 2);
}

#[test]
fn prepared_retirement_rejects_unpublished_corrections_and_same_generation_changes() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_record(token("old", DAY, "active", /*input*/ 10))
        .expect("capture");
    let original = publish(&root);
    let job = ReviewRetentionJob::prepare(&root, 5 * DAY, /*inactive_cutoff*/ 0)
        .expect("prepare pinned retirement")
        .expect("old detail");
    journal
        .append_correction(UsageCorrection::new(
            "late",
            "old",
            12 * DAY / 1_000,
            /*input_tokens_delta*/ 5,
            /*output_tokens_delta*/ 0,
            "captured during preparation",
        ))
        .expect("pending correction");
    assert!(!job.install().expect("reject unpublished correction race"));
    assert_eq!(
        ManifestStore::new(root.join("manifest.json"))
            .load()
            .expect("unchanged publication"),
        original
    );
    assert!(
        ReviewRetentionJob::prepare(&root, 5 * DAY, /*inactive_cutoff*/ 0)
            .expect("defer for backlog")
            .is_none()
    );
    publish(&root);
    let job = ReviewRetentionJob::prepare(&root, 5 * DAY, /*inactive_cutoff*/ 0)
        .expect("prepare again")
        .expect("old detail remains");
    let store = ManifestStore::new(root.join("manifest.json"));
    let mut changed = store.load().expect("source");
    changed.correction_retention_cutoff_at_ms = Some(7 * DAY);
    store
        .save(&changed)
        .expect("same generation metadata change");
    assert!(!job.install().expect("reject stale metadata"));
    assert_eq!(store.load().expect("preserved concurrent change"), changed);
}

#[test]
fn large_publication_fragments_retire_without_a_partition_sized_decode_limit() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let count = REVIEW_PARTITION_SIZE + 1;
    let mut records = vec![JournalPayload::Record(token(
        "old", DAY, "active", /*input*/ 1,
    ))];
    for index in 0..count {
        records.push(JournalPayload::Record(token(
            &format!("recent-{index}"),
            11 * DAY,
            "active",
            /*input*/ 1,
        )));
        let mut classification = started(&format!("unknown-{index}"), DAY, "active");
        classification.event = Some("classification_correction".into());
        records.push(JournalPayload::Record(classification));
    }
    UsageJournal::new(root.join("events.jsonl"))
        .append_batch(records)
        .expect("large capture batch");
    UsagePublisher::open_at(&root)
        .expect("publisher")
        .publish_batch(JournalReadLimits {
            max_entries: (2 * count + 1) as usize,
            max_bytes: 64 * 1024 * 1024,
        })
        .expect("larger bounded publication");
    let mut maintenance = Default::default();
    for _ in 0..3 {
        crate::retention::maintain(
            &root,
            RetentionPolicy {
                raw_retention_days: 7,
                inactive_chat_days: 30,
            },
            12 * DAY,
            &mut maintenance,
        )
        .expect("SQL retirement after raw detail expires");
    }
    let retained = descriptor(&root);
    assert!(retained.classification_files.is_empty());
    let source = parquet_source(
        &root,
        &retained
            .contribution_fragments
            .iter()
            .map(|row| row.file.clone())
            .collect::<Vec<_>>(),
    )
    .expect("retained large fragment");
    let connection = Connection::open_in_memory().expect("connection");
    let (rows, minimum): (u64, i64) = connection
        .query_row(
            &format!("SELECT count(*), min(observed_at_ms) FROM {source}"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("remaining contribution bounds");
    assert_eq!(
        (rows, minimum, retained.retired_contribution_rows),
        (count, 11 * DAY, count + 1)
    );
}

#[test]
fn retry_waits_and_attempt_counts_survive_retention_without_inflating_execution() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let mut first = started("first-attempt", DAY, "retrying");
    first.event = Some("model_request_started".into());
    first.tool = None;
    first.attempt_number = Some(1);
    let mut first_finished = finished(&first);
    first_finished.event = Some("model_request_finished".into());
    first_finished.outcome = Some(UsageOutcome::Failed);
    let mut wait = started("retry-timer", DAY + 1_000, "retrying");
    wait.event = Some("retry_wait_started".into());
    wait.tool = Some("http_retry".into());
    wait.retry_of_operation_id = first.operation_id.clone();
    let mut wait_finished = finished(&wait);
    wait_finished.event = Some("retry_wait_finished".into());
    let mut second = started("second-attempt", DAY + 2_000, "retrying");
    second.event = Some("model_request_started".into());
    second.tool = None;
    second.attempt_number = Some(2);
    second.retry_of_operation_id = first.operation_id.clone();
    second.rework_of_operation_id = Some("prior-explicit-redo".into());
    let mut second_finished = finished(&second);
    second_finished.event = Some("model_request_finished".into());
    second_finished.response_id = Some("recovered-response".into());
    let mut usage = token("tokens", DAY + 3_000, "retrying", /*input*/ 10);
    usage.response_id = second_finished.response_id.clone();
    UsageJournal::new(root.join("events.jsonl"))
        .append_batch(
            [
                first,
                first_finished,
                wait,
                wait_finished,
                second,
                second_finished,
                usage,
            ]
            .into_iter()
            .map(JournalPayload::Record)
            .collect(),
        )
        .expect("capture retry chain");
    publish(&root);
    let filter = UsageFilter::default();
    let summary = UsageReader::open_at(&root)
        .expect("reader")
        .summary(TimeRange::all(), &filter)
        .expect("cached summary counts");
    assert_eq!(
        (
            &summary["counts"]["operations"],
            &summary["counts"]["modelRequests"],
            &summary["counts"]["tools"]
        ),
        (
            &serde_json::json!(2),
            &serde_json::json!(2),
            &serde_json::json!(0)
        )
    );
    let initial = UsageReader::open_at(&root)
        .expect("reader")
        .performance_review(TimeRange::all(), &filter)
        .expect("retry report");
    assert_eq!(
        (
            &initial["counts"]["operations"],
            &initial["counts"]["model_requests"],
            &initial["counts"]["interval_sum_ms"],
            &initial["links"]["retryOperations"],
            &initial["links"]["observedAttempts"],
            &initial["links"]["reworkOperations"],
        ),
        (
            &serde_json::json!(2),
            &serde_json::json!(2),
            &serde_json::json!(2_000),
            &serde_json::json!(1),
            &serde_json::json!(2),
            &serde_json::json!(1)
        )
    );
    assert_eq!(
        initial["waits"],
        serde_json::json!([
            {"reason":"http_retry","outcome":"completed","count":1,"measuredWaitMs":1_000,"unknownIntervals":0}
        ])
    );
    assert_eq!(
        initial["outcomes"]["totals"]["recordedWaitMs"]["measured"],
        serde_json::json!(1_000)
    );
    let clipped = UsageReader::open_at(&root)
        .expect("reader")
        .performance_review(
            TimeRange::from_millis(DAY + 1_250, DAY + 1_750).expect("timer interval"),
            &filter,
        )
        .expect("clipped retry wait");
    assert_eq!(
        (
            &clipped["counts"]["operations"],
            &clipped["counts"]["interval_sum_ms"],
            &clipped["outcomes"]["totals"]["recordedWaitMs"]["measured"]
        ),
        (
            &serde_json::json!(0),
            &serde_json::json!(0),
            &serde_json::json!(500)
        )
    );
    let mut maintenance = Default::default();
    for _ in 0..4 {
        crate::retention::maintain(
            &root,
            RetentionPolicy {
                raw_retention_days: 7,
                inactive_chat_days: 2,
            },
            12 * DAY,
            &mut maintenance,
        )
        .expect("retain operation statistics");
    }
    let retained_summary = UsageReader::open_at(&root)
        .expect("reader")
        .summary(TimeRange::all(), &filter)
        .expect("retained cached counts");
    assert_eq!(retained_summary["counts"], summary["counts"]);
    let retained = UsageReader::open_at(&root)
        .expect("retained reader")
        .performance_review(TimeRange::all(), &filter)
        .expect("retained retry report");
    assert_eq!(retained["counts"], initial["counts"]);
    assert_eq!(retained["waits"], initial["waits"]);
    assert_eq!(retained["links"]["retryOperations"], serde_json::json!(1));
    assert_eq!(retained["links"]["observedAttempts"], serde_json::json!(2));
    assert_eq!(retained["links"]["sample"], serde_json::json!([]));
    assert_eq!(retained["links"]["omittedOperations"], serde_json::json!(1));
    assert_eq!(retained["coverage"]["waits"], serde_json::json!("partial"));
    assert!(retained["outcomes"]["totals"]["recordedWaitMs"]["exact"].is_null());
    // Retain a separate pre-extension Parquet fixture; never rewrite a published segment.
    let connection = Connection::open_in_memory().expect("legacy fixture connection");
    let mut legacy = descriptor(&root);
    let source =
        parquet_source(&root, &legacy.operation_rollup_files).expect("retained aggregates");
    let legacy_rollup = "legacy-operation-rollup.parquet";
    crate::review_storage::write_query(
        &connection,
        &root,
        legacy_rollup,
        &format!(
            "SELECT * EXCLUDE (retry_operations,observed_attempts,linked_operations) FROM {source}"
        ),
    )
    .expect("pre-extension aggregate columns");
    legacy.operation_rollup_files = vec![legacy_rollup.into()];
    let legacy_descriptor = "review-projection-legacy-fixture.json";
    crate::review_storage::write_json(&root, legacy_descriptor, &legacy)
        .expect("legacy descriptor");
    let store = ManifestStore::new(root.join("manifest.json"));
    let mut manifest = store.load().expect("manifest");
    manifest.review_projection_file = Some(legacy_descriptor.into());
    store
        .save(&manifest)
        .expect("select isolated legacy fixture");
    let legacy_report = UsageReader::open_at(&root)
        .expect("legacy reader")
        .performance_review(TimeRange::all(), &filter)
        .expect("legacy aggregates remain readable");
    assert_eq!(
        legacy_report["links"]["reworkOperations"],
        serde_json::json!(1)
    );
    assert_eq!(
        legacy_report["links"]["omittedOperations"],
        serde_json::json!(1)
    );
    assert_eq!(
        legacy_report["links"]["retryCoverage"],
        serde_json::json!("unobserved")
    );
    assert_eq!(
        legacy_report["links"]["unknownAttemptOperations"],
        serde_json::json!(2)
    );
    assert!(legacy_report["links"]["retryOperations"].is_null());
}
