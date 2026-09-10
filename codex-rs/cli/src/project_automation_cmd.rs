use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_protocol as api;

use crate::InteractiveRemoteOptions;
use crate::resolve_remote_endpoint;

const MAX_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Debug, Parser)]
pub(crate) struct ProjectAutomationCommand {
    #[command(subcommand)]
    action: ProjectAction,
    /// Project scope; inferred from --thread when omitted.
    #[arg(long, global = true)]
    project: Option<String>,
    /// Persistent task UUID performing the change.
    #[arg(long, global = true)]
    thread: Option<String>,
    /// Revision returned by the most recent status request.
    #[arg(long, global = true)]
    expected_revision: Option<u64>,
    /// Print structured output, compacted if it exceeds 16 KiB.
    #[arg(long, global = true)]
    json: bool,
    #[clap(flatten)]
    remote: InteractiveRemoteOptions,
}

#[derive(Debug, Subcommand)]
enum ProjectAction {
    /// Link this task to its existing outcome and optimization experiment.
    #[command(group(clap::ArgGroup::new("work_links").required(true).multiple(true)
        .args(["outcome_id", "experiment_ref", "clear_outcome", "clear_experiment"])))]
    LinkWork {
        #[arg(long, conflicts_with = "clear_outcome")]
        outcome_id: Option<String>,
        #[arg(long, conflicts_with = "clear_experiment")]
        experiment_ref: Option<String>,
        /// Remove the saved outcome link without changing the experiment.
        #[arg(long)]
        clear_outcome: bool,
        /// Remove the saved experiment link without changing the outcome.
        #[arg(long)]
        clear_experiment: bool,
    },
    /// Read project status, or detect capability when --project is omitted.
    Status,
    /// Bind a persistent task to its work purpose.
    Bind {
        #[arg(value_enum)]
        purpose: Purpose,
        #[arg(long)]
        workstream: Option<String>,
    },
    /// Activate delivery for an authorized inspectable result.
    Delivery {
        #[arg(long)]
        target: String,
        #[arg(long)]
        surface: String,
        #[arg(long)]
        acceptance: String,
        #[arg(long)]
        delivery_interval_ms: Option<i64>,
        #[arg(long)]
        hard_stop_interval_ms: Option<i64>,
    },
    /// Change deadlines using an explicit authorization reference.
    Postpone {
        #[arg(long)]
        target: String,
        #[arg(long)]
        delivery_due_at_ms: i64,
        #[arg(long)]
        hard_stop_at_ms: i64,
        #[arg(long)]
        authorization_ref: String,
    },
    /// Pause one delivery target or the whole project.
    Pause {
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        authorization_ref: String,
    },
    /// Resume without resetting the original deadlines.
    Resume {
        #[arg(long)]
        target: Option<String>,
    },
    /// Record actual delivery backed by a Coordinator receipt.
    RecordDelivery {
        #[arg(long)]
        target: String,
        #[arg(long)]
        delivered_at_ms: i64,
        #[arg(long)]
        evidence_ref: String,
    },
    /// Request an evidence-backed performance review.
    Review {
        #[arg(long)]
        evidence_ref: String,
    },
    /// Finish a review backed by a Coordinator receipt.
    CompleteReview {
        #[arg(long)]
        job_id: String,
        #[arg(long)]
        decision_ref: String,
    },
    /// Transfer automation to another persistent task.
    Transfer {
        #[arg(long)]
        owner_thread_id: String,
        #[arg(long)]
        authorization_ref: String,
    },
    /// Complete the current task's project work.
    Complete {
        #[arg(long)]
        outcome_ref: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Purpose {
    Discussion,
    Specification,
    Analysis,
    Implementation,
    Recovery,
}

pub(crate) async fn run(
    command: ProjectAutomationCommand,
    root_remote: Option<String>,
    root_remote_auth_token_env: Option<String>,
) -> anyhow::Result<String> {
    let ProjectAutomationCommand {
        action,
        project,
        thread,
        expected_revision,
        json,
        remote,
    } = command;
    let command = match action {
        ProjectAction::Status => None,
        ProjectAction::LinkWork {
            outcome_id,
            experiment_ref,
            clear_outcome,
            clear_experiment,
        } => Some(api::ProjectAutomationCommand::LinkWork {
            outcome_id,
            experiment_ref,
            clear_outcome,
            clear_experiment,
        }),
        ProjectAction::Bind {
            purpose,
            workstream,
        } => Some(api::ProjectAutomationCommand::Bind {
            purpose: match purpose {
                Purpose::Discussion => api::ProjectWorkPurpose::Discussion,
                Purpose::Specification => api::ProjectWorkPurpose::Specification,
                Purpose::Analysis => api::ProjectWorkPurpose::Analysis,
                Purpose::Implementation => api::ProjectWorkPurpose::Implementation,
                Purpose::Recovery => api::ProjectWorkPurpose::Recovery,
            },
            workstream,
        }),
        ProjectAction::Delivery {
            target,
            surface,
            acceptance,
            delivery_interval_ms,
            hard_stop_interval_ms,
        } => Some(api::ProjectAutomationCommand::ActivateDelivery {
            target,
            surface,
            acceptance,
            delivery_interval_ms,
            hard_stop_interval_ms,
        }),
        ProjectAction::Postpone {
            target,
            delivery_due_at_ms,
            hard_stop_at_ms,
            authorization_ref,
        } => Some(api::ProjectAutomationCommand::Postpone {
            target,
            delivery_due_at_ms,
            hard_stop_at_ms,
            authorization_ref,
        }),
        ProjectAction::Pause {
            target,
            authorization_ref,
        } => Some(api::ProjectAutomationCommand::Pause {
            target,
            authorization_ref,
        }),
        ProjectAction::Resume { target } => Some(api::ProjectAutomationCommand::Resume { target }),
        ProjectAction::RecordDelivery {
            target,
            delivered_at_ms,
            evidence_ref,
        } => Some(api::ProjectAutomationCommand::RecordDelivery {
            target,
            delivered_at_ms,
            evidence_ref,
        }),
        ProjectAction::Review { evidence_ref } => {
            Some(api::ProjectAutomationCommand::RequestReview { evidence_ref })
        }
        ProjectAction::CompleteReview {
            job_id,
            decision_ref,
        } => Some(api::ProjectAutomationCommand::CompleteReview {
            job_id,
            decision_ref,
        }),
        ProjectAction::Transfer {
            owner_thread_id,
            authorization_ref,
        } => Some(api::ProjectAutomationCommand::Transfer {
            owner_thread_id,
            authorization_ref,
        }),
        ProjectAction::Complete { outcome_ref } => {
            Some(api::ProjectAutomationCommand::Complete { outcome_ref })
        }
    };
    if let Some(command) = &command {
        anyhow::ensure!(
            thread.is_some(),
            "project changes require --thread; --project is inferred from that task when omitted"
        );
        anyhow::ensure!(
            matches!(command, api::ProjectAutomationCommand::Bind { .. })
                || expected_revision.is_some(),
            "project changes require --expected-revision from current status"
        );
    } else {
        anyhow::ensure!(
            expected_revision.is_none(),
            "status does not accept --expected-revision"
        );
    }
    let endpoint = resolve_remote_endpoint(
        remote
            .remote
            .or(root_remote)
            .or_else(|| Some("unix://".to_string())),
        remote.remote_auth_token_env.or(root_remote_auth_token_env),
    )?
    .context("missing app-server endpoint")?;
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint,
        client_name: "codex-project".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 32,
    })
    .await
    .context(
        "connect to a persistent app server; run `codex app-server daemon start` or use --remote",
    )?;
    let response = client
        .request_typed::<api::ProjectAutomationCommandResponse>(
            api::ClientRequest::ProjectAutomationCommand {
                request_id: api::RequestId::Integer(1),
                params: api::ProjectAutomationCommandParams {
                    project_id: project,
                    thread_id: thread.clone(),
                    expected_revision,
                    command,
                },
            },
        )
        .await;
    let shutdown = client.shutdown().await;
    let response = response.context("project automation command failed")?;
    shutdown.context("command returned successfully, but closing the app-server connection failed; read status before retrying")?;
    render(&response, json, thread.as_deref())
}

