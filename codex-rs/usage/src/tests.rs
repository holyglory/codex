use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;

fn record(id: &str, observed_at: i64, input_tokens: u64, output_tokens: u64) -> UsageRecord {
    UsageRecord::new(id, observed_at, input_tokens, output_tokens)
}

#[test]
fn journal_is_append_only_and_replays_corrections() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let journal = UsageJournal::new(&path);

    let first = journal
        .append_record(record("first", 3_600, 10, 4))
        .expect("append first record");
    let second = journal
        .append_correction(UsageCorrection::new(
            "fix-first",
            "first",
            3_700,
            -2,
            1,
            "provider reconciliation",
        ))
        .expect("append correction");

    assert_eq!(first.sequence, 1);
    assert_eq!(second.sequence, 2);
    assert_eq!(journal.last_sequence().expect("last sequence"), 2);
    assert_eq!(
        journal.read_after(1).expect("read after watermark").len(),
        1
    );
    assert_eq!(journal.records().expect("records").len(), 1);
    assert_eq!(journal.corrections().expect("corrections").len(), 1);

    let lines = std::fs::read_to_string(path).expect("journal bytes");
    assert_eq!(lines.lines().count(), 2);
    assert_eq!(journal.replay().expect("replay").len(), 2);
}

#[test]
fn capture_remains_durable_when_report_metadata_is_unreadable() {
    let directory = tempdir().expect("temporary directory");
    let root = directory.path().join(USAGE_DIRECTORY_NAME);
    std::fs::create_dir(&root).expect("usage directory");
    // Capture must not consult either the new report snapshot or retired history.
    std::fs::write(root.join("manifest.json"), b"incomplete snapshot").expect("manifest");
    let retired = directory.path().join("usage.sqlite3");
    std::fs::write(&retired, b"retired history").expect("retired history");
    let observation = record("captured", 3_600, 10, 4).with_account("local-profile");
    capture_record(directory.path(), observation.clone()).expect("capture");
    assert_eq!(
        UsageJournal::new(root.join("events.jsonl"))
            .records()
            .expect("restart journal"),
        vec![observation]
    );
    assert_eq!(
        std::fs::read(retired).expect("retired file"),
        b"retired history"
    );
}

#[test]
fn manifest_round_trip_tracks_generation_and_pending_corrections() {
    let directory = tempdir().expect("temporary directory");
    let store = ManifestStore::new(directory.path().join("manifest.json"));
    let mut manifest = UsageManifest::new(Freshness::Stale);
    manifest.add_pending_correction("fix-1");
    manifest.mark_snapshot(7, Freshness::Fresh);
    store.save(&manifest).expect("save manifest");

    let loaded = store.load().expect("load manifest");
    assert_eq!(loaded.snapshot_generation, 1);
    assert_eq!(loaded.watermark, 7);
    assert_eq!(loaded.freshness, Freshness::Fresh);
    assert_eq!(loaded.pending_corrections, vec!["fix-1"]);
    assert!(loaded.updated_at > 0);
    assert!(store.path().is_file());
    let first = store.load_shared().expect("pin parsed metadata");
    let repeated = store.load_shared().expect("reuse parsed metadata");
    assert!(std::sync::Arc::ptr_eq(&first.value, &repeated.value));
    assert_eq!(first.revision, repeated.revision);

    // A reset or replaced store can reuse its generation/watermark. Its cache
    // identity must still change, and old pinned readers must remain valid.
    manifest.snapshot_totals.input_tokens = 123;
    store.save(&manifest).expect("replace same generation");
    let replaced = store.load_shared().expect("reload changed identity");
    assert_eq!(replaced.value.as_ref(), &manifest);
    assert_ne!(first.revision, replaced.revision);
    assert_eq!(first.value.as_ref(), &loaded);
}

#[test]
fn immutable_hourly_segment_validates_and_round_trips_through_jsonl() {
    let segment = UsageSegment::new(
        7_200,
        vec![record("late", 7_230, 3, 5), record("early", 7_205, 1, 2)],
    )
    .expect("segment");
    assert_eq!(segment.records()[0].id, "early");
    assert_eq!(
        segment.totals(
            TimeRange::new(7_200, 7_300).unwrap(),
            &UsageFilter::default()
        ),
        UsageTotals {
            input_tokens: 4,
            cached_input_tokens: 0,
            output_tokens: 7,
            reasoning_output_tokens: 0,
            records: 2,
        }
    );

    let codec = JsonLinesSegmentCodec;
    let encoded = codec.encode(&segment).expect("encode segment");
    let decoded = codec.decode(&encoded).expect("decode segment");
    assert_eq!(decoded, segment);
    assert!(
        UsageSegment::new(
            7_200,
            vec![record("wrong-hour", 7_200 + HOUR_SECONDS, 1, 1)]
        )
        .is_err()
    );
    assert_eq!(ArrowSegmentCodec.format(), SegmentFormat::Arrow);
    assert!(ArrowSegmentCodec.encode(&segment).is_err());
}

