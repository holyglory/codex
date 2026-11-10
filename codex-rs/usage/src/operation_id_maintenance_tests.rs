use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::JournalReadLimits;
use crate::UsageJournal;
use crate::UsagePublisher;
use crate::UsageReader;
use crate::UsageRecord;

fn publish(root: &Path, id: &str) -> crate::UsageManifest {
    let mut fact = UsageRecord::new_millis(
        id, /*observed_at_ms*/ 1_000, /*input_tokens*/ 0, /*output_tokens*/ 0,
    )
    .with_operation(id);
    fact.event = Some("tool_started".into());
    fact.started_at_ms = Some(1_000);
    UsageJournal::new(root.join("events.jsonl"))
        .append_record(fact)
        .expect("real capture");
    UsagePublisher::open_at(root)
        .expect("publisher")
        .publish_batch(JournalReadLimits {
            max_bytes: 64 * 1024,
            max_entries: 16,
        })
        .expect("publication")
}

#[test]
fn streaming_merge_preserves_appended_runs_and_pinned_reader_files() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    publish(root, "first");
    let manifest = publish(root, "second");
    let pinned = UsageReader::open_at(root).expect("pin two-run generation");
    let source = review_projection::load_review_projection_shared(root, &manifest)
        .expect("metadata")
        .expect("source");
    let index = operation_id_index::load(root, &source)
        .expect("index")
        .expect("fresh index");
    let old_files = index
        .runs
        .iter()
        .flat_map(|entry| {
            [
                Some(entry.run.data_file.clone()),
                Some(entry.run.fence_file.clone()),
                entry.run.bloom_file.clone(),
            ]
        })
        .flatten()
        .collect::<Vec<_>>();
    let mut job = Job::start(
        root,
        source.clone(),
        index.runs.clone(),
        SnapshotPin::acquire(root).expect("job pin"),
    )
    .expect("two-run job");
    assert!(!job.step(/*max_entries*/ 1).expect("bounded first input"));
    let appended = publish(root, "third");
    let mut maintenance = Maintenance { job: Some(job) };
    for _ in 0..8 {
        maintenance
            .step(root, /*max_entries*/ 1)
            .expect("bounded merge and install");
        if maintenance.job.is_none() {
            break;
        }
    }
    assert!(maintenance.job.is_none());
    let current = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("installed manifest");
    let review = review_projection::load_review_projection(root, &current)
        .expect("metadata")
        .expect("current review");
    let merged = operation_id_index::load(root, &review)
        .expect("index")
        .expect("merged index");
    assert_eq!(
        (
            merged.watermark.sequence,
            merged.runs.len(),
            current.review_counts
        ),
        (appended.watermark, 2, appended.review_counts)
    );
    let partitions = review
        .partitions
        .iter()
        .cloned()
        .map(|p| (p.partition, p))
        .collect();
    let ids = BTreeSet::from([
        "first".into(),
        "second".into(),
        "third".into(),
        "absent".into(),
    ]);
    assert_eq!(
        operation_id_index::find_addresses(root, &merged, &partitions, &ids)
            .expect("restart exact lookup"),
        BTreeMap::from([
            ("first".into(), 1),
            ("second".into(), 2),
            ("third".into(), 3)
        ])
    );
    let old_partitions = source
        .partitions
        .iter()
        .cloned()
        .map(|p| (p.partition, p))
        .collect();
    assert_eq!(
        operation_id_index::find_addresses(root, &index, &old_partitions, &ids)
            .expect("pinned old exact lookup"),
        BTreeMap::from([("first".into(), 1), ("second".into(), 2)])
    );
    assert_eq!(
        crate::reclamation::reclaim(root).expect("reader pins old graph"),
        crate::reclamation::Reclamation::default()
    );
    assert!(
        old_files
            .iter()
            .all(|file| root.join("segments").join(file).exists())
    );
    drop(pinned);
    crate::reclamation::reclaim(root).expect("release obsolete exact runs");
    assert!(
        old_files
            .iter()
            .all(|file| !root.join("segments").join(file).exists())
    );
    for entry in &merged.runs {
        for file in [
            Some(&entry.run.data_file),
            Some(&entry.run.fence_file),
            entry.run.bloom_file.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            codex_private_storage::verify_private_file(&root.join("segments").join(file))
                .expect("current run graph retained privately");
        }
    }
}

