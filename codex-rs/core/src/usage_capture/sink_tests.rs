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
