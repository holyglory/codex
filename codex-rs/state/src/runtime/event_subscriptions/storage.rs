use std::collections::BTreeSet;
use std::collections::HashSet;

use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::EventMetadata;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::StoreError;
use codex_event_subscriptions::Subscription;
use codex_event_subscriptions::WakeReason;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::Transaction;
use uuid::Uuid;

pub(super) async fn next_revision(tx: &mut Transaction<'_, Sqlite>) -> Result<i64, StoreError> {
    sqlx::query_scalar(
        "UPDATE event_subscription_revisions
         SET revision = revision + 1 WHERE singleton = 1 RETURNING revision",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(store_error)
}

pub(super) async fn upsert_event_wake(
    tx: &mut Transaction<'_, Sqlite>,
    subscription_id: Uuid,
    revision: i64,
    event: &PublishedEvent,
    now_ms: i64,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO event_subscription_pending_wakes (
            subscription_id, revision, event_pending, event_id, event_source,
            event_type, event_sequence, event_cursor, event_labels_json,
            event_occurred_at_ms, event_count, updated_at_ms
         ) VALUES (?, ?, 1, ?, ?, ?, ?, ?, ?, ?, 1, ?)
         ON CONFLICT(subscription_id) DO UPDATE SET
            revision = excluded.revision,
            event_pending = 1,
            event_id = excluded.event_id,
            event_source = excluded.event_source,
            event_type = excluded.event_type,
            event_sequence = excluded.event_sequence,
            event_cursor = excluded.event_cursor,
            event_labels_json = excluded.event_labels_json,
            event_occurred_at_ms = excluded.event_occurred_at_ms,
            event_count = MIN(event_subscription_pending_wakes.event_count + 1, 4294967295),
            updated_at_ms = excluded.updated_at_ms",
    )
    .bind(subscription_id.to_string())
    .bind(revision)
    .bind(&event.id)
    .bind(&event.source)
    .bind(&event.event_type)
    .bind(event.cursor.sequence.to_string())
    .bind(event.cursor.value.as_deref())
    .bind(encode(&event.labels)?)
    .bind(event.occurred_at_ms)
    .bind(now_ms)
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    Ok(())
}

pub(super) async fn upsert_reason_wake(
    tx: &mut Transaction<'_, Sqlite>,
    subscription_id: Uuid,
    revision: i64,
    reason: WakeReason,
    heartbeat_due_at_ms: Option<i64>,
    now_ms: i64,
) -> Result<(), StoreError> {
    let (heartbeat, manual) = match reason {
        WakeReason::Heartbeat => (true, false),
        WakeReason::Manual => (false, true),
        WakeReason::Event => return Err(StoreError::InvalidData),
    };
    sqlx::query(
        "INSERT INTO event_subscription_pending_wakes (
            subscription_id, revision, heartbeat_pending, manual_pending,
            heartbeat_due_at_ms, updated_at_ms
         ) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(subscription_id) DO UPDATE SET
            revision = excluded.revision,
            heartbeat_pending = MAX(
                event_subscription_pending_wakes.heartbeat_pending,
                excluded.heartbeat_pending
            ),
            manual_pending = MAX(
                event_subscription_pending_wakes.manual_pending,
                excluded.manual_pending
            ),
            heartbeat_due_at_ms = CASE
                WHEN event_subscription_pending_wakes.heartbeat_due_at_ms IS NULL
                    THEN excluded.heartbeat_due_at_ms
                WHEN excluded.heartbeat_due_at_ms IS NULL
                    THEN event_subscription_pending_wakes.heartbeat_due_at_ms
                ELSE MIN(
                    event_subscription_pending_wakes.heartbeat_due_at_ms,
                    excluded.heartbeat_due_at_ms
                )
            END,
            updated_at_ms = excluded.updated_at_ms",
    )
    .bind(subscription_id.to_string())
    .bind(revision)
    .bind(heartbeat)
    .bind(manual)
    .bind(heartbeat_due_at_ms)
    .bind(now_ms)
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    Ok(())
}

