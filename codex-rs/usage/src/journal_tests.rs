use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;

use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;

fn payload(id: &str) -> JournalPayload {
    JournalPayload::Record(UsageRecord::new(
        id, /*observed_at*/ 3_600, /*input_tokens*/ 10, /*output_tokens*/ 4,
    ))
}

#[test]
fn restart_checkpoint_replays_only_new_complete_entries() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let journal = UsageJournal::new(&path);
    let first = payload(&"long-observation-".repeat(10_000));
    journal.append(first.clone()).expect("append long entry");
    let checkpoint = journal.checkpoint().expect("saved checkpoint");
    assert_eq!(
        checkpoint,
        JournalCheckpoint {
            sequence: 1,
            byte_offset: fs::metadata(&path).expect("journal metadata").len(),
        }
    );

    let independent = UsageJournal::new(&path);
    let second = payload("second");
    let third = payload("third");
    independent
        .append_batch(vec![second.clone(), third.clone()])
        .expect("append through independent handle");
    let (tail, consumed) = journal.read_from(checkpoint).expect("resume checkpoint");
    assert_eq!(
        tail,
        vec![
            JournalEntry {
                sequence: 2,
                payload: second,
            },
            JournalEntry {
                sequence: 3,
                payload: third,
            },
        ]
    );
    assert_eq!(
        consumed,
        independent.checkpoint().expect("current checkpoint")
    );
    assert_eq!(
        independent.read_from(consumed).expect("caught up"),
        (Vec::new(), consumed)
    );
    assert_eq!(journal.replay().expect("full replay")[0].payload, first);
    for invalid in [
        JournalCheckpoint {
            sequence: 0,
            ..checkpoint
        },
        JournalCheckpoint {
            byte_offset: checkpoint.byte_offset - 1,
            ..checkpoint
        },
        JournalCheckpoint {
            byte_offset: consumed.byte_offset + 1,
            ..consumed
        },
    ] {
        assert!(journal.read_from(invalid).is_err());
    }
    let before = fs::metadata(&path).expect("before retirement").len();
    assert!(crate::journal_rotation::rotate_prefix(&path, checkpoint).expect("retire prefix"));
    assert!(fs::metadata(&path).expect("reclaimed bytes").len() < before);
    assert_eq!(journal.checkpoint().expect("stable logical end"), consumed);
    assert_eq!(
        journal.read_from(checkpoint).expect("retained tail").0,
        tail
    );
    assert!(journal.read_from(JournalCheckpoint::default()).is_err());
    assert_eq!(
        independent
            .append(payload("fourth"))
            .expect("append after replacement"),
        JournalPosition {
            sequence: 4,
            byte_offset: consumed.byte_offset
        }
    );
    let complete = journal.checkpoint().expect("new complete checkpoint");
    assert!(crate::journal_rotation::rotate_prefix(&path, complete).expect("retire complete log"));
    assert_eq!(
        journal.checkpoint().expect("header-only checkpoint"),
        complete
    );
    assert!(journal.replay().expect("no retained entries").is_empty());
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("interrupted writer")
        .write_all(b"{\"sequence\":5")
        .expect("partial entry after header");
    assert_eq!(
        journal.checkpoint().expect("ignore partial suffix"),
        complete
    );
    journal
        .append(payload("fifth"))
        .expect("recover append after retirement");
    assert_eq!(
        journal.replay().expect("retained fifth entry"),
        vec![JournalEntry {
            sequence: 5,
            payload: payload("fifth"),
        }]
    );
}

