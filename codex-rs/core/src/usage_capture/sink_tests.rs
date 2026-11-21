use super::*;
use crate::usage_capture::work_binding::UsageWorkBoundaryState;
use pretty_assertions::assert_eq;

fn queued_observation(id: impl Into<String>) -> UsageRecord {
    UsageRecord::new(
        id, /*observed_at*/ 1, /*input_tokens*/ 0, /*output_tokens*/ 0,
    )
    .with_thread("thread")
}

#[tokio::test]
async fn deferred_batch_replays_and_exposes_every_omitted_observation() {
    let home = tempfile::tempdir().expect("temporary home");
    let storage = home.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    std::fs::write(&storage, b"intentional storage outage").expect("block capture directory");
    let sink = CaptureSink::new(home.path());
    for index in 0..MAX_PENDING_RECORDS + 3 {
        let persistence = sink
            .submit_confirmed(
                UsageRecord::new(
                    format!("observation-{index}"),
                    /*observed_at*/ 1,
                    /*input_tokens*/ 0,
                    /*output_tokens*/ 0,
                )
                .with_thread("thread"),
            )
            .await;
        assert_eq!(
            persistence,
            if index < MAX_PENDING_RECORDS {
                Persistence::Pending
            } else {
                Persistence::Omitted
            }
        );
    }
    std::fs::remove_file(&storage).expect("recover fixture storage");
    sink.submit(
        UsageRecord::new(
            "recovery", /*observed_at*/ 2, /*input_tokens*/ 0, /*output_tokens*/ 0,
        )
        .with_thread("thread"),
    );
    sink.flush().await;
    let records = codex_usage::UsageJournal::new(storage.join("events.jsonl"))
        .records()
        .expect("read recovered observations");
    let gaps = records
        .iter()
        .filter(|record| record.event.as_deref() == Some("coverage_gap"))
        .collect::<Vec<_>>();
    assert_eq!(
        gaps.iter()
            .map(|gap| gap.missing_observations.unwrap_or_default())
            .sum::<u64>(),
        4
    );
    assert!(
        gaps.iter()
            .all(|gap| gap.thread_id.as_deref() == Some("thread")
                && gap.coverage.as_deref() == Some("incomplete"))
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.id.starts_with("observation-"))
            .count(),
        MAX_PENDING_RECORDS
    );
    assert!(
        records
            .iter()
            .all(|record| record.turn_id.is_none() && record.account_id.is_none())
    );
    assert_eq!(sink.dropped.load(Ordering::Acquire), 0);
    let recorded = sink
        .submit_confirmed(UsageRecord::new(
            "confirmed",
            /*observed_at*/ 3,
            /*input_tokens*/ 0,
            /*output_tokens*/ 0,
        ))
        .await;
    assert_eq!(recorded, Persistence::Recorded);
}