#[test]
fn merge_filters_retired_sequence_before_selecting_same_id_and_rejects_live_duplicates() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    publish(root, "old");
    publish(root, "keeper");
    let manifest = publish(root, "new");
    let mut source = review_projection::load_review_projection(root, &manifest)
        .expect("metadata")
        .expect("review");
    // Exercise the isolated streaming merge with both incarnations in its
    // inputs. Publication/retirement integration is covered by review_retention.
    let entries = [
        OperationIdEntry {
            operation_id: "repeat".into(),
            first_sequence: 1,
        },
        OperationIdEntry {
            operation_id: "repeat".into(),
            first_sequence: 3,
        },
    ];
    let mut inputs = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let mut writer = RunWriter::new(
            root,
            &format!("review-operation-id-test-{i}.bin"),
            &format!("review-operation-id-test-fence-{i}.bin"),
            /*level*/ 0,
            /*estimated_entries*/ 1,
        )
        .expect("writer");
        writer.push_entry(entry).expect("address");
        inputs.push(OperationIdRun {
            run: writer.finish().expect("run"),
            retired_through: 0,
        });
    }
    let mut live = vec![0; REVIEW_PARTITION_SIZE as usize / 64];
    live[0] = 1 << 2;
    source.partitions[0].operation_sequences = Some(live);
    source.partitions[0].operation_count = 1;
    source.partitions[0].open_operations = 1;
    source.retired_operation_rows = 1;
    let exact_index = operation_id_index::OperationIdIndex {
        version: 1,
        hash_version: crate::id_index::ID_HASH_VERSION,
        watermark: source.watermark,
        complete_from_sequence: 1,
        runs: inputs.clone(),
    };
    let ids = BTreeSet::from(["repeat".into()]);
    let partitions = source
        .partitions
        .iter()
        .cloned()
        .map(|partition| (partition.partition, partition))
        .collect();
    assert_eq!(
        operation_id_index::find_addresses(root, &exact_index, &partitions, &ids)
            .expect("exact lookup ignores the retired incarnation"),
        BTreeMap::from([("repeat".into(), 3)])
    );
    let mut job = Job::start(
        root,
        Arc::new(source.clone()),
        inputs.clone(),
        SnapshotPin::acquire(root).expect("pin"),
    )
    .expect("merge");
    assert!(
        !job.step(/*max_entries*/ 1)
            .expect("retired input consumes budget")
    );
    assert!(
        !job.step(/*max_entries*/ 1)
            .expect("new incarnation consumes budget")
    );
    assert!(job.step(/*max_entries*/ 1).expect("finish"));
    let mut reader =
        RunReader::open(root, job.output.as_ref().expect("output")).expect("output reader");
    assert_eq!(
        (
            reader.next_entry::<OperationIdEntry>().expect("winner"),
            reader.next_entry::<OperationIdEntry>().expect("end")
        ),
        (Some(entries[1].clone()), None)
    );
    source.partitions[0].operation_sequences = None;
    let partitions = source
        .partitions
        .iter()
        .cloned()
        .map(|partition| (partition.partition, partition))
        .collect();
    assert!(matches!(
        operation_id_index::find_addresses(root, &exact_index, &partitions, &ids),
        Err(crate::StorageError::InvalidData(message))
            if message == "conflicting current operation addresses"
    ));
    let mut duplicate = Job::start(
        root,
        Arc::new(source),
        inputs,
        SnapshotPin::acquire(root).expect("pin"),
    )
    .expect("duplicate job");
    assert!(duplicate.step(/*max_entries*/ 4).is_err());
}

