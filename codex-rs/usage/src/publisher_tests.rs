use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::TimeRange;
use crate::UsageFilter;
use crate::UsageReader;
use crate::lineage::ThreadLineageScope;
use crate::lineage::read_descendant_threads;

fn record(id: &str, at: i64, input: u64) -> UsageRecord {
    UsageRecord::new(id, at, input, /*output_tokens*/ 1)
        .with_account("account-a")
        .with_thread(format!("thread-{id}"))
        .with_repository("repo-a")
}

fn limits() -> JournalReadLimits {
    JournalReadLimits {
        max_bytes: 8 * 1024 * 1024,
        max_entries: 64,
    }
}

fn assert_private_store(root: &Path, manifest: &UsageManifest) {
    for directory in [root.to_path_buf(), root.join("segments")] {
        codex_private_storage::verify_private_directory(&directory)
            .expect("private usage directory");
    }
    for file in ["events.jsonl", "manifest.json", ".snapshot.lock"] {
        codex_private_storage::verify_private_file(&root.join(file))
            .expect("private usage metadata/journal");
    }
    for file in manifest
        .segment_files
        .iter()
        .chain(&manifest.correction_files)
        .chain(manifest.membership_file.iter())
        .chain(manifest.lineage_file.iter())
    {
        codex_private_storage::verify_private_file(&root.join("segments").join(file))
            .expect("private published Parquet");
    }
}

fn linked_record(id: &str, child: &str, parent: &str) -> UsageRecord {
    let mut record = UsageRecord::new(
        id, /*observed_at*/ 3_600, /*input_tokens*/ 0, /*output_tokens*/ 0,
    )
    .with_thread(child);
    record.parent_thread_id = Some(parent.to_string());
    record
}

#[test]
fn lineage_survives_late_parent_facts_stale_publishers_and_restart() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut publisher = UsagePublisher::open_at(root).expect("publisher");
    let mut stale = UsagePublisher::open_at(root).expect("independent publisher");
    journal
        .append_batch(vec![
            JournalPayload::Record(
                record(
                    "child-started-unknown-parent",
                    /*at*/ 3_600,
                    /*input*/ 0,
                )
                .with_thread("child"),
            ),
            JournalPayload::Record(linked_record("grandchild-started", "grandchild", "child")),
        ])
        .expect("initial lifecycle facts");
    let before = publisher
        .publish_batch(limits())
        .expect("publish known links only");
    assert_private_store(root, &before);
    let pinned = UsageReader::open_at(root).expect("pin old report generation");
    assert_eq!(
        read_descendant_threads(root, &before, "root", ThreadLineageScope::Descendants, &[])
            .expect("unknown child ancestry"),
        vec!["root"]
    );
    assert_eq!(
        read_descendant_threads(root, &before, "child", ThreadLineageScope::Descendants, &[])
            .expect("explicit grandchild"),
        vec!["child", "grandchild"]
    );
    let first_file = before.lineage_file.clone().expect("initial sidecar");
    let first_bytes =
        fs::read(root.join("segments").join(&first_file)).expect("pinned lineage bytes");
    journal
        .append_record(linked_record("child-parent-now-known", "child", "root"))
        .expect("late verified parent fact");
    let after = stale
        .publish_batch(limits())
        .expect("independent publisher merges previous links");
    let restarted = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("restart manifest");
    assert_eq!(restarted, after);
    assert_eq!(
        read_descendant_threads(
            root,
            &restarted,
            "root",
            ThreadLineageScope::Descendants,
            &[]
        )
        .expect("restart descendants"),
        vec!["child", "grandchild", "root"]
    );
    assert_eq!(
        read_descendant_threads(root, &before, "root", ThreadLineageScope::Descendants, &[])
            .expect("old generation remains pinned"),
        vec!["root"]
    );
    assert_eq!(
        fs::read(root.join("segments").join(&first_file)).expect("immutable prior index"),
        first_bytes
    );
    assert_eq!(
        crate::reclamation::reclaim(root).expect("defer for pinned reader"),
        crate::reclamation::Reclamation::default()
    );
    assert_eq!(
        pinned
            .thread_scope("root", ThreadLineageScope::Descendants)
            .expect("pinned report"),
        vec!["root"]
    );
    drop(pinned);
    let checkpoint = journal.checkpoint().expect("before retirement");
    let reclaimed = crate::reclamation::reclaim(root).expect("reclaim obsolete generations");
    assert!(reclaimed.files_removed > 0);
    assert!(reclaimed.journal_rotated);
    assert!(!root.join("segments").join(first_file).exists());
    assert_eq!(journal.checkpoint().expect("stable frontier"), checkpoint);
    assert!(fs::metadata(journal.path()).expect("retired journal").len() < checkpoint.byte_offset);
    assert_eq!(
        UsageReader::open_at(root)
            .expect("reopen report")
            .thread_scope("root", ThreadLineageScope::Descendants)
            .expect("retained lineage"),
        vec!["child", "grandchild", "root"]
    );
    journal
        .append_record(linked_record("same-parent-confirmed", "child", "root"))
        .expect("repeated consistent relationship");
    let confirmed = UsagePublisher::open_at(root)
        .expect("restarted publisher")
        .publish_batch(limits())
        .expect("idempotent relationship");
    assert_eq!(confirmed.lineage_file, restarted.lineage_file);
}