#[tokio::test]
async fn switches_wait_for_queued_starts_and_reject_failed_prefixes() {
    let home = tempfile::tempdir().expect("temporary home");
    let storage = home.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    std::fs::create_dir_all(&storage).expect("usage directory");
    let sink = CaptureSink::new(home.path());
    let set = |outcome: &str, expected_revision| UsageWorkAction::Set {
        native_project_id: "project".into(),
        workstream_id: None,
        outcome_id: Some(outcome.into()),
        expected_revision,
    };
    let a = sink
        .submit_work("thread".into(), set("a", /*expected_revision*/ 0))
        .await
        .expect("A receipt");
    sink.flush().await;
    // Hold queue processing to model a delayed capture worker, without racing a disk lock.
    sink.writing.store(true, Ordering::Release);
    sink.submit(queued_observation("old-start").with_operation("old"));
    let (receipt, received) = tokio::sync::oneshot::channel();
    sink.enqueue(PendingCapture::Work(PendingWork {
        thread_id: "thread".into(),
        action: set("b", /*expected_revision*/ 1),
        receipt,
    }));
    sink.submit(queued_observation("old-finish").with_operation("old"));
    sink.submit(queued_observation("new-start").with_operation("new"));
    sink.writing.store(false, Ordering::Release);
    sink.flush().await;
    let b = received.await.expect("control reply").expect("B receipt");
    let boundary = b
        .transition
        .expect("B transition")
        .effective_capture_sequence;
    let entries = codex_usage::UsageJournal::new(storage.join("events.jsonl"))
        .replay()
        .expect("captured records");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.sequence < boundary)
            .collect::<Vec<_>>(),
        vec![true, false, false]
    );
    assert_eq!(a.revision, 1);

    // A storage outage before the control must not apply that control after recovery.
    let backup = home.path().join("saved-usage");
    std::fs::rename(&storage, &backup).expect("preserve fixture data");
    std::fs::write(&storage, b"intentional storage outage").expect("block capture storage");
    sink.writing.store(true, Ordering::Release);
    sink.submit(queued_observation("retry-start"));
    let (receipt, received) = tokio::sync::oneshot::channel();
    sink.enqueue(PendingCapture::Work(PendingWork {
        thread_id: "thread".into(),
        action: set("c", /*expected_revision*/ 2),
        receipt,
    }));
    sink.writing.store(false, Ordering::Release);
    sink.flush().await;
    assert!(received.await.expect("failed prefix reply").is_err());
    std::fs::remove_file(&storage).expect("remove fixture outage");
    std::fs::rename(&backup, &storage).expect("restore fixture storage");
    sink.flush().await;
    let status = sink
        .submit_work("thread".into(), UsageWorkAction::Status)
        .await
        .expect("recovered status");
    assert_eq!(
        status.revision, 2,
        "failed controls never replay on accounting recovery"
    );
    let c = sink
        .submit_work("thread".into(), set("c", /*expected_revision*/ 2))
        .await
        .expect("explicit retry");
    assert_eq!(c.revision, 3);
    assert_eq!(
        codex_usage::read_work_binding_transitions(home.path())
            .expect("transitions")
            .len(),
        3
    );
}

#[tokio::test]
async fn cancelled_and_overflowed_controls_do_not_create_switches() {
    let home = tempfile::tempdir().expect("temporary home");
    let storage = home.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    std::fs::create_dir_all(&storage).expect("usage directory");
    let sink = CaptureSink::new(home.path());
    sink.writing.store(true, Ordering::Release);
    let (receipt, received) = tokio::sync::oneshot::channel();
    sink.enqueue(PendingCapture::Work(PendingWork {
        thread_id: "thread".into(),
        action: UsageWorkAction::Clear {
            expected_revision: 0,
        },
        receipt,
    }));
    drop(received);
    for index in 1..MAX_PENDING_RECORDS {
        sink.submit(queued_observation(format!("queued-{index}")));
    }
    let (receipt, received) = tokio::sync::oneshot::channel();
    sink.enqueue(PendingCapture::Work(PendingWork {
        thread_id: "thread".into(),
        action: UsageWorkAction::Clear {
            expected_revision: 0,
        },
        receipt,
    }));
    assert!(received.await.expect("overflow reply").is_err());
    sink.writing.store(false, Ordering::Release);
    sink.flush().await;
    assert_eq!(
        codex_usage::read_work_binding_transitions(home.path()).expect("transitions"),
        vec![]
    );
    assert_eq!(
        sink.dropped.load(Ordering::Acquire),
        0,
        "control rejection is not an omitted observation"
    );
}

