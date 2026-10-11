use std::collections::BTreeMap;

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionsCapability {
    pub version: u32,
    pub supports_heartbeats: bool,
    pub supports_event_ingress: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventSourceCursor {
    #[ts(type = "number")]
    pub sequence: u64,
    pub value: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionFilter {
    pub source: String,
    pub event_types: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionHeartbeat {
    #[ts(type = "number")]
    pub interval_seconds: u64,
    /// Nullable first deadline as whole Unix seconds.
    pub first_deadline_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventSubscription {
    pub id: String,
    pub thread_id: String,
    pub filter: Option<EventSubscriptionFilter>,
    pub source_cursor: Option<EventSourceCursor>,
    #[ts(type = "number | null")]
    pub heartbeat_interval_seconds: Option<u64>,
    pub next_heartbeat_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionCreateParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub filter: Option<EventSubscriptionFilter>,
    #[ts(optional = nullable)]
    pub source_cursor: Option<EventSourceCursor>,
    #[ts(optional = nullable)]
    pub heartbeat: Option<EventSubscriptionHeartbeat>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionCreateResponse {
    pub subscription: EventSubscription,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionListParams {
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionListResponse {
    pub data: Vec<EventSubscription>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionCancelParams {
    pub subscription_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionCancelResponse {
    pub cancelled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionTriggerParams {
    pub subscription_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventSubscriptionTriggerResponse {
    pub triggered_subscription_ids: Vec<String>,
    pub missing_subscription_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct EventPublishParams {
    pub event: IngressEvent,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
/// Security boundary from `security-assumptions.md`: only these bounded metadata fields cross
/// ingress; unknown publisher payload fields are rejected rather than retained or injected.
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct IngressEvent {
    pub id: String,
    pub source: String,
    pub event_type: String,
    pub cursor: EventSourceCursor,
    pub labels: BTreeMap<String, String>,
    /// Event occurrence time as whole Unix seconds.
    pub occurred_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EventPublishResponse {
    pub accepted_subscription_ids: Vec<String>,
    pub ignored_subscription_ids: Vec<String>,
}
