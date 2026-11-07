use std::collections::BTreeSet;
use std::fs;

use duckdb::Connection;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::JournalPayload;
use crate::JournalReadLimits;
use crate::ManifestStore;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageJournal;
use crate::UsageOutcome;
use crate::UsagePublisher;
use crate::review_counts::ReviewCountScope;
use crate::review_counts::ReviewCounts;
use crate::review_counts::read_review_counts;
use crate::review_index::select_review_contribution_files;
use crate::review_index::select_review_files;

fn limits() -> JournalReadLimits {
    JournalReadLimits {
        max_bytes: 16 * 1024 * 1024,
        max_entries: REVIEW_PARTITION_SIZE as usize,
    }
}

fn started(operation: &str, kind: &str, at_ms: i64) -> UsageRecord {
    let mut record = UsageRecord::new_millis(
        format!("{operation}:started"),
        at_ms,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation(operation)
    .with_account("account-a")
    .with_thread("thread-a")
    .with_repository("repository-a");
    record.event = Some(format!("{kind}_started"));
    record.started_at_ms = Some(at_ms);
    record.phase = Some("implementation".into());
    record.activity = Some("coding".into());
    record.classification = Some("agent_declared".into());
    record.coverage = Some("capture_started".into());
    record
}

fn finished(start: &UsageRecord, at_ms: i64, duration_ms: u64) -> UsageRecord {
    let mut record = start.clone();
    record.id = format!(
        "{}:finished",
        start.operation_id.as_deref().expect("operation")
    );
    record.observed_at = at_ms.div_euclid(1_000);
    record.observed_at_ms = Some(at_ms);
    record.ended_at_ms = Some(at_ms);
    record.duration_ms = Some(duration_ms);
    record.outcome = Some(UsageOutcome::Completed);
    record.coverage = Some("recorded".into());
    record.event = start
        .event
        .as_ref()
        .map(|event| event.replace("_started", "_finished"));
    record
}

fn all_operations(root: &Path, descriptor: &ReviewProjectionDescriptor) -> Vec<ReviewOperation> {
    if descriptor.partitions.is_empty() {
        return Vec::new();
    }
    let connection = Connection::open_in_memory().expect("connection");
    let source = parquet_source(
        root,
        &descriptor
            .partitions
            .iter()
            .map(|partition| partition.file.clone())
            .collect::<Vec<_>>(),
    )
    .expect("current files");
    query_rows(
        &connection,
        &format!("SELECT * FROM {source} ORDER BY operation_id"),
        REVIEW_PARTITION_SIZE as usize,
    )
    .expect("persisted operation states")
}

fn descriptor(root: &Path, manifest: &UsageManifest) -> ReviewProjectionDescriptor {
    load_review_projection(root, manifest)
        .expect("descriptor metadata")
        .expect("published projection")
}

fn assert_private_projection(root: &Path, manifest: &UsageManifest) {
    let descriptor = descriptor(root, manifest);
    let mut files = vec![
        manifest
            .review_projection_file
            .as_ref()
            .expect("descriptor file")
            .clone(),
    ];
    if let Some(file) = &descriptor.operation_id_index_file {
        files.push(file.clone());
        for entry in operation_id_index::load(root, &descriptor)
            .expect("operation index")
            .expect("indexed projection")
            .runs
        {
            files.extend([entry.run.data_file, entry.run.fence_file]);
            files.extend(entry.run.bloom_file);
        }
    }
    for partition in descriptor.partitions {
        files.extend([
            partition.file,
            partition.membership_file,
            partition.operation_index_file,
        ]);
    }
    for fragment in descriptor.contribution_fragments {
        files.extend([fragment.file, fragment.membership_file]);
    }
    files.extend(descriptor.classification_files);
    files.extend(descriptor.counts_file);
    for file in files {
        codex_private_storage::verify_private_file(&root.join("segments").join(file))
            .expect("private immutable review file");
    }
}

#[test]
fn spanning_operations_tokens_and_late_classification_survive_publication_and_restart() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut publisher = UsagePublisher::open_at(root).expect("publisher");
    let mut stale = UsagePublisher::open_at(root).expect("independent publisher");
    let mut start = started("model-op", "model_request", /*at_ms*/ 100);
    start.turn_id = Some("turn-a".into());
    start.model = Some("model-a".into());
    start.attempt_number = Some(2);
    start.rework_of_operation_id = Some("previous-operation".into());
    journal.append_record(start.clone()).expect("capture start");
    let open_manifest = publisher
        .publish_batch(limits())
        .expect("publish open operation");
    let open_descriptor = descriptor(root, &open_manifest);
    let open_index = operation_id_index::load(root, &open_descriptor)
        .expect("first index")
        .expect("fresh index");
    let pinned_reader = crate::UsageReader::open_at(root).expect("pin open summary");
    let open_summary = pinned_reader
        .summary(TimeRange::all(), &UsageFilter::default())
        .expect("public open summary");
    assert_eq!(
        read_review_counts(root, &open_descriptor, &ReviewCountScope::Global)
            .expect("open operation summary"),
        ReviewCounts {
            operations: 1,
            model_requests: 1,
            unfinished: 1,
            unknown_intervals: 1,
            ..ReviewCounts::default()
        }
    );
    let pinned_bytes = fs::read(
        root.join("segments")
            .join(&open_descriptor.partitions[0].file),
    )
    .expect("pinned operation bytes");
    let mut terminal = finished(&start, /*at_ms*/ 10_100, /*duration_ms*/ 10_000);
    terminal.response_id = Some("provider-response".into());
    let mut tokens = UsageRecord::new_millis(
        "tokens-once",
        /*observed_at_ms*/ 4_500,
        /*input_tokens*/ 7,
        /*output_tokens*/ 3,
    )
    .with_operation("provider-response")
    .with_thread("thread-a")
    .with_account("account-a")
    .with_repository("repository-a");
    tokens.response_id = Some("provider-response".into());
    tokens.event = Some("token_observation".into());
    tokens.coverage = Some("recorded".into());
    tokens.provider_total_tokens = Some(10);
    let mut duplicate = tokens.clone();
    duplicate.input_tokens = 900;
    journal
        .append_batch(vec![
            JournalPayload::Record(tokens.clone()),
            JournalPayload::Record(duplicate),
            JournalPayload::Record(terminal),
        ])
        .expect("durable bounded observations and terminal");
    let closed_manifest = stale
        .publish_batch(limits())
        .expect("stale publisher merges current state");
    let restarted = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("restart metadata");
    assert_eq!(restarted, closed_manifest);
    let closed_summary = crate::UsageReader::open_at(root)
        .expect("restart public reader")
        .summary(TimeRange::all(), &UsageFilter::default())
        .expect("public closed summary");
    assert_eq!(
        closed_summary["counts"],
        serde_json::json!({"operations": 1, "modelRequests": 1, "tools": 0,
            "warmups": 0, "unfinished": 0, "unknownIntervals": 0,
            "unclassified": 0, "missingRepository": 0})
    );
    assert_eq!(
        pinned_reader
            .summary(TimeRange::all(), &UsageFilter::default())
            .expect("pinned summary"),
        open_summary
    );
    let closed = descriptor(root, &restarted);
    let closed_index = operation_id_index::load(root, &closed)
        .expect("advanced index")
        .expect("fresh index");
    assert_eq!(
        closed_index.runs, open_index.runs,
        "late facts retain the original first-sequence address"
    );
    assert_eq!(closed_index.watermark.sequence, restarted.watermark);
    assert_eq!(
        (closed.complete_from_sequence, closed.watermark),
        (1, journal.checkpoint().expect("head"))
    );
    let window = TimeRange::from_millis(/*start_ms*/ 4_000, /*end_ms*/ 5_000).expect("window");
    let filter = UsageFilter::default().for_repository("repository-a");
    assert_eq!(
        select_review_files(root, &closed, window, &filter).expect("interval selection"),
        vec![closed.partitions[0].file.clone()]
    );
    let current = all_operations(root, &closed);
    assert_eq!(
        current,
        vec![ReviewOperation {
            operation_id: "model-op".into(),
            kind: crate::ReviewOperationKind::ModelRequest,
            first_sequence: 1,
            source_sequence: 4,
            observed_at_ms: 10_100,
            start_observed: true,
            terminal_observed: true,
            started_at_ms: Some(100),
            ended_at_ms: Some(10_100),
            duration_ms: Some(10_000),
            outcome: Some(UsageOutcome::Completed),
            account_id: Some("account-a".into()),
            thread_id: Some("thread-a".into()),
            repository_id: Some("repository-a".into()),
            turn_id: Some("turn-a".into()),
            model: Some("model-a".into()),
            tool: None,
            response_id: Some("provider-response".into()),
            phase: Some("implementation".into()),
            activity: Some("coding".into()),
            classification: Some("agent_declared".into()),
            coverage: Some("recorded".into()),
            attempt_number: Some(2),
            rework_of_operation_id: Some("previous-operation".into()),
            process_id: None,
            agent_id: None,
            retry_of_operation_id: None,
        }]
    );
    assert_eq!(closed.partitions[0].open_operations, 0);
    for scope in [
        ReviewCountScope::Global,
        ReviewCountScope::Account("account-a".into()),
        ReviewCountScope::Thread("thread-a".into()),
        ReviewCountScope::Repository("repository-a".into()),
    ] {
        assert_eq!(
            read_review_counts(root, &closed, &scope).expect("exact dimension summary"),
            ReviewCounts {
                operations: 1,
                model_requests: 1,
                ..ReviewCounts::default()
            }
        );
    }
    assert_eq!(
        closed.partitions[0].operation_index_file,
        open_descriptor.partitions[0].operation_index_file
    );
    assert_eq!(all_operations(root, &open_descriptor)[0].ended_at_ms, None);
    assert_eq!(
        fs::read(
            root.join("segments")
                .join(&open_descriptor.partitions[0].file)
        )
        .expect("unchanged pinned bytes"),
        pinned_bytes
    );
    let files = select_review_contribution_files(root, &closed, window, &filter)
        .expect("indexed record coverage files");
    let connection = Connection::open_in_memory().expect("connection");
    let source = parquet_source(root, &files).expect("contribution source");
    let rows: Vec<ReviewContribution> = query_rows(
        &connection,
        &format!("SELECT * FROM {source} WHERE observed_at_ms >= 4000 AND observed_at_ms < 5000"),
        /*max_rows*/ 4,
    )
    .expect("exact observation window");
    assert_eq!(
        rows,
        vec![ReviewContribution::from_fact(/*sequence*/ 2, &tokens)]
    );
    let mut correction = UsageRecord::new_millis(
        "late-classification",
        /*observed_at_ms*/ 86_400_001,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation("model-op");
    correction.event = Some("classification_correction".into());
    correction.phase = Some("testing".into());
    correction.activity = Some("integration_testing".into());
    correction.classification = Some("agent_corrected".into());
    journal
        .append_record(correction)
        .expect("late correction for old window");
    let corrected = publisher
        .publish_batch(limits())
        .expect("publish append-only classification");
    let corrected = descriptor(root, &corrected);
    assert_eq!(corrected.partitions, closed.partitions);
    assert_eq!(
        corrected.membership_index_file,
        closed.membership_index_file
    );
    let classifications =
        parquet_source(root, &corrected.classification_files).expect("all classification files");
    let rows: Vec<ReviewClassification> = query_rows(
        &connection,
        &format!("SELECT * FROM {classifications} WHERE operation_id = 'model-op'"),
        /*max_rows*/ 1,
    )
    .expect("correction selected by operation, independent of window");
    assert_eq!(
        (rows[0].observed_at_ms, rows[0].phase.as_deref()),
        (86_400_001, Some("testing"))
    );
    assert_eq!(
        select_review_files(root, &corrected, window, &filter).expect("same operation files"),
        vec![closed.partitions[0].file.clone()]
    );
    assert_private_projection(
        root,
        &ManifestStore::new(root.join("manifest.json"))
            .load()
            .expect("manifest"),
    );
}

#[test]
fn known_terminal_replaces_unknown_open_revision_before_interval_pruning() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut publisher = UsagePublisher::open_at(root).expect("publisher");
    let known = started("late-start-time", "tool", /*at_ms*/ 20_000);
    let mut unknown = known.clone();
    unknown.started_at_ms = None;
    journal
        .append_record(unknown)
        .expect("start observed with missing timing");
    let before = publisher
        .publish_batch(limits())
        .expect("publish honest unknown interval");
    let before = descriptor(root, &before);
    let old_window = TimeRange::from_millis(/*start_ms*/ 0, /*end_ms*/ 1_000).expect("window");
    assert_eq!(
        select_review_files(root, &before, old_window, &UsageFilter::default())
            .expect("unknown start cannot exclude old window"),
        vec![before.partitions[0].file.clone()]
    );
    journal
        .append_record(finished(
            &known, /*at_ms*/ 30_000, /*duration_ms*/ 10_000,
        ))
        .expect("terminal supplies factual start and end");
    let after = publisher
        .publish_batch(limits())
        .expect("replace current canonical partition");
    let after = descriptor(root, &after);
    assert_eq!(
        select_review_files(root, &after, old_window, &UsageFilter::default())
            .expect("closed interval excludes old window"),
        Vec::<String>::new()
    );
    assert_eq!(all_operations(root, &before)[0].started_at_ms, None);
    let state = all_operations(root, &after).remove(0);
    assert_eq!(
        (
            state.started_at_ms,
            state.ended_at_ms,
            state.start_observed,
            state.terminal_observed
        ),
        (Some(20_000), Some(30_000), true, true)
    );
    let mut terminal_only = finished(
        &started("terminal-only", "model_warmup", /*at_ms*/ 40_000),
        /*at_ms*/ 50_000,
        /*duration_ms*/ 10_000,
    );
    terminal_only.started_at_ms = None;
    terminal_only.ended_at_ms = None;
    terminal_only.duration_ms = None;
    terminal_only.outcome = None;
    terminal_only.phase = None;
    terminal_only.activity = None;
    journal
        .append_record(terminal_only)
        .expect("terminal fact with incomplete measurements");
    let incomplete = publisher
        .publish_batch(limits())
        .expect("retain unknown measurements");
    let rows = all_operations(root, &descriptor(root, &incomplete));
    let row = rows
        .iter()
        .find(|row| row.operation_id == "terminal-only")
        .expect("terminal-only row");
    assert_eq!(
        (
            row.start_observed,
            row.terminal_observed,
            row.started_at_ms,
            row.ended_at_ms,
            row.duration_ms,
            row.outcome
        ),
        (false, true, None, None, None, None)
    );
    let before_correction = descriptor(root, &incomplete);
    assert_eq!(
        read_review_counts(root, &before_correction, &ReviewCountScope::Global)
            .expect("unknown measurements and classification counted"),
        ReviewCounts {
            operations: 2,
            tools: 1,
            warmups: 1,
            unknown_intervals: 1,
            unclassified: 1,
            tool_completed: 1,
            ..ReviewCounts::default()
        }
    );
    let mut classification = UsageRecord::new_millis(
        "terminal-only-classification",
        /*observed_at_ms*/ 90_000,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation("terminal-only");
    classification.event = Some("classification_correction".into());
    classification.phase = Some("testing".into());
    classification.activity = Some("verification_review".into());
    journal
        .append_record(classification)
        .expect("late classification without lifecycle update");
    let corrected = publisher
        .publish_batch(limits())
        .expect("correction-only summary delta");
    let corrected = descriptor(root, &corrected);
    assert_eq!(corrected.partitions, before_correction.partitions);
    let expected = ReviewCounts {
        operations: 2,
        tools: 1,
        warmups: 1,
        unknown_intervals: 1,
        tool_completed: 1,
        ..ReviewCounts::default()
    };
    assert_eq!(
        read_review_counts(root, &corrected, &ReviewCountScope::Global)
            .expect("late classification included exactly once"),
        expected
    );
    assert_eq!(
        read_review_counts(root, &before_correction, &ReviewCountScope::Global)
            .expect("prior summary remains pinned")
            .unclassified,
        1
    );
}

#[test]
fn finite_dimension_selection_and_id_lookup_do_not_open_unrelated_operation_data() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut records = vec![JournalPayload::Record(started(
        "first-partition",
        "tool",
        /*at_ms*/ 5_000,
    ))];
    // Cross the bulk publication threshold, then publish a single new
    // operation. Cached summaries must advance even on the small batch.
    records.extend((1..=REVIEW_PARTITION_SIZE).map(|sequence| {
        JournalPayload::Record(UsageRecord::new_millis(
            format!("coverage-{sequence}"),
            /*observed_at_ms*/ 5_000,
            /*input_tokens*/ 0,
            /*output_tokens*/ 0,
        ))
    }));
    journal
        .append_batch(records)
        .expect("real sequence boundary");
    let mut publisher = UsagePublisher::open_at(root).expect("publisher");
    let first = publisher
        .publish_batch(JournalReadLimits {
            max_entries: REVIEW_PARTITION_SIZE as usize + 1,
            ..limits()
        })
        .expect("first bounded partition");
    let first = descriptor(root, &first);
    // Subsequent publication must reuse the consolidated index for unchanged
    // partitions, rather than reopening their individual membership fragments.
    let first_membership = root
        .join("segments")
        .join(&first.partitions[0].membership_file);
    let unavailable_membership = first_membership.with_extension("unavailable");
    fs::rename(&first_membership, &unavailable_membership)
        .expect("make unrelated fragment unavailable");
    let old_address = root
        .join("segments")
        .join(&first.partitions[0].operation_index_file);
    let unavailable_address = old_address.with_extension("unavailable");
    fs::rename(&old_address, &unavailable_address)
        .expect("publication must use the global exact index");
    let mut unrelated = started("second-partition", "tool", /*at_ms*/ 6_000);
    unrelated.repository_id = Some("repository-b".into());
    unrelated.thread_id = Some("thread-b".into());
    journal
        .append_record(unrelated)
        .expect("next fixed sequence partition");
    let second = publisher
        .publish_batch(limits())
        .expect("only new partition added");
    fs::rename(&unavailable_membership, &first_membership)
        .expect("restore unrelated fragment for pinned readers");
    fs::rename(&unavailable_address, &old_address).expect("restore legacy fixture");
    assert_eq!(second.review_counts_watermark, Some(second.watermark));
    assert_eq!(
        crate::UsageReader::open_at(root)
            .expect("restart after small publication")
            .summary(TimeRange::all(), &UsageFilter::default())
            .expect("public summary after small publication")["counts"],
        serde_json::json!({"operations": 2, "modelRequests": 0, "tools": 2,
            "warmups": 0, "unfinished": 2, "unknownIntervals": 2,
            "unclassified": 0, "missingRepository": 0})
    );
    assert_private_projection(root, &second);
    let second = descriptor(root, &second);
    assert_eq!(second.partitions.len(), 2);
    assert_eq!(second.partitions[0], first.partitions[0]);
    // An unrelated operation body is deliberately unavailable. Membership and
    // id indexes must still make the selected repository usable after restart.
    fs::remove_file(root.join("segments").join(&second.partitions[1].file))
        .expect("unavailable unrelated data");
    assert_eq!(
        read_review_counts(root, &second, &ReviewCountScope::Global)
            .expect("global summary needs no operation data")
            .operations,
        2
    );
    assert_eq!(
        read_review_counts(root, &second, &ReviewCountScope::Thread("thread-a".into()))
            .expect("one indexed summary row")
            .operations,
        1
    );
    let filter = UsageFilter::default()
        .for_repository("repository-a")
        .for_thread("thread-a");
    let range =
        TimeRange::from_millis(/*start_ms*/ 4_000, /*end_ms*/ 9_000).expect("finite window");
    assert_eq!(
        select_review_files(root, &second, range, &filter).expect("metadata-only finite selection"),
        vec![first.partitions[0].file.clone()]
    );
    let requested = BTreeSet::from([
        "first-partition".to_string(),
        "not-yet-captured".to_string(),
    ]);
    let connection = Connection::open_in_memory().expect("connection");
    let mut legacy = second.clone();
    legacy.operation_id_index_file = None;
    assert_eq!(
        load_review_operations(root, &legacy, &requested, &connection)
            .expect("optional legacy fallback"),
        all_operations(root, &first)
    );
    for partition in &second.partitions {
        fs::remove_file(root.join("segments").join(&partition.operation_index_file))
            .expect("global exact lookup must not open legacy Parquet indexes");
    }
    let rows = load_review_operations(
        root,
        &second,
        &BTreeSet::from([
            "first-partition".to_string(),
            "not-yet-captured".to_string(),
        ]),
        &Connection::open_in_memory().expect("connection"),
    )
    .expect("indexed canonical operation lookup");
    assert_eq!(rows, all_operations(root, &first));
    assert_eq!(
        select_review_files(
            root,
            &second,
            range,
            &UsageFilter::default()
                .for_repository("repository-b")
                .for_thread("thread-a")
        )
        .expect("dimension intersection"),
        Vec::<String>::new()
    );
}