#[test]
fn interrupted_final_lines_preserve_acknowledged_bytes() {
    let second = JournalEntry {
        sequence: 2,
        payload: payload("second"),
    };
    for suffix in [
        b"{\"sequence\":".to_vec(),
        serde_json::to_vec(&second).expect("unterminated entry"),
        vec![b'x'; 100_000],
    ] {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("events.jsonl");
        let journal = UsageJournal::new(&path);
        let first = JournalEntry {
            sequence: 1,
            payload: payload("first"),
        };
        journal
            .append(first.payload.clone())
            .expect("acknowledged append");
        let prefix = fs::read(&path).expect("acknowledged bytes");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("crashed writer")
            .write_all(&suffix)
            .expect("interrupted write");

        let reopened = UsageJournal::new(&path);
        assert_eq!(
            reopened.replay().expect("ignore incomplete line"),
            vec![first]
        );
        assert_eq!(
            reopened.checkpoint().expect("complete end"),
            JournalCheckpoint {
                sequence: 1,
                byte_offset: prefix.len() as u64,
            }
        );
        assert_eq!(
            reopened
                .append(second.payload.clone())
                .expect("recover append"),
            JournalPosition {
                sequence: 2,
                byte_offset: prefix.len() as u64,
            }
        );
        let mut expected = prefix;
        expected.extend(serde_json::to_vec(&second).expect("complete entry"));
        expected.push(b'\n');
        assert_eq!(fs::read(&path).expect("recovered bytes"), expected);
        assert_eq!(
            reopened.replay().expect("recovered replay").last(),
            Some(&second)
        );
    }
}

#[test]
fn replay_rejects_sequence_gaps_duplicates_and_out_of_order_entries() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let journal = UsageJournal::new(&path);
    for sequences in [vec![0], vec![2], vec![1, 1], vec![1, 3], vec![1, 2, 1]] {
        let mut bytes = Vec::new();
        for sequence in sequences {
            bytes.extend(
                serde_json::to_vec(&JournalEntry {
                    sequence,
                    payload: payload("entry"),
                })
                .expect("entry"),
            );
            bytes.push(b'\n');
        }
        fs::write(&path, bytes).expect("corrupted journal");
        assert!(matches!(
            journal.replay(),
            Err(StorageError::InvalidData(_))
        ));
    }
    fs::write(&path, b"{\"sequence\":1}\n").expect("invalid complete line");
    let before = fs::read(&path).expect("original bytes");
    assert!(journal.append(payload("new")).is_err());
    assert_eq!(fs::read(&path).expect("unchanged bytes"), before);
}

#[test]
fn independent_processes_append_one_contiguous_log() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    fs::write(&path, b"{\"sequence\":").expect("interrupted entry before concurrent recovery");
    let executable = std::env::current_exe().expect("test executable");
    let mut children = Vec::new();
    for _ in 0..4 {
        children.push(
            Command::new(&executable)
                .args(["--exact", "journal::tests::process_writer", "--ignored"])
                .env("CODEX_USAGE_JOURNAL_TEST_PATH", &path)
                .spawn()
                .expect("spawn independent writer"),
        );
    }
    let journal = UsageJournal::new(&path);
    let mut consumed = JournalCheckpoint::default();
    let mut observed = Vec::new();
    for index in 0..40 {
        journal
            .append(payload(&format!("parent-{index}")))
            .expect("parent append");
        let (entries, checkpoint) = journal.read_from(consumed).expect("concurrent replay");
        observed.extend(entries);
        consumed = checkpoint;
        if index % 5 == 0 {
            crate::journal_rotation::rotate_prefix(&path, consumed)
                .expect("rotate while independent processes append");
        }
    }
    let statuses: Vec<_> = children
        .iter_mut()
        .map(|child| child.wait().expect("writer exited"))
        .collect();
    assert!(
        statuses.iter().all(std::process::ExitStatus::success),
        "{statuses:?}"
    );
    observed.extend(journal.read_from(consumed).expect("final replay").0);
    assert_eq!(
        observed
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        (1..=200).collect::<Vec<_>>()
    );
    assert_eq!(
        journal.last_sequence().expect("all writers acknowledged"),
        200
    );
    let ids: std::collections::BTreeSet<_> = observed
        .into_iter()
        .map(|entry| match entry.payload {
            JournalPayload::Record(record) => record.id,
            JournalPayload::Correction(_) => panic!("expected observation"),
        })
        .collect();
    assert_eq!(ids.len(), 200);
}

