use super::*;
use codex_event_subscriptions::AlarmWorkEvent;
use codex_event_subscriptions::OperationOutcome;
use codex_event_subscriptions::OperationResult;

impl UsageRuntime {
    pub(super) async fn start_alarm_work(
        &self,
        operation: OperationId,
        thread: &str,
        eligible: bool,
    ) {
        if let Ok(thread_id) = codex_protocol::ThreadId::from_string(thread) {
            self.alarm_work(
                operation,
                AlarmWorkEvent::Start {
                    thread_id,
                    eligible,
                },
            )
            .await;
        }
    }
    pub(super) async fn start_alarm_tool(
        &self,
        attempt: &tool::UsageToolAttempt,
        context: &ToolAttemptContext<'_>,
    ) {
        let eligible = matches!(context.descriptor.activity_state, ActivityState::ToolActive)
            && context.execution_role != codex_usage::ToolExecutionRole::Wrapper;
        self.start_alarm_work(attempt.operation_id, context.thread_id, eligible)
            .await;
    }

    pub(crate) fn bind_alarm_store(&self, store: codex_state::SqliteEventSubscriptionStore) {
        let _ = self.alarm_store.set(store);
    }

    pub(super) async fn alarm_work(&self, operation: OperationId, event: AlarmWorkEvent) {
        if let Some(store) = self.alarm_store.get()
            && let Err(error) = store
                .observe_alarm_work(&operation.as_string(), event, now_ms())
                .await
        {
            tracing::warn!(%error,"alarm activity observation unavailable");
        }
    }

    pub(super) async fn alarm_tool_result(
        &self,
        thread: &str,
        operation: OperationId,
        call_id: &str,
        tool_name: &str,
        status: TerminalStatus,
        code: Option<ErrorCategory>,
    ) {
        self.alarm_work(operation, AlarmWorkEvent::Finish).await;
        let Some(store) = self.alarm_store.get() else {
            return;
        };
        let Ok(thread) = codex_protocol::ThreadId::from_string(thread) else {
            return;
        };
        let outcome_class = match status {
            TerminalStatus::Completed => OperationOutcome::Completed,
            TerminalStatus::Failed => OperationOutcome::Failed,
            TerminalStatus::Denied => OperationOutcome::Denied,
            TerminalStatus::TimedOut => OperationOutcome::TimedOut,
            TerminalStatus::Cancelled => OperationOutcome::Cancelled,
            TerminalStatus::Incomplete | TerminalStatus::Interrupted => OperationOutcome::Failed,
        };
        for id in [operation.as_string(), call_id.to_owned()] {
            let result = OperationResult {
                operation_id: id,
                tool_name: tool_name.into(),
                outcome_class: outcome_class.clone(),
                result_code: code.map(|code| code.as_str().to_owned()),
            };
            if let Err(error) = store.observe_alarm_result(thread, &result, now_ms()).await {
                tracing::warn!(%error,"alarm tool result unavailable");
            }
        }
    }
}

impl Drop for UsageAttempt {
    fn drop(&mut self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runtime = Arc::clone(&self.runtime);
            let operation = self.operation_id;
            drop(handle.spawn(async move {
                runtime.alarm_work(operation, AlarmWorkEvent::Finish).await;
            }));
        }
    }
}
