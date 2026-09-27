use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_event_subscriptions::ProjectAutomation;
use codex_event_subscriptions::ProjectIdentityCandidate;
use codex_event_subscriptions::ProjectIdentityCandidates;
use codex_event_subscriptions::ProjectIdentityKind;
use codex_event_subscriptions::StoreError;
use sha1::Digest;
use sha1::Sha1;

use crate::session::session::Session;
use crate::session::step_context::StepContext;

#[path = "project_automation_evidence.rs"]
mod evidence;
pub use evidence::validate_project_evidence;

pub fn project_identity_candidates(cwd: &Path) -> ProjectIdentityCandidates {
    let canonical_workspace = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let workspace_id = format!(
        "project-{:x}",
        Sha1::digest(canonical_workspace.as_os_str().as_encoded_bytes())
    );
    let mut root = cwd;
    loop {
        if let Some(common) = codex_usage::discover_git_common_dir(root) {
            return ProjectIdentityCandidates {
                canonical: ProjectIdentityCandidate {
                    project_id: format!("project-{:x}", Sha1::digest(common.as_str().as_bytes())),
                    kind: ProjectIdentityKind::GitCommonDirectory,
                },
                aliases: vec![ProjectIdentityCandidate {
                    project_id: workspace_id,
                    kind: ProjectIdentityKind::WorkspacePath,
                }],
            };
        }
        let Some(parent) = root.parent() else { break };
        root = parent;
    }
    ProjectIdentityCandidates {
        canonical: ProjectIdentityCandidate {
            project_id: workspace_id,
            kind: ProjectIdentityKind::WorkspacePath,
        },
        aliases: Vec::new(),
    }
}

pub fn project_automation_id(cwd: &Path) -> String {
    project_identity_candidates(cwd).canonical.project_id
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
) -> Result<Option<ProjectAutomation>, StoreError> {
    if !step.turn.config.local_control_tools_enabled
        || step.turn.config.ephemeral
        || crate::guardian::is_basic_session_source(&step.turn.session_source)
    {
        return Ok(None);
    }
    let Some(state) = session.state_db() else {
        return Ok(None);
    };
    let Some(environment) = step.environments.primary() else {
        return Ok(None);
    };
    let identities = project_identity_candidates(&environment.cwd().to_path_buf());
    let store = state.event_subscriptions();
    let thread_id = session.thread_id();
    let project_id = store
        .resolve_project_identity(&identities, project_automation_now_ms())
        .await?;
    let previous = store.project_status(&project_id).await?;
    session
        .services
        .usage_runtime
        .bind_alarm_store(store.clone());
    session
        .services
        .usage_runtime
        .restore_work_context(previous.as_ref(), thread_id)
        .await;
    Ok(previous)
}