#[tokio::test]
async fn boundary_requests_are_one_shot_and_explicit_states_are_preserved() {
    let home = tempfile::tempdir().expect("temporary home");
    std::fs::create_dir_all(home.path().join(codex_usage::USAGE_DIRECTORY_NAME))
        .expect("usage directory");
    let sink = CaptureSink::new(home.path());

    let first = sink
        .boundary_status("thread".into(), "turn-1".into(), None)
        .await
        .expect("first boundary status");
    assert_eq!(first.state, UsageWorkBoundaryState::Unknown);
    assert_eq!(
        first.request.as_ref().map(|request| request.reason),
        Some("missing_binding")
    );
    assert_eq!(
        first.request.as_ref().map(|request| request.options.len()),
        Some(5)
    );

    let repeated = sink
        .boundary_status("thread".into(), "turn-1".into(), None)
        .await
        .expect("repeated boundary status");
    assert_eq!(repeated.state, UsageWorkBoundaryState::Unknown);
    assert!(repeated.request.is_none(), "one request per boundary");

    let explicit = sink
        .boundary_status(
            "thread".into(),
            "turn-2".into(),
            Some(UsageWorkBoundaryState::Clear),
        )
        .await
        .expect("explicit boundary state");
    assert_eq!(explicit.state, UsageWorkBoundaryState::Clear);
    assert!(explicit.request.is_none());
    assert_eq!(
        explicit.revision, 0,
        "clearing an unbound scope is not a switch"
    );
    assert!(explicit.transition.is_none());

    sink.submit_work(
        "thread".into(),
        UsageWorkAction::Set {
            native_project_id: "project".into(),
            workstream_id: None,
            outcome_id: None,
            expected_revision: explicit.revision,
        },
    )
    .await
    .expect("binding before explicit clear");
    let cleared = sink
        .boundary_status(
            "thread".into(),
            "turn-4".into(),
            Some(UsageWorkBoundaryState::Clear),
        )
        .await
        .expect("clear bound work at boundary");
    assert_eq!(cleared.state, UsageWorkBoundaryState::Clear);
    assert!(
        cleared
            .transition
            .as_ref()
            .is_some_and(|value| value.update.native_project_id.is_none())
    );

    let unknown = sink
        .boundary_status(
            "thread".into(),
            "turn-3".into(),
            Some(UsageWorkBoundaryState::Unknown),
        )
        .await
        .expect("explicit unknown boundary state");
    assert_eq!(unknown.state, UsageWorkBoundaryState::Unknown);
    assert!(unknown.request.is_none());
}

#[tokio::test]
async fn child_inherits_parent_binding_before_override_and_resume_keeps_binding() {
    let home = tempfile::tempdir().expect("temporary home");
    std::fs::create_dir_all(home.path().join(codex_usage::USAGE_DIRECTORY_NAME))
        .expect("usage directory");
    let sink = CaptureSink::new(home.path());
    let set = |expected_revision| UsageWorkAction::Set {
        native_project_id: "project".into(),
        workstream_id: Some("stream".into()),
        outcome_id: Some("outcome".into()),
        expected_revision,
    };
    sink.submit_work("parent".into(), set(0))
        .await
        .expect("parent binding");

    let inherited = sink
        .inherit_work("parent".into(), "child".into())
        .await
        .expect("inherit parent binding")
        .expect("inherited binding receipt");
    assert_eq!(inherited.revision, 1);
    let child = sink
        .submit_work("child".into(), UsageWorkAction::Status)
        .await
        .expect("child status");
    assert_eq!(
        child
            .transition
            .as_ref()
            .map(|value| &value.update.native_project_id),
        Some(&Some("project".into()))
    );

    sink.submit_work(
        "child".into(),
        UsageWorkAction::Clear {
            expected_revision: 1,
        },
    )
    .await
    .expect("child override");
    assert!(
        sink.inherit_work("parent".into(), "child".into())
            .await
            .expect("explicit child override wins")
            .is_none()
    );
    let child_after_override = sink
        .submit_work("child".into(), UsageWorkAction::Status)
        .await
        .expect("child status after override");
    assert!(
        child_after_override
            .transition
            .as_ref()
            .is_some_and(|value| value.update.native_project_id.is_none())
    );

    drop(sink);
    let resumed = CaptureSink::new(home.path());
    let parent_status = resumed
        .boundary_status("parent".into(), "resume-turn".into(), None)
        .await
        .expect("resumed parent status");
    assert_eq!(parent_status.state, UsageWorkBoundaryState::Continue);
    assert!(parent_status.request.is_none());
}
