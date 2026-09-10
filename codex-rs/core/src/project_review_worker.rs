use std::sync::Arc;

use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeDisposition;
use codex_extension_api::ExtensionData;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::turn_input::StartIfIdleSubmission;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnStartOptions;
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::CodexThread;
use crate::ThreadManager;
use crate::agent::AgentControl;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::next_thread_spawn_depth;
use crate::context::ContextualUserFragment;
use crate::context::EventSubscriptionWakeContext;
use crate::context::ProjectPerformanceReview;
use crate::project_automation::project_automation_id;
use crate::project_automation::project_automation_now_ms;

struct ReviewWorkerLifecycle {
    owner_thread_id: ThreadId,
    worker_thread_id: ThreadId,
    project_id: String,
    job_id: Uuid,
    control: AgentControl,
    state: crate::StateDbHandle,
    completion: OnceCell<()>,
}

impl ThreadManager {
    /// Starts or resumes one persisted review worker without waiting for its review to finish.
    pub async fn run_project_review_worker(
        &self,
        owner_thread_id: ThreadId,
        project_id: &str,
    ) -> CodexResult<WakeDisposition> {
        let owner = self.get_thread(owner_thread_id).await?;
        let config = owner.config().await;
        let snapshot = owner.config_snapshot().await;
        if !config.local_control_tools_enabled
            || project_automation_id(snapshot.cwd().as_path()) != project_id
        {
            return Err(CodexErr::InvalidRequest(
                "project review requires the owner's local project context".into(),
            ));
        }
        let state = owner.state_db().ok_or_else(|| {
            CodexErr::InvalidRequest("project review requires persistent state".into())
        })?;
        let store = state.event_subscriptions();
        let Some(project) = store
            .project_status(project_id)
            .await
            .map_err(review_error)?
        else {
            return Ok(WakeDisposition::Started);
        };
        let Some(job) = project.review.as_ref() else {
            return Ok(WakeDisposition::Started);
        };
        if project.paused || project.owner_thread_id != owner_thread_id {
            return Ok(WakeDisposition::DeferredUntilIdle);
        }
        let review = ProjectPerformanceReview::new(&project).map_err(CodexErr::InvalidRequest)?;
        let Some(worker_thread_id) = store
            .claim_project_review_worker(
                project_id,
                job.id,
                self.reserve_thread_id(),
                project_automation_now_ms(),
            )
            .await
            .map_err(review_error)?
        else {
            return Ok(WakeDisposition::DeferredUntilIdle);
        };
        let control = owner.session.services.agent_control.clone();
        let mut worker_config = (*config).clone();
        worker_config.ephemeral = false;
        let authority_roots = if snapshot.profile_workspace_roots.is_empty() {
            &snapshot.workspace_roots
        } else {
            &snapshot.profile_workspace_roots
        };
        let authority = snapshot
            .permission_profile
            .clone()
            .materialize_project_roots_with_workspace_roots(authority_roots);
        let requested = codex_protocol::models::PermissionProfile::workspace_write_with(
            &[],
            snapshot.permission_profile.network_sandbox_policy(),
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        )
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(snapshot.cwd()));
        let review_permissions = codex_protocol::intersect_effective_permission_profiles(
            &authority,
            &requested,
            snapshot.cwd().as_path(),
        )
        .map_err(|error| {
            CodexErr::InvalidRequest(format!(
                "cannot restrict review writes to the authorized repository: {error}"
            ))
        })?;
        worker_config
            .permissions
            .set_permission_profile(review_permissions)
            .map_err(|error| {
                CodexErr::InvalidRequest(format!(
                    "cannot apply repository-scoped review permissions: {error}"
                ))
            })?;
        let mut submitted = false;
        let worker = match self.get_thread(worker_thread_id).await {
            Ok(worker) => worker,
            Err(error) if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                let depth = next_thread_spawn_depth(&snapshot.session_source);
                let parent_path = control
                    .get_agent_metadata(owner_thread_id)
                    .and_then(|metadata| metadata.agent_path)
                    .unwrap_or_else(AgentPath::root);
                let agent_path = parent_path
                    .join(&format!("project_review_{}", job.id.simple()))
                    .map_err(CodexErr::InvalidRequest)?;
                let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: owner_thread_id,
                    depth,
                    agent_path: Some(agent_path),
                    agent_nickname: None,
                    agent_role: Some("project_review".into()),
                });
                match control
                    .resume_agent_from_rollout(
                        worker_config.clone(),
                        worker_thread_id,
                        source.clone(),
                    )
                    .await
                {
                    Ok(_) => self.get_thread(worker_thread_id).await?,
                    Err(error)
                        if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) =>
                    {
                        let spawned = control
                            .spawn_agent_with_context(
                                worker_config.clone(),
                                ContextualUserFragment::into(review.clone()),
                                Some(source),
                                SpawnAgentOptions {
                                    reserved_thread_id: Some(worker_thread_id),
                                    parent_thread_id: Some(owner_thread_id),
                                    environments: Some(snapshot.environment_selections().to_vec()),
                                    ..Default::default()
                                },
                            )
                            .await?;
                        if spawned.thread_id != worker_thread_id {
                            return Err(CodexErr::Fatal(
                                "project review worker did not use its persisted identity".into(),
                            ));
                        }
                        submitted = true;
                        self.get_thread(worker_thread_id).await?
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        worker
            .thread_extension_data()
            .get_or_init(|| ReviewWorkerLifecycle {
                owner_thread_id,
                worker_thread_id,
                project_id: project_id.to_owned(),
                job_id: job.id,
                control: control.clone(),
                state: Arc::clone(&state),
                completion: OnceCell::new(),
            });
        let history = worker.session.clone_history().await;
        let previous_input = history.raw_items().rev().find(|item| {
            matches!(item, ResponseItem::Message { content, .. } if content.iter().any(|item| {
                matches!(item, ContentItem::InputText { text } if ProjectPerformanceReview::matches_text(text))
            }))
        });
        if !submitted && !previous_input.is_some_and(|item| review.matches_signal(item)) {
            if matches!(worker.agent_status().await, AgentStatus::Running) {
                return Ok(WakeDisposition::DeferredUntilIdle);
            }
            worker.refresh_runtime_config(worker_config).await;
            let submission = worker
                .start_turn_if_idle(
                    TurnInputRequest::new(TurnInput::ResponseItem(ContextualUserFragment::into(
                        review,
                    )))
                    .on_start(TurnStartOptions {
                        turn_trigger: Some("project_review".into()),
                        ..Default::default()
                    }),
                )
                .await?;
            if let StartIfIdleSubmission::NotSubmitted { reason } = submission {
                if matches!(
                    reason,
                    codex_protocol::turn_input::NotSubmittedReason::NotIdle
                        | codex_protocol::turn_input::NotSubmittedReason::PendingTriggerTurn
                ) {
                    return Ok(WakeDisposition::DeferredUntilIdle);
                }
                return Err(CodexErr::InvalidRequest(format!(
                    "project review continuation declined: {reason:?}"
                )));
            }
        }
        worker.ensure_rollout_materialized().await;
        worker.flush_rollout().await?;
        if !matches!(
            worker.agent_status().await,
            AgentStatus::PendingInit | AgentStatus::Running
        ) {
            CodexThread::project_review_worker_idle(worker.thread_extension_data()).await?;
        }
        Ok(WakeDisposition::Started)
    }
}