#[test]
fn startup_worker_prewarms_ten_most_recent_chats_from_persisted_activity() {
    let directory = tempdir().expect("home");
    let root = directory.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_batch(
            (0..12)
                .map(|index| {
                    JournalPayload::Record(record(
                        &index.to_string(),
                        /*at*/ index,
                        /*input*/ 2,
                    ))
                })
                .collect(),
        )
        .expect("capture chats");
    journal
        .append_record(record("reopened", /*at*/ 100, /*input*/ 3).with_thread("thread-0"))
        .expect("reopen oldest chat");
    let manifest = UsagePublisher::open_at(&root)
        .expect("publisher")
        .publish_batch(limits())
        .expect("persist before startup");
    assert_eq!(manifest.thread_last_activity_at_ms["thread-0"], 100_000);
    let runtime = crate::UsagePublisherRuntime::start(directory.path()).expect("startup runtime");
    let reader = UsageReader::open_at(&root).expect("foreground independent snapshot");
    let expected = std::iter::once("thread-0".to_string())
        .chain((3..12).map(|index| format!("thread-{index}")))
        .collect::<Vec<_>>();
    let cached = |thread: &str| {
        crate::summary_cache::get(
            root.clone(),
            reader.metadata_revision,
            reader.manifest().snapshot_generation,
            reader.manifest().watermark,
            reader.journal_watermark(),
            reader.pending_entries(),
            reader.manifest().pending_corrections.len(),
            TimeRange::all(),
            &UsageFilter::default().for_thread(thread),
        )
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if expected.iter().all(|thread| cached(thread).is_some()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background startup warmup did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(cached("thread-1").is_none());
    assert!(cached("thread-2").is_none());
    assert_eq!(
        cached("thread-0").expect("recent summary")["inputTokens"],
        5
    );
    assert_eq!(
        journal
            .checkpoint()
            .expect("unchanged captured data")
            .sequence,
        manifest.watermark
    );
    drop(runtime);
}

#[test]
fn lineage_overlay_uses_canonical_tail_facts_and_validates_conflicts() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_record(linked_record("published-child", "child", "root"))
        .expect("canonical child");
    let manifest = UsagePublisher::open_at(root)
        .expect("publisher")
        .publish_batch(limits())
        .expect("published parent");
    let mut unknown = linked_record("unknown-first", "unknown", "root");
    unknown.parent_thread_id = None;
    let tail = vec![
        linked_record("new-grandchild", "grandchild", "child"),
        linked_record("published-child", "child", "arbitrary-duplicate-parent"),
        unknown,
        linked_record("unknown-first", "unknown", "root"),
    ];
    assert_eq!(
        read_descendant_threads(
            root,
            &manifest,
            "root",
            ThreadLineageScope::Descendants,
            &tail
        )
        .expect("complete unmaterialized tail"),
        vec!["child", "grandchild", "root"]
    );
    for invalid_tail in [
        vec![linked_record("conflict", "child", "other")],
        vec![linked_record("cycle", "root", "child")],
        vec![
            linked_record("first-parent", "new-child", "root"),
            linked_record("second-parent", "new-child", "other"),
        ],
    ] {
        assert!(matches!(
            read_descendant_threads(
                root,
                &manifest,
                "root",
                ThreadLineageScope::Descendants,
                &invalid_tail
            ),
            Err(StorageError::InvalidData(_))
        ));
    }
    let mut missing = manifest;
    missing.lineage_file = Some("missing-index.parquet".into());
    assert_eq!(
        read_descendant_threads(root, &missing, "root", ThreadLineageScope::RootOnly, &tail)
            .expect("root-only needs no index"),
        vec!["root"]
    );
}

#[test]
fn conflicting_or_cyclic_parent_facts_do_not_advance_publication() {
    for invalid in [
        linked_record("conflict", "child", "other"),
        linked_record("cycle", "root", "child"),
        linked_record("self", "self", "self"),
    ] {
        let directory = tempdir().expect("directory");
        let root = directory.path();
        let journal = UsageJournal::new(root.join("events.jsonl"));
        journal
            .append_record(linked_record("canonical-child", "child", "root"))
            .expect("verified child");
        let mut publisher = UsagePublisher::open_at(root).expect("publisher");
        let before = publisher
            .publish_batch(limits())
            .expect("valid publication");
        journal
            .append_record(invalid)
            .expect("retain conflicting canonical fact for diagnosis");
        assert!(matches!(
            publisher.publish_batch(limits()),
            Err(StorageError::InvalidData(_))
        ));
        assert_eq!(
            ManifestStore::new(root.join("manifest.json"))
                .load()
                .expect("prior snapshot intact"),
            before
        );
        assert_eq!(journal.last_sequence().expect("no canonical loss"), 2);
        assert_eq!(
            read_descendant_threads(root, &before, "root", ThreadLineageScope::Descendants, &[])
                .expect("valid pinned descendants"),
            vec!["child", "root"]
        );
    }
}

#[test]
fn stale_publishers_preserve_first_ids_pinned_fragments_and_exact_membership() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let mut first = UsagePublisher::open_at(root).expect("metadata-only open");
    let mut stale = UsagePublisher::open_at(root).expect("independent publisher");
    journal
        .append_batch(vec![
            JournalPayload::Record(record("one", /*at*/ 5, /*input*/ 8)),
            JournalPayload::Record(record("two", /*at*/ 3_605, /*input*/ 4)),
        ])
        .expect("initial records");
    let first_manifest = first
        .publish_batch(JournalReadLimits {
            max_entries: 1,
            ..limits()
        })
        .expect("bounded first publication");
    assert_eq!(
        (
            first_manifest.watermark,
            first_manifest.snapshot_totals.records
        ),
        (1, 1)
    );
    let first_file = first_manifest.segment_files[0].clone();
    let first_bytes =
        fs::read(root.join("segments").join(&first_file)).expect("first immutable bytes");
    let second_manifest = stale
        .publish_batch(JournalReadLimits {
            max_entries: 1,
            ..limits()
        })
        .expect("stale publisher reconciles");
    assert_eq!(
        (
            second_manifest.watermark,
            second_manifest.snapshot_totals.records
        ),
        (2, 2)
    );
    assert!(second_manifest.segment_files.contains(&first_file));
    let pinned = UsageReader::open_at(root).expect("pin old generation");
    journal
        .append_batch(vec![
            JournalPayload::Record(record("three", /*at*/ 10, /*input*/ 2)),
            JournalPayload::Record(
                record("one", /*at*/ 7_205, /*input*/ 999).with_account("ignored-duplicate"),
            ),
        ])
        .expect("late records and duplicate");
    let latest = first
        .publish_batch(limits())
        .expect("incremental publication");
    assert_private_store(root, &latest);
    assert_eq!(
        latest.snapshot_totals,
        UsageTotals {
            input_tokens: 14,
            output_tokens: 3,
            records: 3,
            ..UsageTotals::default()
        }
    );
    assert!(!latest.account_totals.contains_key("ignored-duplicate"));
    assert_eq!(
        pinned
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("pinned summary"),
        UsageTotals {
            input_tokens: 12,
            output_tokens: 2,
            records: 2,
            ..UsageTotals::default()
        }
    );
    assert_eq!(
        fs::read(root.join("segments").join(&first_file)).expect("retained old file"),
        first_bytes
    );
    assert!(latest.segment_files.contains(&first_file));
    assert_eq!(latest.segment_files.len(), 3);
    let current = UsageReader::open_at(root).expect("current reader");
    assert_eq!(
        current
            .totals(
                TimeRange::new(/*start*/ 0, /*end*/ 3_600).expect("range"),
                &UsageFilter::default().for_thread("thread-one")
            )
            .expect("membership-selected query"),
        record("one", /*at*/ 5, /*input*/ 8).totals()
    );
    let connection = Connection::open_in_memory().expect("membership reader");
    let index = root
        .join("segments")
        .join(latest.membership_file.expect("complete membership"));
    let count: u64 = connection
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM read_parquet({}) WHERE segment_file = {}",
                quoted(&index.to_string_lossy()),
                quoted(&first_file)
            ),
            [],
            |row| row.get(0),
        )
        .expect("index rows");
    assert_eq!(count, 3);
}

