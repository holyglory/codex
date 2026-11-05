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
    }
    let statuses: Vec<_> = children
        .iter_mut()
        .map(|child| child.wait().expect("writer exited"))
        .collect();
    assert!(
        statuses.iter().all(std::process::ExitStatus::success),
        "{statuses:?}"
    );
    let entries = journal.replay().expect("all writers replay");
    observed.extend(journal.read_from(consumed).expect("final replay").0);
    assert_eq!(observed, entries);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        (1..=200).collect::<Vec<_>>()
    );
    let ids: std::collections::BTreeSet<_> = entries
        .into_iter()
        .map(|entry| match entry.payload {
            JournalPayload::Record(record) => record.id,
            JournalPayload::Correction(_) => panic!("expected observation"),
        })
        .collect();
    assert_eq!(ids.len(), 200);
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