#[test]
fn run_cap_preserves_capture_and_snapshot_until_bounded_maintenance_unblocks_publication() {
    let directory = tempdir().expect("directory");
    let root_path = directory.path().join(crate::USAGE_DIRECTORY_NAME);
    std::fs::create_dir_all(&root_path).expect("usage root");
    let root = root_path.as_path();
    let now_ms = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()) as i64;
    let journal = UsageJournal::new(root.join("events.jsonl"));
    let records = (0..crate::id_index::MAX_ID_RUNS)
        .map(|i| {
            let mut fact = UsageRecord::new_millis(
                format!("cap-{i}"),
                /*observed_at_ms*/ now_ms,
                /*input_tokens*/ 0,
                /*output_tokens*/ 0,
            )
            .with_operation(format!("cap-{i}"));
            fact.event = Some("tool_started".into());
            fact.started_at_ms = Some(now_ms);
            crate::JournalPayload::Record(fact)
        })
        .collect();
    journal.append_batch(records).expect("real captures");
    let mut publisher = UsagePublisher::open_at(root).expect("publisher");
    let mut manifest = publisher
        .publish_batch(JournalReadLimits {
            max_bytes: 1024 * 1024,
            max_entries: 512,
        })
        .expect("publish complete canonical partition");
    let mut review = review_projection::load_review_projection(root, &manifest)
        .expect("metadata")
        .expect("review");
    let mut index = operation_id_index::load(root, &review)
        .expect("index")
        .expect("fresh index");
    index.runs.clear();
    for i in 0..crate::id_index::MAX_ID_RUNS {
        let mut writer = RunWriter::new(
            root,
            &format!("review-operation-id-cap-{i}.bin"),
            &format!("review-operation-id-cap-fence-{i}.bin"),
            /*level*/ 0,
            /*estimated_entries*/ 1,
        )
        .expect("run");
        writer
            .push_entry(&OperationIdEntry {
                operation_id: format!("cap-{i}"),
                first_sequence: i as u64 + 1,
            })
            .expect("exact address");
        index.runs.push(OperationIdRun {
            run: writer.finish().expect("immutable run"),
            retired_through: 0,
        });
    }
    review.operation_id_index_file =
        Some(operation_id_index::write(root, &index, "at-cap").expect("cap is valid"));
    let mut invalid = index.clone();
    invalid.runs.push(index.runs[0].clone());
    assert!(operation_id_index::write(root, &invalid, "over-cap").is_err());
    let review_file = "review-projection-at-cap.json";
    write_json(root, review_file, &review).expect("descriptor");
    manifest.review_projection_file = Some(review_file.into());
    ManifestStore::new(root.join("manifest.json"))
        .save(&manifest)
        .expect("install real cap fixture");
    let mut late_start = UsageRecord::new_millis(
        "cap-0:late-start",
        /*observed_at_ms*/ now_ms + 1,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation("cap-0");
    late_start.event = Some("tool_started".into());
    late_start.started_at_ms = Some(now_ms);
    journal
        .append_record(late_start)
        .expect("late start capture");
    let updated = publisher
        .publish_batch(JournalReadLimits {
            max_bytes: 64 * 1024,
            max_entries: 16,
        })
        .expect("existing operation updates still publish at the run cap");
    let updated_review = review_projection::load_review_projection(root, &updated)
        .expect("updated metadata")
        .expect("updated review");
    let updated_index = operation_id_index::load(root, &updated_review)
        .expect("updated index")
        .expect("updated exact coverage");
    assert_eq!(
        (updated.watermark, updated_index.runs),
        (manifest.watermark + 1, index.runs)
    );
    manifest = updated;
    let pinned = UsageReader::open_at(root).expect("snapshot at cap");
    let mut late = UsageRecord::new_millis(
        "after-cap",
        /*observed_at_ms*/ now_ms + 1_000,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    )
    .with_operation("after-cap");
    late.event = Some("tool_started".into());
    late.started_at_ms = Some(now_ms + 1_000);
    journal
        .append_record(late)
        .expect("capture remains append-only at cap");
    let error = publisher
        .publish_batch(JournalReadLimits {
            max_bytes: 64 * 1024,
            max_entries: 16,
        })
        .expect_err("full operation index blocks publication");
    assert_eq!(
        error.to_string(),
        "invalid usage storage data: operation-id publication awaits bounded run maintenance"
    );
    assert!(matches!(
        error,
        crate::StorageError::OperationIndexAtCapacity
    ));
    assert_eq!(
        ManifestStore::new(root.join("manifest.json"))
            .load()
            .expect("unchanged readable snapshot"),
        manifest
    );
    let pending = UsageReader::open_at(root).expect("read snapshot and durable tail at cap");
    assert!(
        pending
            .records
            .iter()
            .any(|record| record.id == "after-cap")
    );
    crate::retention::Maintenance::default()
        .resume_publication(root)
        .expect("bounded maintenance releases publication capacity");
    let maintained = ManifestStore::new(root.join("manifest.json"))
        .load()
        .expect("maintenance snapshot");
    let maintained_review = review_projection::load_review_projection(root, &maintained)
        .expect("maintenance metadata")
        .expect("maintenance review");
    let maintained_index = operation_id_index::load(root, &maintained_review)
        .expect("maintenance index")
        .expect("fresh maintenance index");
    assert_eq!(
        maintained_index.runs.len(),
        crate::id_index::MAX_ID_RUNS - MAX_COMPACTION_INPUTS + 1,
        "yield publication capacity before draining the compaction backlog"
    );
    let resumed = publisher
        .publish_batch(JournalReadLimits {
            max_bytes: 64 * 1024,
            max_entries: 16,
        })
        .expect("publication resumes with remaining maintenance work");
    assert!(resumed.watermark > manifest.watermark);
    // Exercise automatic recovery from the same full immutable index below.
    ManifestStore::new(root.join("manifest.json"))
        .save(&manifest)
        .expect("restore full-run fixture for background recovery");
    let base_target = journal.checkpoint().expect("durable target").sequence;
    let captured = Arc::new(AtomicU64::new(/*v*/ 0));
    let capture_progress = Arc::clone(&captured);
    let capture_home = directory.path().to_path_buf();
    let resumed = std::thread::scope(|scope| {
        let (stop_capture, capture_stop) = std::sync::mpsc::channel::<()>();
        let (capture_ready, ready) = std::sync::mpsc::channel::<()>();
        let capture_thread = scope.spawn(move || {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(/*secs*/ 10);
            for index in 0..16_384_u64 {
                if capture_stop.try_recv() != Err(std::sync::mpsc::TryRecvError::Empty)
                    || std::time::Instant::now() >= deadline
                {
                    break;
                }
                let mut fact = UsageRecord::new_millis(
                    format!("live-cap-{index}"),
                    now_ms + 2_000 + index as i64,
                    /*input_tokens*/ 0,
                    /*output_tokens*/ 0,
                )
                .with_operation(format!("live-cap-{index}"));
                fact.event = Some("tool_started".into());
                fact.started_at_ms = Some(now_ms + 2_000 + index as i64);
                crate::capture_records(&capture_home, vec![fact]).expect("live capture");
                capture_progress.fetch_add(/*val*/ 1, Ordering::Release);
                if index == 0 {
                    capture_ready.send(()).expect("capture readiness");
                }
                std::thread::sleep(std::time::Duration::from_millis(/*millis*/ 1));
            }
        });
        ready
            .recv_timeout(std::time::Duration::from_secs(/*secs*/ 5))
            .expect("capture starts before the publisher");
        let _runtime = crate::UsagePublisherRuntime::start(directory.path())
            .expect("runtime maintenance starts");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(/*secs*/ 10);
        let progressed_during_capture = loop {
            let manifest = ManifestStore::new(root.join("manifest.json"))
                .load()
                .expect("runtime manifest");
            if manifest.watermark > base_target && !capture_thread.is_finished() {
                break true;
            }
            if std::time::Instant::now() >= deadline || capture_thread.is_finished() {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(/*millis*/ 25));
        };
        drop(stop_capture);
        capture_thread.join().expect("live capture thread");
        assert!(
            progressed_during_capture,
            "publication must resume during capture"
        );
        let captured = captured.load(Ordering::Acquire);
        assert!(captured > 0, "capture remains independent of maintenance");
        let target = journal
            .checkpoint()
            .expect("acknowledged capture frontier")
            .sequence;
        assert_eq!(target, base_target + captured);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(/*secs*/ 10);
        loop {
            let manifest = ManifestStore::new(root.join("manifest.json"))
                .load()
                .expect("runtime drain manifest");
            if manifest.watermark >= target {
                break manifest;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runtime drains acknowledged captures"
            );
            std::thread::sleep(std::time::Duration::from_millis(/*millis*/ 25));
        }
    });
    assert_eq!(
        resumed.watermark,
        journal.checkpoint().expect("durable frontier").sequence
    );
    let current = review_projection::load_review_projection(root, &resumed)
        .expect("metadata")
        .expect("review");
    let current_index = operation_id_index::load(root, &current)
        .expect("index")
        .expect("fresh index");
    assert!(current_index.runs.len() <= crate::id_index::MAX_ID_RUNS);
    let partitions = current
        .partitions
        .iter()
        .cloned()
        .map(|p| (p.partition, p))
        .collect();
    let ids = BTreeSet::from(["cap-0".into(), "after-cap".into()]);
    assert_eq!(
        operation_id_index::find_addresses(root, &current_index, &partitions, &ids)
            .expect("exact resumed addresses"),
        BTreeMap::from([
            ("cap-0".into(), 1),
            ("after-cap".into(), manifest.watermark + 1)
        ])
    );
    assert_eq!(pinned.manifest(), &manifest);
}
