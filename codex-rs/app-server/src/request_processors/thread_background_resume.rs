use std::sync::Arc;

use codex_core::CodexThread;
use codex_core::NewThread;
use codex_core::config::ConfigOverrides;
use codex_protocol::ThreadId;
use codex_protocol::mcp::ClientMcpExtensions;

use super::ThreadRequestProcessor;
use super::thread_input::can_accept_direct_input;

impl ThreadRequestProcessor {
    pub(crate) async fn ensure_background_thread_loaded(
        &self,
        thread_id: ThreadId,
    ) -> Result<Arc<CodexThread>, String> {
        if let Ok(thread) = self.thread_manager.get_thread(thread_id).await {
            return Ok(thread);
        }
        if self
            .pending_thread_unloads
            .lock()
            .await
            .contains(&thread_id)
        {
            return Err(format!("thread {thread_id} is still unloading"));
        }
        let stored_thread = self
            .read_stored_thread_for_resume(
                &thread_id.to_string(),
                /*path*/ None,
                /*include_history*/ false,
            )
            .await
            .map_err(|error| error.message)?;
        let stored_model = stored_thread.model.clone();
        let stored_model_provider = stored_thread.model_provider.clone();
        let stored_cwd = stored_thread.cwd.clone();
        let stored_approval_policy = stored_thread.approval_mode;
        let stored_permission_profile = stored_thread.permission_profile.clone();
        let stored_reasoning_effort = stored_thread.reasoning_effort.clone();
        let (thread_history, _) = self
            .load_resume_initial_history_from_stored_thread(stored_thread)
            .await
            .map_err(|error| error.message)?;

        if let Some((source, _)) = thread_history.get_resumed_session_sources()
            && !can_accept_direct_input(thread_history.get_multi_agent_version(), &source)
        {
            self.thread_manager
                .ensure_multi_agent_v2_child_loaded(thread_id)
                .await
                .map_err(|error| error.to_string())?;
            return self
                .thread_manager
                .get_thread(thread_id)
                .await
                .map_err(|error| error.to_string());
        }

        let mut request_overrides = None;
        let mut config_overrides = ConfigOverrides {
            model: stored_model,
            cwd: Some(stored_cwd.clone()),
            approval_policy: Some(stored_approval_policy),
            permission_profile: Some(stored_permission_profile),
            model_provider: Some(stored_model_provider),
            ..Default::default()
        };
        self.load_and_apply_persisted_resume_metadata(
            &thread_history,
            &mut request_overrides,
            &mut config_overrides,
        )
        .await;
        let mut config = self
            .config_manager
            .load_for_cwd(request_overrides, config_overrides, Some(stored_cwd))
            .await
            .map_err(|error| format!("failed to load thread configuration: {error}"))?;
        config.model_reasoning_effort = stored_reasoning_effort;
        let thread = match self
            .thread_manager
            .resume_thread_with_history(
                config,
                thread_history,
                self.auth_manager.clone(),
                /*parent_trace*/ None,
                ClientMcpExtensions::default(),
            )
            .await
        {
            Ok(NewThread { thread, .. }) => thread,
            Err(error) => self
                .thread_manager
                .get_thread(thread_id)
                .await
                .map_err(|_| format!("failed to resume thread {thread_id}: {error}"))?,
        };
        let thread_state = self.thread_state_manager.thread_state(thread_id).await;
        self.ensure_listener_task_running(thread_id, Arc::clone(&thread), thread_state)
            .await
            .map_err(|error| error.message)?;
        Ok(thread)
    }
}
