use std::collections::BTreeMap;

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProjectAutomationCapability {
    pub version: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ProjectAutomationCommandParams {
    #[ts(optional = nullable)]
    pub project_id: Option<String>,
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable, type = "number | null")]
    pub expected_revision: Option<u64>,
    #[ts(optional = nullable)]
    pub command: Option<ProjectAutomationCommand>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[ts(tag = "action", rename_all = "camelCase", export_to = "v2/")]
pub enum ProjectAutomationCommand {
    #[schemars(title = "ProjectAutomationLinkWorkCommand")]
    LinkWork {
        #[serde(rename = "outcomeId")]
        #[ts(rename = "outcomeId")]
        outcome_id: Option<String>,
        #[serde(rename = "experimentRef")]
        #[ts(rename = "experimentRef")]
        experiment_ref: Option<String>,
        #[serde(
            rename = "clearOutcome",
            default,
            skip_serializing_if = "std::ops::Not::not"
        )]
        #[ts(rename = "clearOutcome")]
        clear_outcome: bool,
        #[serde(
            rename = "clearExperiment",
            default,
            skip_serializing_if = "std::ops::Not::not"
        )]
        #[ts(rename = "clearExperiment")]
        clear_experiment: bool,
    },
    #[schemars(title = "ProjectAutomationStatusCommand")]
    Status,
    Bind {
        purpose: ProjectWorkPurpose,
        workstream: Option<String>,
    },
    ActivateDelivery {
        target: String,
        surface: String,
        acceptance: String,
        #[serde(rename = "deliveryIntervalMs")]
        #[ts(rename = "deliveryIntervalMs", type = "number | null")]
        delivery_interval_ms: Option<i64>,
        #[serde(rename = "hardStopIntervalMs")]
        #[ts(rename = "hardStopIntervalMs", type = "number | null")]
        hard_stop_interval_ms: Option<i64>,
    },
    Postpone {
        target: String,
        #[serde(rename = "deliveryDueAtMs")]
        #[ts(rename = "deliveryDueAtMs", type = "number")]
        delivery_due_at_ms: i64,
        #[serde(rename = "hardStopAtMs")]
        #[ts(rename = "hardStopAtMs", type = "number")]
        hard_stop_at_ms: i64,
        #[serde(rename = "authorizationRef")]
        #[ts(rename = "authorizationRef")]
        authorization_ref: String,
    },
    Pause {
        target: Option<String>,
        #[serde(rename = "authorizationRef")]
        #[ts(rename = "authorizationRef")]
        authorization_ref: String,
    },
    #[schemars(title = "ProjectAutomationResumeCommand")]
    Resume { target: Option<String> },
    RecordDelivery {
        target: String,
        #[serde(rename = "deliveredAtMs")]
        #[ts(rename = "deliveredAtMs", type = "number")]
        delivered_at_ms: i64,
        #[serde(rename = "evidenceRef")]
        #[ts(rename = "evidenceRef")]
        evidence_ref: String,
    },
    CompleteReview {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
        #[serde(rename = "decisionRef")]
        #[ts(rename = "decisionRef")]
        decision_ref: String,
    },
    RequestReview {
        #[serde(rename = "evidenceRef")]
        #[ts(rename = "evidenceRef")]
        evidence_ref: String,
    },
    Transfer {
        #[serde(rename = "ownerThreadId")]
        #[ts(rename = "ownerThreadId")]
        owner_thread_id: String,
        #[serde(rename = "authorizationRef")]
        #[ts(rename = "authorizationRef")]
        authorization_ref: String,
    },
    Complete {
        #[serde(rename = "outcomeRef")]
        #[ts(rename = "outcomeRef")]
        outcome_ref: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ProjectWorkPurpose {
    Discussion,
    Specification,
    Analysis,
    Implementation,
    Recovery,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ProjectAutomationMode {
    PerformanceOnly,
    Normal,
    DeliveryDue,
    RecoveryOnly,
    Paused,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ProjectAutomationJobKind {
    PerformanceReview,
    Delivery,
    DeliveryRecovery,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProjectAutomationJob {
    pub id: String,
    pub kind: ProjectAutomationJobKind,
    #[ts(type = "number")]
    pub due_at_ms: i64,
    #[ts(type = "number")]
    pub revision: u64,
    pub notified: bool,
    pub decision_ref: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProjectDeliveryObligation {
    pub target: String,
    pub workstream: String,
    pub surface: String,
    pub acceptance: String,
    #[ts(type = "number")]
    pub started_at_ms: i64,
    #[ts(type = "number | null")]
    pub delivered_at_ms: Option<i64>,
    #[ts(type = "number")]
    pub delivery_interval_ms: i64,
    #[ts(type = "number")]
    pub hard_stop_interval_ms: i64,
    #[ts(type = "number")]
    pub delivery_due_at_ms: i64,
    #[ts(type = "number")]
    pub hard_stop_at_ms: i64,
    #[ts(type = "number")]
    pub revision: u64,
    pub paused: bool,
    pub job: Option<ProjectAutomationJob>,
    pub evidence_ref: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProjectAutomation {
    pub project_id: String,
    pub owner_thread_id: String,
    #[ts(type = "number")]
    pub revision: u64,
    pub mode: ProjectAutomationMode,
    #[ts(type = "number")]
    pub started_at_ms: i64,
    #[ts(type = "number")]
    pub last_activity_at_ms: i64,
    #[ts(type = "number")]
    pub review_window_start_ms: i64,
    #[ts(type = "number")]
    pub next_review_at_ms: i64,
    #[ts(type = "number")]
    pub review_interval_ms: i64,
    pub paused: bool,
    pub threads: BTreeMap<String, ProjectWorkPurpose>,
    pub completed: bool,
    pub thread_workstreams: BTreeMap<String, String>,
    #[serde(default)]
    pub thread_outcomes: BTreeMap<String, String>,
    #[serde(default)]
    pub thread_experiments: BTreeMap<String, String>,
    pub delivery: BTreeMap<String, ProjectDeliveryObligation>,
    pub review: Option<ProjectAutomationJob>,
    pub last_review_ref: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProjectAutomationCommandResponse {
    pub capability: Option<ProjectAutomationCapability>,
    pub project: Option<ProjectAutomation>,
}

#[cfg(test)]
#[path = "project_automation_tests.rs"]
mod tests;
