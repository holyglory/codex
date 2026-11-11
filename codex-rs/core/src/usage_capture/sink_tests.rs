use super::*;
use pretty_assertions::assert_eq;

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
    let a = sink.submit_work("thread".into(), set("a", /*expected_revision*/ 0)).await.expect("A receipt");
    sink.flush().await;
    // Hold queue processing to model a delayed capture worker, without racing a disk lock.
    sink.writing.store(true, Ordering::Release);
    sink.submit(UsageRecord::new("old-start", /*observed_at*/ 1, /*input_tokens*/ 0, /*output_tokens*/ 0).with_thread("thread").with_operation("old"));
    let (receipt, received) = tokio::sync::oneshot::channel();
    sink.enqueue(PendingCapture::Work(PendingWork { thread_id: "thread".into(), action: set("b", /*expected_revision*/ 1), receipt }));
    sink.submit(UsageRecord::new("old-finish", /*observed_at*/ 2, /*input_tokens*/ 0, /*output_tokens*/ 0).with_thread("thread").with_operation("old"));
    sink.submit(UsageRecord::new("new-start", /*observed_at*/ 3, /*input_tokens*/ 0, /*output_tokens*/ 0).with_thread("thread").with_operation("new"));
    sink.writing.store(false, Ordering::Release);
    sink.flush().await;
    let b = received.await.expect("control reply").expect("B receipt");
    let boundary = b.transition.expect("B transition").effective_capture_sequence;
    let entries = codex_usage::UsageJournal::new(storage.join("events.jsonl")).replay().expect("captured records");
    assert_eq!(entries.iter().map(|entry| entry.sequence < boundary).collect::<Vec<_>>(), vec![true, false, false]);
    assert_eq!(a.revision, 1);

    // A storage outage before the control must not apply that control after recovery.
    let backup = home.path().join("saved-usage");
    std::fs::rename(&storage, &backup).expect("preserve fixture data");
    std::fs::write(&storage, b"intentional storage outage").expect("block capture storage");
    sink.writing.store(true, Ordering::Release);
    sink.submit(UsageRecord::new("retry-start", /*observed_at*/ 4, /*input_tokens*/ 0, /*output_tokens*/ 0).with_thread("thread"));
    let (receipt, received) = tokio::sync::oneshot::channel();
    sink.enqueue(PendingCapture::Work(PendingWork { thread_id: "thread".into(), action: set("c", /*expected_revision*/ 2), receipt }));
    sink.writing.store(false, Ordering::Release);
    sink.flush().await;
    assert!(received.await.expect("failed prefix reply").is_err());
    std::fs::remove_file(&storage).expect("remove fixture outage");
    std::fs::rename(&backup, &storage).expect("restore fixture storage");
    sink.flush().await;
    let status = sink.submit_work("thread".into(), UsageWorkAction::Status).await.expect("recovered status");
    assert_eq!(status.revision, 2, "failed controls never replay on accounting recovery");
    let c = sink.submit_work("thread".into(), set("c", /*expected_revision*/ 2)).await.expect("explicit retry");
    assert_eq!(c.revision, 3);
    assert_eq!(codex_usage::read_work_binding_transitions(home.path()).expect("transitions").len(), 3);
}

#[tokio::test]
async fn cancelled_and_overflowed_controls_do_not_create_switches() {
    let home = tempfile::tempdir().expect("temporary home");
    let storage = home.path().join(codex_usage::USAGE_DIRECTORY_NAME);
    std::fs::create_dir_all(&storage).expect("usage directory");
    let sink = CaptureSink::new(home.path());
    sink.writing.store(true, Ordering::Release);
    let (receipt, received) = tokio::sync::oneshot::channel();
    sink.enqueue(PendingCapture::Work(PendingWork { thread_id: "thread".into(), action: UsageWorkAction::Clear { expected_revision: 0 }, receipt }));
    drop(received);
    for index in 1..MAX_PENDING_RECORDS {
        sink.submit(UsageRecord::new(format!("queued-{index}"), /*observed_at*/ 1, /*input_tokens*/ 0, /*output_tokens*/ 0));
    }
    let (receipt, received) = tokio::sync::oneshot::channel();
    sink.enqueue(PendingCapture::Work(PendingWork { thread_id: "thread".into(), action: UsageWorkAction::Clear { expected_revision: 0 }, receipt }));
    assert!(received.await.expect("overflow reply").is_err());
    sink.writing.store(false, Ordering::Release);
    sink.flush().await;
    assert_eq!(codex_usage::read_work_binding_transitions(home.path()).expect("transitions"), vec![]);
    assert_eq!(sink.dropped.load(Ordering::Acquire), 0, "control rejection is not an omitted observation");
}
