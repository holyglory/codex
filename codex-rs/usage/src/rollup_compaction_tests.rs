use std::collections::BTreeSet;

use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::JournalPayload;
use crate::JournalReadLimits;
use crate::RetentionPolicy;
use crate::TimeRange;
use crate::UsageCorrection;
use crate::UsageFilter;
use crate::UsageJournal;
use crate::UsagePublisher;
use crate::UsageReader;
use crate::UsageRecord;
use crate::UsageTotals;

const DAY: i64 = 86_400;
const NOW: i64 = 12 * DAY * 1_000;
type Bucket = (
    String,
    String,
    Option<i64>,
    i64,
    i64,
    u64,
    i64,
    i64,
    i64,
    i64,
);

fn limits() -> JournalReadLimits {
    JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    }
}

fn policy() -> RetentionPolicy {
    RetentionPolicy {
        raw_retention_days: 7,
        inactive_chat_days: 2,
    }
}

fn seed_rollups(root: &Path) -> crate::UsageManifest {
    let journal = UsageJournal::new(root.join("events.jsonl"));
    for hour in 0..10 {
        for (thread, model) in [("idle", "model-a"), ("active", "model-b")] {
            let mut record = UsageRecord::new(
                format!("{thread}-{hour}"),
                DAY + hour * 3_600,
                /*input_tokens*/ 10,
                /*output_tokens*/ 4,
            )
            .with_thread(thread)
            .with_model(model)
            .with_account("account-a")
            .with_repository("repo-a");
            record.cached_input_tokens = 3;
            record.reasoning_output_tokens = 2;
            journal.append_record(record).expect("hourly capture");
        }
    }
    journal
        .append_record(
            UsageRecord::new(
                "recent",
                11 * DAY,
                /*input_tokens*/ 5,
                /*output_tokens*/ 1,
            )
            .with_thread("active")
            .with_model("model-b"),
        )
        .expect("keep one chat active");
    journal
        .append_correction(UsageCorrection::new(
            "adjust-idle",
            "idle-0",
            2 * DAY,
            /*input_tokens_delta*/ -15,
            /*output_tokens_delta*/ 0,
            "preserve signed aggregate values",
        ))
        .expect("correct a retired bucket below zero");
    UsagePublisher::open_at(root)
        .expect("publisher")
        .publish_batch(limits())
        .expect("publish captures");
    let hourly = RetentionPolicy {
        inactive_chat_days: 100,
        ..policy()
    };
    for _ in 0..10 {
        assert!(crate::token_rollup::compact_one(root, hourly, NOW).expect("hourly rollup"));
    }
    let store = ManifestStore::new(root.join("manifest.json"));
    let before_review = store.load().expect("raw retirement manifest");
    assert!(
        crate::review_retention::maintain(
            root,
            hourly.raw_cutoff(NOW),
            hourly.inactive_cutoff(NOW),
        )
        .expect("retire the review rows released by the final raw rollup")
    );
    let retained = store.load().expect("manifest");
    assert_eq!(
        retained.snapshot_generation,
        before_review.snapshot_generation + 1
    );
    retained
}

fn buckets(root: &Path) -> Vec<Bucket> {
    let reader = UsageReader::open_at(root).expect("pinned rollups");
    let source = parquet_source(root, &reader.manifest().token_rollup_files).expect("source");
    let connection = crate::duckdb_connection::open_in_memory().expect("query connection");
    let mut statement = connection
        .prepare(&format!(
            "SELECT thread_id,model,hour_at_ms::BIGINT,first_at_ms,last_at_ms,records,input_tokens,\
         cached_input_tokens,output_tokens,reasoning_output_tokens FROM {source} \
         ORDER BY thread_id,model,hour_at_ms NULLS FIRST"
        ))
        .expect("bucket query");
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
            ))
        })
        .expect("buckets")
        .collect::<std::result::Result<_, _>>()
        .expect("bucket rows")
}

