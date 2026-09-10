use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_event_subscriptions::ProjectAutomation;
use codex_event_subscriptions::ProjectAutomationCommand;
use codex_event_subscriptions::ProjectMode;
use codex_event_subscriptions::WorkPurpose;
use sha1::Digest;
use sha1::Sha1;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolInvocation;

#[path = "project_automation_evidence.rs"]
mod evidence;
pub use evidence::validate_project_evidence;

pub fn project_automation_id(cwd: &Path) -> String {
    let mut root = cwd;
    loop {
        if let Some(common) = codex_usage::discover_git_common_dir(root) {
            return format!("project-{:x}", Sha1::digest(common.as_str().as_bytes()));
        }
        let Some(parent) = root.parent() else { break };
        root = parent;
    }
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    format!(
        "project-{:x}",
        Sha1::digest(canonical.as_os_str().as_encoded_bytes())
    )
}

pub fn project_automation_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

pub(crate) async fn ensure_project_enrollment(
    session: &Session,
    step: &StepContext,
) -> Result<Option<ProjectAutomation>, FunctionCallError> {
    if !step.turn.config.local_control_tools_enabled || step.turn.config.ephemeral {
        return Ok(None);
    }
    let Some(state) = session.state_db() else {
        return Ok(None);
    };
    let Some(environment) = step.environments.primary() else {
        return Ok(None);
    };
    let project_id = project_automation_id(&environment.cwd().to_path_buf());
    let store = state.event_subscriptions();
    let thread_id = session.thread_id();
    let previous = store.project_status(&project_id).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!("Cannot read project scheduling state: {error}"))
    })?;
    let was_bound = previous
        .as_ref()
        .is_some_and(|project| project.threads.contains_key(&thread_id.to_string()));
    let now_ms = project_automation_now_ms();
    let project = store
        .project_enroll_thread(&project_id, thread_id, step.turn.parent_thread_id, now_ms)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!("Cannot enroll project work: {error}"))
        })?;
    if !was_bound && project.threads.contains_key(&thread_id.to_string()) {
        crate::project_work_context::capture_project_work_binding(
            &step.turn.config.codex_home,
            &project,
            thread_id,
            now_ms,
        )
        .await;
    }
    Ok(Some(project))
}

pub(crate) async fn enforce_project_admission(
    invocation: &ToolInvocation,
) -> Result<(), FunctionCallError> {
    if !invocation.turn.config.local_control_tools_enabled
        || invocation.tool_name.name == "project_automation"
    {
        return Ok(());
    }
    let Some(state) = invocation.session.state_db() else {
        return Ok(());
    };
    let Some(environment) = invocation.step_context.environments.primary() else {
        return Ok(());
    };
    let project_id = project_automation_id(&environment.cwd().to_path_buf());
    let store = state.event_subscriptions();
    let now_ms = project_automation_now_ms();
    let project = store.project_status(&project_id).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!("Project deadline state is unavailable: {error}. Diagnose or restore the scheduling store before new implementation."))
    })?;
    if let Some(project) = project {
        let purpose = project
            .threads
            .get(&invocation.session.thread_id().to_string())
            .copied()
            .unwrap_or(WorkPurpose::Implementation);
        let read_only_control = matches!(
            invocation.tool_name.name.as_str(),
            "usage_stats"
                | "usage_activity"
                | "list_mcp_resources"
                | "list_mcp_resource_templates"
                | "read_mcp_resource"
                | "view_image"
                | "read_thread"
                | "list_threads"
                | "wait_threads"
                | "wait"
                | "write_stdin"
                | "await_work"
        );
        if project.mode_for_thread(invocation.session.thread_id(), now_ms)
            == ProjectMode::RecoveryOnly
            && purpose == WorkPurpose::Implementation
            && !read_only_control
        {
            return Err(FunctionCallError::RespondToModel(
                "The project reached its delivery hard-stop deadline. Ordinary implementation is paused; continue necessary delivery diagnosis, repair, checks and publication by binding purpose=recovery. Specification/analysis work is not a delivery obligation. Only verified delivery evidence or an explicit user postponement clears this deadline; do not relabel ordinary implementation to bypass it.".into(),
            ));
        }
        if project.mode(now_ms) == ProjectMode::Paused
            && (!read_only_control || invocation.tool_name.name == "await_work")
        {
            return Err(FunctionCallError::RespondToModel("This project is explicitly paused. Preserve existing results; resume only when the user resumes the work.".into()));
        }
        store
            .project_activity(&project_id, invocation.session.thread_id(), now_ms)
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "Cannot persist project activity: {error}"
                ))
            })?;
    }
    Ok(())
}

pub(crate) async fn observe_project_bottleneck(invocation: &ToolInvocation) {
    let observation = async {
        let state = invocation.session.state_db()?;
        let environment = invocation.step_context.environments.primary()?;
        let project_id = project_automation_id(&environment.cwd().to_path_buf());
        let store = state.event_subscriptions();
        let project = store.project_status(&project_id).await.ok()??;
        if project.paused || project.review.is_some() {
            return None;
        }
        let now_ms = project_automation_now_ms();
        let usage = codex_usage::UsageStore::open(&invocation.turn.config.codex_home)
            .await
            .ok()?;
        let packet = usage
            .performance_review_packet(codex_usage::PerformanceReviewQuery {
                repository_id: None,
                thread_id: Some(
                    codex_usage::ThreadId::new(invocation.session.thread_id().to_string()).ok()?,
                ),
                time_range: Some(
                    codex_usage::UtcTimeRange::new(project.review_window_start_ms, now_ms).ok()?,
                ),
            })
            .await
            .ok()?;
        let candidate = packet.candidates.iter().find(|candidate| {
            candidate
                .measured_interval_ms
                .is_some_and(|duration| duration >= 60_000)
        })?;
        let reference = format!("usage-operation:{}", candidate.evidence.operation_id);
        store
            .project_command(
                &project_id,
                invocation.session.thread_id(),
                Some(project.revision),
                ProjectAutomationCommand::RequestReview {
                    evidence_ref: reference,
                },
                now_ms,
            )
            .await
            .ok()
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), observation).await;
}

#[cfg(test)]
#[path = "project_automation_tests.rs"]
mod tests;
