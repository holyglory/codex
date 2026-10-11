use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_event_subscriptions::AlarmSpec;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;
use uuid::Uuid;

pub struct AlarmHandler(pub &'static str);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    alarm_id: Option<Uuid>,
    offset: Option<usize>,
    limit: Option<usize>,
}

impl ToolExecutor<ToolInvocation> for AlarmHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.0)
    }
    fn spec(&self) -> ToolSpec {
        let set = self.0 == "alarm_set";
        let mut fields = BTreeMap::new();
        if set {
            for key in [
                "dedupe_key",
                "project_id",
                "workstream_id",
                "subject",
                "summary",
            ] {
                fields.insert(key.into(), JsonSchema::string(None));
            }
            for key in [
                "absolute_at_ms",
                "relative_ms",
                "active_work_ms",
                "expires_at_ms",
            ] {
                fields.insert(key.into(), JsonSchema::number(None));
            }
            fields.insert(
                "operation_result".into(),
                JsonSchema::object(
                    BTreeMap::from([
                        ("operation_id".into(), JsonSchema::string(None)),
                        ("tool_name".into(), JsonSchema::string(None)),
                        (
                            "outcome_class".into(),
                            JsonSchema::string(Some(
                                "completed, failed, denied, timed_out, or cancelled".into(),
                            )),
                        ),
                        ("result_code".into(), JsonSchema::string(None)),
                    ]),
                    Some(vec![
                        "operation_id".into(),
                        "tool_name".into(),
                        "outcome_class".into(),
                    ]),
                    Some(false.into()),
                ),
            );
        } else if self.0 == "alarm_list" {
            fields.insert("offset".into(), JsonSchema::number(None));
            fields.insert(
                "limit".into(),
                JsonSchema::number(Some("1–5, default 5".into())),
            );
        } else {
            fields.insert("alarm_id".into(), JsonSchema::string(None));
        }
        ToolSpec::Function(ResponsesApiTool {
            name:self.0.into(),description:"Durable generic reminders for this task; no project policy, worker, or business completion. alarm_set requires a stable dedupe_key, subject, summary and exactly one trigger: absolute_at_ms (UTC), relative_ms, active_work_ms (measured model/tool time excluding waits), or operation_result (exact call/operation and tool with terminal outcome). Opaque project_id/workstream_id group reminders. An existing scope/key retains its original alarm and terminal state; use a new key for changed intent. Never copy tool output or logs into reminder text. alarm_status/list read state; alarm_ack acknowledges a delivered message only; alarm_cancel retires a reminder. Background wake permission is separate and Stop is respected.".into(),strict:false,defer_loading:None,
            parameters:JsonSchema::object(fields,Some(if set {vec!["dedupe_key".into(),"subject".into(),"summary".into()]} else if self.0=="alarm_list" {vec![]} else {vec!["alarm_id".into()]}),Some(false.into())),output_schema:None,
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
            if arguments.len() > 4096 {
                return Err(error("alarm request exceeds 4 KiB"));
            }
            if invocation.turn.config.ephemeral {
                return Err(error("durable alarms require a persistent task"));
            }
            let state = invocation
                .session
                .state_db()
                .ok_or_else(|| error("alarms require persistent state"))?;
            let store = state.event_subscriptions();
            let thread = invocation.session.thread_id();
            let now = crate::project_automation::project_automation_now_ms();
            let value = if self.0 == "alarm_set" {
                let spec: AlarmSpec = serde_json::from_str(arguments)
                    .map_err(|_| error("invalid alarm specification"))?;
                spec.validate(now).map_err(error)?;
                serde_json::to_value(
                    store
                        .set_alarm(thread, spec, now)
                        .await
                        .map_err(|e| error(&e.to_string()))?,
                )
            } else {
                let args: ReadArgs = serde_json::from_str(arguments)
                    .map_err(|_| error("invalid alarm arguments"))?;
                if self.0 == "alarm_list" {
                    if args.alarm_id.is_some() || args.limit.is_some_and(|n| !(1..=5).contains(&n))
                    {
                        return Err(error("invalid alarm page"));
                    }
                    let offset = args.offset.unwrap_or(0);
                    let mut page = store
                        .list_alarms(thread, offset, args.limit.unwrap_or(5))
                        .await
                        .map_err(|e| error(&e.to_string()))?;
                    while serde_json::to_vec(&page)
                        .map_err(|_| error("cannot encode alarm page"))?
                        .len()
                        > 8192
                        && page.data.len() > 1
                    {
                        page.data.pop();
                        page.next_offset = Some(offset + page.data.len());
                    }
                    serde_json::to_value(page)
                } else {
                    if args.offset.is_some() || args.limit.is_some() {
                        return Err(error("pagination only applies to alarm_list"));
                    }
                    let id = args.alarm_id.ok_or_else(|| error("alarm_id is required"))?;
                    let alarm = match self.0 {
                        "alarm_ack" => store.acknowledge_alarm(thread, id, now).await,
                        "alarm_cancel" => store.cancel_alarm(thread, id, now).await,
                        _ => store.alarm_status(thread, id).await.and_then(|a| {
                            a.ok_or(codex_event_subscriptions::StoreError::InvalidData)
                        }),
                    }
                    .map_err(|e| error(&e.to_string()))?;
                    serde_json::to_value(alarm)
                }
            }
            .map_err(|_| error("cannot encode alarm"))?;
            let output = value.to_string();
            if output.len() > 8192 {
                return Err(error("alarm result exceeds the 8 KiB context bound"));
            }
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                output,
                Some(true),
            )))
        })
    }
}
impl CoreToolRuntime for AlarmHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}
fn error(message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}