#[test]
fn merge_is_bounded_and_keeps_pinned_buckets_and_counters() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let before = seed_rollups(&root);
    let original_buckets = buckets(&root);
    let pinned = UsageReader::open_at(&root).expect("pin original generation");
    let filter = UsageFilter::default().for_model("model-a");
    let expected_tokens = pinned
        .totals(TimeRange::all(), &filter)
        .expect("model totals");
    let hourly = RetentionPolicy {
        inactive_chat_days: 100,
        ..policy()
    };
    let job = RollupCompactionJob::prepare(&root, hourly, NOW, MergeMode::FullBatch)
        .expect("prepare merge")
        .expect("ten fragments");
    assert_eq!(job.inputs, before.token_rollup_files[..8]);
    let output = job.output.clone();
    assert!(
        !crate::token_rollup::compact_one(&root, hourly, NOW + DAY * 1_000)
            .expect("advance correction admission metadata without retiring recent raw data")
    );
    let store = ManifestStore::new(root.join("manifest.json"));
    let mut expected = store.load().expect("independently advanced metadata");
    assert_eq!(
        (
            expected.snapshot_generation,
            expected.correction_retention_cutoff_at_ms,
            expected.review_retention_cutoff_at_ms,
        ),
        (
            before.snapshot_generation,
            Some(6 * DAY * 1_000),
            Some(6 * DAY * 1_000),
        )
    );
    assert!(job.install().expect("install merge"));
    let after = store.load().expect("new manifest");
    expected.token_rollup_files = before.token_rollup_files[8..].to_vec();
    expected.token_rollup_files.push(output);
    expected.snapshot_generation += 1;
    expected.watermark_at_ms = after.watermark_at_ms;
    expected.updated_at = after.updated_at;
    assert_eq!(after, expected);
    assert!(
        !compact_one(&root, hourly, NOW, MergeMode::FullBatch)
            .expect("amortize the smaller remainder until raw retirement is idle")
    );
    assert_eq!(buckets(&root), original_buckets);
    assert_eq!(
        pinned
            .totals(TimeRange::all(), &filter)
            .expect("old pinned buckets"),
        expected_tokens
    );
    assert_eq!(
        crate::reclamation::reclaim(&root).expect("defer pinned deletion"),
        crate::reclamation::Reclamation::default()
    );
    drop(pinned);
    crate::reclamation::reclaim(&root).expect("retire superseded inputs");
    for input in &before.token_rollup_files[..8] {
        assert!(!root.join("segments").join(input).exists());
    }
    assert_eq!(buckets(&root), original_buckets);
}

#[test]
fn maintenance_collapses_inactive_buckets_and_keeps_resumed_chat_totals() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    seed_rollups(&root);
    UsageJournal::new(root.join("events.jsonl"))
        .append_batch(
            (0..40)
                .map(|hour| {
                    JournalPayload::Record(
                        UsageRecord::new(
                            format!("recent-{hour}"),
                            NOW / 1_000 - hour * 3_600,
                            /*input_tokens*/ 0,
                            /*output_tokens*/ 0,
                        )
                        .with_thread("active")
                        .with_model("model-b"),
                    )
                })
                .collect(),
        )
        .expect("more recent raw files than one bounded scan");
    UsagePublisher::open_at(&root)
        .expect("publisher")
        .publish_batch(limits())
        .expect("publish recent raw hours");
    let original = buckets(&root);
    let mut maintenance = crate::retention::Maintenance::default();
    crate::retention::maintain(&root, policy(), NOW, &mut maintenance)
        .expect("drain rollup fragments");
    let manifest = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("manifest");
    assert_eq!(manifest.token_rollup_files.len(), 1);
    let rows = buckets(&root);
    assert_eq!(
        rows.iter()
            .filter(|row| row.0 == "idle")
            .cloned()
            .collect::<Vec<_>>(),
        vec![(
            "idle".into(),
            "model-a".into(),
            None,
            DAY * 1_000,
            (DAY + 9 * 3_600) * 1_000,
            10,
            85,
            30,
            40,
            20
        )]
    );
    assert_eq!(
        rows.into_iter()
            .filter(|row| row.0 == "active")
            .collect::<Vec<_>>(),
        original
            .into_iter()
            .filter(|row| row.0 == "active")
            .collect::<Vec<_>>()
    );
    let before_resume = buckets(&root);
    UsageJournal::new(root.join("events.jsonl"))
        .append_record(
            UsageRecord::new(
                "resumed",
                NOW / 1_000 + 1,
                /*input_tokens*/ 7,
                /*output_tokens*/ 1,
            )
            .with_thread("idle")
            .with_model("model-a"),
        )
        .expect("resume chat");
    UsagePublisher::open_at(&root)
        .expect("resumed publisher")
        .publish_batch(limits())
        .expect("publish resumed chat");
    assert!(
        !compact_one(&root, policy(), NOW + 1_000, MergeMode::Available)
            .expect("no discarded hourly detail is reconstructed")
    );
    assert_eq!(buckets(&root), before_resume);
    let expected = UsageTotals {
        input_tokens: 92,
        cached_input_tokens: 30,
        output_tokens: 41,
        reasoning_output_tokens: 20,
        records: 11,
    };
    assert_eq!(
        UsageReader::open_at(&root)
            .expect("resumed reader")
            .totals(
                TimeRange::all(),
                &UsageFilter::default().for_model("model-a")
            )
            .expect("lifetime plus fresh usage"),
        expected
    );
    assert!(
        compact_one(&root, policy(), NOW + 3 * DAY * 1_000, MergeMode::Available)
            .expect("convert one remaining hourly fragment")
    );
    assert!(buckets(&root).iter().all(|row| row.2.is_none()));
    assert!(
        !compact_one(&root, policy(), NOW + 3 * DAY * 1_000, MergeMode::Available)
            .expect("lifetime conversion is idempotent")
    );
}

