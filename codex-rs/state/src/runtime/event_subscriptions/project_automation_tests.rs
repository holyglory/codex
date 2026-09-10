use super::*;
use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::ProjectMode;
use codex_event_subscriptions::WorkPurpose;
use pretty_assertions::assert_eq;

async fn store() -> (SqliteEventSubscriptionStore, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::sqlite::open_sqlite_pool(
        &directory.path().join("queue.sqlite"),
        crate::sqlite::SqlitePoolProfile::Runtime,
    )
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../queue_migrations/0003_event_subscriptions.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../queue_migrations/0004_project_automation.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    (
        SqliteEventSubscriptionStore::new(std::sync::Arc::new(pool)),
        directory,
    )
}

#[tokio::test]
async fn automatic_enrollment_inherits_concurrent_children_without_reopening_completed_work() {
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    let project = store
        .project_command(
            "inherited",
            owner,
            None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: Some("delivery".into()),
            },
            100,
        )
        .await
        .unwrap();
    let project = store
        .project_command(
            "inherited",
            owner,
            Some(project.revision),
            ProjectAutomationCommand::LinkWork {
                outcome_id: Some("outcome".into()),
                experiment_ref: Some("review@1".into()),
                clear_outcome: false,
                clear_experiment: false,
            },
            101,
        )
        .await
        .unwrap();
    let first = ThreadId::new();
    let second = ThreadId::new();
    let (first_result, second_result) = tokio::join!(
        store.project_enroll_thread("inherited", first, Some(owner), 102),
        store.project_enroll_thread("inherited", second, Some(owner), 102),
    );
    first_result.unwrap();
    second_result.unwrap();
    let inherited = store.project_status("inherited").await.unwrap().unwrap();
    for child in [first, second] {
        assert_eq!(
            inherited.threads[&child.to_string()],
            WorkPurpose::Implementation
        );
        assert_eq!(inherited.thread_workstreams[&child.to_string()], "delivery");
        assert_eq!(inherited.thread_outcomes[&child.to_string()], "outcome");
        assert_eq!(inherited.thread_experiments[&child.to_string()], "review@1");
    }
    assert_eq!(
        inherited.implementation_starts,
        project.implementation_starts
    );
    let completed = store
        .project_command(
            "inherited",
            owner,
            Some(inherited.revision),
            ProjectAutomationCommand::Complete {
                outcome_ref: "outcome".into(),
            },
            103,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .project_enroll_thread("inherited", ThreadId::new(), Some(owner), 104)
            .await
            .unwrap(),
        completed
    );
    assert_eq!(
        store
            .project_enroll_thread("inherited", first, Some(owner), 105)
            .await
            .unwrap(),
        completed
    );
    assert_eq!(store.next_heartbeat_deadline().await.unwrap(), None);
}