#[test]
fn corrections_fold_once_and_unknown_targets_resolve_after_restart() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_batch(vec![
            JournalPayload::Correction(UsageCorrection::new(
                "late",
                "future",
                /*observed_at*/ 7_200,
                /*input_tokens_delta*/ -2,
                /*output_tokens_delta*/ 1,
                "target not yet observed",
            )),
            JournalPayload::Record(record("one", /*at*/ 3_605, /*input*/ 10)),
            JournalPayload::Correction(UsageCorrection::new(
                "fix",
                "one",
                /*observed_at*/ 7_201,
                /*input_tokens_delta*/ -3,
                /*output_tokens_delta*/ 2,
                "reconciliation",
            )),
            JournalPayload::Correction(UsageCorrection::new(
                "fix",
                "one",
                /*observed_at*/ 8_000,
                /*input_tokens_delta*/ -999,
                /*output_tokens_delta*/ 999,
                "duplicate ID ignored",
            )),
        ])
        .expect("records and corrections");
    let mut publisher = UsagePublisher::open_at(root).expect("publisher");
    let first = publisher
        .publish_batch(limits())
        .expect("publish known correction");
    assert_private_store(root, &first);
    assert_eq!(
        first.snapshot_totals,
        UsageTotals {
            input_tokens: 7,
            output_tokens: 3,
            records: 1,
            ..UsageTotals::default()
        }
    );
    assert_eq!(
        first.applied_correction_ids,
        BTreeSet::from(["fix".to_string()])
    );
    assert_eq!(first.pending_corrections, vec!["late"]);
    assert!(!first.summaries_include_corrections);
    let correction_files = first.correction_files.clone();
    let retried = publisher
        .publish_batch(limits())
        .expect("unresolved target retry");
    assert_eq!(retried.correction_files, correction_files);
    assert_eq!(retried.snapshot_totals, first.snapshot_totals);
    journal
        .append_batch(vec![
            JournalPayload::Record(record("future", /*at*/ 5, /*input*/ 5)),
            JournalPayload::Record(record("future", /*at*/ 9_000, /*input*/ 999)),
        ])
        .expect("late target and duplicate");
    let mut restarted = UsagePublisher::open_at(root).expect("restart publisher");
    let resolved = restarted
        .publish_batch(limits())
        .expect("resolve canonical correction");
    assert_eq!(
        resolved.snapshot_totals,
        UsageTotals {
            input_tokens: 10,
            output_tokens: 5,
            records: 2,
            ..UsageTotals::default()
        }
    );
    assert!(resolved.pending_corrections.is_empty());
    assert!(resolved.summaries_include_corrections);
    assert_eq!(resolved.correction_files, correction_files);
    assert_eq!(
        resolved.applied_correction_ids,
        BTreeSet::from(["fix".to_string(), "late".to_string()])
    );
    assert_eq!(
        resolved.thread_totals["thread-future"],
        UsageTotals {
            input_tokens: 3,
            output_tokens: 2,
            records: 1,
            ..UsageTotals::default()
        }
    );
    let reader = UsageReader::open_at(root).expect("restarted report");
    assert_eq!(
        reader
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("corrected cached summary"),
        resolved.snapshot_totals
    );
    assert_eq!(
        reader
            .totals(
                TimeRange::new(/*start*/ 3_600, /*end*/ 7_200).expect("range"),
                &UsageFilter::default().for_thread("thread-one")
            )
            .expect("correction uses target dimensions and time"),
        first.snapshot_totals
    );
    assert_eq!(
        restarted.publish_batch(limits()).expect("caught-up no-op"),
        resolved
    );
}