#[test]
fn bounded_batches_continue_through_one_captured_target() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let journal = UsageJournal::new(&path);
    let expected = ["one", "two", "three", "four", "five"]
        .into_iter()
        .enumerate()
        .map(|(index, id)| JournalEntry {
            sequence: index as u64 + 1,
            payload: payload(id),
        })
        .collect::<Vec<_>>();
    journal
        .append_batch(expected.iter().map(|entry| entry.payload.clone()).collect())
        .expect("initial append");
    let target = journal.checkpoint().expect("captured target");
    UsageJournal::new(&path)
        .append(payload("later"))
        .expect("independent later append");
    let limits = JournalReadLimits {
        max_bytes: u64::MAX,
        max_entries: 2,
    };
    let mut after = JournalCheckpoint::default();
    let mut collected = Vec::new();
    for chunk in expected.chunks(2) {
        let batch = journal
            .read_batch(after, target, limits)
            .expect("bounded batch");
        assert_eq!(batch.entries.as_slice(), chunk);
        assert_eq!(batch.target, target);
        assert_eq!(
            batch.status,
            if batch.next == target {
                JournalReadStatus::Complete
            } else {
                JournalReadStatus::More
            }
        );
        after = batch.next;
        collected.extend(batch.entries);
    }
    assert_eq!(collected, expected);
    assert_eq!(after, target);
    assert_eq!(
        journal
            .read_batch(target, target, limits)
            .expect("caught up"),
        JournalReadBatch {
            entries: Vec::new(),
            next: target,
            target,
            status: JournalReadStatus::Complete,
        }
    );
    assert_eq!(journal.last_sequence().expect("later append retained"), 6);
}

#[test]
fn byte_limits_never_skip_an_entry_that_cannot_fit() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let journal = UsageJournal::new(&path);
    let first = JournalEntry {
        sequence: 1,
        payload: payload("one"),
    };
    journal.append(first.payload.clone()).expect("first append");
    let first_end = journal.checkpoint().expect("first checkpoint");
    let second = JournalEntry {
        sequence: 2,
        payload: payload(&"large".repeat(10_000)),
    };
    let third = JournalEntry {
        sequence: 3,
        payload: payload("three"),
    };
    journal
        .append_batch(vec![second.payload.clone(), third.payload.clone()])
        .expect("remaining appends");
    let target = journal.checkpoint().expect("target");
    let original = fs::read(&path).expect("canonical bytes");
    let limits = JournalReadLimits {
        max_bytes: first_end.byte_offset + 13,
        max_entries: 3,
    };
    assert_eq!(
        journal
            .read_batch(JournalCheckpoint::default(), target, limits)
            .expect("first bounded read"),
        JournalReadBatch {
            entries: vec![first],
            next: first_end,
            target,
            status: JournalReadStatus::More,
        }
    );
    assert_eq!(
        journal
            .read_batch(first_end, target, limits)
            .expect("oversized entry"),
        JournalReadBatch {
            entries: Vec::new(),
            next: first_end,
            target,
            status: JournalReadStatus::EntryExceedsByteLimit {
                minimum_bytes: limits.max_bytes + 1
            },
        }
    );
    let larger = JournalReadLimits {
        max_bytes: target.byte_offset - first_end.byte_offset,
        ..limits
    };
    assert_eq!(
        journal
            .read_batch(first_end, target, larger)
            .expect("resume with explicit larger budget"),
        JournalReadBatch {
            entries: vec![second, third],
            next: target,
            target,
            status: JournalReadStatus::Complete,
        }
    );
    assert_eq!(fs::read(&path).expect("retained bytes"), original);
}

#[test]
fn report_overlay_is_complete_or_explicitly_deferred_without_mutation() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let journal = UsageJournal::new(&path);
    journal
        .append(payload("published"))
        .expect("published entry");
    let published = journal.checkpoint().expect("published checkpoint");
    let expected = vec![
        JournalEntry {
            sequence: 2,
            payload: payload("second"),
        },
        JournalEntry {
            sequence: 3,
            payload: payload("third"),
        },
    ];
    journal
        .append_batch(expected.iter().map(|entry| entry.payload.clone()).collect())
        .expect("unpublished tail");
    let target = journal.checkpoint().expect("complete tail target");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("interrupted writer")
        .write_all(b"{\"sequence\":4")
        .expect("torn suffix");
    let original = fs::read(&path).expect("original canonical bytes");
    let pending_bytes = target.byte_offset - published.byte_offset;
    for limits in [
        JournalReadLimits {
            max_bytes: u64::MAX,
            max_entries: 1,
        },
        JournalReadLimits {
            max_bytes: pending_bytes - 1,
            max_entries: 2,
        },
    ] {
        assert_eq!(
            journal
                .read_tail(published, limits)
                .expect("deferred overlay"),
            JournalTailRead::Deferred {
                target,
                pending_entries: 2,
                pending_bytes
            }
        );
    }
    assert_eq!(
        journal
            .read_tail(
                published,
                JournalReadLimits {
                    max_bytes: pending_bytes,
                    max_entries: 2
                }
            )
            .expect("complete overlay"),
        JournalTailRead::Complete(JournalReadBatch {
            entries: expected,
            next: target,
            target,
            status: JournalReadStatus::Complete,
        })
    );
    assert_eq!(fs::read(&path).expect("retained canonical bytes"), original);
}

