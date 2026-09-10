use std::collections::BTreeMap;
use std::collections::BTreeSet;

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

pub const DAY_MS: i64 = 86_400_000;
pub const MAX_PROJECT_TARGETS: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPurpose {
    Discussion,
    Specification,
    Analysis,
    Implementation,
    Recovery,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectMode {
    PerformanceOnly,
    Normal,
    DeliveryDue,
    RecoveryOnly,
    Paused,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationJobKind {
    PerformanceReview,
    Delivery,
    DeliveryRecovery,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationJob {
    pub id: Uuid,
    pub kind: AutomationJobKind,
    pub due_at_ms: i64,
    pub revision: u64,
    pub notified: bool,
    pub decision_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryObligation {
    pub target: String,
    pub workstream: String,
    pub surface: String,
    pub acceptance: String,
    pub started_at_ms: i64,
    pub delivered_at_ms: Option<i64>,
    pub delivery_interval_ms: i64,
    pub hard_stop_interval_ms: i64,
    pub delivery_due_at_ms: i64,
    pub hard_stop_at_ms: i64,
    pub revision: u64,
    pub paused: bool,
    pub job: Option<AutomationJob>,
    pub evidence_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectAutomation {
    pub project_id: String,
    pub owner_thread_id: ThreadId,
    pub revision: u64,
    pub started_at_ms: i64,
    pub last_activity_at_ms: i64,
    pub review_window_start_ms: i64,
    pub next_review_at_ms: i64,
    pub review_interval_ms: i64,
    pub paused: bool,
    pub threads: BTreeMap<String, WorkPurpose>,
    #[serde(default)]
    pub completed: bool,
    pub thread_workstreams: BTreeMap<String, String>,
    #[serde(default)]
    pub implementation_starts: BTreeMap<String, i64>,
    #[serde(default)]
    pub thread_outcomes: BTreeMap<String, String>,
    #[serde(default)]
    pub thread_experiments: BTreeMap<String, String>,
    pub delivery: BTreeMap<String, DeliveryObligation>,
    pub review: Option<AutomationJob>,
    pub last_review_ref: Option<String>,
    pub reviewed_signals: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProjectAutomationCommand {
    Status,
    LinkWork {
        outcome_id: Option<String>,
        experiment_ref: Option<String>,
        #[serde(default)]
        clear_outcome: bool,
        #[serde(default)]
        clear_experiment: bool,
    },
    Bind {
        purpose: WorkPurpose,
        #[serde(default)]
        workstream: Option<String>,
    },
    ActivateDelivery {
        target: String,
        surface: String,
        acceptance: String,
        delivery_interval_ms: Option<i64>,
        hard_stop_interval_ms: Option<i64>,
    },
    Postpone {
        target: String,
        delivery_due_at_ms: i64,
        hard_stop_at_ms: i64,
        authorization_ref: String,
    },
    Pause {
        target: Option<String>,
        authorization_ref: String,
    },
    Resume {
        target: Option<String>,
    },
    RecordDelivery {
        target: String,
        delivered_at_ms: i64,
        evidence_ref: String,
    },
    CompleteReview {
        job_id: Uuid,
        decision_ref: String,
    },
    RequestReview {
        evidence_ref: String,
    },
    Transfer {
        owner_thread_id: ThreadId,
        authorization_ref: String,
    },
    Complete {
        outcome_ref: String,
    },
}

impl ProjectAutomation {
    pub fn new(project_id: String, owner_thread_id: ThreadId, now_ms: i64) -> Self {
        Self {
            project_id,
            owner_thread_id,
            revision: 1,
            started_at_ms: now_ms,
            last_activity_at_ms: now_ms,
            review_window_start_ms: now_ms,
            next_review_at_ms: now_ms.saturating_add(DAY_MS),
            review_interval_ms: DAY_MS,
            paused: false,
            threads: BTreeMap::new(),
            completed: false,
            thread_workstreams: BTreeMap::new(),
            implementation_starts: BTreeMap::new(),
            thread_outcomes: BTreeMap::new(),
            thread_experiments: BTreeMap::new(),
            delivery: BTreeMap::new(),
            review: None,
            last_review_ref: None,
            reviewed_signals: BTreeSet::new(),
        }
    }

    pub fn mode(&self, now_ms: i64) -> ProjectMode {
        if self.paused {
            return ProjectMode::Paused;
        }
        let active = self.delivery.values().filter(|target| !target.paused);
        if active
            .clone()
            .any(|target| now_ms >= target.hard_stop_at_ms)
        {
            ProjectMode::RecoveryOnly
        } else if active
            .clone()
            .any(|target| now_ms >= target.delivery_due_at_ms)
        {
            ProjectMode::DeliveryDue
        } else if active.count() > 0 {
            ProjectMode::Normal
        } else {
            ProjectMode::PerformanceOnly
        }
    }

    pub fn mode_for_thread(&self, thread_id: ThreadId, now_ms: i64) -> ProjectMode {
        if self.paused {
            return ProjectMode::Paused;
        }
        let purpose = self.threads.get(&thread_id.to_string());
        if !matches!(
            purpose,
            Some(WorkPurpose::Implementation | WorkPurpose::Recovery)
        ) {
            return ProjectMode::PerformanceOnly;
        }
        let stream = self
            .thread_workstreams
            .get(&thread_id.to_string())
            .map(String::as_str)
            .unwrap_or("default");
        let active = self
            .delivery
            .values()
            .filter(|target| !target.paused && target.workstream == stream);
        if active
            .clone()
            .any(|target| now_ms >= target.hard_stop_at_ms)
        {
            ProjectMode::RecoveryOnly
        } else if active
            .clone()
            .any(|target| now_ms >= target.delivery_due_at_ms)
        {
            ProjectMode::DeliveryDue
        } else if active.count() > 0 {
            ProjectMode::Normal
        } else {
            ProjectMode::PerformanceOnly
        }
    }

    pub fn next_deadline(&self) -> Option<i64> {
        if self.paused || self.completed {
            return None;
        }
        let review = self
            .review
            .as_ref()
            .map_or(Some(self.next_review_at_ms), |job| {
                (!job.notified).then_some(job.due_at_ms)
            });
        self.delivery
            .values()
            .filter(|target| !target.paused)
            .filter_map(|target| match &target.job {
                None => Some(target.delivery_due_at_ms),
                Some(job) if !job.notified => Some(job.due_at_ms),
                Some(job) if job.kind == AutomationJobKind::Delivery => {
                    Some(target.hard_stop_at_ms)
                }
                Some(_) => None,
            })
            .chain(review)
            .min()
    }

    pub fn collect_due(&mut self, now_ms: i64) -> Vec<AutomationJob> {
        if self.paused || self.completed {
            return Vec::new();
        }
        if self.review.is_none() && now_ms >= self.next_review_at_ms {
            if self.last_activity_at_ms > self.review_window_start_ms {
                self.review = Some(self.job(AutomationJobKind::PerformanceReview, now_ms));
            } else {
                self.review_window_start_ms = now_ms;
                self.next_review_at_ms = now_ms.saturating_add(self.review_interval_ms);
            }
        }
        let mut jobs = Vec::new();
        if let Some(job) = &mut self.review
            && !job.notified
        {
            job.notified = true;
            jobs.push(job.clone());
        }
        for target in self.delivery.values_mut().filter(|target| !target.paused) {
            let kind = if now_ms >= target.hard_stop_at_ms {
                Some(AutomationJobKind::DeliveryRecovery)
            } else if now_ms >= target.delivery_due_at_ms {
                Some(AutomationJobKind::Delivery)
            } else {
                None
            };
            if let Some(kind) = kind
                && target.job.as_ref().is_none_or(|job| job.kind != kind)
            {
                target.job = Some(AutomationJob {
                    id: Uuid::now_v7(),
                    kind,
                    due_at_ms: if kind == AutomationJobKind::DeliveryRecovery {
                        target.hard_stop_at_ms
                    } else {
                        target.delivery_due_at_ms
                    },
                    revision: target.revision,
                    notified: false,
                    decision_ref: None,
                });
            }
            if let Some(job) = &mut target.job
                && !job.notified
            {
                job.notified = true;
                jobs.push(job.clone());
            }
        }
        jobs
    }

    pub(super) fn job(&self, kind: AutomationJobKind, due_at_ms: i64) -> AutomationJob {
        AutomationJob {
            id: Uuid::now_v7(),
            kind,
            due_at_ms,
            revision: self.revision,
            notified: false,
            decision_ref: None,
        }
    }
}

#[path = "project_commands.rs"]
mod commands;
#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;
