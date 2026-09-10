use super::*;
use pretty_assertions::assert_eq;

fn implementation(now_ms: i64) -> ProjectAutomation {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("project-alpha".into(), owner, now_ms);
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: None,
            },
            now_ms,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::ActivateDelivery {
                target: "linux-cli".into(),
                surface: "local executable".into(),
                acceptance: "CLI starts and reads a saved task".into(),
                delivery_interval_ms: None,
                hard_stop_interval_ms: None,
            },
            now_ms,
        )
        .unwrap();
    project
}

#[test]
fn specification_weeks_never_create_delivery_alarms() {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("project-alpha".into(), owner, 1_000_000);
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Specification,
                workstream: None,
            },
            1_000_001,
        )
        .unwrap();
    let jobs = project.collect_due(1_000_000 + 7 * DAY_MS);
    assert_eq!(
        jobs.iter().map(|job| job.kind).collect::<Vec<_>>(),
        vec![AutomationJobKind::PerformanceReview]
    );
    assert_eq!(
        project.mode(1_000_000 + 40 * DAY_MS),
        ProjectMode::PerformanceOnly
    );
    assert!(project.delivery.is_empty());
    let previous_start = project.started_at_ms;
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: None,
            },
            7 * DAY_MS,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::ActivateDelivery {
                target: "linux-cli".into(),
                surface: "local executable".into(),
                acceptance: "smoke passed".into(),
                delivery_interval_ms: None,
                hard_stop_interval_ms: None,
            },
            7 * DAY_MS,
        )
        .unwrap();
    assert_eq!(
        (
            project.started_at_ms,
            project.delivery["linux-cli"].started_at_ms,
            project.mode(7 * DAY_MS)
        ),
        (previous_start, 7 * DAY_MS, ProjectMode::Normal)
    );
}

#[test]
fn overdue_delivery_is_nonblocking_until_hard_stop() {
    let mut project = implementation(1_000_000);
    assert_eq!(project.mode(1_000_000 + DAY_MS - 1), ProjectMode::Normal);
    let jobs = project.collect_due(1_000_000 + DAY_MS);
    assert_eq!(
        jobs.iter().map(|job| job.kind).collect::<Vec<_>>(),
        vec![AutomationJobKind::Delivery]
    );
    assert_eq!(project.mode(1_000_000 + DAY_MS), ProjectMode::DeliveryDue);
    assert_eq!(
        project.mode(1_000_000 + DAY_MS + DAY_MS / 2 - 1),
        ProjectMode::DeliveryDue
    );
    assert!(project.collect_due(1_000_000 + DAY_MS).is_empty());
    let jobs = project.collect_due(1_000_000 + DAY_MS + DAY_MS / 2);
    assert_eq!(
        jobs.iter().map(|job| job.kind).collect::<Vec<_>>(),
        vec![AutomationJobKind::DeliveryRecovery]
    );
    assert_eq!(
        project.mode(1_000_000 + DAY_MS + DAY_MS / 2),
        ProjectMode::RecoveryOnly
    );
}

#[test]
fn explicit_postponement_releases_block_without_fake_delivery() {
    let mut project = implementation(1_000_000);
    project.collect_due(1_000_000 + 2 * DAY_MS);
    let original = project.delivery["linux-cli"].clone();
    project
        .apply(
            project.owner_thread_id,
            ProjectAutomationCommand::Postpone {
                target: "linux-cli".into(),
                delivery_due_at_ms: 1_000_000 + 3 * DAY_MS,
                hard_stop_at_ms: 1_000_000 + 4 * DAY_MS,
                authorization_ref: "owner explicitly postponed".into(),
            },
            1_000_000 + 2 * DAY_MS,
        )
        .unwrap();
    assert_eq!(project.mode(1_000_000 + 2 * DAY_MS), ProjectMode::Normal);
    assert_eq!(
        (
            project.delivery["linux-cli"].started_at_ms,
            project.delivery["linux-cli"].delivered_at_ms
        ),
        (original.started_at_ms, None)
    );
    assert!(
        project
            .collect_due(1_000_000 + 2 * DAY_MS)
            .iter()
            .all(|job| job.kind == AutomationJobKind::PerformanceReview)
    );
}

#[test]
fn empty_windows_do_not_request_model_work_and_review_needs_completion() {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("quiet".into(), owner, 1_000_000);
    assert!(project.collect_due(1_000_000 + 5 * DAY_MS).is_empty());
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Analysis,
                workstream: None,
            },
            1_000_001 + 5 * DAY_MS,
        )
        .unwrap();
    let jobs = project.collect_due(1_000_000 + 6 * DAY_MS);
    assert_eq!(jobs.len(), 1);
    let restored: ProjectAutomation =
        serde_json::from_str(&serde_json::to_string(&project).unwrap()).unwrap();
    assert_eq!(restored.review, project.review);
    assert!(
        project
            .apply(
                owner,
                ProjectAutomationCommand::CompleteReview {
                    job_id: Uuid::now_v7(),
                    decision_ref: "review-valid".into()
                },
                1_000_000 + 6 * DAY_MS
            )
            .is_err()
    );
    project
        .apply(
            owner,
            ProjectAutomationCommand::CompleteReview {
                job_id: jobs[0].id,
                decision_ref: "review-valid".into(),
            },
            1_000_000 + 6 * DAY_MS,
        )
        .unwrap();
    assert!(project.review.is_none());
}