fn review_error(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Fatal(format!("project review state is unavailable: {error}"))
}

impl CodexThread {
    /// Handles review-worker idle events using the existing agent lifecycle and durable job state.
    pub async fn project_review_worker_idle(data: &ExtensionData) -> CodexResult<Option<ThreadId>> {
        let Some(lifecycle) = data.get::<ReviewWorkerLifecycle>() else {
            return Ok(None);
        };
        if lifecycle.worker_thread_id.to_string() != data.level_id() {
            return Ok(None);
        }
        let current = lifecycle
            .state
            .event_subscriptions()
            .project_status(&lifecycle.project_id)
            .await
            .map_err(review_error)?;
        if current
            .as_ref()
            .and_then(|project| project.review.as_ref())
            .is_none_or(|job| job.id != lifecycle.job_id)
        {
            lifecycle
                .completion
                .get_or_try_init(|| async {
                    lifecycle
                        .control
                        .close_agent(lifecycle.worker_thread_id)
                        .await?;
                    Ok::<(), CodexErr>(())
                })
                .await?;
        }
        Ok(Some(current.map_or(lifecycle.owner_thread_id, |project| {
            project.owner_thread_id
        })))
    }

    /// Preserves delivery progress when a coalesced review notification remains pending.
    pub async fn start_project_subscription_wake_once(
        &self,
        wake: WakeBatch,
    ) -> CodexResult<WakeDisposition> {
        let expected: ResponseItem =
            ContextualUserFragment::into(EventSubscriptionWakeContext::new(wake.clone()));
        let history = self.session.clone_history().await;
        let already_delivered = history.raw_items().any(|item| match (item, &expected) {
            (
                ResponseItem::Message { role, content, .. },
                ResponseItem::Message {
                    role: expected_role,
                    content: expected_content,
                    ..
                },
            ) => role == expected_role && content == expected_content,
            _ => false,
        });
        if already_delivered {
            return Ok(WakeDisposition::Started);
        }
        match self.start_event_subscription_wake_if_idle(wake).await? {
            StartIfIdleSubmission::Started { .. } => Ok(WakeDisposition::Started),
            StartIfIdleSubmission::NotSubmitted {
                reason:
                    codex_protocol::turn_input::NotSubmittedReason::NotIdle
                    | codex_protocol::turn_input::NotSubmittedReason::PendingTriggerTurn,
            } => Ok(WakeDisposition::DeferredUntilIdle),
            StartIfIdleSubmission::NotSubmitted { reason } => Err(CodexErr::InvalidRequest(
                format!("project subscription wake declined: {reason:?}"),
            )),
        }
    }
}
