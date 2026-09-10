use std::collections::BTreeMap;

use codex_event_subscriptions::ProjectAutomationCommand;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::function_tool::FunctionCallError;
use crate::project_automation::project_automation_id;
use crate::project_automation::project_automation_now_ms;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub struct ProjectAutomationHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    command: ProjectAutomationCommand,
    expected_revision: Option<u64>,
}

impl ToolExecutor<ToolInvocation> for ProjectAutomationHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("project_automation")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "project_automation".into(),
            description: "Manage durable project clocks through Codex's scheduler. Bind purpose discussion/specification/analysis/implementation/recovery before substantial work. Specifications and analysis get reviews only, never delivery alarms. Delivery requires a meaningful authorized preliminary result. At delivery_due start delivery and continue development; only hard_stop blocks ordinary implementation. Use status before mutations and expected_revision. Commands use action: status; bind{purpose,workstream?}; link_work{outcome_id?,experiment_ref?,clear_outcome?,clear_experiment?}; activate_delivery{target,surface,acceptance,delivery_interval_ms?,hard_stop_interval_ms?}; postpone{target,delivery_due_at_ms,hard_stop_at_ms,authorization_ref}; pause{target?,authorization_ref}; resume{target?}; record_delivery{target,delivered_at_ms,evidence_ref}; request_review{evidence_ref}; complete_review{job_id,decision_ref}; transfer{owner_thread_id,authorization_ref}; complete{outcome_ref}. Omitted work links are preserved; clear flags remove only the named link. Postponements require explicit user direction; evidence refs must identify qualified Coordinator records. Review completion requires a recorded reasoned intervention or justified no-change decision, not token totals.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("command".into(), JsonSchema::object(BTreeMap::new(), None, Some(true.into()))),
                ("expected_revision".into(), JsonSchema::number(Some("Revision returned by status; required for mutations other than first bind.".into()))),
            ]), Some(vec!["command".into()]), Some(false.into())), output_schema: None,
        })
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(error("expected function arguments"));
            };
            if arguments.len() > 8192 {
                return Err(error("project command exceeds the safe bound"));
            }
            let args: Arguments = serde_json::from_str(arguments)
                .map_err(|_| error("invalid typed project command"))?;
            let state = invocation.session.state_db().ok_or_else(|| {
                error("durable project scheduling requires the persistent state store")
            })?;
            let environment = invocation
                .step_context
                .environments
                .primary()
                .ok_or_else(|| error("project requires a working directory"))?;
            let cwd = environment.cwd().to_path_buf();
            let project_id = project_automation_id(&cwd);
            let store = state.event_subscriptions();
            let now_ms = project_automation_now_ms();
            let capture_binding = matches!(
                &args.command,
                ProjectAutomationCommand::Bind { .. }
                    | ProjectAutomationCommand::LinkWork { .. }
                    | ProjectAutomationCommand::ActivateDelivery { .. }
            );
            let project = if matches!(args.command, ProjectAutomationCommand::Status) {
                store
                    .project_status(&project_id)
                    .await
                    .map_err(|failure| error(&failure.to_string()))?
                    .ok_or_else(|| error("project not yet enrolled; bind its purpose"))?
            } else {
                let expected = store
                    .project_status(&project_id)
                    .await
                    .map_err(|failure| error(&failure.to_string()))?;
                crate::project_automation::validate_project_evidence(
                    &args.command,
                    &cwd,
                    expected.as_ref(),
                )
                .await
                .map_err(|failure| error(&failure))?;
                store
                    .project_command(
                        &project_id,
                        invocation.session.thread_id(),
                        args.expected_revision,
                        args.command,
                        now_ms,
                    )
                    .await
                    .map_err(|failure| error(&failure.to_string()))?
            };
            if capture_binding {
                crate::project_work_context::capture_project_work_binding(
                    &invocation.turn.config.codex_home,
                    &project,
                    invocation.session.thread_id(),
                    now_ms,
                )
                .await;
            }
            let targets = project.delivery.values().map(|target| json!({"target":target.target,"workstream":target.workstream,"deliveryDueAtMs":target.delivery_due_at_ms,"hardStopAtMs":target.hard_stop_at_ms,"deliveredAtMs":target.delivered_at_ms,"paused":target.paused,"jobId":target.job.as_ref().map(|job|job.id),"jobKind":target.job.as_ref().map(|job|job.kind)})).collect::<Vec<_>>();
            let thread_id = invocation.session.thread_id().to_string();
            let value = json!({"projectId":project.project_id,"revision":project.revision,"mode":project.mode(now_ms),"taskMode":project.mode_for_thread(invocation.session.thread_id(),now_ms),"completed":project.completed,"ownerThreadId":project.owner_thread_id,"purpose":project.threads.get(&thread_id),"outcomeId":project.thread_outcomes.get(&thread_id),"experimentRef":project.thread_experiments.get(&thread_id),"nextReviewAtMs":project.next_review_at_ms,"reviewWindowStartMs":project.review_window_start_ms,"review":project.review,"delivery":targets});
            let output = serde_json::to_string(&value)
                .map_err(|_| error("cannot serialize project result"))?;
            if output.len() > 16384 {
                return Err(error(
                    "project result exceeds the safe bound; use the paginated CLI evidence interface",
                ));
            }
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                output,
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for ProjectAutomationHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

fn error(message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.to_owned())
}
