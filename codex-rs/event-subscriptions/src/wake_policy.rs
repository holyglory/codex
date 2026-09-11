use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::StoreError;
use crate::WakeItem;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WakePolicy {
    #[default]
    RunningOnly,
    AllowBackground,
}

/// A stable permission scope, independent of any individual alarm firing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum WakeScope {
    Thread,
    Subscription { subscription_id: Uuid },
    ProjectDelivery { project_id: String, target: String },
    ProjectReview { project_id: String },
}

impl WakeScope {
    pub fn key(&self) -> Result<String, StoreError> {
        let fields = match self {
            Self::Thread | Self::Subscription { .. } => Vec::new(),
            Self::ProjectDelivery { project_id, target } => vec![project_id, target],
            Self::ProjectReview { project_id } => vec![project_id],
        };
        if fields.iter().any(|field| {
            field.is_empty() || field.len() > 256 || field.chars().any(char::is_control)
        }) {
            return Err(StoreError::InvalidData);
        }
        serde_json::to_string(self).map_err(|_| StoreError::InvalidData)
    }

    pub fn for_wake(item: &WakeItem) -> Self {
        if let Some(event) = &item.event
            && event.source == "codex.project"
            && let Some(project_id) = event.labels.get("project")
        {
            if event.event_type == "performance_review_due" {
                return Self::ProjectReview {
                    project_id: project_id.clone(),
                };
            }
            if let Some(target) = event.labels.get("target") {
                return Self::ProjectDelivery {
                    project_id: project_id.clone(),
                    target: target.clone(),
                };
            }
        }
        Self::Subscription {
            subscription_id: item.subscription_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedWakePolicy {
    pub scope: WakeScope,
    pub policy: WakePolicy,
    pub revision: i64,
    pub authorization_ref: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadWakePolicy {
    pub revision: i64,
    pub stopped_revision: i64,
    pub resumed_revision: i64,
    pub policies: Vec<ScopedWakePolicy>,
}

impl ThreadWakePolicy {
    pub fn allows_background(&self, item: &WakeItem) -> bool {
        let scopes = [
            WakeScope::for_wake(item),
            WakeScope::Subscription {
                subscription_id: item.subscription_id,
            },
            WakeScope::Thread,
        ];
        self.allows_scopes(&scopes)
    }

    pub fn allows_scopes(&self, scopes: &[WakeScope]) -> bool {
        scopes
            .iter()
            .find_map(|scope| self.policies.iter().find(|entry| &entry.scope == scope))
            .is_some_and(|entry| {
                entry.policy == WakePolicy::AllowBackground
                    && (self.resumed_revision >= self.stopped_revision
                        || entry.revision > self.stopped_revision)
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeLifecycle {
    UserStarted,
    UserStopped,
}

/// Marks a turn actually started by the user, rather than a background loader.
#[derive(Clone, Copy)]
pub struct UserStartedSubscriptionWork;

/// Runtime admission during the interval between a Stop request and its durable receipt.
pub struct SubscriptionRunState {
    pub dispatch: tokio::sync::Mutex<()>,
    pub running: std::sync::atomic::AtomicBool,
    pub user_work: std::sync::atomic::AtomicBool,
    pub stop_pending: std::sync::atomic::AtomicBool,
}

impl Default for SubscriptionRunState {
    fn default() -> Self {
        Self {
            dispatch: tokio::sync::Mutex::new(()),
            running: std::sync::atomic::AtomicBool::new(/*v*/ false),
            user_work: std::sync::atomic::AtomicBool::new(/*v*/ true),
            stop_pending: std::sync::atomic::AtomicBool::new(/*v*/ false),
        }
    }
}

#[derive(Clone, Copy)]
pub enum SubscriptionWorkOrigin {
    UserWork,
    Background,
}

#[derive(Clone, Debug)]
pub struct WakePolicyChange {
    pub thread_id: ThreadId,
    pub scope: WakeScope,
    pub policy: WakePolicy,
    pub expected_revision: i64,
    pub authorization_ref: String,
}

#[cfg(test)]
#[path = "wake_policy_tests.rs"]
mod tests;
