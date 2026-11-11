//! Runtime entity observations. Native process IDs, paths and remotes never enter facts.
use super::CaptureContext;
use super::Operation;
use super::TurnCapture;
use super::model::ResponseOperations;
use super::now_ms;
use super::sink::CaptureSink;
use crate::session::turn_context::TurnContext;
use codex_usage::USAGE_TAXONOMY_VERSION;
use codex_usage::UsageRecord;
use std::sync::Arc;
use std::sync::OnceLock;

struct ProcessIdentity {
    id: String,
    first_observed_at_ms: i64,
}

fn process() -> &'static ProcessIdentity {
    static PROCESS: OnceLock<ProcessIdentity> = OnceLock::new();
    PROCESS.get_or_init(|| ProcessIdentity {
        id: uuid::Uuid::new_v4().to_string(),
        first_observed_at_ms: now_ms(),
    })
}

pub(super) fn process_id() -> String {
    process().id.clone()
}

pub(super) fn register(sink: &Arc<CaptureSink>) {
    let process = process();
    let mut record = UsageRecord::new_millis(
        format!("{}:observed", process.id),
        process.first_observed_at_ms,
        /*input_tokens*/ 0,
        /*output_tokens*/ 0,
    );
    record.process_id = Some(process.id.clone());
    record.event = Some("process_observed".to_string());
    record.classification = Some("runtime_observed".to_string());
    record.coverage = Some("recorded".to_string());
    sink.submit(record.clone());
    record.id = format!("{}:taxonomy:{USAGE_TAXONOMY_VERSION}", process.id);
    record.event = Some("taxonomy_registered".to_string());
    record.taxonomy_version = Some(USAGE_TAXONOMY_VERSION);
    record.classification = Some("codex_phase_activity_v1".to_string());
    sink.submit(record);
}

pub(crate) fn annotate_record(turn: &TurnContext, record: &mut UsageRecord) {
    if let Some(responses) = turn.extension_data.get::<ResponseOperations>() {
        responses.annotate_token_record(record);
    } else {
        record.operation_id = None;
        record.account_id = None;
    }
    if let Some(capture) = turn.extension_data.get::<TurnCapture>() {
        record.process_id = Some(process_id());
        record.agent_id = Some(capture.context.thread_id.clone());
        record.parent_thread_id = capture.context.parent_thread_id.clone();
        record.taxonomy_version = Some(USAGE_TAXONOMY_VERSION);
    }
}

pub(super) enum AgentLifecycle {
    Started,
    Stopped,
}

pub(super) fn agent(operation: &Operation, transition: AgentLifecycle) {
    let context = &operation.context;
    let (event, suffix) = match transition {
        AgentLifecycle::Started => ("agent_started", "started"),
        AgentLifecycle::Stopped => ("agent_stopped", "stopped"),
    };
    let incarnation = operation
        .record
        .operation_id
        .as_deref()
        .unwrap_or(&operation.record.id);
    let mut record = context.fact(format!("{incarnation}:agent:{suffix}"), event);
    record.operation_id = operation.record.operation_id.clone();
    record.started_at = operation.record.started_at;
    record.started_at_ms = operation.record.started_at_ms;
    if matches!(transition, AgentLifecycle::Stopped) {
        record.ended_at = Some(record.observed_at);
        record.ended_at_ms = record.observed_at_ms;
    }
    context.sink.submit(record);
}

pub(super) fn repository_seen(context: &CaptureContext) {
    if context.repository_id.is_none() {
        return;
    }
    for event in ["repository_identity_observed", "repository_seen"] {
        let record = context.fact(uuid::Uuid::new_v4().to_string(), event);
        context.sink.submit(record);
    }
}

pub(super) fn repository_attribution(context: &CaptureContext, operation: &UsageRecord) {
    if !matches!(
        operation.event.as_deref(),
        Some("tool_started" | "model_request_started" | "model_warmup_started")
    ) {
        return;
    }
    let Some(id) = operation.operation_id.as_deref() else {
        return;
    };
    let mut record = context.fact(format!("{id}:repository"), "repository_attributed");
    record.operation_id = Some(id.to_string());
    if record.repository_id.is_none() {
        record.coverage = Some("repository_unavailable".to_string());
    }
    context.sink.submit(record);
}
