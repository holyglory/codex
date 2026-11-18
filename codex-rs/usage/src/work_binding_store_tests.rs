use std::fs;

use super::*;
use crate::UsageJournal;
use crate::UsageReader;
use crate::UsageRecord;
use crate::capture_work_binding_transition;
use crate::read_current_work_binding;
use crate::read_work_binding_transitions;
use crate::work_binding_index::SCOPE_DIRECTORY;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

fn update(thread: &str, revision: u64, outcome: Option<&str>) -> WorkBindingUpdate {
    WorkBindingUpdate {
        version: 1,
        native_project_id: outcome.map(|_| "project".to_owned()),
        thread_id: thread.to_owned(),
        workstream_id: outcome.map(|_| "stream".to_owned()),
        outcome_id: outcome.map(str::to_owned),
        transition_sequence: revision,
        source: "test".into(),
        authority: "explicit".into(),
    }
}

fn advance(
    home: &std::path::Path,
    thread: &str,
    revision: u64,
    outcome: Option<&str>,
) -> crate::Result<WorkBindingTransition> {
    let transition = capture_work_binding_transition(home, update(thread, revision, outcome))?;
    let root = home.join(crate::USAGE_DIRECTORY_NAME);
    UsageJournal::new(root.join("events.jsonl")).append_record(
        UsageRecord::new_millis(format!("event-{thread}-{revision}"), revision as i64, 0, 0)
            .with_thread(thread),
    )?;
    Ok(transition)
}

#[test]
fn current_state_is_small_idempotent_and_preserves_explicit_clear() -> crate::Result<()> {
    let home = tempdir()?;
    let first = advance(home.path(), "thread", 1, Some("a"))?;
    assert_eq!(
        read_current_work_binding(home.path(), "thread")?,
        Some(first.clone())
    );
    assert_eq!(
        capture_work_binding_transition(home.path(), first.update.clone())?,
        first
    );
    assert!(
        capture_work_binding_transition(home.path(), update("thread", 1, Some("stale"))).is_err()
    );
    let clear = advance(home.path(), "thread", 2, None)?;
    assert_eq!(clear.update.native_project_id, None);
    assert_eq!(
        read_current_work_binding(home.path(), "thread")?,
        Some(clear.clone())
    );
    let lookup = WorkBindingLookup::open(&home.path().join(crate::USAGE_DIRECTORY_NAME))?;
    assert_eq!(
        lookup.lookup("thread", first.effective_capture_sequence)?,
        Some(first)
    );
    assert_eq!(
        lookup.lookup("thread", clear.effective_capture_sequence)?,
        Some(clear)
    );
    Ok(())
}

#[test]
fn rotation_has_no_global_transition_cap_and_old_lookup_stays_pinned() -> crate::Result<()> {
    let home = tempdir()?;
    let first = advance(home.path(), "thread", 1, Some("a"))?;
    let pinned = WorkBindingLookup::open(&home.path().join(crate::USAGE_DIRECTORY_NAME))?;
    let mut latest = first.clone();
    for revision in 2..=300 {
        latest = advance(home.path(), "thread", revision, Some("a"))?;
    }
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let page_count = fs::read_dir(
        root.join(crate::work_binding_index::STORE_DIRECTORY)
            .join(crate::work_binding_index::PAGE_DIRECTORY),
    )?
    .count();
    assert!(page_count >= 1);
    assert_eq!(
        pinned.lookup("thread", first.effective_capture_sequence)?,
        Some(first)
    );
    assert_eq!(
        pinned
            .lookup("thread", u64::MAX)?
            .unwrap()
            .effective_capture_sequence,
        1
    );
    let fresh = WorkBindingLookup::open(&root)?;
    assert_eq!(fresh.lookup("thread", u64::MAX)?, Some(latest));
    assert_eq!(read_work_binding_transitions(home.path())?.len(), 300);
    Ok(())
}

