use std::path::Path;

use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::JournalReadLimits;
use crate::ParquetSegmentCodec;
use crate::RetentionPolicy;
use crate::SegmentCodec;
use crate::UsageCorrection;
use crate::UsageHeadlinePublisher;
use crate::UsageManifest;
use crate::UsagePublisher;
use crate::UsageRecord;
use crate::UsageSegment;

fn limits() -> JournalReadLimits {
    JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 128,
    }
}

fn observation(id: &str, at_ms: i64, provider_total: Option<u64>) -> UsageRecord {
    let mut record = UsageRecord::new_millis(
        id, at_ms, /*input_tokens*/ 11, /*output_tokens*/ 3,
    )
    .with_repository("repo")
    .with_account("account")
    .with_thread("parent");
    record.event = Some("token_observation".into());
    record.provider_total_tokens = provider_total;
    record.cached_input_tokens = 4;
    record.reasoning_output_tokens = 2;
    record.cache_write_input_tokens = Some(5);
    record
}

fn publish(root: &Path) -> crate::UsageManifest {
    UsagePublisher::open_at(root)
        .expect("canonical publisher")
        .publish_batch(limits())
        .expect("canonical prefix")
}

fn index(root: &Path) -> usize {
    let mut builder = UsageHeadlinePublisher::open_at(root)
        .expect("headline writer")
        .expect("new prefix");
    while !builder.step().expect("bounded headline step") {}
    builder.imported_files()
}

fn copy_fixture(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("copy directory");
    for entry in std::fs::read_dir(source).expect("fixture files") {
        let entry = entry.expect("fixture entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            copy_fixture(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy immutable fixture");
        }
    }
}

#[test]
fn published_tokens_preserve_provider_unknowns_signed_corrections_and_exact_milliseconds() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut first = observation("first", /*at_ms*/ 1_000, Some(9_007_199_254_740_993));
    first.cache_write_input_tokens = Some(9_007_199_254_740_995);
    journal.append_record(first.clone()).expect("capture");
    let mut child = observation("child", /*at_ms*/ 1_001, None);
    child.thread_id = Some("child".into());
    child.parent_thread_id = Some("parent".into());
    child.input_tokens = 7;
    child.output_tokens = 2;
    child.cache_write_input_tokens = None;
    journal.append_record(child).expect("child capture");
    journal.append_record(first).expect("duplicate replay");
    journal
        .append_record(observation("upper-bound", /*at_ms*/ 1_002, Some(1000)))
        .expect("boundary capture");
    journal
        .append_correction(UsageCorrection::new(
            "adjust",
            "first",
            /*observed_at*/ 4,
            /*input_tokens_delta*/ -20,
            /*output_tokens_delta*/ 0,
            "signed correction",
        ))
        .expect("correction");
    let manifest = publish(&root);
    assert_eq!(index(&root), 2);
    let reader = UsageHeadlineReader::open_at(&root)
        .expect("reader")
        .expect("publication");
    let filter = UsageFilter::default()
        .for_repository("repo")
        .for_account("account");
    assert_eq!(
        reader
            .totals(
                TimeRange::from_millis(1_000, 1_002).expect("range"),
                &filter
            )
            .expect("headline"),
        Some(UsageHeadlineTotals {
            input_tokens: -2,
            cached_input_tokens: 8,
            output_tokens: 5,
            reasoning_output_tokens: 4,
            token_observations: 2,
            known_provider_total_tokens: 9_007_199_254_740_993,
            provider_total_unknowns: 1,
            known_cache_write_input_tokens: 9_007_199_254_740_995,
            cache_write_unknowns: 1,
            missing_observations: 0,
            provider_total_tokens: None,
            cache_write_input_tokens: None,
        })
    );
    assert_eq!(
        reader
            .totals(
                TimeRange::from_millis(1_000, 1_001).expect("range"),
                &filter
            )
            .expect("boundary"),
        Some(UsageHeadlineTotals {
            input_tokens: -9,
            cached_input_tokens: 4,
            output_tokens: 3,
            reasoning_output_tokens: 2,
            token_observations: 1,
            known_provider_total_tokens: 9_007_199_254_740_993,
            provider_total_unknowns: 0,
            known_cache_write_input_tokens: 9_007_199_254_740_995,
            cache_write_unknowns: 0,
            missing_observations: 0,
            provider_total_tokens: Some(9_007_199_254_740_993),
            cache_write_input_tokens: Some(9_007_199_254_740_995),
        })
    );
    assert_eq!(
        reader.snapshot().source_watermark.sequence,
        manifest.watermark
    );
    assert_eq!(reader.snapshot().pending_entries, 0);
    journal
        .append_record(observation("pending", /*at_ms*/ 1_001, Some(600)))
        .expect("unpublished capture");
    // The headline must remain usable when rich metadata is unavailable.
    std::fs::rename(root.join("manifest.json"), root.join("cold-manifest.json"))
        .expect("hide rich metadata");
    let later = UsageHeadlineReader::open_at(&root)
        .expect("small reader")
        .expect("existing headline");
    assert_eq!(
        (
            later.snapshot().source_watermark.sequence,
            later.snapshot().pending_entries,
            later.snapshot().freshness
        ),
        (manifest.watermark, 1, Freshness::Stale)
    );
    assert_eq!(
        later
            .totals(TimeRange::all(), &filter)
            .expect("published only"),
        reader
            .totals(TimeRange::all(), &filter)
            .expect("same publication")
    );
}

