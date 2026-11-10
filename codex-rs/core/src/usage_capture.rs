//! Content-free observations of work that the runtime actually started.

mod activity;
mod activity_taxonomy;
mod approval;
mod entities;
mod lifecycle;
mod model;
mod model_transport;
mod sink;

use crate::client::ModelClient;
use crate::config::Config;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolInvocation;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_protocol::ThreadId;
use codex_usage::UsageOutcome;
use codex_usage::UsageRecord;
use sink::CaptureSink;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(crate) use activity::ActivityAction;
pub(crate) use activity::apply_activity;
pub(crate) use activity_taxonomy::Activity;
pub(crate) use activity_taxonomy::Phase;
pub(crate) use activity_taxonomy::Relation;
pub(crate) use approval::ApprovalObservation;
pub(crate) use entities::annotate_record;
pub(crate) use model_transport::retry_wait;
pub(crate) use sink::Persistence;

struct ThreadLineage {
    thread_id: String,
    parent_thread_id: Option<String>,
}

struct ThreadCapture {
    context: Arc<CaptureContext>,
    operation: Mutex<Option<Operation>>,
}

struct TurnCapture {
    context: Arc<CaptureContext>,
    operation: Arc<Mutex<Option<Operation>>>,
    failed: Arc<AtomicBool>,
}

pub(crate) struct CapturedModelClient(pub(crate) ModelClient);

pub(crate) struct UsageCaptureRuntime(Arc<CaptureSink>);

struct CaptureContext {
    sink: Arc<CaptureSink>,
    thread_id: String,
    parent_thread_id: Option<String>,
    turn_id: Option<String>,
    account_id: Option<String>,
    repository_id: Option<String>,
    activity: Arc<activity::ActivityState>,
    tool_operations: Arc<Mutex<HashMap<String, String>>>,
}

impl CaptureContext {
    fn fact(&self, id: String, event: &str) -> UsageRecord {
        let mut record = UsageRecord::new_millis(
            id,
            now_ms(),
            /*input_tokens*/ 0,
            /*output_tokens*/ 0,
        )
        .with_thread(self.thread_id.clone());
        record.process_id = Some(entities::process_id());
        record.agent_id = Some(self.thread_id.clone());
        record.taxonomy_version = Some(codex_usage::USAGE_TAXONOMY_VERSION);
        record.parent_thread_id = self.parent_thread_id.clone();
        record.turn_id = self.turn_id.clone();
        record.account_id = self.account_id.clone();
        record.repository_id = self.repository_id.clone();
        record.event = Some(event.to_string());
        record.classification = Some("runtime_observed".to_string());
        record.coverage = Some("recorded".to_string());
        record
    }
}

impl UsageCaptureRuntime {
    pub(crate) fn new(codex_home: &Path) -> Self {
        let sink = CaptureSink::new(codex_home);
        entities::register(&sink);
        Self(sink)
    }

    pub(crate) fn instrument_extensions(
        &self,
        extensions: Arc<ExtensionRegistry<Config>>,
    ) -> Arc<ExtensionRegistry<Config>> {
        let mut builder = extensions.to_builder();
        let contributor = Arc::new(lifecycle::CaptureLifecycle(Arc::clone(&self.0)));
        builder.thread_lifecycle_contributor(contributor.clone());
        builder.turn_lifecycle_contributor(contributor);
        Arc::new(builder.build())
    }
}

pub(crate) fn seed_thread(
    store: &ExtensionData,
    thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
) {
    store.insert(ThreadLineage {
        thread_id: thread_id.to_string(),
        parent_thread_id: parent_thread_id.map(|id| id.to_string()),
    });
}

impl TurnContext {
    pub(crate) fn install_usage_capture(&self, session: &Session) {
        let Some(thread) = session
            .services
            .thread_extension_data
            .get::<ThreadCapture>()
        else {
            return;
        };
        let context = Arc::new(CaptureContext {
            sink: Arc::clone(&thread.context.sink),
            thread_id: thread.context.thread_id.clone(),
            parent_thread_id: thread.context.parent_thread_id.clone(),
            turn_id: Some(bounded_label(&self.sub_id)),
            account_id: self.local_profile_id(),
            repository_id: crate::usage_identity::repository_id(self.config.cwd.as_path()),
            activity: Arc::clone(&thread.context.activity),
            tool_operations: Arc::default(),
        });
        entities::repository_seen(&context);
        if let Ok(mut client) = self.model_client(session) {
            client
                .request_contributors
                .push(Arc::new(model::CaptureRequests(Arc::clone(&context))));
            self.extension_data.insert(CapturedModelClient(client));
        }
        self.extension_data.insert(TurnCapture {
            context,
            operation: Arc::default(),
            failed: Arc::default(),
        });
    }

