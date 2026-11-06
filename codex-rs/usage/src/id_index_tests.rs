use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;
use crate::JournalCheckpoint;
use crate::StorageError;
use crate::id_index_compaction::IdCompactionJob;
use crate::id_index_compaction::IdCompactionProgress;
use crate::id_index_compaction::install_id_compaction;
use crate::id_index_compaction::plan_id_compaction;

fn checkpoint(sequence: u64) -> JournalCheckpoint {
    JournalCheckpoint {
        sequence,
        byte_offset: sequence * 100,
    }
}

fn entry(id: &str, sequence: u64, segment: &str) -> RecordIdEntry {
    RecordIdEntry {
        id: id.into(),
        first_sequence: sequence,
        account_id: Some("account-a".into()),
        thread_id: Some("thread-a".into()),
        repository_id: Some("repository-a".into()),
        segment_file: segment.into(),
    }
}

fn ids(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn assert_private(root: &Path, descriptor: &IdIndexDescriptor) {
    codex_private_storage::verify_private_directory(root).expect("private usage root");
    codex_private_storage::verify_private_directory(&root.join("segments"))
        .expect("private segments directory");
    for run in &descriptor.runs {
        for file in [&run.data_file, &run.fence_file] {
            codex_private_storage::verify_private_file(&root.join("segments").join(file))
                .expect("private immutable index run");
        }
    }
}

#[test]
fn exact_lookup_uses_full_ids_and_preserves_first_sequence_across_runs() {
    let directory = tempdir().expect("index directory");
    let root = directory.path();
    let first = publish_id_index(
        root,
        None,
        checkpoint(3),
        "first",
        &[
            entry("same-id", 1, "segment-one.parquet"),
            entry("alpha", 2, "segment-one.parquet"),
            entry("same-id", 3, "segment-two.parquet"),
        ],
    )
    .expect("first immutable run");
    let second = publish_id_index(
        root,
        Some(&first.descriptor),
        checkpoint(5),
        "second",
        &[
            entry("same-id", 4, "segment-two.parquet"),
            entry("bravo", 5, "segment-two.parquet"),
        ],
    )
    .expect("second immutable run");
    let lookup = lookup_ids(
        root,
        &second.descriptor,
        &ids(&["same-id", "alpha", "bravo", "missing"]),
    )
    .expect("bounded exact lookup");
    assert_eq!(lookup.watermark, checkpoint(5));
    assert_eq!(lookup.complete_from_sequence, 1);
    assert_eq!(lookup.found["same-id"].first_sequence, 1);
    assert_eq!(lookup.found["same-id"].segment_file, "segment-one.parquet");
    assert_eq!(lookup.found["alpha"].first_sequence, 2);
    assert_eq!(lookup.found["bravo"].first_sequence, 5);
    assert!(!lookup.found.contains_key("missing"));
    let loaded = load_id_index(root, Some(&second.file), checkpoint(5)).expect("metadata restart");
    assert_eq!(loaded, second.descriptor);
    assert_private(root, &loaded);
    assert_eq!(
        lookup_ids(root, &first.descriptor, &ids(&["same-id"]))
            .expect("pinned first generation")
            .found["same-id"]
            .first_sequence,
        1
    );
}

#[test]
fn missing_pointer_is_explicitly_incomplete_and_entry_limits_are_hard() {
    let directory = tempdir().expect("index directory");
    let root = directory.path();
    let missing = load_id_index(root, None, checkpoint(19)).expect("missing index metadata");
    assert_eq!(missing.complete_from_sequence, 20);
    assert_eq!(missing.runs, Vec::<IdRunDescriptor>::new());
    assert!(
        lookup_ids(root, &missing, &ids(&["old-record"]))
            .expect("incomplete lookup does not claim absence")
            .found
            .is_empty()
    );
    let huge = RecordIdEntry {
        id: "huge".into(),
        first_sequence: 1,
        account_id: None,
        thread_id: None,
        repository_id: None,
        segment_file: "segment.parquet".into(),
    };
    let mut huge = huge;
    huge.id.push_str(&"x".repeat(MAX_ID_ENTRY_BYTES));
    assert!(matches!(
        publish_id_index(root, None, checkpoint(1), "too-large", &[huge]),
        Err(StorageError::InvalidData(_))
    ));
}

#[test]
fn compaction_streams_runs_and_rejects_stale_install_without_releasing_pins() {
    let directory = tempdir().expect("index directory");
    let root = directory.path();
    let mut current = publish_id_index(
        root,
        None,
        checkpoint(1),
        "run-000",
        &[entry("record-001", 1, "segment-000.parquet")],
    )
    .expect("initial run")
    .descriptor;
    for sequence in 2..=130 {
        current = publish_id_index(
            root,
            Some(&current),
            checkpoint(sequence),
            &format!("run-{sequence:03}"),
            &[entry(
                &format!("record-{sequence:03}"),
                sequence,
                &format!("segment-{sequence:03}.parquet"),
            )],
        )
        .expect("small immutable run")
        .descriptor;
    }
    let plan = plan_id_compaction(&current).expect("bounded compaction plan");
    let old_descriptor = current.clone();
    let mut job = IdCompactionJob::start(root, plan.clone(), "compact-001").expect("streaming job");
    let output = loop {
        match job.step(3).expect("bounded compaction step") {
            IdCompactionProgress::More { .. } => continue,
            IdCompactionProgress::Finished(output) => break output,
        }
    };
    let mut output_reader =
        crate::id_index_run::RunReader::open(root, &output).expect("open compacted output");
    let mut output_ids = Vec::new();
    while let Some(entry) = output_reader.next().expect("read compacted output") {
        output_ids.push(entry.id);
    }
    assert_eq!(output.entry_count, 8, "compacted IDs: {output_ids:?}");
    let installed = install_id_compaction(root, &current, &plan, output, "installed-001")
        .expect("short descriptor install");
    assert!(installed.descriptor.runs.len() < current.runs.len());
    assert_eq!(installed.descriptor.watermark, current.watermark);
    let lookup = lookup_ids(
        root,
        &installed.descriptor,
        &ids(&["record-001", "record-064", "record-130", "not-present"]),
    )
    .expect("lookup after compaction");
    assert_eq!(
        lookup
            .found
            .get("record-001")
            .map(|entry| entry.first_sequence),
        Some(1),
        "found IDs: {:?}",
        lookup.found.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        lookup
            .found
            .get("record-064")
            .map(|entry| entry.first_sequence),
        Some(64),
        "found IDs: {:?}",
        lookup.found.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        lookup
            .found
            .get("record-130")
            .map(|entry| entry.first_sequence),
        Some(130),
        "found IDs: {:?}",
        lookup.found.keys().collect::<Vec<_>>()
    );
    assert!(!lookup.found.contains_key("not-present"));
    assert_private(root, &installed.descriptor);
    // The old generation is still valid for a pinned reader after installation.
    assert_eq!(
        lookup_ids(root, &old_descriptor, &ids(&["record-001", "record-130"]))
            .expect("old pinned generation")
            .found
            .len(),
        2
    );
    let mut stale = current;
    stale.watermark = checkpoint(129);
    assert!(matches!(
        install_id_compaction(
            root,
            &stale,
            &plan,
            installed.run.expect("output run"),
            "stale"
        ),
        Err(StorageError::InvalidData(_))
    ));
    // Compaction never deletes input files; a crash before installation leaves
    // all source runs and the prepared output recoverable for diagnosis.
    for run in &plan.inputs {
        assert!(fs::metadata(root.join("segments").join(&run.data_file)).is_ok());
    }
}