#[test]
fn pause_retains_deadlines_and_resume_does_not_reset_them() {
    let mut project = implementation(1_000_000);
    project
        .apply(
            project.owner_thread_id,
            ProjectAutomationCommand::Pause {
                target: None,
                authorization_ref: "owner pause".into(),
            },
            1_000_001,
        )
        .unwrap();
    assert_eq!(project.next_deadline(), None);
    assert!(project.collect_due(1_000_000 + 2 * DAY_MS).is_empty());
    project
        .apply(
            project.owner_thread_id,
            ProjectAutomationCommand::Resume { target: None },
            1_000_000 + 2 * DAY_MS,
        )
        .unwrap();
    assert_eq!(
        project.mode(1_000_000 + 2 * DAY_MS),
        ProjectMode::RecoveryOnly
    );
}

#[test]
fn target_delivery_cannot_reset_another_platform_or_accept_future_time() {
    let mut project = implementation(1_000_000);
    project
        .apply(
            project.owner_thread_id,
            ProjectAutomationCommand::ActivateDelivery {
                target: "windows".into(),
                surface: "package".into(),
                acceptance: "launches".into(),
                delivery_interval_ms: None,
                hard_stop_interval_ms: None,
            },
            1_000_000,
        )
        .unwrap();
    assert!(
        project
            .apply(
                project.owner_thread_id,
                ProjectAutomationCommand::RecordDelivery {
                    target: "linux-cli".into(),
                    delivered_at_ms: 9 * DAY_MS,
                    evidence_ref: "receipt".into()
                },
                2 * DAY_MS
            )
            .is_err()
    );
    project
        .apply(
            project.owner_thread_id,
            ProjectAutomationCommand::RecordDelivery {
                target: "linux-cli".into(),
                delivered_at_ms: 2 * DAY_MS,
                evidence_ref: "qualified-receipt".into(),
            },
            2 * DAY_MS,
        )
        .unwrap();
    assert_eq!(project.mode(2 * DAY_MS), ProjectMode::RecoveryOnly);
    assert_eq!(project.delivery["windows"].delivered_at_ms, None);
}

#[test]
fn hard_stop_does_not_block_unrelated_workstreams_or_specification() {
    let mut project = implementation(1_000_000);
    let other = ThreadId::new();
    project
        .apply(
            other,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: Some("unrelated".into()),
            },
            1_000_001,
        )
        .unwrap();
    let spec = ThreadId::new();
    project
        .apply(
            spec,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Specification,
                workstream: None,
            },
            1_000_002,
        )
        .unwrap();
    let now_ms = 1_000_000 + 2 * DAY_MS;
    assert_eq!(
        (
            project.mode_for_thread(project.owner_thread_id, now_ms),
            project.mode_for_thread(other, now_ms),
            project.mode_for_thread(spec, now_ms)
        ),
        (
            ProjectMode::RecoveryOnly,
            ProjectMode::PerformanceOnly,
            ProjectMode::PerformanceOnly
        )
    );
}

#[test]
fn review_completion_preserves_work_after_the_reviewed_window() {
    let mut project = implementation(1_000_000);
    project.last_activity_at_ms = 1_000_001;
    project.collect_due(1_000_000 + DAY_MS);
    let job_id = project.review.as_ref().unwrap().id;
    project.last_activity_at_ms = 1_000_000 + DAY_MS + 100;
    project
        .apply(
            project.owner_thread_id,
            ProjectAutomationCommand::CompleteReview {
                job_id,
                decision_ref: "review@1".into(),
            },
            1_000_000 + DAY_MS + 200,
        )
        .unwrap();
    assert_eq!(project.review_window_start_ms, 1_000_000 + DAY_MS);
    assert_eq!(project.last_activity_at_ms, 1_000_000 + DAY_MS + 100);
    assert!(
        project
            .collect_due(1_000_000 + 2 * DAY_MS)
            .iter()
            .any(|job| job.kind == AutomationJobKind::PerformanceReview)
    );
}