#[test]
fn crash_orphans_are_not_read_or_overwritten_and_open_is_metadata_only() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_record(record("one", /*at*/ 5, /*input*/ 2))
        .expect("capture");
    let mut publisher = UsagePublisher::open_at(root).expect("publisher");
    let before = publisher.publish_batch(limits()).expect("first generation");
    let orphan = root.join("segments/segment-00000000000000000002_abandoned-0.parquet");
    fs::write(&orphan, b"interrupted unreferenced output").expect("crash orphan");
    journal
        .append_record(record("two", /*at*/ 7, /*input*/ 3))
        .expect("acknowledged tail");
    let next = UsagePublisher::open_at(root)
        .expect("recovery open")
        .publish_batch(limits())
        .expect("recovery publication");
    assert_eq!(next.snapshot_totals.records, 2);
    assert_eq!(next.watermark, 2);
    assert_eq!(
        fs::read(&orphan).expect("retained orphan evidence"),
        b"interrupted unreferenced output"
    );
    for file in before.segment_files {
        assert!(root.join("segments").join(file).is_file());
    }
    assert_eq!(journal.replay().expect("canonical observations").len(), 2);
    // Opening the publisher must not touch even a damaged operation segment.
    fs::write(
        root.join("segments").join(&next.segment_files[0]),
        b"damaged operation rows",
    )
    .expect("isolated damage");
    UsagePublisher::open_at(root).expect("metadata-only restart");
}

