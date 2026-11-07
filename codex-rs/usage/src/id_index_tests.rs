use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
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
        for file in [&run.data_file, &run.fence_file]
            .into_iter()
            .chain(run.bloom_file.iter())
        {
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
fn bloom_routes_missing_hashes_without_opening_exact_run_pages() {
    let directory = tempdir().expect("index directory");
    let root = directory.path();
    let entries = (1..=512)
        .map(|sequence| {
            entry(
                &format!("present-{sequence:04}"),
                sequence,
                "segment.parquet",
            )
        })
        .collect::<Vec<_>>();
    let write =
        publish_id_index(root, None, checkpoint(512), "bloom", &entries).expect("hinted run");
    let descriptor = load_id_index(root, Some(&write.file), checkpoint(512)).expect("restart");
    let run = &descriptor.runs[0];
    let hint = crate::id_index_bloom::load(root, run).expect("valid optional hint");
    let missing = (0..4096)
        .map(|index| format!("absent-{index}"))
        .find(|id| !hint.might_contain(id_hash(id)))
        .expect("a definitely absent routing hash");
    let mut requested = entries
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<BTreeSet<_>>();
    requested.insert(missing.clone());
    assert_eq!(
        lookup_ids(root, &descriptor, &requested).expect("present and absent IDs"),
        IdLookup {
            found: entries
                .into_iter()
                .map(|entry| (entry.id.clone(), entry))
                .collect::<BTreeMap<_, _>>(),
            watermark: checkpoint(512),
            complete_from_sequence: 1,
        }
    );
    assert_private(root, &descriptor);
    let data = root.join("segments").join(&run.data_file);
    let fence = root.join("segments").join(&run.fence_file);
    fs::rename(&data, data.with_extension("saved")).expect("unavailable data fixture");
    fs::rename(&fence, fence.with_extension("saved")).expect("unavailable fence fixture");
    assert!(
        lookup_ids(root, &descriptor, &BTreeSet::from([missing.clone()]))
            .expect("negative hint avoids opening canonical files")
            .found
            .is_empty()
    );
    let mut legacy = descriptor.clone();
    legacy.runs[0].bloom_file = None;
    assert!(lookup_ids(root, &legacy, &BTreeSet::from([missing])).is_err());
}

#[test]
fn missing_corrupt_and_unsupported_hints_fall_back_to_exact_ids() {
    #[derive(Clone, Copy, Debug)]
    enum Mode {
        Legacy,
        Missing,
        Corrupt,
        Unsupported,
    }
    for mode in [
        Mode::Legacy,
        Mode::Missing,
        Mode::Corrupt,
        Mode::Unsupported,
    ] {
        let directory = tempdir().expect("index directory");
        let root = directory.path();
        let entries = [
            entry("first", 1, "one.parquet"),
            entry("last", 2, "two.parquet"),
        ];
        let mut descriptor = publish_id_index(root, None, checkpoint(2), "fallback", &entries)
            .expect("hinted run")
            .descriptor;
        let path = root
            .join("segments")
            .join(descriptor.runs[0].bloom_file.as_ref().expect("hint file"));
        match mode {
            Mode::Legacy => {
                let mut value = serde_json::to_value(&descriptor).expect("descriptor JSON");
                value["runs"][0]
                    .as_object_mut()
                    .expect("run")
                    .remove("bloomFile");
                descriptor = serde_json::from_value(value).expect("legacy descriptor without hint");
            }
            Mode::Missing => fs::remove_file(path).expect("missing optional hint fixture"),
            Mode::Corrupt => {
                let mut bytes = fs::read(&path).expect("hint bytes");
                bytes[40..].fill(0);
                fs::write(path, bytes).expect("zeroed bits with the original checksum");
            }
            Mode::Unsupported => {
                let run = &mut descriptor.runs[0];
                let mut bytes = fs::read(path).expect("hint bytes");
                bytes[8..12].copy_from_slice(&2_u32.to_le_bytes());
                // Give the unsupported format a valid content checksum so this
                // exercises version fallback independently of corruption.
                let digest = bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
                    (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
                });
                let name = format!("{}.bloom-{digest:016x}.bin", run.data_file);
                crate::storage_file::create_new(&root.join("segments").join(&name))
                    .expect("private unsupported hint")
                    .write_all(&bytes)
                    .expect("hint fixture");
                run.bloom_file = Some(name);
            }
        }
        assert!(crate::id_index_bloom::load(root, &descriptor.runs[0]).is_none());
        assert_eq!(
            lookup_ids(root, &descriptor, &ids(&["first", "last", "absent"]))
                .expect("exact fallback remains available")
                .found,
            entries
                .into_iter()
                .map(|entry| (entry.id.clone(), entry))
                .collect::<BTreeMap<_, _>>(),
            "mode {mode:?}"
        );
    }
}