#[test]
fn review_signal_is_deduplicated_and_bookkeeping_does_not_create_work() {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("project".into(), owner, 100);
    project
        .apply(
            owner,
            ProjectAutomationCommand::RequestReview {
                evidence_ref: "usage-operation:real-id".into(),
            },
            200,
        )
        .unwrap();
    let job_id = project.review.as_ref().unwrap().id;
    project
        .apply(
            owner,
            ProjectAutomationCommand::CompleteReview {
                job_id,
                decision_ref: "review@1".into(),
            },
            300,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::RequestReview {
                evidence_ref: "usage-operation:real-id".into(),
            },
            400,
        )
        .unwrap();
    assert!(project.review.is_none());
    assert_eq!(project.last_activity_at_ms, 100);
}

#[test]
fn a_new_work_cycle_does_not_revive_completed_delivery_obligations() {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("project".into(), owner, 100);
    project
        .apply(
            owner,
            ProjectAutomationCommand::Complete {
                outcome_ref: "outcome-done".into(),
            },
            200,
        )
        .unwrap();
    assert!(project.completed);
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Specification,
                workstream: None,
            },
            5 * DAY_MS,
        )
        .unwrap();
    assert!(!project.completed);
    assert_eq!(project.mode(5 * DAY_MS), ProjectMode::PerformanceOnly);
    assert_eq!(project.started_at_ms, 100);
    assert_eq!(project.review_window_start_ms, 5 * DAY_MS);
    assert!(project.delivery.is_empty());
}

#[test]
fn implementation_discovery_counts_toward_delivery_without_charging_specification() {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("project".into(), owner, 100);
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Specification,
                workstream: None,
            },
            100,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: None,
            },
            7 * DAY_MS,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::ActivateDelivery {
                target: "cli".into(),
                surface: "binary".into(),
                acceptance: "runs".into(),
                delivery_interval_ms: None,
                hard_stop_interval_ms: None,
            },
            7 * DAY_MS + 1000,
        )
        .unwrap();
    assert_eq!(project.delivery["cli"].started_at_ms, 7 * DAY_MS);
    assert_eq!(project.delivery["cli"].delivery_due_at_ms, 8 * DAY_MS);
}

#[test]
fn partial_work_links_preserve_other_associations_and_clear_only_explicit_targets() {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("project".into(), owner, 100);
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Analysis,
                workstream: None,
            },
            100,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::LinkWork {
                outcome_id: Some("outcome".into()),
                experiment_ref: Some("experiment@1".into()),
                clear_outcome: false,
                clear_experiment: false,
            },
            101,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::LinkWork {
                outcome_id: None,
                experiment_ref: Some("experiment@2".into()),
                clear_outcome: false,
                clear_experiment: false,
            },
            102,
        )
        .unwrap();
    assert_eq!(
        (
            &project.thread_outcomes[&owner.to_string()],
            &project.thread_experiments[&owner.to_string()]
        ),
        (&"outcome".to_owned(), &"experiment@2".to_owned())
    );
    project
        .apply(
            owner,
            ProjectAutomationCommand::LinkWork {
                outcome_id: None,
                experiment_ref: None,
                clear_outcome: false,
                clear_experiment: true,
            },
            103,
        )
        .unwrap();
    assert!(project.thread_experiments.is_empty());
    assert_eq!(project.thread_outcomes[&owner.to_string()], "outcome");
    let saved = project.clone();
    assert!(
        project
            .apply(
                owner,
                ProjectAutomationCommand::LinkWork {
                    outcome_id: Some("different".into()),
                    experiment_ref: None,
                    clear_outcome: true,
                    clear_experiment: false
                },
                104
            )
            .is_err()
    );
    assert_eq!(project, saved);
}

#[test]
fn completed_work_cannot_resume_old_alarms_and_new_work_gets_a_new_owner() {
    let owner = ThreadId::new();
    let mut project = ProjectAutomation::new("project".into(), owner, 100);
    project
        .apply(
            owner,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Analysis,
                workstream: None,
            },
            100,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::LinkWork {
                outcome_id: Some("done-outcome".into()),
                experiment_ref: None,
                clear_outcome: false,
                clear_experiment: false,
            },
            101,
        )
        .unwrap();
    project
        .apply(
            owner,
            ProjectAutomationCommand::Complete {
                outcome_ref: "done-outcome".into(),
            },
            102,
        )
        .unwrap();
    let completed = project.clone();
    assert!(
        project
            .apply(
                owner,
                ProjectAutomationCommand::Resume { target: None },
                103
            )
            .is_err()
    );
    assert_eq!(project, completed);
    assert!(project.collect_due(40 * DAY_MS).is_empty());
    assert_eq!(project.next_deadline(), None);
    let successor = ThreadId::new();
    project
        .apply(
            successor,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Specification,
                workstream: None,
            },
            40 * DAY_MS,
        )
        .unwrap();
    assert_eq!(project.owner_thread_id, successor);
    assert_eq!(
        project.threads,
        BTreeMap::from([(successor.to_string(), WorkPurpose::Specification)])
    );
    assert!(project.thread_outcomes.is_empty());
    assert_eq!(project.started_at_ms, 100);
    assert_eq!(project.next_review_at_ms, 41 * DAY_MS);
}
