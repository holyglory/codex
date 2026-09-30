//! Reloads only the recorded children in a sealed maintenance inventory.
use super::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::MultiAgentVersion;
use codex_thread_store::ReadThreadParams;

impl ThreadManager {
    pub async fn ensure_maintenance_child_loaded(
        &self,
        child: ThreadId,
        expected_parent: ThreadId,
    ) -> Result<()> {
        let stored = self
            .state
            .read_stored_thread(ReadThreadParams {
                thread_id: child,
                include_archived: true,
                include_history: false,
            })
            .await?;
        if stored.parent_thread_id != Some(expected_parent)
            || stored.source.parent_thread_id() != Some(expected_parent)
        {
            return Err(CodexErr::InvalidRequest(
                "maintenance parent ownership changed".into(),
            ));
        }
        let parent = self.get_thread(expected_parent).await?;
        if parent.multi_agent_version() == Some(MultiAgentVersion::V2) {
            return self.ensure_multi_agent_v2_child_loaded(child).await;
        }
        if self.get_thread(child).await.is_ok() {
            return Ok(());
        }
        let mut config = parent.session.get_config().await.as_ref().clone();
        config.model = stored.model.clone();
        config.model_reasoning_effort = stored.reasoning_effort.clone();
        if config.model_provider_id != stored.model_provider {
            config.model_provider = config
                .model_providers
                .get(&stored.model_provider)
                .cloned()
                .ok_or_else(|| {
                    CodexErr::InvalidRequest(format!(
                        "Model provider `{}` not found",
                        stored.model_provider
                    ))
                })?;
            config.model_provider_id = stored.model_provider.clone();
        }
        let control = parent
            .session
            .services
            .local_agent_runtime
            .control(parent.session.session_id());
        control
            .resume_single_agent_from_rollout(config, child, stored.source)
            .await?;
        Ok(())
    }
}
