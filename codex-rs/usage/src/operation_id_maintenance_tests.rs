use std::collections::{BTreeMap, BTreeSet};

use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::{JournalReadLimits, UsageJournal, UsagePublisher, UsageReader, UsageRecord};

fn publish(root: &Path, id: &str) -> crate::UsageManifest {
    let mut fact = UsageRecord::new_millis(id, /*observed_at_ms*/ 1_000, /*input_tokens*/ 0, /*output_tokens*/ 0)
        .with_operation(id);
    fact.event = Some("tool_started".into());
    fact.started_at_ms = Some(1_000);
    UsageJournal::new(root.join("events.jsonl")).append_record(fact).expect("real capture");
    UsagePublisher::open_at(root).expect("publisher").publish_batch(JournalReadLimits {
        max_bytes: 64 * 1024, max_entries: 16,
    }).expect("publication")
}

#[test]
fn streaming_merge_preserves_appended_runs_and_pinned_reader_files() {
    let directory = tempdir().expect("directory");
    let root = directory.path();
    publish(root, "first");
    let manifest = publish(root, "second");
    let pinned = UsageReader::open_at(root).expect("pin two-run generation");
    let source = review_projection::load_review_projection_shared(root, &manifest).expect("metadata").expect("source");
    let index = operation_id_index::load(root, &source).expect("index").expect("fresh index");
    let old_files = index.runs.iter().flat_map(|entry| {
        [Some(entry.run.data_file.clone()), Some(entry.run.fence_file.clone()), entry.run.bloom_file.clone()]
    }).flatten().collect::<Vec<_>>();
    let mut job = Job::start(root, source.clone(), index.runs.clone(), SnapshotPin::acquire(root).expect("job pin"))
        .expect("two-run job");
    assert!(!job.step(/*max_entries*/ 1).expect("bounded first input"));
    let appended = publish(root, "third");
    let mut maintenance = Maintenance { job: Some(job) };
    for _ in 0..8 {
        maintenance.step(root, /*max_entries*/ 1).expect("bounded merge and install");
        if maintenance.job.is_none() { break; }
    }
    assert!(maintenance.job.is_none());
    let current = ManifestStore::new(root.join("manifest.json")).load().expect("installed manifest");
    let review = review_projection::load_review_projection(root, &current).expect("metadata").expect("current review");
    let merged = operation_id_index::load(root, &review).expect("index").expect("merged index");
    assert_eq!((merged.watermark.sequence, merged.runs.len(), current.review_counts.clone()),
        (appended.watermark, 2, appended.review_counts));
    let partitions = review.partitions.iter().cloned().map(|p| (p.partition, p)).collect();
    let ids = BTreeSet::from(["first".into(), "second".into(), "third".into(), "absent".into()]);
    assert_eq!(operation_id_index::find_addresses(root, &merged, &partitions, &ids).expect("restart exact lookup"),
        BTreeMap::from([("first".into(), 1), ("second".into(), 2), ("third".into(), 3)]));
    let old_partitions = source.partitions.iter().cloned().map(|p| (p.partition, p)).collect();
    assert_eq!(operation_id_index::find_addresses(root, &index, &old_partitions, &ids).expect("pinned old exact lookup"),
        BTreeMap::from([("first".into(), 1), ("second".into(), 2)]));
    assert_eq!(crate::reclamation::reclaim(root).expect("reader pins old graph"), crate::reclamation::Reclamation::default());
    assert!(old_files.iter().all(|file| root.join("segments").join(file).exists()));
    drop(pinned);
    crate::reclamation::reclaim(root).expect("release obsolete exact runs");
    assert!(old_files.iter().all(|file| !root.join("segments").join(file).exists()));
    for entry in &merged.runs {
        for file in [Some(&entry.run.data_file), Some(&entry.run.fence_file), entry.run.bloom_file.as_ref()].into_iter().flatten() {
            codex_private_storage::verify_private_file(&root.join("segments").join(file)).expect("current run graph retained privately");
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
    let mut source = review_projection::load_review_projection(root, &manifest).expect("metadata").expect("review");
    // Exercise the isolated streaming merge with both incarnations in its
    // inputs. Publication/retirement integration is covered by review_retention.
    let entries = [
        OperationIdEntry { operation_id: "repeat".into(), first_sequence: 1 },
        OperationIdEntry { operation_id: "repeat".into(), first_sequence: 3 },
    ];
    let mut inputs = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let mut writer = RunWriter::new(root, &format!("review-operation-id-test-{i}.bin"),
            &format!("review-operation-id-test-fence-{i}.bin"), /*level*/ 0, /*estimated_entries*/ 1).expect("writer");
        writer.push_entry(entry).expect("address");
        inputs.push(OperationIdRun { run: writer.finish().expect("run"), retired_through: 0 });
    }
    let mut live = vec![0; REVIEW_PARTITION_SIZE as usize / 64];
    live[0] = 1 << 2;
    source.partitions[0].operation_sequences = Some(live);
    source.partitions[0].operation_count = 1;
    source.retired_operation_rows = 1;
    let mut job = Job::start(root, Arc::new(source.clone()), inputs.clone(), SnapshotPin::acquire(root).expect("pin")).expect("merge");
    assert!(!job.step(/*max_entries*/ 1).expect("retired input consumes budget"));
    assert!(!job.step(/*max_entries*/ 1).expect("new incarnation consumes budget"));
    assert!(job.step(/*max_entries*/ 1).expect("finish"));
    let mut reader = RunReader::open(root, job.output.as_ref().expect("output")).expect("output reader");
    assert_eq!((reader.next_entry::<OperationIdEntry>().expect("winner"), reader.next_entry::<OperationIdEntry>().expect("end")),
        (Some(entries[1].clone()), None));
    source.partitions[0].operation_sequences = None;
    let mut duplicate = Job::start(root, Arc::new(source), inputs, SnapshotPin::acquire(root).expect("pin")).expect("duplicate job");
    assert!(duplicate.step(/*max_entries*/ 4).is_err());
}
