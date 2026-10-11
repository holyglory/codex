use std::collections::BTreeMap;

use codex_event_subscriptions::ProjectAutomationCommand;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::function_tool::FunctionCallError;
use crate::project_automation::project_automation_now_ms;
use crate::project_automation::project_identity_candidates;
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
            description: "Read legacy project clock state for migration using command {action:status}. Scheduling and mutations are retired. Use alarm_set for reminders and DevCoordinator2 for reviews.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("command".into(), JsonSchema::object(BTreeMap::new(), None, Some(true.into()))),
                ("expected_revision".into(), JsonSchema::number(Some("Not accepted by the legacy reader.".into()))),
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
            if !matches!(args.command, ProjectAutomationCommand::Status)
                || args.expected_revision.is_some()
            {
                return Err(error(
                    "project_automation is read-only migration evidence; use alarms and Coordinator review operations",
                ));
            }
            let state = invocation.session.state_db().ok_or_else(|| {
                error("durable project scheduling requires the persistent state store")
            })?;
            let environment = invocation
                .step_context
                .environments
                .primary()
                .ok_or_else(|| error("project requires a working directory"))?;
            let cwd = environment.cwd().to_path_buf();
            let store = state.event_subscriptions();
            let now_ms = project_automation_now_ms();
            let identities = project_identity_candidates(&cwd);
            let project_id = store
                .resolve_project_identity(&identities, now_ms)
                .await
                .map_err(|failure| error(&failure.to_string()))?;
            let Some(project) = store
                .project_status(&project_id)
                .await
                .map_err(|failure| error(&failure.to_string()))?
            else {
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    json!({"migration":"no_legacy_state","schedulingActive":false}).to_string(),
                    Some(true),
                )));
            };
            let targets = project.delivery.values().map(|target| json!({"target":target.target,"workstream":target.workstream,"deliveryDueAtMs":target.delivery_due_at_ms,"hardStopAtMs":target.hard_stop_at_ms,"deliveredAtMs":target.delivered_at_ms,"paused":target.paused,"jobId":target.job.as_ref().map(|job|job.id),"jobKind":target.job.as_ref().map(|job|job.kind)})).collect::<Vec<_>>();
            let thread_id = invocation.session.thread_id().to_string();
            let value = json!({"migration":"retained_legacy_state","schedulingActive":false,"projectId":project.project_id,"revision":project.revision,"mode":project.mode(now_ms),"taskMode":project.mode_for_thread(invocation.session.thread_id(),now_ms),"completed":project.completed,"ownerThreadId":project.owner_thread_id,"purpose":project.threads.get(&thread_id),"outcomeId":project.thread_outcomes.get(&thread_id),"experimentRef":project.thread_experiments.get(&thread_id),"nextReviewAtMs":project.next_review_at_ms,"reviewWindowStartMs":project.review_window_start_ms,"review":project.review,"delivery":targets});
            let output = serde_json::to_string(&value)
                .map_err(|_| error("cannot serialize project result"))?;
            if output.len() > 8192 {
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