#[test]
fn explicit_operation_conflicts_leave_published_frontier_and_canonical_journal_intact() {
    for conflict in ["kind", "identity", "timing", "terminal"] {
        let directory = tempdir().expect("directory");
        let root = directory.path();
        let journal = UsageJournal::new(root.join("events.jsonl"));
        let start = started("operation", "tool", /*at_ms*/ 100);
        journal
            .append_batch(vec![
                JournalPayload::Record(start.clone()),
                JournalPayload::Record(finished(
                    &start, /*at_ms*/ 500, /*duration_ms*/ 400,
                )),
            ])
            .expect("canonical lifecycle");
        let mut publisher = UsagePublisher::open_at(root).expect("publisher");
        let before = publisher.publish_batch(limits()).expect("valid projection");
        let mut fact = finished(&start, /*at_ms*/ 500, /*duration_ms*/ 400);
        fact.id = format!("conflict-{conflict}");
        match conflict {
            "kind" => fact.event = Some("model_request_finished".into()),
            "identity" => fact.repository_id = Some("other-repository".into()),
            "timing" => fact.started_at_ms = Some(101),
            "terminal" => fact.outcome = Some(UsageOutcome::Failed),
            _ => unreachable!("declared test cases"),
        }
        journal
            .append_record(fact)
            .expect("retain conflicting acknowledged fact");
        assert!(matches!(
            publisher.publish_batch(limits()),
            Err(crate::StorageError::InvalidData(_))
        ));
        assert_eq!(
            ManifestStore::new(root.join("manifest.json"))
                .load()
                .expect("restart"),
            before
        );
        assert_eq!(journal.checkpoint().expect("canonical head").sequence, 3);
        assert_eq!(all_operations(root, &descriptor(root, &before)).len(), 1);
    }
}