#[test]
fn in_memory_store_queries_snapshot_tail_and_corrections() {
    let snapshot_record = record("snapshot", 3_610, 10, 2).with_account("one");
    let duplicate = record("snapshot", 3_620, 100, 100).with_account("one");
    let tail_record = record("tail", 7_210, 5, 3).with_account("one");
    let other_account = record("other", 7_220, 50, 50).with_account("two");
    let first_segment = UsageSegment::from_records(vec![snapshot_record.clone()]).expect("segment");
    let snapshot =
        UsageSnapshot::new(UsageManifest::default(), vec![first_segment]).expect("snapshot");
    let mut store = InMemoryUsageStore::new(snapshot);
    store.append(duplicate);
    store.append(tail_record.clone());
    store.append(other_account);
    store.append_correction(UsageCorrection::new(
        "fix-tail",
        "tail",
        7_300,
        -1,
        2,
        "late reconciliation",
    ));

    let range = TimeRange::new(3_600, 7_300).expect("range");
    let filter = UsageFilter::default().for_account("one");
    let rows = store.query(range, &filter);
    assert_eq!(rows, vec![snapshot_record, tail_record]);
    assert_eq!(
        store.totals(range, &filter),
        UsageTotals {
            input_tokens: 14,
            cached_input_tokens: 0,
            output_tokens: 7,
            reasoning_output_tokens: 0,
            records: 2,
        }
    );
    assert_eq!(store.pending_corrections().len(), 1);
    assert_eq!(store.take_tail().len(), 3);
    assert!(store.tail().is_empty());
}

#[test]
fn durable_store_replays_tail_and_materialized_segments_after_restart() {
    let directory = tempdir().expect("temporary directory");
    let first_record = record("first", 3_605, 8, 1).with_model("gpt");
    let second_record = record("second", 7_205, 4, 2).with_model("gpt");
    let correction = UsageCorrection::new("fix-second", "second", 7_250, -1, 1, "late billing");

    {
        let mut store = UsageStore::open(directory.path()).expect("open store");
        assert_eq!(
            store.root().file_name().and_then(|name| name.to_str()),
            Some(USAGE_DIRECTORY_NAME)
        );
        store
            .append_record(first_record.clone())
            .expect("append first");
        store
            .append_record(second_record.clone())
            .expect("append second");
        store
            .append_correction(correction)
            .expect("append correction");
        assert_eq!(store.manifest().snapshot_generation, 0);
        assert_eq!(store.pending_corrections().len(), 1);
        store
            .materialize_snapshot(Freshness::Fresh)
            .expect("materialize snapshot");
        assert_eq!(store.manifest().correction_files.len(), 1);
    }

    let mut restarted = UsageStore::open(directory.path()).expect("reopen store");
    assert_eq!(restarted.manifest().snapshot_generation, 1);
    assert_eq!(restarted.manifest().watermark, 3);
    assert_eq!(restarted.manifest().freshness, Freshness::Fresh);
    codex_private_storage::verify_private_directory(restarted.root())
        .expect("private recovered store");
    codex_private_storage::verify_private_directory(&restarted.root().join("segments"))
        .expect("private recovered segments");
    for file in ["events.jsonl", "manifest.json", ".snapshot.lock"] {
        codex_private_storage::verify_private_file(&restarted.root().join(file))
            .expect("private recovered metadata");
    }
    for file in restarted
        .manifest()
        .segment_files
        .iter()
        .chain(&restarted.manifest().correction_files)
    {
        codex_private_storage::verify_private_file(&restarted.root().join("segments").join(file))
            .expect("private recovered Parquet");
    }
    assert_eq!(
        restarted.query(TimeRange::all(), &UsageFilter::default()),
        vec![first_record, second_record]
    );
    assert_eq!(
        restarted.totals(TimeRange::all(), &UsageFilter::default()),
        UsageTotals {
            input_tokens: 11,
            cached_input_tokens: 0,
            output_tokens: 4,
            reasoning_output_tokens: 0,
            records: 2,
        }
    );

    restarted
        .append_record(record("tail", 7_300, 2, 3))
        .expect("append after restart");
    assert_eq!(restarted.memory().tail().len(), 1);
}