#[test]
fn incremental_reconciliation_survives_interruption_and_preserves_pinned_publication() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_record(observation("one", /*at_ms*/ 1_000, Some(97)))
        .expect("capture");
    publish(&root);
    assert_eq!(index(&root), 1);
    let old = UsageHeadlineReader::open_at(&root)
        .expect("reader")
        .expect("old publication");
    let old_id = old.snapshot().snapshot_id.clone();
    journal
        .append_record(observation("two", /*at_ms*/ 1_001, Some(101)))
        .expect("capture");
    publish(&root);
    {
        let mut interrupted = UsageHeadlinePublisher::open_at(&root)
            .expect("writer")
            .expect("new source");
        assert!(!interrupted.step().expect("commit one bounded import"));
        assert_eq!(interrupted.imported_files(), 1);
        assert!(
            UsageHeadlinePublisher::open_at(&root)
                .expect("exclusive writer")
                .is_none()
        );
    }
    assert_eq!(
        UsageHeadlineReader::open_at(&root)
            .expect("reader")
            .expect("old snapshot until install")
            .snapshot()
            .snapshot_id,
        old_id
    );
    // Restart reuses the already committed new file; the older file is never decoded again.
    assert_eq!(index(&root), 0);
    let current = UsageHeadlineReader::open_at(&root)
        .expect("reader")
        .expect("new publication");
    assert_ne!(current.snapshot().snapshot_id, old_id);
    let filter = UsageFilter::default().for_thread("parent");
    assert_eq!(
        old.totals(TimeRange::all(), &filter)
            .expect("old exact tokens")
            .expect("exact")
            .provider_total_tokens,
        Some(97)
    );
    assert_eq!(
        current
            .totals(TimeRange::all(), &filter)
            .expect("new exact tokens")
            .expect("exact")
            .provider_total_tokens,
        Some(198)
    );
    crate::reclamation::reclaim(&root).expect("raw reclamation remains independent");
    assert!(
        root.join("headline-segments")
            .join(format!("headline-{old_id}.parquet"))
            .exists()
    );
    drop(old);
    drop(current);
    assert!(
        UsageHeadlinePublisher::open_at(&root)
            .expect("reclaim released publication")
            .is_none()
    );
    assert!(
        !root
            .join("headline-segments")
            .join(format!("headline-{old_id}.parquet"))
            .exists()
    );
    codex_private_storage::verify_private_file(&root.join("headline-writer.duckdb"))
        .expect("private retained writer");
    codex_private_storage::verify_private_file(&root.join("headline.json"))
        .expect("private small descriptor");
}

