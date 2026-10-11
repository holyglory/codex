use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionWakePolicyParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub command: Option<EventWakePolicyCommand>,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "camelCase", export_to = "v2/")]
pub enum EventWakePolicyCommand {
    Read,
    Set {
        scope: EventWakeScope,
        policy: EventWakePolicy,
        #[serde(rename = "expectedRevision")]
        #[ts(rename = "expectedRevision", type = "number")]
        expected_revision: i64,
        #[serde(rename = "authorizationRef")]
        #[ts(rename = "authorizationRef")]
        authorization_ref: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum EventWakePolicy {
    RunningOnly,
    AllowBackground,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "camelCase", export_to = "v2/")]
pub enum EventWakeScope {
    Thread,
    Subscription {
        #[serde(rename = "subscriptionId")]
        #[ts(rename = "subscriptionId")]
        subscription_id: String,
    },
    ProjectDelivery {
        #[serde(rename = "projectId")]
        #[ts(rename = "projectId")]
        project_id: String,
        target: String,
    },
    ProjectReview {
        #[serde(rename = "projectId")]
        #[ts(rename = "projectId")]
        project_id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventWakePolicyEntry {
    pub scope: EventWakeScope,
    pub policy: EventWakePolicy,
    pub suspended: bool,
    pub authorization_ref: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionWakePolicyResponse {
    #[ts(type = "number")]
    pub revision: i64,
    /// Ordinary user work is running; a scoped background alarm does not resume it.
    pub running: bool,
    pub pending_alarm_count: u32,
    pub data: Vec<EventWakePolicyEntry>,
    pub next_cursor: Option<String>,
}