#[test]
fn racing_publication_keeps_current_inputs_and_pins_unpublished_output() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    seed_rollups(&root);
    let job = RollupCompactionJob::prepare(&root, policy(), NOW, MergeMode::Available)
        .expect("prepare merge")
        .expect("fragments");
    let orphan = root.join("segments").join(&job.output);
    assert!(orphan.exists());
    assert_eq!(
        crate::reclamation::reclaim(&root).expect("prepared output is pinned"),
        crate::reclamation::Reclamation::default()
    );
    UsageJournal::new(root.join("events.jsonl"))
        .append_record(UsageRecord::new(
            "raced",
            NOW / 1_000,
            /*input_tokens*/ 11,
            /*output_tokens*/ 2,
        ))
        .expect("capture during merge");
    let raced = UsagePublisher::open_at(&root)
        .expect("publisher")
        .publish_batch(limits())
        .expect("racing publication");
    assert!(!job.install().expect("reject stale merge"));
    let store = ManifestStore::new(root.join("manifest.json"));
    assert_eq!(store.load().expect("current manifest"), raced);
    crate::reclamation::reclaim(&root).expect("reclaim stale orphan");
    assert!(!orphan.exists());
    let job = RollupCompactionJob::prepare(&root, policy(), NOW, MergeMode::Available)
        .expect("prepare fresh merge")
        .expect("fragments");
    let mut changed_inputs = raced;
    changed_inputs.token_rollup_files.rotate_left(1);
    {
        let lock = storage_file::open_read_write(&root.join(".snapshot.lock")).expect("lock");
        lock.lock().expect("hold snapshot lock");
        // Exercise the input check independently of generation verification.
        store
            .save(&changed_inputs)
            .expect("changed input references");
    }
    assert!(!job.install().expect("reject changed selected inputs"));
    assert_eq!(store.load().expect("preserved manifest"), changed_inputs);
    let retained = changed_inputs
        .token_rollup_files
        .iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(retained.len(), 10);
    assert!(
        retained
            .iter()
            .all(|file| root.join("segments").join(file).exists())
    );
    let expected = UsageReader::open_at(&root)
        .expect("read before reclamation race")
        .totals(TimeRange::all(), &UsageFilter::default())
        .expect("retained totals");
    let abandoned = root.join("segments/rollup-abandoned-before-reclamation.parquet");
    std::fs::write(&abandoned, b"interrupted immutable output").expect("old orphan");
    let reclamation = crate::reclamation::ReclamationPlan::prepare(&root)
        .expect("collect candidates before admitting workers")
        .expect("no previous pins");
    let job = RollupCompactionJob::prepare(&root, policy(), NOW, MergeMode::Available)
        .expect("prepare after reader admission reopens")
        .expect("fragments");
    let output = root.join("segments").join(&job.output);
    let bytes = std::fs::read(&output).expect("new pinned output");
    let reclaimed = reclamation
        .reclaim()
        .expect("delete only admitted candidates");
    assert!(reclaimed.files_removed > 0);
    assert!(!abandoned.exists());
    assert_eq!(
        std::fs::read(&output).expect("new output survives reclamation"),
        bytes
    );
    assert!(job.install().expect("publish surviving output"));
    assert_eq!(
        UsageReader::open_at(&root)
            .expect("reopen published merge")
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("same retained totals"),
        expected
    );
}

#[test]
fn correction_cutoff_advances_without_raw_data_and_never_regresses() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let store = ManifestStore::new(root.join("manifest.json"));
    let before = crate::UsageManifest {
        snapshot_generation: 3,
        ..Default::default()
    };
    store.save(&before).expect("published empty raw set");
    let mut expected = before;
    for now in [NOW, NOW - DAY * 1_000, NOW, NOW + DAY * 1_000] {
        assert!(
            !crate::token_rollup::compact_one(&root, policy(), now)
                .expect("advance retention metadata without raw segments")
        );
        let cutoff = policy().raw_cutoff(now);
        expected.correction_retention_cutoff_at_ms = Some(
            expected
                .correction_retention_cutoff_at_ms
                .map_or(cutoff, |prior| prior.max(cutoff)),
        );
        expected.review_retention_cutoff_at_ms = Some(
            expected
                .review_retention_cutoff_at_ms
                .map_or(cutoff, |prior| prior.max(cutoff)),
        );
        assert_eq!(store.load().expect("admission metadata"), expected);
    }
}