fn render(
    response: &api::ProjectAutomationCommandResponse,
    json: bool,
    thread_id: Option<&str>,
) -> anyhow::Result<String> {
    if json {
        let output = serde_json::to_string(response)?;
        if output.len() < MAX_OUTPUT_BYTES {
            return Ok(output);
        }
        let project = response
            .project
            .as_ref()
            .context("missing project in oversized response")?;
        return Ok(serde_json::json!({
            "capability": response.capability,
            "projectId": project.project_id,
            "revision": project.revision,
            "mode": project.mode,
            "completed": project.completed,
            "ownerThreadId": project.owner_thread_id,
            "nextReviewAtMs": project.next_review_at_ms,
            "review": project.review,
            "deliveryTargetCount": project.delivery.len(),
            "threadCount": project.threads.len(),
            "threadId": thread_id,
            "outcomeId": thread_id.and_then(|thread| project.thread_outcomes.get(thread)),
            "experimentRef": thread_id.and_then(|thread| project.thread_experiments.get(thread)),
            "detailsOmitted": true,
        })
        .to_string());
    }
    let Some(capability) = &response.capability else {
        return Ok("Project automation is unavailable on this server.".to_string());
    };
    let Some(project) = &response.project else {
        return Ok(format!(
            "Project automation v{} available; no enrolled project returned.",
            capability.version
        ));
    };
    let status = if project.completed {
        "completed".to_string()
    } else {
        serde_json::to_string(&project.mode)?
    };
    let mut output = format!(
        "Project {} | revision {} | {status}",
        serde_json::to_string(&project.project_id)?,
        project.revision
    );
    if let Some(thread_id) = thread_id {
        let outcome = project
            .thread_outcomes
            .get(thread_id)
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_else(|| "none".to_string());
        let experiment = project
            .thread_experiments
            .get(thread_id)
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_else(|| "none".to_string());
        output.push_str(&format!(
            "\nTask {} | outcome: {outcome} | experiment: {experiment}",
            serde_json::to_string(thread_id)?
        ));
    }
    if project.completed {
        return Ok(output);
    }
    output.push_str(&format!(
        "\nOwner: {} | next review: {} ms UTC",
        project.owner_thread_id, project.next_review_at_ms
    ));
    for (index, obligation) in project.delivery.values().enumerate() {
        let line = format!(
            "\n{} | delivery: {} | hard stop: {} ms UTC | {}",
            serde_json::to_string(&obligation.target)?,
            obligation.delivery_due_at_ms,
            obligation.hard_stop_at_ms,
            if obligation.paused {
                "paused"
            } else {
                "active"
            }
        );
        if output.len() + line.len() + 100 >= MAX_OUTPUT_BYTES {
            output.push_str(&format!(
                "\n{} more delivery targets omitted.",
                project.delivery.len() - index
            ));
            break;
        }
        output.push_str(&line);
    }
    Ok(output)
}

#[cfg(test)]
#[path = "project_automation_cmd_tests.rs"]
mod tests;