    pub(crate) fn retarget_usage_capture(&self, session: &Session) {
        self.extension_data.remove::<CapturedModelClient>();
        let Some(previous) = self.extension_data.get::<TurnCapture>() else {
            return;
        };
        let context = Arc::new(CaptureContext {
            sink: Arc::clone(&previous.context.sink),
            thread_id: previous.context.thread_id.clone(),
            parent_thread_id: previous.context.parent_thread_id.clone(),
            turn_id: previous.context.turn_id.clone(),
            account_id: self.local_profile_id(),
            repository_id: previous.context.repository_id.clone(),
            activity: Arc::clone(&previous.context.activity),
            tool_operations: Arc::clone(&previous.context.tool_operations),
        });
        if let Ok(mut client) = self.model_client(session) {
            client
                .request_contributors
                .push(Arc::new(model::CaptureRequests(Arc::clone(&context))));
            self.extension_data.insert(CapturedModelClient(client));
        }
        self.extension_data.insert(TurnCapture {
            context,
            operation: Arc::clone(&previous.operation),
            failed: Arc::clone(&previous.failed),
        });
    }
}

pub(crate) struct Operation {
    context: Arc<CaptureContext>,
    record: UsageRecord,
    terminal_event: &'static str,
    start: Instant,
    response_id: Mutex<Option<String>>,
    finished: AtomicBool,
    drop_outcome: UsageOutcome,
    call_id: Option<String>,
}

impl Operation {
    fn start(
        context: Arc<CaptureContext>,
        started_event: &'static str,
        terminal_event: &'static str,
    ) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let mut record = context
            .fact(format!("{id}:started"), started_event)
            .with_operation(id);
        record.coverage = Some("capture_started".to_string());
        record.started_at = Some(record.observed_at);
        record.started_at_ms = record.observed_at_ms;
        Self {
            context,
            record,
            terminal_event,
            start: Instant::now(),
            response_id: Mutex::new(/*t*/ None),
            finished: AtomicBool::new(false),
            drop_outcome: UsageOutcome::Cancelled,
            call_id: None,
        }
    }

    fn publish_started(&self) {
        entities::repository_attribution(&self.context, &self.record);
        self.context.sink.submit(self.record.clone());
    }

    pub(crate) fn tool(invocation: &ToolInvocation) -> Option<Self> {
        let turn = invocation.turn.extension_data.get::<TurnCapture>()?;
        let mut operation = Self::start(Arc::clone(&turn.context), "tool_started", "tool_finished");
        operation.record.model = Some(bounded_label(
            &invocation.step_context.settings.model_info.slug,
        ));
        let name = if invocation.tool_name.is_default_namespace() {
            invocation.tool_name.name.clone()
        } else {
            format!(
                "{}::{}",
                invocation
                    .tool_name
                    .namespace
                    .as_deref()
                    .unwrap_or_default(),
                invocation.tool_name.name
            )
        };
        operation.record.tool = Some(bounded_label(&name));
        if invocation.call_id.len() <= 160 {
            let mut operations = turn
                .context
                .tool_operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if operations.len() < 256
                && let Some(id) = operation.record.operation_id.as_ref()
            {
                operations.insert(invocation.call_id.clone(), id.clone());
                operation.call_id = Some(invocation.call_id.clone());
            }
        }
        turn.context.activity.annotate(&mut operation.record);
        operation.publish_started();
        Some(operation)
    }

    pub(crate) fn finish(&self, outcome: UsageOutcome) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(call_id) = &self.call_id {
            self.context
                .tool_operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(call_id);
        }
        let mut record = self.record.clone();
        if let Some(id) = &record.operation_id {
            record.id = format!("{id}:finished");
        }
        let observed_at_ms = now_ms();
        record.observed_at = observed_at_ms.div_euclid(1_000);
        record.observed_at_ms = Some(observed_at_ms);
        record.ended_at = Some(record.observed_at);
        record.ended_at_ms = Some(observed_at_ms);
        record.event = Some(self.terminal_event.to_string());
        record.response_id = self
            .response_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        record.outcome = Some(outcome);
        record.duration_ms =
            Some(u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX));
        record.coverage = Some("recorded".to_string());
        self.context.sink.submit(record);
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        self.finish(self.drop_outcome);
    }
}

fn now_ms() -> i64 {
    i64::try_from(codex_analytics::now_unix_millis()).unwrap_or(i64::MAX)
}

fn bounded_label(value: &str) -> String {
    if value.len() <= 160 && !value.chars().any(char::is_control) {
        value.to_string()
    } else {
        "unavailable".to_string()
    }
}