#[test]
fn correction_segments_round_trip_through_parquet() {
    let correction = UsageCorrection::new("fix", "record", 7_205, -2, 1, "late fact");
    let segment =
        UsageCorrectionSegment::new(7_200, vec![correction.clone()]).expect("correction segment");
    let codec = ParquetCorrectionSegmentCodec;
    let encoded = codec
        .encode_corrections(&segment)
        .expect("encode corrections");
    let decoded = codec
        .decode_corrections(&encoded)
        .expect("decode corrections");
    assert_eq!(decoded.corrections(), &[correction]);
}

#[test]
fn materialization_reuses_untouched_hour_segments() {
    let directory = tempdir().expect("temporary directory");
    let first = record("first", 3_605, 8, 1);
    let second = record("second", 7_205, 4, 2);
    let mut store = UsageStore::open(directory.path()).expect("open store");
    store.append_record(first).expect("append first");
    let first_manifest = store
        .materialize_snapshot(Freshness::Fresh)
        .expect("first snapshot");
    store.append_record(second).expect("append second");
    let second_manifest = store
        .materialize_snapshot(Freshness::Fresh)
        .expect("second snapshot");
    assert_eq!(
        first_manifest.segment_files[0],
        second_manifest.segment_files[0]
    );
    assert_eq!(second_manifest.segment_files.len(), 2);
}

#[test]
fn materialized_dimension_totals_are_mergeable_without_scanning_rows() {
    let directory = tempdir().expect("temporary directory");
    let mut store = UsageStore::open(directory.path()).expect("open store");
    let mut expected = record("one", 3_605, 8, 1)
        .with_account("account-a")
        .with_thread("thread-a")
        .with_repository("repo-a");
    expected.parent_thread_id = Some("parent".to_string());
    expected.turn_id = Some("turn-a".to_string());
    expected.response_id = Some("response-a".to_string());
    expected.outcome = Some(UsageOutcome::Completed);
    expected.duration_ms = Some(42);
    expected.attempt_number = Some(2);
    expected.provider_total_tokens = Some(9);
    expected.cache_write_input_tokens = Some(3);
    expected.missing_observations = Some(4);
    expected.rework_of_operation_id = Some("previous-model-request".to_string());
    expected.observed_at_ms = Some(3_605_123);
    expected.started_at = Some(3_605);
    expected.started_at_ms = Some(3_605_081);
    expected.ended_at = Some(3_605);
    expected.ended_at_ms = Some(3_605_123);
    store.append_record(expected.clone()).expect("append one");
    store
        .materialize_snapshot(Freshness::Fresh)
        .expect("materialize");
    assert_eq!(
        store.totals(
            TimeRange::all(),
            &UsageFilter::default().for_account("account-a")
        ),
        UsageTotals {
            input_tokens: 8,
            cached_input_tokens: 0,
            output_tokens: 1,
            reasoning_output_tokens: 0,
            records: 1,
        }
    );
    assert_eq!(
        store.totals(
            TimeRange::all(),
            &UsageFilter::default().for_thread("thread-a")
        ),
        store.totals(
            TimeRange::all(),
            &UsageFilter::default().for_repository("repo-a")
        )
    );
    assert_eq!(
        DuckDbSnapshot
            .totals(
                &store.root().join("segments"),
                &store.manifest().segment_files,
                TimeRange::all(),
                &UsageFilter::default().for_repository("repo-a"),
            )
            .expect("duckdb snapshot totals"),
        UsageTotals {
            input_tokens: 8,
            cached_input_tokens: 0,
            output_tokens: 1,
            reasoning_output_tokens: 0,
            records: 1,
        }
    );
    assert_eq!(
        DuckDbSnapshot
            .query(
                &store.root().join("segments"),
                &store.manifest().segment_files,
                TimeRange::all(),
                &UsageFilter::default().for_repository("repo-a"),
            )
            .expect("duckdb snapshot rows"),
        vec![expected.clone()]
    );
    let segment = UsageSegment::from_records(vec![expected.clone()]).expect("timed segment");
    let encoded = ParquetSegmentCodec
        .encode(&segment)
        .expect("encode timed segment");
    assert_eq!(
        ParquetSegmentCodec
            .decode(&encoded)
            .expect("decode timed segment"),
        segment
    );
    let mut batches = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::from(encoded),
    )
    .expect("read current schema")
    .build()
    .expect("read current rows");
    let batch = batches.next().expect("timed batch").expect("decode batch");
    let legacy = batch
        .project(&(0..batch.num_columns() - 3).collect::<Vec<_>>())
        .expect("legacy seconds schema");
    let mut legacy_bytes = Vec::new();
    let mut writer = parquet::arrow::ArrowWriter::try_new(
        &mut legacy_bytes,
        legacy.schema(),
        /*props*/ None,
    )
    .expect("legacy writer");
    writer.write(&legacy).expect("legacy batch");
    writer.close().expect("close legacy Parquet");
    expected.observed_at_ms = None;
    expected.started_at_ms = None;
    expected.ended_at_ms = None;
    assert_eq!(
        ParquetSegmentCodec
            .decode(&legacy_bytes)
            .expect("legacy decode")
            .records(),
        &[expected.clone()]
    );
    let name = "legacy-seconds.parquet".to_string();
    std::fs::write(store.root().join("segments").join(&name), legacy_bytes)
        .expect("legacy fixture");
    assert_eq!(
        DuckDbSnapshot
            .query(
                &store.root().join("segments"),
                &[name],
                TimeRange::from_millis(/*start_ms*/ 3_605_000, /*end_ms*/ 3_605_001)
                    .expect("legacy ms range"),
                &UsageFilter::default()
            )
            .expect("legacy SQL fallback"),
        vec![expected]
    );
}