pub(super) fn subscription_from_row(
    row: sqlx::sqlite::SqliteRow,
) -> Result<Subscription, StoreError> {
    let source = row
        .try_get::<Option<String>, _>("source")
        .map_err(store_error)?;
    let filter = source
        .map(|source| {
            Ok(EventFilter {
                source,
                event_types: decode(row.try_get("event_types_json").map_err(store_error)?)?,
                labels: decode(row.try_get("label_filters_json").map_err(store_error)?)?,
            })
        })
        .transpose()?;
    let cursor_sequence = row
        .try_get::<Option<String>, _>("cursor_sequence")
        .map_err(store_error)?;
    let cursor_value = row
        .try_get::<Option<String>, _>("cursor_value")
        .map_err(store_error)?;
    let source_cursor = cursor_sequence
        .map(|sequence| {
            Ok(SourceCursor {
                sequence: sequence.parse().map_err(|_| StoreError::InvalidData)?,
                value: cursor_value,
            })
        })
        .transpose()?;
    Ok(Subscription {
        id: parse_uuid(row.try_get("id").map_err(store_error)?)?,
        thread_id: parse_thread_id(row.try_get("thread_id").map_err(store_error)?)?,
        filter,
        source_cursor,
        heartbeat_interval_ms: row.try_get("heartbeat_interval_ms").map_err(store_error)?,
        next_heartbeat_at_ms: row.try_get("next_heartbeat_at_ms").map_err(store_error)?,
        created_at_ms: row.try_get("created_at_ms").map_err(store_error)?,
        updated_at_ms: row.try_get("updated_at_ms").map_err(store_error)?,
    })
}

pub(super) fn event_metadata_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<Option<EventMetadata>, StoreError> {
    let Some(id) = row
        .try_get::<Option<String>, _>("event_id")
        .map_err(store_error)?
    else {
        return Ok(None);
    };
    let sequence = row
        .try_get::<Option<String>, _>("event_sequence")
        .map_err(store_error)?
        .ok_or(StoreError::InvalidData)?
        .parse()
        .map_err(|_| StoreError::InvalidData)?;
    let event_count = row.try_get::<i64, _>("event_count").map_err(store_error)?;
    Ok(Some(EventMetadata {
        id,
        source: row
            .try_get::<Option<String>, _>("event_source")
            .map_err(store_error)?
            .ok_or(StoreError::InvalidData)?,
        event_type: row
            .try_get::<Option<String>, _>("event_type")
            .map_err(store_error)?
            .ok_or(StoreError::InvalidData)?,
        cursor: SourceCursor {
            sequence,
            value: row.try_get("event_cursor").map_err(store_error)?,
        },
        labels: decode(
            row.try_get::<Option<String>, _>("event_labels_json")
                .map_err(store_error)?
                .ok_or(StoreError::InvalidData)?,
        )?,
        occurred_at_ms: row
            .try_get::<Option<i64>, _>("event_occurred_at_ms")
            .map_err(store_error)?
            .ok_or(StoreError::InvalidData)?,
        coalesced_event_count: u32::try_from(event_count).map_err(|_| StoreError::InvalidData)?,
    }))
}

pub(super) fn next_heartbeat_after(due_at_ms: i64, interval_ms: i64, now_ms: i64) -> i64 {
    let elapsed = now_ms.saturating_sub(due_at_ms);
    let intervals = elapsed
        .checked_div(interval_ms)
        .unwrap_or_default()
        .saturating_add(1);
    due_at_ms.saturating_add(intervals.saturating_mul(interval_ms))
}

pub(super) fn parse_uuid(value: String) -> Result<Uuid, StoreError> {
    Uuid::parse_str(&value).map_err(|_| StoreError::InvalidData)
}

pub(super) fn parse_thread_id(value: String) -> Result<ThreadId, StoreError> {
    ThreadId::try_from(value).map_err(|_| StoreError::InvalidData)
}

pub(super) fn encode(value: &impl serde::Serialize) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|_| StoreError::InvalidData)
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(value: String) -> Result<T, StoreError> {
    serde_json::from_str(&value).map_err(|_| StoreError::InvalidData)
}

pub(super) fn store_error(error: sqlx::Error) -> StoreError {
    StoreError::Unavailable(error.to_string())
}

pub(super) fn sorted_thread_ids(thread_ids: HashSet<ThreadId>) -> Vec<ThreadId> {
    let mut thread_ids = thread_ids.into_iter().collect::<Vec<_>>();
    thread_ids.sort_by_key(ToString::to_string);
    thread_ids
}

pub(super) fn wake_reasons(event: bool, heartbeat: bool, manual: bool) -> BTreeSet<WakeReason> {
    let mut reasons = BTreeSet::new();
    if event {
        reasons.insert(WakeReason::Event);
    }
    if heartbeat {
        reasons.insert(WakeReason::Heartbeat);
    }
    if manual {
        reasons.insert(WakeReason::Manual);
    }
    reasons
}