#[test]
fn retired_aggregate_boundaries_and_expired_corrections_are_explicit() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_record(observation("one", /*at_ms*/ 1_000, Some(97)))
        .expect("capture");
    journal
        .append_record(observation("two", /*at_ms*/ 2_000, Some(101)))
        .expect("capture");
    for (id, at_ms) in [("lifecycle-before", 500), ("lifecycle-after", 2_500)] {
        journal
            .append_record(
                UsageRecord::new_millis(
                    id, at_ms, /*input_tokens*/ 0, /*output_tokens*/ 0,
                )
                .with_repository("repo")
                .with_account("account")
                .with_thread("parent"),
            )
            .expect("unrelated lifecycle coverage");
    }
    publish(&root);
    assert_eq!(index(&root), 1);
    let now = 12 * 86_400_000;
    assert!(
        crate::token_rollup::compact_one(&root, RetentionPolicy::default(), now)
            .expect("retire raw detail")
    );
    assert_eq!(index(&root), 1);
    let reader = UsageHeadlineReader::open_at(&root)
        .expect("reader")
        .expect("retained projection");
    let filter = UsageFilter::default().for_repository("repo");
    assert_eq!(
        reader
            .totals(
                TimeRange::from_millis(/*start_ms*/ 1_500, /*end_ms*/ 1_500)
                    .expect("empty range inside aggregate"),
                &filter
            )
            .expect("exact empty selection"),
        reader
            .totals(
                TimeRange::from_millis(/*start_ms*/ 3_000, /*end_ms*/ 3_001)
                    .expect("empty selection outside aggregate"),
                &filter
            )
            .expect("no selected observations")
    );
    assert_eq!(
        reader
            .totals(
                TimeRange::from_millis(1_000, 2_001).expect("range"),
                &filter
            )
            .expect("whole aggregate")
            .expect("exact")
            .provider_total_tokens,
        Some(198)
    );
    assert_eq!(
        reader
            .totals(
                TimeRange::from_millis(1_500, 2_001).expect("range"),
                &filter
            )
            .expect("bisected aggregate"),
        None
    );
    drop(reader);
    journal
        .append_correction(UsageCorrection::new(
            "too-late",
            "one",
            now / 1000,
            /*input_tokens_delta*/ -2,
            /*output_tokens_delta*/ 0,
            "target retired",
        ))
        .expect("late correction");
    publish(&root);
    assert_eq!(index(&root), 1);
    let reader = UsageHeadlineReader::open_at(&root)
        .expect("reader")
        .expect("correction coverage");
    assert_eq!(reader.snapshot().unapplied_retention_corrections, 1);
    assert_eq!(
        reader
            .totals(TimeRange::all(), &filter)
            .expect("retained values")
            .expect("exact")
            .input_tokens,
        22
    );
}

#[test]
fn bootstrap_commits_bounded_batches_and_resumes_without_redecoding() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    crate::storage_file::ensure_usage_directory(&root).expect("owned directory");
    let mut manifest = UsageManifest::default();
    for index in 0..65 {
        let file = format!("segment-{index}.parquet");
        let segment = UsageSegment::new(
            /*segment_hour*/ 0,
            vec![observation(
                &format!("record-{index}"),
                /*at_ms*/ 1_000,
                Some(1),
            )],
        )
        .expect("segment");
        std::fs::write(
            root.join("segments").join(&file),
            ParquetSegmentCodec.encode(&segment).expect("encode"),
        )
        .expect("immutable fixture");
        manifest.segment_files.push(file);
    }
    manifest.mark_snapshot(/*watermark*/ 0, Freshness::Fresh);
    crate::ManifestStore::new(root.join("manifest.json"))
        .save(&manifest)
        .expect("source snapshot");
    {
        let mut builder = UsageHeadlinePublisher::open_at(&root)
            .expect("writer")
            .expect("bootstrap");
        assert!(!builder.step().expect("one batch"));
        assert_eq!(builder.imported_files(), 64);
        assert!(
            UsageHeadlineReader::open_at(&root)
                .expect("reader")
                .is_none()
        );
    }
    assert_eq!(index(&root), 1);
    let reader = UsageHeadlineReader::open_at(&root)
        .expect("reader")
        .expect("complete publication");
    assert_eq!(
        reader
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("tokens")
            .expect("exact")
            .provider_total_tokens,
        Some(65)
    );
}