#[test]
fn bounded_reads_do_not_replay_history_before_the_checkpoint() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("events.jsonl");
    let journal = UsageJournal::new(&path);
    let limits = JournalReadLimits {
        max_bytes: 8 * 1024,
        max_entries: 1,
    };
    assert_eq!(
        journal
            .read_tail(JournalCheckpoint::default(), limits)
            .expect("absent journal"),
        JournalTailRead::Complete(JournalReadBatch {
            entries: Vec::new(),
            next: JournalCheckpoint::default(),
            target: JournalCheckpoint::default(),
            status: JournalReadStatus::Complete,
        })
    );
    assert!(!path.exists());
    journal.append(payload("historical")).expect("old entry");
    let after = journal.checkpoint().expect("saved checkpoint");
    let expected = JournalEntry {
        sequence: 2,
        payload: payload("new"),
    };
    journal.append(expected.payload.clone()).expect("new entry");
    let target = journal.checkpoint().expect("target");
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("isolated damaged history")
        .write_all(b"x")
        .expect("damage old entry");
    assert_eq!(
        journal
            .read_batch(after, target, limits)
            .expect("only new range decoded"),
        JournalReadBatch {
            entries: vec![expected],
            next: target,
            target,
            status: JournalReadStatus::Complete,
        }
    );
    assert!(journal.replay().is_err());
}

#[test]
#[cfg(unix)]
fn private_usage_files_do_not_change_generic_or_codex_home_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempdir().expect("isolated owner directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(/*mode*/ 0o755))
        .expect("owner boundary mode");
    let generic = UsageJournal::new(directory.path().join("standalone.jsonl"));
    generic
        .append(payload("generic"))
        .expect("private standalone append");
    codex_private_storage::verify_private_file(generic.path()).expect("private standalone journal");
    assert_eq!(
        fs::metadata(directory.path())
            .expect("unchanged generic parent")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    let root = directory.path().join(crate::USAGE_DIRECTORY_NAME);
    fs::create_dir_all(&root).expect("existing usage directory");
    fs::set_permissions(&root, fs::Permissions::from_mode(/*mode*/ 0o755))
        .expect("old broad directory mode");
    let journal = UsageJournal::new(root.join("events.jsonl"));
    journal
        .append(payload("first"))
        .expect("first private append");
    let prefix = fs::read(journal.path()).expect("acknowledged prefix");
    fs::set_permissions(journal.path(), fs::Permissions::from_mode(/*mode*/ 0o644))
        .expect("old broad file mode");
    journal
        .append(payload("second"))
        .expect("repair permissions before append");
    codex_private_storage::verify_private_directory(&root).expect("private usage namespace");
    codex_private_storage::verify_private_file(journal.path())
        .expect("private acknowledged journal");
    assert!(
        fs::read(journal.path())
            .expect("retained facts")
            .starts_with(&prefix)
    );
    assert_eq!(journal.replay().expect("all facts remain").len(), 2);
    assert_eq!(
        fs::metadata(directory.path())
            .expect("unchanged CODEX_HOME owner boundary")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
#[ignore = "child process fixture for independent_processes_append_one_contiguous_log"]
fn process_writer() {
    let path = std::env::var_os("CODEX_USAGE_JOURNAL_TEST_PATH").expect("child journal path");
    let journal = UsageJournal::new(PathBuf::from(path));
    for index in 0..40 {
        journal
            .append(payload(&format!("child-{}-{index}", std::process::id())))
            .expect("child append");
    }
}
