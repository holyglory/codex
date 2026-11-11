//! Explicit work switches share the capture queue with operation starts.

use super::TurnCapture;
use super::UsageCaptureRuntime;
use crate::tools::context::ToolInvocation;
use codex_usage::WorkBindingTransition;
use codex_usage::WorkBindingUpdate;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use tokio::sync::oneshot;

/// A sparse declaration for the current thread, sent only when its job changes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum UsageWorkAction {
    Set {
        native_project_id: String,
        workstream_id: Option<String>,
        outcome_id: Option<String>,
        expected_revision: u64,
    },
    Clear {
        expected_revision: u64,
    },
    Status,
}

/// A persisted switch and its capture boundary, or revision zero before any declaration.
/// Attribution records declared work; it does not certify delivered outcomes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWorkReceipt {
    pub revision: u64,
    pub transition: Option<WorkBindingTransition>,
}

pub(super) struct PendingWork {
    pub(super) thread_id: String,
    pub(super) action: UsageWorkAction,
    pub(super) receipt: oneshot::Sender<Result<UsageWorkReceipt, String>>,
}

impl UsageWorkAction {
    pub(super) fn validate(&self) -> Result<(), String> {
        if let Self::Set {
            native_project_id,
            workstream_id,
            outcome_id,
            ..
        } = self
            && [
                Some(native_project_id),
                workstream_id.as_ref(),
                outcome_id.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|id| {
                id.is_empty()
                    || id.len() > 128
                    || !id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
            })
        {
            return Err(
                "usage_work identifiers must contain 1–128 ASCII letters, digits, or -_.:".into(),
            );
        }
        Ok(())
    }
}

impl PendingWork {
    pub(super) fn execute(self, home: &Path) {
        // Cancellation before processing cannot introduce a later, unacknowledged switch.
        // Once persistence starts, a retry/status can recover the immutable receipt.
        if self.receipt.is_closed() {
            return;
        }
        let result = apply(home, self.thread_id, self.action);
        let _ = self.receipt.send(result);
    }
}

fn apply(
    home: &Path,
    thread_id: String,
    action: UsageWorkAction,
) -> Result<UsageWorkReceipt, String> {
    // Read only the bounded sparse switch journal, on the capture worker. Refresh on
    // each explicit control so another producer's revision cannot become a stale grant.
    let transition = codex_usage::read_work_binding_transitions(home)
        .map_err(|_| "usage_work storage unavailable; inspect status before retrying".to_string())?
        .into_iter()
        .rev()
        .find(|transition| transition.update.thread_id == thread_id);
    let revision =
        transition.as_ref().map_or(
            /*default*/ 0,
            |transition| transition.update.transition_sequence,
        );
    let current = UsageWorkReceipt {
        revision,
        transition,
    };
    let (native_project_id, workstream_id, outcome_id, expected_revision) = match action {
        UsageWorkAction::Status => return Ok(current),
        UsageWorkAction::Set {
            native_project_id,
            workstream_id,
            outcome_id,
            expected_revision,
        } => (
            Some(native_project_id),
            workstream_id,
            outcome_id,
            expected_revision,
        ),
        UsageWorkAction::Clear { expected_revision } => (None, None, None, expected_revision),
    };
    let update = WorkBindingUpdate {
        version: 1,
        native_project_id,
        thread_id,
        workstream_id,
        outcome_id,
        transition_sequence: expected_revision
            .checked_add(/*rhs*/ 1)
            .ok_or_else(|| "usage_work revision exhausted".to_string())?,
        source: "agent_declared".into(),
        authority: "agent_declared".into(),
    };
    if current
        .transition
        .as_ref()
        .is_some_and(|previous| previous.update == update)
    {
        return Ok(current);
    }
    if expected_revision != revision {
        return Err(format!(
            "usage_work revision conflict; current revision is {revision}; inspect status"
        ));
    }
    if current.transition.as_ref().is_some_and(|previous| {
        previous.update.native_project_id == update.native_project_id
            && previous.update.workstream_id == update.workstream_id
            && previous.update.outcome_id == update.outcome_id
    }) {
        return Ok(current);
    }
    let transition = codex_usage::capture_work_binding_transition(home, update).map_err(|_| {
        "usage_work persistence unavailable or revision changed; inspect status before retrying"
            .to_string()
    })?;
    Ok(UsageWorkReceipt {
        revision: transition.update.transition_sequence,
        transition: Some(transition),
    })
}

pub(crate) async fn apply_work(
    invocation: &ToolInvocation,
    action: UsageWorkAction,
) -> Result<UsageWorkReceipt, String> {
    let turn = invocation
        .turn
        .extension_data
        .get::<TurnCapture>()
        .ok_or_else(|| "usage_work capture unavailable".to_string())?;
    turn.context
        .sink
        .submit_work(turn.context.thread_id.clone(), action)
        .await
}

impl UsageCaptureRuntime {
    pub(crate) async fn usage_work(
        &self,
        thread_id: String,
        action: UsageWorkAction,
    ) -> Result<UsageWorkReceipt, String> {
        self.0.submit_work(thread_id, action).await
    }
}