#[tokio::test]
async fn deadlines_persist_and_share_the_subscription_scheduler() {
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    let project = store
        .project_command(
            "project-alpha",
            owner,
            None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: None,
            },
            1_000_000,
        )
        .await
        .unwrap();
    let project = store
        .project_command(
            "project-alpha",
            owner,
            Some(project.revision),
            ProjectAutomationCommand::ActivateDelivery {
                target: "linux-cli".into(),
                surface: "executable".into(),
                acceptance: "smoke".into(),
                delivery_interval_ms: Some(1000),
                hard_stop_interval_ms: Some(2000),
            },
            1_000_000,
        )
        .await
        .unwrap();
    assert_eq!(
        store.next_heartbeat_deadline().await.unwrap(),
        Some(1_001_000)
    );
    assert_eq!(
        store.collect_due_heartbeats(1_001_000).await.unwrap(),
        vec![owner]
    );
    let persisted = store
        .project_status("project-alpha")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.mode(1_001_000), ProjectMode::DeliveryDue);
    let pending = store.pending_wake(owner).await.unwrap().unwrap();
    assert_eq!(
        pending.wake.items[0].event.as_ref().unwrap().event_type,
        "delivery_due"
    );
    assert!(
        store
            .collect_due_heartbeats(1_001_000)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.collect_due_heartbeats(1_002_000).await.unwrap(),
        vec![owner]
    );
    assert_eq!(
        store
            .project_status("project-alpha")
            .await
            .unwrap()
            .unwrap()
            .mode(1_002_000),
        ProjectMode::RecoveryOnly
    );
    assert!(
        store
            .project_command(
                "project-alpha",
                owner,
                Some(project.revision - 1),
                ProjectAutomationCommand::Resume { target: None },
                1_002_000
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn specification_has_no_delivery_and_postponement_is_atomic() {
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    let project = store
        .project_command(
            "spec",
            owner,
            None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Specification,
                workstream: None,
            },
            100,
        )
        .await
        .unwrap();
    store.project_activity("spec", owner, 101).await.unwrap();
    assert_eq!(
        store.collect_due_heartbeats(7 * 86_400_000).await.unwrap(),
        vec![owner]
    );
    let restored = store.project_status("spec").await.unwrap().unwrap();
    assert_eq!(restored.mode(7 * 86_400_000), ProjectMode::PerformanceOnly);
    assert!(restored.delivery.is_empty());
    assert_eq!(restored.started_at_ms, project.started_at_ms);
    assert!(
        store
            .project_command(
                "spec",
                owner,
                None,
                ProjectAutomationCommand::Pause {
                    target: None,
                    authorization_ref: "owner".into()
                },
                7 * 86_400_000
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn interrupted_project_job_is_restored_and_explicit_postponement_retires_stale_wake() {
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    let project = store
        .project_command(
            "recover",
            owner,
            None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: None,
            },
            100,
        )
        .await
        .unwrap();
    let project = store
        .project_command(
            "recover",
            owner,
            Some(project.revision),
            ProjectAutomationCommand::ActivateDelivery {
                target: "cli".into(),
                surface: "binary".into(),
                acceptance: "launches".into(),
                delivery_interval_ms: Some(1000),
                hard_stop_interval_ms: Some(2000),
            },
            100,
        )
        .await
        .unwrap();
    store.collect_due_heartbeats(1100).await.unwrap();
    let pending = store.pending_wake(owner).await.unwrap().unwrap();
    store
        .acknowledge_wake(owner, pending.through_revision)
        .await
        .unwrap();
    assert!(store.pending_wake(owner).await.unwrap().is_none());
    store.restore_project_jobs().await.unwrap();
    assert_eq!(
        store.collect_due_heartbeats(1200).await.unwrap(),
        vec![owner]
    );
    store
        .project_command(
            "recover",
            owner,
            Some(project.revision),
            ProjectAutomationCommand::Postpone {
                target: "cli".into(),
                delivery_due_at_ms: 3100,
                hard_stop_at_ms: 4100,
                authorization_ref: "owner postponed".into(),
            },
            1200,
        )
        .await
        .unwrap();
    assert!(store.pending_wake(owner).await.unwrap().is_none());
    assert!(store.collect_due_heartbeats(2100).await.unwrap().is_empty());
}

#[tokio::test]
async fn event_wait_returns_only_its_matching_subscription_and_cancels_cleanly() {
    use codex_event_subscriptions::EventFilter;
    use codex_event_subscriptions::NewSubscription;
    use std::collections::BTreeSet;
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    let subscription = store
        .create(
            NewSubscription {
                thread_id: owner,
                filter: Some(EventFilter {
                    source: "build".into(),
                    event_types: BTreeSet::from(["completed".into()]),
                    labels: BTreeMap::from([("run".into(), "actual-run".into())]),
                }),
                source_cursor: None,
                heartbeat: None,
            },
            100,
        )
        .await
        .unwrap();
    let waiting_store = store.clone();
    let waiting = tokio::spawn(async move {
        waiting_store
            .await_subscription(owner, subscription.id)
            .await
    });
    store
        .publish(
            PublishedEvent {
                id: "unrelated".into(),
                source: "build".into(),
                event_type: "completed".into(),
                cursor: SourceCursor {
                    sequence: 1,
                    value: None,
                },
                labels: BTreeMap::from([("run".into(), "other-run".into())]),
                occurred_at_ms: 101,
            },
            101,
        )
        .await
        .unwrap();
    assert!(!waiting.is_finished());
    store
        .publish(
            PublishedEvent {
                id: "matching".into(),
                source: "build".into(),
                event_type: "completed".into(),
                cursor: SourceCursor {
                    sequence: 2,
                    value: None,
                },
                labels: BTreeMap::from([("run".into(), "actual-run".into())]),
                occurred_at_ms: 102,
            },
            102,
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.event.unwrap().cursor.sequence, 2);
    store.cancel(subscription.id).await.unwrap();
    assert!(
        store
            .await_subscription(owner, subscription.id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn event_wait_replays_completion_between_status_read_and_subscription() {
    use codex_event_subscriptions::EventFilter;
    use codex_event_subscriptions::NewSubscription;
    use std::collections::BTreeSet;
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    store
        .publish(
            PublishedEvent {
                id: "before-subscribe".into(),
                source: "build".into(),
                event_type: "completed".into(),
                cursor: SourceCursor {
                    sequence: 2,
                    value: None,
                },
                labels: BTreeMap::new(),
                occurred_at_ms: 102,
            },
            102,
        )
        .await
        .unwrap();
    let subscription = store
        .create(
            NewSubscription {
                thread_id: owner,
                filter: Some(EventFilter {
                    source: "build".into(),
                    event_types: BTreeSet::from(["completed".into()]),
                    labels: BTreeMap::new(),
                }),
                source_cursor: Some(SourceCursor {
                    sequence: 1,
                    value: None,
                }),
                heartbeat: None,
            },
            103,
        )
        .await
        .unwrap();
    store
        .replay_subscription_events(subscription.id, 103)
        .await
        .unwrap();
    let result = store
        .await_subscription(owner, subscription.id)
        .await
        .unwrap();
    assert_eq!(result.event.unwrap().cursor.sequence, 2);
    store.cancel(subscription.id).await.unwrap();
}

#[tokio::test]
async fn deleting_owner_preserves_deadlines_and_pauses_or_transfers() {
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    store
        .project_command(
            "owned",
            owner,
            None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Analysis,
                workstream: None,
            },
            100,
        )
        .await
        .unwrap();
    store.delete_thread(owner).await.unwrap();
    let project = store.project_status("owned").await.unwrap().unwrap();
    assert!(project.paused);
    assert_eq!(project.started_at_ms, 100);
    assert!(store.next_heartbeat_deadline().await.unwrap().is_none());
}

#[tokio::test]
async fn review_worker_claim_is_idempotent_and_does_not_review_itself() {
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    let project = store
        .project_command(
            "reviewed",
            owner,
            None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Analysis,
                workstream: None,
            },
            100,
        )
        .await
        .unwrap();
    store
        .project_activity("reviewed", owner, 101)
        .await
        .unwrap();
    let project = store
        .project_command(
            "reviewed",
            owner,
            Some(project.revision),
            ProjectAutomationCommand::RequestReview {
                evidence_ref: "usage-operation:measured".into(),
            },
            200,
        )
        .await
        .unwrap();
    let job = project.review.as_ref().unwrap();
    let worker = ThreadId::new();
    assert_eq!(
        store
            .claim_project_review_worker("reviewed", job.id, worker, 200)
            .await
            .unwrap(),
        Some(worker)
    );
    assert_eq!(
        store
            .claim_project_review_worker("reviewed", job.id, ThreadId::new(), 201)
            .await
            .unwrap(),
        Some(worker)
    );
    assert_eq!(
        store
            .claim_project_review_worker("reviewed", Uuid::now_v7(), ThreadId::new(), 201)
            .await
            .unwrap(),
        None
    );
    store
        .project_command(
            "reviewed",
            worker,
            None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Analysis,
                workstream: None,
            },
            202,
        )
        .await
        .unwrap();
    store
        .project_activity("reviewed", worker, 203)
        .await
        .unwrap();
    let current = store.project_status("reviewed").await.unwrap().unwrap();
    assert_eq!(current.last_activity_at_ms, 101);
}

#[tokio::test]
async fn source_attention_preserves_cursor_and_cannot_replace_real_completion() {
    use codex_event_subscriptions::EventFilter;
    use codex_event_subscriptions::NewSubscription;
    use std::collections::BTreeSet;
    let (store, _directory) = store().await;
    let owner = ThreadId::new();
    let subscription = store
        .create(
            NewSubscription {
                thread_id: owner,
                filter: Some(EventFilter {
                    source: "devcoordinator".into(),
                    event_types: BTreeSet::from([
                        "test.finished".into(),
                        "source.unavailable".into(),
                    ]),
                    labels: BTreeMap::new(),
                }),
                source_cursor: Some(SourceCursor {
                    sequence: 10,
                    value: None,
                }),
                heartbeat: None,
            },
            100,
        )
        .await
        .unwrap();
    store
        .publish(
            PublishedEvent {
                id: "outage".into(),
                source: "devcoordinator".into(),
                event_type: "source.unavailable".into(),
                cursor: SourceCursor {
                    sequence: 999,
                    value: None,
                },
                labels: BTreeMap::new(),
                occurred_at_ms: 101,
            },
            101,
        )
        .await
        .unwrap();
    let attention = store.pending_wake(owner).await.unwrap().unwrap();
    assert_eq!(
        attention.wake.items[0]
            .event
            .as_ref()
            .unwrap()
            .cursor
            .sequence,
        10
    );
    store
        .publish(
            PublishedEvent {
                id: "finished".into(),
                source: "devcoordinator".into(),
                event_type: "test.finished".into(),
                cursor: SourceCursor {
                    sequence: 11,
                    value: None,
                },
                labels: BTreeMap::new(),
                occurred_at_ms: 102,
            },
            102,
        )
        .await
        .unwrap();
    store
        .publish(
            PublishedEvent {
                id: "later-outage".into(),
                source: "devcoordinator".into(),
                event_type: "source.unavailable".into(),
                cursor: SourceCursor {
                    sequence: 999,
                    value: None,
                },
                labels: BTreeMap::new(),
                occurred_at_ms: 103,
            },
            103,
        )
        .await
        .unwrap();
    let completed = store
        .await_subscription(owner, subscription.id)
        .await
        .unwrap();
    assert_eq!(completed.event.unwrap().event_type, "test.finished");
    let journal_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_event_observations")
        .fetch_one(store.pool.as_ref())
        .await
        .unwrap();
    assert_eq!(journal_count, 1);
}
