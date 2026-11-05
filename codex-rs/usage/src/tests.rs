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
    store
        .append_record(
            record("one", 3_605, 8, 1)
                .with_account("account-a")
                .with_thread("thread-a")
                .with_repository("repo-a"),
        )
        .expect("append one");
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
}

#[test]
#[ignore = "large acceptance fixture; run explicitly with --ignored"]
fn synthetic_volume_fixture_covers_rebase_acceptance_scale() {
    let directory = tempdir().expect("temporary directory");
    let mut store = UsageStore::open(directory.path()).expect("open store");
    for index in 0..3_000_000_u32 {
        store
            .append_record(
                UsageRecord::new(format!("operation-{index}"), i64::from(index), 10, 5)
                    .with_account(format!("account-{}", index % 8))
                    .with_thread(format!("thread-{}", index % 256))
                    .with_repository(format!("repository-{}", index % 16)),
            )
            .expect("append synthetic record");
    }
    store
        .materialize_snapshot(Freshness::Fresh)
        .expect("materialize synthetic fixture");
    assert_eq!(
        store
            .totals(TimeRange::all(), &UsageFilter::default())
            .records,
        3_000_000
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
