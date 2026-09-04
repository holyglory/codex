use std::collections::BTreeMap;
use std::collections::BTreeSet;

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::MAX_EVENT_TYPES;
use crate::MAX_LABELS;
use crate::MAX_LIST_LIMIT;
use crate::MAX_TRIGGER_SUBSCRIPTIONS;

const MAX_SOURCE_CHARS: usize = 128;
const MAX_EVENT_TYPE_CHARS: usize = 128;
const MAX_EVENT_ID_CHARS: usize = 128;
const MAX_CURSOR_CHARS: usize = 512;
const MAX_LABEL_KEY_CHARS: usize = 64;
const MAX_LABEL_VALUE_CHARS: usize = 256;
const MIN_HEARTBEAT_INTERVAL_MS: i64 = 1_000;
const MAX_HEARTBEAT_INTERVAL_MS: i64 = 365 * 24 * 60 * 60 * 1_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceCursor {
    pub sequence: u64,
    pub value: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventFilter {
    pub source: String,
    pub event_types: BTreeSet<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatSpec {
    pub interval_ms: i64,
    pub first_deadline_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewSubscription {
    pub thread_id: ThreadId,
    pub filter: Option<EventFilter>,
    pub source_cursor: Option<SourceCursor>,
    pub heartbeat: Option<HeartbeatSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Subscription {
    pub id: Uuid,
    pub thread_id: ThreadId,
    pub filter: Option<EventFilter>,
    pub source_cursor: Option<SourceCursor>,
    pub heartbeat_interval_ms: Option<i64>,
    pub next_heartbeat_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListSubscriptionsQuery {
    pub thread_id: Option<ThreadId>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPage {
    pub data: Vec<Subscription>,
    pub next_offset: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublishedEvent {
    pub id: String,
    pub source: String,
    pub event_type: String,
    pub cursor: SourceCursor,
    pub labels: BTreeMap<String, String>,
    pub occurred_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishEventOutcome {
    pub accepted_subscription_ids: Vec<Uuid>,
    pub ignored_subscription_ids: Vec<Uuid>,
    pub affected_thread_ids: Vec<ThreadId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerOutcome {
    pub triggered_subscription_ids: Vec<Uuid>,
    pub missing_subscription_ids: Vec<Uuid>,
    pub affected_thread_ids: Vec<ThreadId>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeReason {
    Event,
    Heartbeat,
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventMetadata {
    pub id: String,
    pub source: String,
    pub event_type: String,
    pub cursor: SourceCursor,
    pub labels: BTreeMap<String, String>,
    pub occurred_at_ms: i64,
    pub coalesced_event_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WakeItem {
    pub subscription_id: Uuid,
    pub reasons: BTreeSet<WakeReason>,
    pub event: Option<EventMetadata>,
    pub heartbeat_due_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WakeBatch {
    pub thread_id: ThreadId,
    pub items: Vec<WakeItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingWakeBatch {
    pub wake: WakeBatch,
    pub through_revision: i64,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ValidationError {
    #[error("an event subscription requires an event filter, a heartbeat, or both")]
    MissingTrigger,
    #[error("source cursor requires an event filter")]
    CursorWithoutFilter,
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} exceeds the maximum length of {max_chars} characters")]
    TooLong {
        field: &'static str,
        max_chars: usize,
    },
    #[error("{field} contains control characters")]
    ControlCharacter { field: &'static str },
    #[error("event filter must contain between 1 and {MAX_EVENT_TYPES} event types")]
    EventTypeCount,
    #[error("event metadata must contain at most {MAX_LABELS} labels")]
    LabelCount,
    #[error(
        "heartbeat interval must be between {MIN_HEARTBEAT_INTERVAL_MS} and {MAX_HEARTBEAT_INTERVAL_MS} milliseconds"
    )]
    HeartbeatInterval,
    #[error("heartbeat deadline must not be earlier than subscription creation")]
    HeartbeatDeadline,
    #[error("list limit must be between 1 and {MAX_LIST_LIMIT}")]
    ListLimit,
    #[error("trigger must contain between 1 and {MAX_TRIGGER_SUBSCRIPTIONS} subscription IDs")]
    TriggerCount,
}

impl NewSubscription {
    pub fn validate(&self, now_ms: i64) -> Result<(), ValidationError> {
        if self.filter.is_none() && self.heartbeat.is_none() {
            return Err(ValidationError::MissingTrigger);
        }
        if self.source_cursor.is_some() && self.filter.is_none() {
            return Err(ValidationError::CursorWithoutFilter);
        }
        if let Some(filter) = &self.filter {
            filter.validate()?;
        }
        if let Some(cursor) = &self.source_cursor {
            cursor.validate()?;
        }
        if let Some(heartbeat) = &self.heartbeat {
            heartbeat.validate(now_ms)?;
        }
        Ok(())
    }
}

impl SourceCursor {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(value) = &self.value {
            validate_text(
                value,
                "cursor.value",
                MAX_CURSOR_CHARS,
                /*allow_empty*/ false,
            )?;
        }
        Ok(())
    }
}

impl EventFilter {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_text(
            &self.source,
            "filter.source",
            MAX_SOURCE_CHARS,
            /*allow_empty*/ false,
        )?;
        if self.event_types.is_empty() || self.event_types.len() > MAX_EVENT_TYPES {
            return Err(ValidationError::EventTypeCount);
        }
        for event_type in &self.event_types {
            validate_text(
                event_type,
                "filter.eventTypes",
                MAX_EVENT_TYPE_CHARS,
                /*allow_empty*/ false,
            )?;
        }
        validate_labels(&self.labels, "filter.labels")
    }

    pub fn matches(&self, event: &PublishedEvent) -> bool {
        self.source == event.source
            && self.event_types.contains(&event.event_type)
            && self
                .labels
                .iter()
                .all(|(key, value)| event.labels.get(key) == Some(value))
    }
}

impl HeartbeatSpec {
    pub fn validate(&self, now_ms: i64) -> Result<(), ValidationError> {
        if !(MIN_HEARTBEAT_INTERVAL_MS..=MAX_HEARTBEAT_INTERVAL_MS).contains(&self.interval_ms) {
            return Err(ValidationError::HeartbeatInterval);
        }
        if self
            .first_deadline_at_ms
            .is_some_and(|deadline| deadline < now_ms)
        {
            return Err(ValidationError::HeartbeatDeadline);
        }
        Ok(())
    }

    pub fn first_deadline(&self, now_ms: i64) -> i64 {
        self.first_deadline_at_ms
            .unwrap_or_else(|| now_ms.saturating_add(self.interval_ms))
    }
}

impl PublishedEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_text(
            &self.id,
            "event.id",
            MAX_EVENT_ID_CHARS,
            /*allow_empty*/ false,
        )?;
        validate_text(
            &self.source,
            "event.source",
            MAX_SOURCE_CHARS,
            /*allow_empty*/ false,
        )?;
        validate_text(
            &self.event_type,
            "event.eventType",
            MAX_EVENT_TYPE_CHARS,
            /*allow_empty*/ false,
        )?;
        self.cursor.validate()?;
        validate_labels(&self.labels, "event.labels")
    }
}

impl ListSubscriptionsQuery {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.limit == 0 || self.limit > MAX_LIST_LIMIT {
            return Err(ValidationError::ListLimit);
        }
        Ok(())
    }
}

pub fn validate_trigger_ids(ids: &[Uuid]) -> Result<(), ValidationError> {
    if ids.is_empty() || ids.len() > MAX_TRIGGER_SUBSCRIPTIONS {
        return Err(ValidationError::TriggerCount);
    }
    Ok(())
}

fn validate_labels(
    labels: &BTreeMap<String, String>,
    field: &'static str,
) -> Result<(), ValidationError> {
    if labels.len() > MAX_LABELS {
        return Err(ValidationError::LabelCount);
    }
    for (key, value) in labels {
        validate_text(key, field, MAX_LABEL_KEY_CHARS, /*allow_empty*/ false)?;
        validate_text(
            value,
            field,
            MAX_LABEL_VALUE_CHARS,
            /*allow_empty*/ true,
        )?;
    }
    Ok(())
}

fn validate_text(
    value: &str,
    field: &'static str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<(), ValidationError> {
    if !allow_empty && value.is_empty() {
        return Err(ValidationError::Empty { field });
    }
    if value.chars().count() > max_chars {
        return Err(ValidationError::TooLong { field, max_chars });
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::ControlCharacter { field });
    }
    Ok(())
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