#[test]
fn overlapping_headline_views_allow_publication_and_reclaim_only_unpinned_files() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_record(observation("first", /*at_ms*/ 1_000, Some(1)))
        .expect("capture");
    publish(&root);
    index(&root);
    let oldest = UsageHeadlineReader::open_at(&root)
        .expect("reader")
        .expect("first view");
    let old_file = root.join("headline-segments").join(format!(
        "headline-{}.parquet",
        oldest.snapshot().snapshot_id
    ));
    let mut current = UsageHeadlineReader::open_at(&root)
        .expect("reader")
        .expect("overlapping view");
    // Exceed the publication cap while keeping two overlapping views alive.
    for index_number in 1..20 {
        journal
            .append_record(observation(
                &format!("record-{index_number}"),
                /*at_ms*/ 1_000 + index_number,
                Some(1),
            ))
            .expect("new capture");
        publish(&root);
        assert_eq!(index(&root), 1);
        let next = UsageHeadlineReader::open_at(&root)
            .expect("reader")
            .expect("next view");
        drop(current);
        current = next;
        assert!(old_file.exists());
    }
    assert_eq!(
        oldest
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("old view")
            .expect("exact")
            .provider_total_tokens,
        Some(1)
    );
    assert_eq!(
        current
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("current view")
            .expect("exact")
            .provider_total_tokens,
        Some(20)
    );
    drop(oldest);
    assert!(
        UsageHeadlinePublisher::open_at(&root)
            .expect("reclaim released old view")
            .is_none()
    );
    assert!(!old_file.exists());
    assert_eq!(
        current
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("current survives cleanup")
            .expect("exact")
            .provider_total_tokens,
        Some(20)
    );
}

#[test]
fn relocated_seed_reuses_indexed_facts_and_only_imports_new_canonical_files() {
    let home = tempdir().expect("home");
    let source = home.path().join("source").join(crate::USAGE_DIRECTORY_NAME);
    UsageJournal::new(source.join("events.jsonl"))
        .append_record(observation("first", /*at_ms*/ 1_000, Some(97)))
        .expect("capture");
    publish(&source);
    assert_eq!(index(&source), 1);
    let target = home
        .path()
        .join("relocated")
        .join(crate::USAGE_DIRECTORY_NAME);
    copy_fixture(&source, &target);
    let imported = UsageHeadlinePublisher::open_at(&target)
        .expect("relocated writer")
        .map_or(0, |mut builder| {
            while !builder.step().expect("reuse relocated state") {}
            builder.imported_files()
        });
    assert_eq!(imported, 0);
    UsageJournal::new(target.join("events.jsonl"))
        .append_record(observation("second", /*at_ms*/ 1_001, Some(101)))
        .expect("new target capture");
    publish(&target);
    assert_eq!(index(&target), 1);
    let old = UsageHeadlineReader::open_at(&source)
        .expect("source reader")
        .expect("unchanged source");
    let new = UsageHeadlineReader::open_at(&target)
        .expect("target reader")
        .expect("advanced target");
    assert_eq!(
        old.totals(TimeRange::all(), &UsageFilter::default())
            .expect("source")
            .expect("exact")
            .provider_total_tokens,
        Some(97)
    );
    assert_eq!(
        new.totals(TimeRange::all(), &UsageFilter::default())
            .expect("target")
            .expect("exact")
            .provider_total_tokens,
        Some(198)
    );
}