#[test]
fn rotation_accepts_repeated_capture_boundaries_without_usage_facts() -> crate::Result<()> {
    let home = tempdir()?;
    for revision in 1..=300 {
        capture_work_binding_transition(home.path(), update("thread", revision, Some("a")))?;
    }
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let lookup = WorkBindingLookup::open(&root)?;
    assert_eq!(
        lookup
            .lookup("thread", u64::MAX)?
            .unwrap()
            .update
            .transition_sequence,
        300
    );
    assert_eq!(read_work_binding_transitions(home.path())?.len(), 300);
    for (revision, outcome) in [(301, Some("b")), (302, None)] {
        let active = read_active(&root)?.expect("committed active checkpoint");
        let scope = read_scope(&root, "thread")?.expect("committed scope checkpoint");
        let current = crate::work_binding_index::read_current(&root, "thread")?
            .expect("committed current checkpoint");
        let accepted =
            capture_work_binding_transition(home.path(), update("thread", revision, outcome))?;
        assert_eq!(
            accepted.effective_capture_sequence,
            current.effective_capture_sequence
        );
        // Keep the real durable append and restore only its earlier metadata
        // checkpoint, reproducing a crash before current/index installation.
        write_atomic(&active_path(&root), &active)?;
        write_atomic(&scope_path(&root, "thread"), &scope)?;
        write_atomic(&current_path(&root, "thread"), &current)?;
        assert_eq!(
            read_current_work_binding(home.path(), "thread")?,
            Some(accepted.clone())
        );
        assert_eq!(
            read_scope(&root, "thread")?
                .expect("recovered scope")
                .latest_revision,
            revision
        );
        let recovered = read_active(&root)?.expect("recovered active checkpoint");
        assert_eq!(
            fs::metadata(page_path(&root, &recovered.file)?)?.len(),
            recovered.bytes
        );
        assert_eq!(
            capture_work_binding_transition(home.path(), accepted.update.clone())?,
            accepted
        );
    }
    assert_eq!(read_work_binding_transitions(home.path())?.len(), 302);
    assert_eq!(
        lookup
            .lookup("thread", u64::MAX)?
            .expect("original pinned current")
            .update
            .transition_sequence,
        300
    );
    Ok(())
}

#[test]
fn corrupt_scope_and_current_are_errors_not_an_inferred_clear() -> crate::Result<()> {
    let home = tempdir()?;
    let first = advance(home.path(), "thread", 1, Some("a"))?;
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    let current = root
        .join(crate::work_binding_index::STORE_DIRECTORY)
        .join(crate::work_binding_index::CURRENT_DIRECTORY)
        .join(format!(
            "{}.json",
            crate::work_binding_index::scope_file_key("thread")
        ));
    fs::write(&current, b"{broken}\n")?;
    assert!(read_current_work_binding(home.path(), "thread").is_err());
    fs::write(&current, format!("{}\n", serde_json::to_string(&first)?))?;
    let scope = root
        .join(crate::work_binding_index::STORE_DIRECTORY)
        .join(SCOPE_DIRECTORY)
        .join(format!(
            "{}.json",
            crate::work_binding_index::scope_file_key("thread")
        ));
    fs::write(scope, b"{broken}\n")?;
    assert!(read_current_work_binding(home.path(), "thread").is_ok());
    assert!(
        WorkBindingLookup::open(&root)?
            .lookup("thread", u64::MAX)
            .is_err()
    );
    Ok(())
}

#[test]
fn legacy_file_bootstraps_once_without_a_migration_scan_on_each_switch() -> crate::Result<()> {
    let home = tempdir()?;
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    fs::create_dir_all(&root)?;
    let transition = WorkBindingTransition {
        update: update("thread", 1, Some("legacy")),
        effective_capture_sequence: 1,
        captured_at_ms: 1,
    };
    fs::write(
        root.join(crate::work_binding_index::LEGACY_FILE),
        format!("{}\n", serde_json::to_string(&transition)?),
    )?;
    assert_eq!(
        read_current_work_binding(home.path(), "thread")?,
        Some(transition)
    );
    UsageJournal::new(root.join("events.jsonl"))
        .append_record(UsageRecord::new_millis("event", 1, 0, 0))?;
    let next = capture_work_binding_transition(home.path(), update("thread", 2, Some("next")))?;
    assert_eq!(next.effective_capture_sequence, 2);
    assert_eq!(read_work_binding_transitions(home.path())?.len(), 2);
    assert!(
        root.join(crate::work_binding_index::STORE_DIRECTORY)
            .join("active.json")
            .is_file()
    );
    Ok(())
}

#[test]
fn pruning_requires_reader_release_and_keeps_one_anchor_per_scope() -> crate::Result<()> {
    let home = tempdir()?;
    let _first = advance(home.path(), "thread", 1, Some("a"))?;
    let pinned = UsageReader::open(home.path())?;
    for revision in 2..=300 {
        advance(home.path(), "thread", revision, Some("a"))?;
    }
    let clear = advance(home.path(), "thread", 301, None)?;
    let root = home.path().join(crate::USAGE_DIRECTORY_NAME);
    assert!(!prune_before(&root, clear.effective_capture_sequence)?);
    drop(pinned);
    assert!(prune_before(&root, clear.effective_capture_sequence)?);
    let rows = read_work_binding_transitions(home.path())?;
    assert!(rows.len() < 302);
    assert_eq!(rows.last(), Some(&clear));
    assert!(!root.join(crate::work_binding_index::LEGACY_FILE).exists());
    let fresh = WorkBindingLookup::open(&root)?;
    assert_eq!(
        fresh.lookup("thread", u64::MAX)?.unwrap().update.outcome_id,
        None
    );
    Ok(())
}
