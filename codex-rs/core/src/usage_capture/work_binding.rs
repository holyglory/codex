//! Explicit work switches share the capture queue with operation starts.

use super::TurnCapture;
use super::UsageCaptureRuntime;
use crate::tools::context::ToolInvocation;
use codex_usage::WorkBindingTransition;
use codex_usage::WorkBindingUpdate;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::VecDeque;
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

/// The explicit decision associated with one turn or job boundary.
///
/// A boundary is intentionally separate from the sparse binding itself. The
/// binding records the identifiers selected for capture; this state records
/// how the user resolved the boundary prompt when identifiers were unavailable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageWorkBoundaryState {
    Continue,
    Select,
    New,
    Clear,
    Unknown,
}

/// One bounded request for an unbound turn or job boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWorkBoundaryRequest {
    pub boundary_id: String,
    pub reason: &'static str,
    pub options: Vec<UsageWorkBoundaryState>,
}

/// Boundary state returned alongside a usage-work control receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWorkBoundaryStatus {
    pub boundary_id: String,
    pub state: UsageWorkBoundaryState,
    pub revision: u64,
    pub transition: Option<WorkBindingTransition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<UsageWorkBoundaryRequest>,
}

/// A persisted switch and its capture boundary, or revision zero before any declaration.
/// Attribution records declared work; it does not certify delivered outcomes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWorkReceipt {
    pub revision: u64,
    pub transition: Option<WorkBindingTransition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<UsageWorkBoundaryStatus>,
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

const BOUNDARY_OPTIONS: [UsageWorkBoundaryState; 5] = [
    UsageWorkBoundaryState::Continue,
    UsageWorkBoundaryState::Select,
    UsageWorkBoundaryState::New,
    UsageWorkBoundaryState::Clear,
    UsageWorkBoundaryState::Unknown,
];

const MAX_BOUNDARY_REQUESTS: usize = 256;

#[derive(Default)]
pub(super) struct BoundaryTracker {
    entries: HashMap<String, BoundaryEntry>,
    order: VecDeque<String>,
}

#[derive(Clone, Copy)]
struct BoundaryEntry {
    requested: bool,
    explicit_state: Option<UsageWorkBoundaryState>,
}

impl BoundaryTracker {
    fn entry_mut(&mut self, key: String) -> &mut BoundaryEntry {
        if !self.entries.contains_key(&key) {
            if self.entries.len() == MAX_BOUNDARY_REQUESTS
                && let Some(oldest) = self.order.pop_front()
            {
                self.entries.remove(&oldest);
            }
            self.order.push_back(key.clone());
            self.entries.insert(
                key.clone(),
                BoundaryEntry {
                    requested: false,
                    explicit_state: None,
                },
            );
        }
        self.entries.entry(key).or_insert(BoundaryEntry {
            requested: false,
            explicit_state: None,
        })
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
    // Read only this thread's current sparse binding on the capture worker. Refresh on
    // each explicit control so another producer's revision cannot become a stale grant.
    let transition = codex_usage::read_current_work_binding(home, &thread_id).map_err(|_| {
        "usage_work storage unavailable; inspect status before retrying".to_string()
    })?;
    let revision =
        transition.as_ref().map_or(
            /*default*/ 0,
            |transition| transition.update.transition_sequence,
        );
    let current = UsageWorkReceipt {
        revision,
        transition,
        boundary: None,
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
        boundary: None,
    })
}

pub(super) fn boundary_status(
    home: &Path,
    tracker: &std::sync::Mutex<BoundaryTracker>,
    thread_id: String,
    boundary_id: String,
    explicit_state: Option<UsageWorkBoundaryState>,
) -> Result<UsageWorkBoundaryStatus, String> {
    validate_boundary_id(&boundary_id)?;
    let transition = codex_usage::read_current_work_binding(home, &thread_id).map_err(|_| {
        "usage_work storage unavailable; inspect status before retrying".to_string()
    })?;
    let revision = transition
        .as_ref()
        .map_or(0, |value| value.update.transition_sequence);
    let key = format!("{thread_id}:{boundary_id}");
    let mut state = tracker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = state.entry_mut(key);
    if let Some(explicit_state) = explicit_state {
        entry.explicit_state = Some(explicit_state);
    }
    let selected = entry.explicit_state.unwrap_or_else(|| {
        if transition
            .as_ref()
            .is_some_and(|value| value.update.native_project_id.is_some())
        {
            UsageWorkBoundaryState::Continue
        } else if transition.is_some() {
            UsageWorkBoundaryState::Clear
        } else {
            UsageWorkBoundaryState::Unknown
        }
    });
    let request = if transition.is_none() && entry.explicit_state.is_none() && !entry.requested {
        entry.requested = true;
        Some(UsageWorkBoundaryRequest {
            boundary_id: boundary_id.clone(),
            reason: "missing_binding",
            options: BOUNDARY_OPTIONS.to_vec(),
        })
    } else {
        None
    };
    Ok(UsageWorkBoundaryStatus {
        boundary_id,
        state: selected,
        revision,
        transition,
        request,
    })
}

fn validate_boundary_id(boundary_id: &str) -> Result<(), String> {
    if boundary_id.is_empty()
        || boundary_id.len() > 128
        || !boundary_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
    {
        return Err(
            "usage_work boundary identifiers must contain 1–128 ASCII letters, digits, or -_.:"
                .into(),
        );
    }
    Ok(())
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
    let boundary_state = match &action {
        UsageWorkAction::Set { .. } => Some(UsageWorkBoundaryState::Select),
        UsageWorkAction::Clear { .. } => Some(UsageWorkBoundaryState::Clear),
        UsageWorkAction::Status => None,
    };
    turn.context
        .sink
        .submit_work_for_turn(
            turn.context.thread_id.clone(),
            action,
            turn.context.turn_id.clone(),
            boundary_state,
        )
        .await
}

pub(crate) async fn apply_work_boundary(
    invocation: &ToolInvocation,
    state: Option<UsageWorkBoundaryState>,
) -> Result<UsageWorkBoundaryStatus, String> {
    let turn = invocation
        .turn
        .extension_data
        .get::<TurnCapture>()
        .ok_or_else(|| "usage_work boundary capture unavailable".to_string())?;
    let boundary_id = turn
        .context
        .turn_id
        .clone()
        .ok_or_else(|| "usage_work boundary requires an active turn".to_string())?;
    turn.context
        .sink
        .boundary_status(turn.context.thread_id.clone(), boundary_id, state)
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
