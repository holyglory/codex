use crate::project_automation::project_automation_id;
use crate::unified_exec::UnifiedExecContext;
use codex_event_subscriptions::ProjectAutomation;
use codex_protocol::ThreadId;
use codex_utils_path_uri::PathUri;
use serde_json::json;
use std::path::Path;

pub async fn capture_project_work_binding(
    codex_home: &Path,
    project: &ProjectAutomation,
    thread_id: ThreadId,
    observed_at_ms: i64,
) {
    let capture = async {
        let usage = codex_usage::UsageStore::open(codex_home).await?;
        let thread = thread_id.to_string();
        let binding = codex_usage::NewWorkBinding {
            thread_id: codex_usage::ThreadId::new(thread.clone())
                .map_err(|_| codex_usage::UsageStoreError::InvalidFact)?,
            native_project_id: project.project_id.clone(),
            workstream_id: project.thread_workstreams.get(&thread).cloned(),
            outcome_id: project.thread_outcomes.get(&thread).cloned(),
            experiment_ref: project.thread_experiments.get(&thread).cloned(),
            observed_at_ms,
        };
        usage.record_work_binding(&binding).await
    };
    if !matches!(
        tokio::time::timeout(std::time::Duration::from_millis(250), capture).await,
        Ok(Ok(()))
    ) {
        tracing::warn!("content-free work binding capture is incomplete");
    }
}

pub(crate) async fn execution_context(
    context: &UnifiedExecContext,
    cwd: &PathUri,
) -> Option<String> {
    let project_id = project_automation_id(&cwd.to_path_buf());
    let thread_id = context.session.thread_id().to_string();
    let operation_id = context
        .session
        .services
        .usage_runtime
        .active_operation_reference(
            &thread_id,
            Some(&context.step_context.turn.sub_id),
            &context.call_id,
        )
        .await;
    let project = match context.session.state_db() {
        Some(state) => state
            .event_subscriptions()
            .project_status(&project_id)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let value = json!({
        "version":1,"native_project_id":project_id,"thread_id":thread_id,
        "turn_id":context.step_context.turn.sub_id,"operation_id":operation_id,
        "workstream_id":project.as_ref().and_then(|project|project.thread_workstreams.get(&thread_id)),
        "outcome_id":project.as_ref().and_then(|project|project.thread_outcomes.get(&thread_id)),
        "experiment_ref":project.as_ref().and_then(|project|project.thread_experiments.get(&thread_id)),
    });
    let encoded = serde_json::to_string(&value).ok()?;
    (encoded.len() <= 2048).then_some(encoded)
}