#[test]
fn classification_for_an_unknown_operation_remains_joinable_after_later_lifecycle_facts() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut correction = UsageRecord::new_millis(
        "unknown-target-classification",
        /*observed_at_ms*/ 1_000,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation("later-operation");
    correction.event = Some("classification_correction".into());
    correction.phase = Some("testing".into());
    correction.activity = Some("verification_review".into());
    correction.classification = Some("agent_corrected".into());
    journal
        .append_record(correction)
        .expect("retain explicit unknown target");
    let before = UsagePublisher::open_at(root)
        .expect("publisher")
        .publish_batch(limits())
        .expect("classification may precede target capture");
    let before = descriptor(root, &before);
    assert!(before.partitions.is_empty());
    let mut start = started("later-operation", "tool", /*at_ms*/ 2_000);
    start.phase = None;
    start.activity = None;
    start.classification = None;
    journal
        .append_batch(vec![
            JournalPayload::Record(start.clone()),
            JournalPayload::Record(finished(
                &start, /*at_ms*/ 3_000, /*duration_ms*/ 1_000,
            )),
        ])
        .expect("later canonical lifecycle");
    let after = UsagePublisher::open_at(root)
        .expect("restart publisher")
        .publish_batch(limits())
        .expect("operation becomes resolvable");
    let after = descriptor(root, &after);
    assert_eq!(after.classification_files, before.classification_files);
    let source =
        parquet_source(root, &after.classification_files).expect("retained classification sidecar");
    let rows: Vec<ReviewClassification> = query_rows(
        &Connection::open_in_memory().expect("connection"),
        &format!("SELECT * FROM {source} WHERE operation_id = 'later-operation'"),
        /*max_rows*/ 1,
    )
    .expect("unknown target remains available for operation join");
    assert_eq!(
        (rows[0].phase.as_deref(), rows[0].activity.as_deref()),
        (Some("testing"), Some("verification_review"))
    );
    let state = all_operations(root, &after).remove(0);
    assert_eq!((state.phase, state.activity), (None, None));
    assert_eq!(
        read_review_counts(root, &after, &ReviewCountScope::Global)
            .expect("earlier unknown-target correction classifies later canonical operation"),
        ReviewCounts {
            operations: 1,
            tools: 1,
            tool_completed: 1,
            ..ReviewCounts::default()
        }
    );
}
