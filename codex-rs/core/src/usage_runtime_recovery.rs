use super::tool::UsageToolAttempt;
use super::*;
use std::time::Duration;

const RECOVERY_DELAYS: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(500)];
const MAX_START_ATTEMPTS: usize = RECOVERY_DELAYS.len() + 1;

impl UsageRuntime {
    pub(crate) async fn begin_model_attempt(
        self: &Arc<Self>,
        context: ModelAttemptContext<'_>,
    ) -> Result<UsageAttempt, CodexErr> {
        let mut last_error = unavailable();
        for attempt in 0..MAX_START_ATTEMPTS {
            if let Some(delay) = attempt
                .checked_sub(1)
                .and_then(|index| RECOVERY_DELAYS.get(index))
            {
                tokio::time::sleep(*delay).await;
            }
            if self.faulted.load(Ordering::Acquire) {
                if let Err(error) = self.recover_after_write_failure().await {
                    last_error = error;
                    continue;
                }
                if self.faulted.load(Ordering::Acquire) {
                    continue;
                }
            }
            match self.begin_model_attempt_once(&context).await {
                Ok(model_attempt) if !self.faulted.load(Ordering::Acquire) => {
                    return Ok(model_attempt);
                }
                Ok(model_attempt) => {
                    drop(model_attempt);
                    last_error = unavailable();
                }
                Err(error) if !self.faulted.load(Ordering::Acquire) => return Err(error),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    pub(crate) async fn begin_tool_attempt(
        self: &Arc<Self>,
        context: ToolAttemptContext<'_>,
    ) -> Result<UsageToolAttempt, CodexErr> {
        let mut last_error = unavailable();
        for attempt in 0..MAX_START_ATTEMPTS {
            if let Some(delay) = attempt
                .checked_sub(1)
                .and_then(|index| RECOVERY_DELAYS.get(index))
            {
                tokio::time::sleep(*delay).await;
            }
            if self.faulted.load(Ordering::Acquire) {
                if let Err(error) = self.recover_after_write_failure().await {
                    last_error = error;
                    continue;
                }
                if self.faulted.load(Ordering::Acquire) {
                    continue;
                }
            }
            match self.begin_tool_attempt_once(&context).await {
                Ok(tool_attempt) if !self.faulted.load(Ordering::Acquire) => {
                    return Ok(tool_attempt);
                }
                Ok(tool_attempt) => {
                    drop(tool_attempt);
                    last_error = unavailable();
                }
                Err(error) if !self.faulted.load(Ordering::Acquire) => return Err(error),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    async fn recover_after_write_failure(&self) -> Result<(), CodexErr> {
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
