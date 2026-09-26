use std::path::PathBuf;
use std::sync::Arc;

use codex_app_server_protocol as api;
use codex_event_subscriptions as native;
use codex_protocol::ThreadId;
use codex_rollout::StateDbHandle;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ThreadStore;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;

#[path = "project_automation_mapping.rs"]
mod mapping;

pub(crate) struct ProjectAutomationRequestProcessor {
    state_db: Option<StateDbHandle>,
    service: Option<native::EventSubscriptionService>,
    thread_store: Arc<dyn ThreadStore>,
    thread_manager: Arc<codex_core::ThreadManager>,
}

impl ProjectAutomationRequestProcessor {
    pub(crate) fn new(
        _codex_home: codex_utils_absolute_path::AbsolutePathBuf,
        state_db: Option<StateDbHandle>,
        service: Option<native::EventSubscriptionService>,
        thread_store: Arc<dyn ThreadStore>,
        thread_manager: Arc<codex_core::ThreadManager>,
    ) -> Self {
        Self {
            state_db,
            service,
            thread_store,
            thread_manager,
        }
    }

    pub(crate) async fn command(
        &self,
        params: api::ProjectAutomationCommandParams,
    ) -> Result<api::ProjectAutomationCommandResponse, api::JSONRPCErrorError> {
        if params
            .command
            .as_ref()
            .is_some_and(|command| !matches!(command, api::ProjectAutomationCommand::Status))
        {
            return Err(invalid_request(
                "project clocks are retired; use generic alarms and Coordinator review operations",
            ));
        }
        let capability = self
            .state_db
            .as_ref()
            .zip(self.service.as_ref())
            .map(|_| api::ProjectAutomationCapability { version: 1 });
        if params.project_id.is_none()
            && params.thread_id.is_none()
            && params.command.is_none()
            && params.expected_revision.is_none()
        {
            return Ok(api::ProjectAutomationCommandResponse {
                capability,
                project: None,
            });
        }
        let (Some(state_db), Some(_service)) = (&self.state_db, &self.service) else {
            return Err(invalid_request(
                "project automation requires local control tools and the durable local state database",
            ));
        };
        let thread = match params.thread_id.as_deref() {
            Some(value) => {
                let thread_id = parse_thread_id(value)?;
                Some((thread_id, self.persistent_thread_cwd(thread_id).await?))
            }
            None => None,
        };
        let inferred_project_id = thread
            .as_ref()
            .map(|(_, cwd)| codex_core::project_automation_id(cwd));
        if let (Some(requested), Some(inferred)) = (&params.project_id, &inferred_project_id)
            && requested != inferred
        {
            return Err(invalid_params(
                "projectId does not match the task's project scope",
            ));
        }
        let project_id = params
            .project_id
            .or(inferred_project_id)
            .ok_or_else(|| invalid_params("projectId or threadId is required"))?;
        if project_id.trim().is_empty() || project_id.len() > 256 || project_id.contains('\0') {
            return Err(invalid_params("invalid projectId"));
        }
        let now_ms = codex_core::project_automation_now_ms();
        let store = state_db.event_subscriptions();
        if params.expected_revision.is_some() {
            return Err(invalid_params("status does not accept expectedRevision"));
        }
        let project = store
            .project_status(&project_id)
            .await
            .map_err(store_error)?;
        Ok(api::ProjectAutomationCommandResponse {
            capability,
            project: project.map(|project| mapping::api_project(project, now_ms)),
        })
    }

    async fn persistent_thread_cwd(
        &self,
        thread_id: ThreadId,
    ) -> Result<PathBuf, api::JSONRPCErrorError> {
        if let Ok(thread) = self.thread_manager.get_thread(thread_id).await
            && thread.config_snapshot().await.ephemeral
        {
            return Err(invalid_params(
                "project automation requires a persistent thread",
            ));
        }
        let thread = self
            .thread_store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .map_err(|_| invalid_params("thread not found"))?;
        if thread.archived_at.is_some() {
            return Err(invalid_params(
                "project automation requires an unarchived thread",
            ));
        }
        Ok(thread.cwd)
    }
}

fn parse_thread_id(value: &str) -> Result<ThreadId, api::JSONRPCErrorError> {
    ThreadId::from_string(value).map_err(|_| invalid_params("invalid threadId"))
}

fn store_error(error: native::StoreError) -> api::JSONRPCErrorError {
    match error {
        native::StoreError::TotalCapacity | native::StoreError::ThreadCapacity => {
            invalid_request(error.to_string())
        }
        native::StoreError::RevisionConflict { .. } => invalid_request(error.to_string()),
        native::StoreError::Unavailable(_) | native::StoreError::InvalidData => {
            internal_error("project automation storage is unavailable")
        }
    }
}
