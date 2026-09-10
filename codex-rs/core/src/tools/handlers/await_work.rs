use crate::function_tool::FunctionCallError;
use crate::project_automation::project_automation_now_ms;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::HeartbeatSpec;
use codex_event_subscriptions::NewSubscription;
use codex_event_subscriptions::SourceCursor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub struct AwaitWorkHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    source: String,
    event_types: BTreeSet<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    after_sequence: Option<u64>,
    deadline_at_ms: i64,
}

impl ToolExecutor<ToolInvocation> for AwaitWorkHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("await_work")
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: "await_work".into(),
            description: "Wait without model polling for a real provider-neutral event or an expected-event deadline. The publisher must already be connected to this Codex app-server's event ingress; naming a source does not create an integration. Use exact operation labels/cursor and inspect existing status before waiting. No model requests run during this wait. The subscription survives server interruption so a pending event can resume this task. Cancellation removes the wait; timeout reports deadline_reached, never success. No raw event content is returned. Do independent work first when appropriate.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("source".into(), JsonSchema::string(Some("Existing event publisher name.".into()))),
                ("event_types".into(), JsonSchema::array(JsonSchema::string(None), Some("Meaningful completion/failure event kinds.".into()))),
                ("labels".into(), JsonSchema::object(BTreeMap::new(), None, Some(JsonSchema::string(None).into()))),
                ("after_sequence".into(), JsonSchema::number(Some("Last observed source sequence.".into()))),
                ("deadline_at_ms".into(), JsonSchema::number(Some("Expected-event deadline as UTC Unix milliseconds.".into()))),
            ]), Some(vec!["source".into(),"event_types".into(),"deadline_at_ms".into()]), Some(false.into())), output_schema: None,
        })
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(error("expected typed arguments"));
            };
            if arguments.len() > 8192 {
                return Err(error("wait arguments exceed the bounded size"));
            }
            let mut args: Arguments = serde_json::from_str(arguments)
                .map_err(|_| error("invalid event wait arguments"))?;
            if args.source == "devcoordinator"
                && (!args.labels.contains_key("repository_id")
                    || !(args.labels.contains_key("run_id")
                        || args.labels.contains_key("deployment_id")))
            {
                return Err(error(
                    "Coordinator waits require repository_id and an exact run_id or deployment_id label",
                ));
            }
            if args.source == "devcoordinator" {
                args.event_types
                    .extend(["source.unavailable".into(), "source.cursor_stale".into()]);
            }
            let now_ms = project_automation_now_ms();
            if args.deadline_at_ms <= now_ms {
                return Err(error("expected-event deadline must be in the future"));
            }
            let request = NewSubscription {
                thread_id: invocation.session.thread_id(),
                filter: Some(EventFilter {
                    source: args.source,
                    event_types: args.event_types,
                    labels: args.labels,
                }),
                source_cursor: args.after_sequence.map(|sequence| SourceCursor {
                    sequence,
                    value: None,
                }),
                heartbeat: Some(HeartbeatSpec {
                    interval_ms: 365 * 86_400_000,
                    first_deadline_at_ms: Some(args.deadline_at_ms),
                }),
            };
            request
                .validate(now_ms)
                .map_err(|failure| error(&failure.to_string()))?;
            let state = invocation
                .session
                .state_db()
                .ok_or_else(|| error("durable waits require a persistent app-server"))?;
            let store = state.event_subscriptions();
            let _event_wait = match &invocation.source {
                ToolCallSource::CodeMode { cell_id, .. } => Some(
                    invocation
                        .session
                        .services
                        .code_mode_service
                        .begin_event_wait(cell_id.to_string()),
                ),
                ToolCallSource::Direct | ToolCallSource::DirectPlaintextMessage => None,
            };
            let subscription = store
                .create_wait(request, now_ms)
                .await
                .map_err(|failure| error(&failure.to_string()))?;
            let subscription_id = subscription.id;
            if let Err(failure) = store
                .replay_subscription_events(subscription_id, now_ms)
                .await
            {
                let _ = subscription.cancel().await;
                return Err(error(&failure.to_string()));
            }
            let observed = tokio::select! {
                result = store.await_subscription(invocation.session.thread_id(),subscription_id) => result.map_err(|failure|error(&failure.to_string())),
                () = invocation.cancellation_token.cancelled() => Err(error("event wait cancelled")),
            };
            subscription
                .cancel()
                .await
                .map_err(|failure| error(&failure.to_string()))?;
            let item = observed?;
            let attention = item.event.as_ref().is_some_and(|event| {
                event.source == "devcoordinator"
                    && matches!(
                        event.event_type.as_str(),
                        "source.unavailable" | "source.cursor_stale"
                    )
            });
            let status = if attention {
                "attention_required"
            } else if item.event.is_some() {
                "event_received"
            } else {
                "deadline_reached"
            };
            let event = item.event.map(|event| {
                let mut metadata = json!({"source":event.source,"type":event.event_type,"occurredAtMs":event.occurred_at_ms,"coalescedCount":event.coalesced_event_count});
                if !attention {metadata["sequence"] = json!(event.cursor.sequence);}
                metadata
            });
            let value = json!({"subscriptionId":subscription_id,"status":status,"event":event});
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                value.to_string(),
                Some(true),
            )))
        })
    }
}
impl CoreToolRuntime for AwaitWorkHandler {}
fn error(message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.to_owned())
}

#[cfg(test)]
#[path = "await_work_tests.rs"]
mod tests;