#[test]
fn materialization_reconciles_records_written_through_another_journal_handle() {
    let directory = tempdir().expect("temporary directory");
    let first_record = record("first", 3_605, 8, 1);
    let second_record = record("second", 7_205, 4, 2);

    {
        let mut store = UsageStore::open(directory.path()).expect("open store");
        store
            .append_record(first_record.clone())
            .expect("append first");
        store
            .journal()
            .clone()
            .append_record(second_record.clone())
            .expect("append through external handle");
        store
            .materialize_snapshot(Freshness::Fresh)
            .expect("materialize snapshot");
    }

    let restarted = UsageStore::open(directory.path()).expect("reopen store");
    assert_eq!(restarted.manifest().watermark, 2);
    assert_eq!(
        restarted.query(TimeRange::all(), &UsageFilter::default()),
        vec![first_record, second_record]
    );
}

#[test]
fn stale_publishers_reconcile_new_generation_and_journal_only_corrections() {
    let directory = tempdir().expect("temporary directory");
    let mut first = UsageStore::open(directory.path()).expect("first publisher");
    let mut second = UsageStore::open(directory.path()).expect("second publisher");
    first
        .append_record(record("first", 3_610, 8, 1))
        .expect("first record");
    second
        .append_record(record("second", 7_210, 4, 2))
        .expect("second record");
    let published = first
        .materialize_snapshot(Freshness::Fresh)
        .expect("publish first");
    second
        .append_correction(UsageCorrection::new(
            "fix-second",
            "second",
            7_220,
            -1,
            1,
            "provider reconciliation",
        ))
        .expect("correction");
    // A writer with stale report metadata may only append to the journal.
    assert_eq!(
        ManifestStore::new(first.root().join("manifest.json"))
            .load()
            .expect("published manifest"),
        published
    );
    second
        .materialize_snapshot(Freshness::Fresh)
        .expect("publish second");
    capture_record(directory.path(), record("tail", 7_230, 2, 3)).expect("capture tail");
    let restarted = UsageStore::open(directory.path()).expect("restart");
    assert_eq!(restarted.manifest().snapshot_generation, 2);
    assert_eq!(
        restarted.memory().tail().records(),
        &[record("tail", 7_230, 2, 3)]
    );
    assert_eq!(
        restarted.totals(TimeRange::all(), &UsageFilter::default()),
        UsageTotals {
            input_tokens: 13,
            cached_input_tokens: 0,
            output_tokens: 7,
            reasoning_output_tokens: 0,
            records: 3,
        }
    );
}