#[test]
fn compaction_hints_survive_restart_and_follow_snapshot_reclamation() {
    let home = tempdir().expect("home");
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let journal = crate::UsageJournal::new(root.join("events.jsonl"));
    let mut publisher = crate::UsagePublisher::open_at(&root).expect("publisher");
    for id in ["first", "second"] {
        journal
            .append_record(crate::UsageRecord::new(
                id, /*observed_at*/ 3_600, /*input_tokens*/ 1, /*output_tokens*/ 2,
            ))
            .expect("capture");
        publisher
            .publish_batch(crate::JournalReadLimits {
                max_bytes: 64 * 1024,
                max_entries: 4,
            })
            .expect("publish one immutable run");
    }
    let pinned = crate::UsageReader::open_at(&root).expect("pin source snapshot");
    let mut manifest = pinned.manifest().clone();
    let current = load_id_index(
        &root,
        manifest.id_index_file.as_deref(),
        crate::JournalCheckpoint {
            sequence: manifest.watermark,
            byte_offset: manifest.journal_byte_offset,
        },
    )
    .expect("current index");
    let old_hints = current
        .runs
        .iter()
        .map(|run| run.bloom_file.clone().expect("per-run hints"))
        .collect::<Vec<_>>();
    let requested = ids(&["first", "second", "absent"]);
    let expected = lookup_ids(&root, &current, &requested).expect("original exact facts");
    let plan = crate::id_index_compaction::IdCompactionPlan {
        source_watermark: current.watermark,
        inputs: current.runs.clone(),
        output_level: 1,
    };
    let mut job = IdCompactionJob::start(&root, plan.clone(), "hint-compact").expect("pinned job");
    let IdCompactionProgress::Finished(output) =
        job.step(/*max_entries*/ 8).expect("bounded merge")
    else {
        panic!("two entries fit one step");
    };
    let output_hint = output
        .bloom_file
        .clone()
        .expect("compaction rebuilt its hint");
    {
        let lock =
            crate::storage_file::open_read_write(&root.join(".snapshot.lock")).expect("lock");
        lock.lock().expect("install lock");
        let installed = install_id_compaction(&root, &current, &plan, output, "hint-install")
            .expect("descriptor install");
        manifest.id_index_file = Some(installed.file);
        manifest.mark_snapshot(manifest.watermark, manifest.freshness);
        crate::ManifestStore::new(root.join("manifest.json"))
            .save(&manifest)
            .expect("publish refs");
    }
    drop(job);
    let restarted = load_id_index(&root, manifest.id_index_file.as_deref(), current.watermark)
        .expect("restart with compacted hint");
    assert_eq!(
        lookup_ids(&root, &restarted, &requested).expect("compacted exact lookup"),
        expected
    );
    assert_private(&root, &restarted);
    assert_eq!(
        crate::reclamation::reclaim(&root).expect("old reader pins hints"),
        crate::reclamation::Reclamation::default()
    );
    assert!(
        old_hints
            .iter()
            .all(|name| root.join("segments").join(name).exists())
    );
    drop(pinned);
    crate::reclamation::reclaim(&root).expect("retire old hints");
    assert!(
        old_hints
            .iter()
            .all(|name| !root.join("segments").join(name).exists())
    );
    assert!(root.join("segments").join(output_hint).exists());
}

