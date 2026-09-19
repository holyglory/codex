use super::tool::UsageToolAttempt;
use super::*;

impl UsageRuntime {
    pub(crate) async fn restore_work_context(
        &self,
        project: Option<&codex_event_subscriptions::ProjectAutomation>,
        thread_id: codex_protocol::ThreadId,
    ) {
        let thread = thread_id.to_string();
        let context = if let Some(project) =
            project.filter(|project| project.threads.contains_key(&thread))
        {
            codex_usage::OperationWorkContext {
                native_project_id: Some(project.project_id.clone()),
                workstream_id: project.thread_workstreams.get(&thread).cloned(),
                outcome_id: project.thread_outcomes.get(&thread).cloned(),
                experiment_ref: project.thread_experiments.get(&thread).cloned(),
            }
        } else {
            codex_usage::OperationWorkContext::default()
        };
        self.work_contexts.lock().await.insert(thread, context);
    }

    pub(crate) async fn begin_model_attempt(
        self: &Arc<Self>,
        context: ModelAttemptContext<'_>,
    ) -> UsageAttempt {
        let work_context = self
            .work_contexts
            .lock()
            .await
            .get(context.thread_id)
            .cloned()
            .unwrap_or_default();
        self.flush_pending_usage().await;
        if !self.faulted.load(Ordering::Acquire)
            && let Ok(model_attempt) = self.begin_model_attempt_once(&context, &work_context).await
        {
            return model_attempt;
        }
        self.begin_buffered_model_attempt(&context, &work_context)
            .await
    }

    pub(crate) async fn begin_tool_attempt(
        self: &Arc<Self>,
        context: ToolAttemptContext<'_>,
    ) -> UsageToolAttempt {
        let work_context = self
            .work_contexts
            .lock()
            .await
            .get(context.thread_id)
            .cloned()
            .unwrap_or_default();
        self.flush_pending_usage().await;
        if !self.faulted.load(Ordering::Acquire)
            && let Ok(tool_attempt) = self.begin_tool_attempt_once(&context, &work_context).await
        {
            return tool_attempt;
        }
        self.begin_buffered_tool_attempt(&context, &work_context)
            .await
    }

    pub(super) async fn recover_after_write_failure(&self) -> Result<(), CodexErr> {
        let _recovery_permit = self
            .recovery_gate
            .acquire()
            .await
            .map_err(|_| unavailable())?;
        if !self.faulted.load(Ordering::Acquire) {
            return Ok(());
        }
        if !self.fault_recovery_allowed.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        let generation = self.fault_generation.load(Ordering::Acquire);
        let affected_operations = self
            .faulted_operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let store = self.store().await?;
        let recovered = store
            .recover_after_write_failure(self.process_id, &affected_operations, now_ms())
            .await
            .map_err(|error| {
                self.latch_write_failure("runtime_recovery", None, error);
                unavailable()
            })?;
        tracing::warn!(
            interrupted_operations = recovered,
            "usage accounting recovered after a write failure"
        );
        if self.fault_generation.load(Ordering::Acquire) == generation {
            self.faulted_operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            self.fault_recovery_allowed.store(true, Ordering::Release);
            self.faulted.store(false, Ordering::Release);
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
#[path = "usage_runtime_recovery_tests.rs"]
mod tests;
