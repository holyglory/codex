use codex_app_server_protocol as api;
use codex_event_subscriptions as native;

use crate::error_code::invalid_params;

pub(super) fn native_command(
    command: api::ProjectAutomationCommand,
) -> Result<native::ProjectAutomationCommand, api::JSONRPCErrorError> {
    Ok(match command {
        api::ProjectAutomationCommand::LinkWork {
            outcome_id,
            experiment_ref,
            clear_outcome,
            clear_experiment,
        } => {
            if outcome_id.is_some() && clear_outcome || experiment_ref.is_some() && clear_experiment
            {
                return Err(invalid_params(
                    "an ID and its matching clear flag cannot be combined",
                ));
            }
            if outcome_id.is_none()
                && experiment_ref.is_none()
                && !clear_outcome
                && !clear_experiment
            {
                return Err(invalid_params("linkWork requires an ID or a clear flag"));
            }
            native::ProjectAutomationCommand::LinkWork {
                outcome_id,
                experiment_ref,
                clear_outcome,
                clear_experiment,
            }
        }
        api::ProjectAutomationCommand::Status => native::ProjectAutomationCommand::Status,
        api::ProjectAutomationCommand::Bind {
            purpose,
            workstream,
        } => native::ProjectAutomationCommand::Bind {
            purpose: match purpose {
                api::ProjectWorkPurpose::Discussion => native::WorkPurpose::Discussion,
                api::ProjectWorkPurpose::Specification => native::WorkPurpose::Specification,
                api::ProjectWorkPurpose::Analysis => native::WorkPurpose::Analysis,
                api::ProjectWorkPurpose::Implementation => native::WorkPurpose::Implementation,
                api::ProjectWorkPurpose::Recovery => native::WorkPurpose::Recovery,
            },
            workstream,
        },
        api::ProjectAutomationCommand::ActivateDelivery {
            target,
            surface,
            acceptance,
            delivery_interval_ms,
            hard_stop_interval_ms,
        } => native::ProjectAutomationCommand::ActivateDelivery {
            target,
            surface,
            acceptance,
            delivery_interval_ms,
            hard_stop_interval_ms,
        },
        api::ProjectAutomationCommand::Postpone {
            target,
            delivery_due_at_ms,
            hard_stop_at_ms,
            authorization_ref,
        } => native::ProjectAutomationCommand::Postpone {
            target,
            delivery_due_at_ms,
            hard_stop_at_ms,
            authorization_ref,
        },
        api::ProjectAutomationCommand::Pause {
            target,
            authorization_ref,
        } => native::ProjectAutomationCommand::Pause {
            target,
            authorization_ref,
        },
        api::ProjectAutomationCommand::Resume { target } => {
            native::ProjectAutomationCommand::Resume { target }
        }
        api::ProjectAutomationCommand::RecordDelivery {
            target,
            delivered_at_ms,
            evidence_ref,
        } => native::ProjectAutomationCommand::RecordDelivery {
            target,
            delivered_at_ms,
            evidence_ref,
        },
        api::ProjectAutomationCommand::CompleteReview {
            job_id,
            decision_ref,
        } => native::ProjectAutomationCommand::CompleteReview {
            job_id: uuid::Uuid::parse_str(&job_id)
                .map_err(|_| invalid_params("invalid review jobId"))?,
            decision_ref,
        },
        api::ProjectAutomationCommand::RequestReview { evidence_ref } => {
            native::ProjectAutomationCommand::RequestReview { evidence_ref }
        }
        api::ProjectAutomationCommand::Transfer {
            owner_thread_id,
            authorization_ref,
        } => native::ProjectAutomationCommand::Transfer {
            owner_thread_id: super::parse_thread_id(&owner_thread_id)?,
            authorization_ref,
        },
        api::ProjectAutomationCommand::Complete { outcome_ref } => {
            native::ProjectAutomationCommand::Complete { outcome_ref }
        }
    })
}

pub(super) fn api_project(
    project: native::ProjectAutomation,
    now_ms: i64,
) -> api::ProjectAutomation {
    let mode = match project.mode(now_ms) {
        native::ProjectMode::PerformanceOnly => api::ProjectAutomationMode::PerformanceOnly,
        native::ProjectMode::Normal => api::ProjectAutomationMode::Normal,
        native::ProjectMode::DeliveryDue => api::ProjectAutomationMode::DeliveryDue,
        native::ProjectMode::RecoveryOnly => api::ProjectAutomationMode::RecoveryOnly,
        native::ProjectMode::Paused => api::ProjectAutomationMode::Paused,
    };
    api::ProjectAutomation {
        project_id: project.project_id,
        owner_thread_id: project.owner_thread_id.to_string(),
        revision: project.revision,
        mode,
        started_at_ms: project.started_at_ms,
        last_activity_at_ms: project.last_activity_at_ms,
        review_window_start_ms: project.review_window_start_ms,
        next_review_at_ms: project.next_review_at_ms,
        review_interval_ms: project.review_interval_ms,
        paused: project.paused,
        threads: project
            .threads
            .into_iter()
            .map(|(thread_id, purpose)| {
                (
                    thread_id,
                    match purpose {
                        native::WorkPurpose::Discussion => api::ProjectWorkPurpose::Discussion,
                        native::WorkPurpose::Specification => {
                            api::ProjectWorkPurpose::Specification
                        }
                        native::WorkPurpose::Analysis => api::ProjectWorkPurpose::Analysis,
                        native::WorkPurpose::Implementation => {
                            api::ProjectWorkPurpose::Implementation
                        }
                        native::WorkPurpose::Recovery => api::ProjectWorkPurpose::Recovery,
                    },
                )
            })
            .collect(),
        thread_workstreams: project.thread_workstreams,
        thread_outcomes: project.thread_outcomes,
        thread_experiments: project.thread_experiments,
        completed: project.completed,
        delivery: project
            .delivery
            .into_iter()
            .map(|(target, obligation)| {
                (
                    target,
                    api::ProjectDeliveryObligation {
                        target: obligation.target,
                        workstream: obligation.workstream,
                        surface: obligation.surface,
                        acceptance: obligation.acceptance,
                        started_at_ms: obligation.started_at_ms,
                        delivered_at_ms: obligation.delivered_at_ms,
                        delivery_interval_ms: obligation.delivery_interval_ms,
                        hard_stop_interval_ms: obligation.hard_stop_interval_ms,
                        delivery_due_at_ms: obligation.delivery_due_at_ms,
                        hard_stop_at_ms: obligation.hard_stop_at_ms,
                        revision: obligation.revision,
                        paused: obligation.paused,
                        job: obligation.job.map(api_job),
                        evidence_ref: obligation.evidence_ref,
                    },
                )
            })
            .collect(),
        review: project.review.map(api_job),
        last_review_ref: project.last_review_ref,
    }
}

fn api_job(job: native::AutomationJob) -> api::ProjectAutomationJob {
    api::ProjectAutomationJob {
        id: job.id.to_string(),
        kind: match job.kind {
            native::AutomationJobKind::PerformanceReview => {
                api::ProjectAutomationJobKind::PerformanceReview
            }
            native::AutomationJobKind::Delivery => api::ProjectAutomationJobKind::Delivery,
            native::AutomationJobKind::DeliveryRecovery => {
                api::ProjectAutomationJobKind::DeliveryRecovery
            }
        },
        due_at_ms: job.due_at_ms,
        revision: job.revision,
        notified: job.notified,
        decision_ref: job.decision_ref,
    }
}