#[test]
fn exact_lookup_scans_full_collision_groups_across_fence_boundaries() {
    let stride = usize::try_from(ID_FENCE_STRIDE).expect("fence stride");
    for prefix_count in [0, stride - 1] {
        let directory = tempdir().expect("index directory");
        let root = directory.path();
        // Deterministic collisions exercise the real run framing and fence I/O
        // without depending on a chance collision in the production hash.
        let hash_id = |id: &str| match id {
            "before" => 0,
            "gap" => 3,
            "tail" => 4,
            "after" => 5,
            _ if id.starts_with("prefix-") => 1,
            _ => 2,
        };
        let write_run = |name: &str, entries: &[RecordIdEntry]| {
            let mut writer = RunWriter::new(
                root,
                &format!("{name}.bin"),
                &format!("{name}-fences.bin"),
                /*level*/ 0,
                entries.len() as u64,
            )
            .expect("run writer");
            for entry in entries {
                writer
                    .push_with_test_hash(entry, hash_id(&entry.id))
                    .expect("write routed entry");
            }
            writer.finish().expect("immutable run")
        };

        let mut original = (0..prefix_count)
            .map(|index| {
                entry(
                    &format!("prefix-{index:03}"),
                    u64::try_from(index + 1).expect("sequence"),
                    "first.parquet",
                )
            })
            .collect::<Vec<_>>();
        original.extend((0..2 * stride + 3).map(|index| {
            entry(
                &format!("collision-{index:03}"),
                u64::try_from(prefix_count + index + 1).expect("sequence"),
                "first.parquet",
            )
        }));
        let tail_sequence = u64::try_from(original.len() + 1).expect("tail sequence");
        original.push(entry("tail", tail_sequence, "first.parquet"));
        let first_run = write_run("first", &original);
        let later = [
            entry("collision-001", tail_sequence + 1, "later.parquet"),
            entry("collision-new", tail_sequence + 2, "later.parquet"),
        ];
        let later_run = write_run("later", &later);
        let descriptor = IdIndexDescriptor {
            version: 1,
            hash_version: ID_HASH_VERSION,
            watermark: checkpoint(tail_sequence + 2),
            complete_from_sequence: 1,
            retained_from_at_ms: None,
            runs: vec![later_run, first_run],
        };
        let mut expected = original
            .iter()
            .map(|entry| (entry.id.clone(), entry.clone()))
            .collect::<BTreeMap<_, _>>();
        expected.insert(later[1].id.clone(), later[1].clone());
        let mut all_requested = expected.keys().cloned().collect::<BTreeSet<_>>();
        all_requested.extend(ids(&["before", "collision-missing", "gap", "after"]));

        for requested in [
            all_requested,
            ids(&["collision-000"]),
            ids(&["collision-001", "collision-missing", "gap", "tail"]),
        ] {
            let lookup = lookup_ids_with_hash(root, &descriptor, &requested, hash_id)
                .expect("exact lookup across collision fences");
            assert_eq!(
                lookup,
                IdLookup {
                    found: expected
                        .iter()
                        .filter(|(id, _)| requested.contains(*id))
                        .map(|(id, entry)| (id.clone(), entry.clone()))
                        .collect(),
                    watermark: descriptor.watermark,
                    complete_from_sequence: 1,
                },
                "prefix count {prefix_count}"
            );
        }

        let mut conflicting = expected["collision-001"].clone();
        conflicting.segment_file = "conflict.parquet".into();
        let mut conflicting_descriptor = descriptor;
        conflicting_descriptor
            .runs
            .insert(/*index*/ 0, write_run("conflict", &[conflicting]));
        assert!(matches!(
            lookup_ids_with_hash(
                root,
                &conflicting_descriptor,
                &ids(&["collision-001"]),
                hash_id,
            ),
            Err(StorageError::InvalidData(message))
                if message == "conflicting record-id facts at one sequence"
        ));
    }
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
    for sequence in 2..=MAX_ID_RUNS as u64 {
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
    let next = checkpoint(MAX_ID_RUNS as u64 + 1);
    let deferred = entry("record-after-cap", next.sequence, "segment-after-cap.parquet");
    assert!(publish_id_index(root, Some(&current), next, "over-cap", std::slice::from_ref(&deferred)).is_err());
    assert!(!root.join("segments/record-id-run-over-cap-l0.bin").exists());
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
    let resumed = publish_id_index(root, Some(&installed.descriptor), next, "after-maintenance", &[deferred])
        .expect("publication resumes after bounded merge");
    assert_eq!(lookup_ids(root, &resumed.descriptor, &ids(&["record-after-cap"]))
        .expect("resumed exact lookup").found["record-after-cap"].first_sequence, next.sequence);
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