#[test]
fn snapshot_reader_pins_rows_and_applies_late_corrections_without_full_replay() {
    let directory = tempfile::tempdir().expect("directory");
    let mut writer = UsageStore::open(directory.path()).expect("writer");
    let first = UsageRecord::new_millis(
        "z-first", /*observed_at_ms*/ 1_100, /*input_tokens*/ 10,
        /*output_tokens*/ 2,
    )
    .with_thread("thread's id");
    let second = UsageRecord::new_millis(
        "a-second", /*observed_at_ms*/ 1_900, /*input_tokens*/ 20,
        /*output_tokens*/ 3,
    )
    .with_thread("thread's id");
    writer.append_record(first.clone()).expect("first");
    UsagePublisher::open_at(writer.root())
        .expect("independent publisher")
        .publish_batch(JournalReadLimits {
            max_bytes: 8 * 1024 * 1024,
            max_entries: 4096,
        })
        .expect("publish with production identity index");
    let pinned = UsageReader::open(directory.path()).expect("pinned reader");
    writer.append_record(second.clone()).expect("second");
    writer
        .append_correction(UsageCorrection::new(
            "correction",
            "z-first",
            3,
            -2,
            1,
            "late fact",
        ))
        .expect("correction");
    let current = UsageReader::open(directory.path()).expect("current reader");
    let filter = UsageFilter::default().for_thread("thread's id");
    let subsecond = TimeRange::from_millis(/*start_ms*/ 1_100, /*end_ms*/ 1_900)
        .expect("half-open milliseconds");
    assert_eq!(
        current
            .page(
                subsecond,
                &filter,
                RecordKind::All,
                /*after*/ None,
                /*limit*/ 10
            )
            .expect("subsecond observations")
            .records,
        vec![first.clone()]
    );
    assert_eq!(
        current
            .totals(subsecond, &filter)
            .expect("subsecond corrected totals"),
        UsageTotals {
            input_tokens: 8,
            output_tokens: 3,
            records: 1,
            ..UsageTotals::default()
        }
    );
    assert_eq!(
        pinned
            .totals(TimeRange::all(), &filter)
            .expect("pinned totals"),
        first.totals()
    );
    assert_eq!(
        current
            .totals(TimeRange::all(), &filter)
            .expect("current totals"),
        UsageTotals {
            input_tokens: 28,
            output_tokens: 6,
            records: 2,
            ..UsageTotals::default()
        }
    );
    let first_page = current
        .page(
            TimeRange::all(),
            &filter,
            RecordKind::All,
            /*after*/ None,
            /*limit*/ 1,
        )
        .expect("first page");
    assert_eq!(first_page.records, vec![first]);
    assert_eq!(
        first_page.next,
        Some(RecordCursor::from_millis(
            /*observed_at_ms*/ 1_100, "z-first"
        ))
    );
    let second_page = current
        .page(
            TimeRange::all(),
            &filter,
            RecordKind::All,
            first_page.next.as_ref(),
            /*limit*/ 1,
        )
        .expect("second page");
    assert_eq!(second_page.records, vec![second]);
    assert!(second_page.next.is_none());
}

#[test]
fn snapshot_reader_does_not_decode_historical_segments_or_hide_query_errors() {
    let directory = tempfile::tempdir().expect("directory");
    let mut writer = UsageStore::open(directory.path()).expect("writer");
    let record = UsageRecord::new("record", 1, 10, 2).with_thread("thread");
    writer.append_record(record.clone()).expect("record");
    let manifest = writer
        .materialize_snapshot(Freshness::Fresh)
        .expect("publish");
    std::fs::write(
        writer
            .root()
            .join("segments")
            .join(&manifest.segment_files[0]),
        b"corrupt parquet",
    )
    .expect("corrupt fixture");
    let reader = UsageReader::open(directory.path()).expect("metadata-only open");
    assert_eq!(
        reader
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("precomputed totals"),
        record.totals()
    );
    assert!(
        reader
            .page(
                TimeRange::all(),
                &UsageFilter::default(),
                RecordKind::All,
                /*after*/ None,
                /*limit*/ 1
            )
            .is_err()
    );
}

#[test]
fn snapshot_reader_serves_consistent_snapshot_while_large_tail_awaits_publication() {
    let directory = tempfile::tempdir().expect("directory");
    let writer = UsageStore::open(directory.path()).expect("writer");
    writer
        .journal()
        .append_batch(
            (0..5000)
                .map(|id| JournalPayload::Record(UsageRecord::new(id.to_string(), 1, 1, 1)))
                .collect(),
        )
        .expect("append backlog");
    let reader = UsageReader::open(directory.path()).expect("bounded open");
    assert_eq!(
        (reader.pending_entries(), reader.freshness()),
        (5000, Freshness::Stale)
    );
    assert_eq!(
        reader
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("published totals"),
        UsageTotals::default()
    );
}
