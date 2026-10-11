use crate::project_automation::project_identity_candidates;
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

pub(crate) struct ShellWorkContext {
    pub work: String,
    pub alarm: Option<String>,
    pub activation: Option<codex_utils_absolute_path::AbsolutePathBuf>,
}

pub(crate) async fn execution_context(
    context: &UnifiedExecContext,
    cwd: &PathUri,
) -> Option<ShellWorkContext> {
    let encoded = runtime_work_context(
        &context.session,
        &context.step_context,
        &context.call_id,
        cwd,
    )
    .await?;
    let mut value: serde_json::Value = serde_json::from_str(&encoded).ok()?;
    let mut alarm = value
        .as_object_mut()?
        .remove("alarm")
        .filter(|value| !value.is_null())
        .map(|value| value.to_string());
    let mut activation = None;
    if alarm.is_some()
        && let Some(state) = context.session.state_db()
        && !state
            .event_subscriptions()
            .event_route_exists(
                context.session.thread_id(),
                &coordinator_filter(context.session.thread_id()),
            )
            .await
            .ok()?
    {
        let directory = context
            .step_context
            .turn
            .config
            .codex_home
            .join("alarm-activations");
        let path = directory.join(context.session.thread_id().to_string());
        if path.as_path().to_str().is_some()
            && tokio::fs::create_dir_all(directory.as_path()).await.is_ok()
        {
            activation = Some(path);
        } else {
            alarm = None;
        }
    }
    Some(ShellWorkContext {
        work: value.to_string(),
        alarm,
        activation,
    })
}

pub(crate) async fn runtime_work_context(
    session: &crate::session::session::Session,
    step: &crate::session::step_context::StepContext,
    call_id: &str,
    cwd: &PathUri,
) -> Option<String> {
    let thread_id = session.thread_id().to_string();
    let operation_id = session
        .services
        .usage_runtime
        .active_operation_reference(&thread_id, Some(&step.turn.sub_id), call_id)
        .await;
    let identities = project_identity_candidates(&cwd.to_path_buf());
    let fallback_project_id = identities.canonical.project_id.clone();
    let (project_id, project) = match session.state_db() {
        Some(state) => {
            let store = state.event_subscriptions();
            let project_id = store
                .resolve_project_identity(
                    &identities,
                    crate::project_automation::project_automation_now_ms(),
                )
                .await
                .ok()?;
            let project = store.project_status(&project_id).await.ok().flatten();
            (project_id, project)
        }
        None => (fallback_project_id, None),
    };
    let alarm = if step.turn.config.local_control_tools_enabled && !step.turn.config.ephemeral {
        if session
            .state_db()
            .is_some_and(|state| state.event_subscriptions().alarm_delivery_available())
        {
            let now = crate::project_automation::project_automation_now_ms();
            Some(
                json!({"alarm_namespace":"codex.review.v1","capability_revision":1,"lease_expires_at":now+172_800_000}),
            )
        } else {
            None
        }
    } else {
        None
    };
    let value = json!({
        "version":1,"alarm":alarm,"native_project_id":project_id,"thread_id":thread_id,
        "turn_id":step.turn.sub_id,"operation_id":operation_id,
        "workstream_id":project.as_ref().and_then(|project|project.thread_workstreams.get(&thread_id)),
        "outcome_id":project.as_ref().and_then(|project|project.thread_outcomes.get(&thread_id)),
        "experiment_ref":project.as_ref().and_then(|project|project.thread_experiments.get(&thread_id)),
    });
    let encoded = serde_json::to_string(&value).ok()?;
    (encoded.len() <= 2048).then_some(encoded)
}

fn coordinator_filter(thread: ThreadId) -> codex_event_subscriptions::EventFilter {
    codex_event_subscriptions::EventFilter {
        source: "devcoordinator".into(),
        event_types: std::collections::BTreeSet::from([
            "review.reminder".into(),
            "source.unavailable".into(),
            "source.cursor_stale".into(),
        ]),
        labels: std::collections::BTreeMap::from([("owner_thread_id".into(), thread.to_string())]),
    }
}

pub(crate) async fn activate_mcp_route(session: &crate::session::session::Session) -> bool {
    let Some(state) = session
        .state_db()
        .filter(|state| state.event_subscriptions().alarm_delivery_available())
    else {
        return false;
    };
    state
        .event_subscriptions()
        .ensure_event_route(
            session.thread_id(),
            coordinator_filter(session.thread_id()),
            crate::project_automation::project_automation_now_ms(),
        )
        .await
        .is_ok()
}