#[test]
fn concurrent_publishers_reload_under_the_publisher_lock() {
    let directory = tempdir().expect("directory");
    let root = directory.path().to_path_buf();
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append_batch(
            (0..24)
                .map(|index| {
                    JournalPayload::Record(record(
                        &format!("record-{index}"),
                        /*at*/ 5,
                        /*input*/ 1,
                    ))
                })
                .collect(),
        )
        .expect("captured observations");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(/*n*/ 2));
    let publishers: Vec<_> = (0..2)
        .map(|_| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut publisher = UsagePublisher::open_at(root).expect("publisher");
                barrier.wait();
                for _ in 0..24 {
                    let manifest = publisher
                        .publish_batch(JournalReadLimits {
                            max_entries: 3,
                            ..limits()
                        })
                        .expect("concurrent publication");
                    if manifest.watermark == 24 {
                        return;
                    }
                }
                panic!("publisher did not reach captured target");
            })
        })
        .collect();
    for publisher in publishers {
        publisher.join().expect("publisher exited");
    }
    let reader = UsageReader::open_at(root).expect("published reader");
    assert_eq!(
        reader
            .totals(TimeRange::all(), &UsageFilter::default())
            .expect("concurrent summary"),
        UsageTotals {
            input_tokens: 24,
            output_tokens: 24,
            records: 24,
            ..UsageTotals::default()
        }
    );
}
use std::fs;
