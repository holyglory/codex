use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::ListSubscriptionsQuery;
use codex_event_subscriptions::MAX_SUBSCRIPTIONS_PER_THREAD;
use codex_event_subscriptions::MAX_TOTAL_SUBSCRIPTIONS;
use codex_event_subscriptions::NewSubscription;
use codex_event_subscriptions::PendingWakeBatch;
use codex_event_subscriptions::PublishEventOutcome;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::StoreError;
use codex_event_subscriptions::Subscription;
use codex_event_subscriptions::SubscriptionPage;
use codex_event_subscriptions::TriggerOutcome;
use codex_event_subscriptions::WakeBatch;
use codex_event_subscriptions::WakeItem;
use codex_event_subscriptions::WakeReason;
use codex_protocol::ThreadId;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use uuid::Uuid;

mod storage;
use storage::*;

#[derive(Clone)]
pub struct SqliteEventSubscriptionStore {
    pool: Arc<SqlitePool>,
}

impl SqliteEventSubscriptionStore {
    pub(crate) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    pub(crate) async fn delete_thread(&self, thread_id: ThreadId) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM event_subscription_pending_wakes
             WHERE subscription_id IN (
                 SELECT id FROM event_subscriptions WHERE thread_id = ?
             )",
        )
        .bind(thread_id.to_string())
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query("DELETE FROM event_subscriptions WHERE thread_id = ?")
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected()
            != 0;
        tx.commit().await?;
        Ok(deleted)
    }
}

impl EventSubscriptionStore for SqliteEventSubscriptionStore {
    async fn create(
        &self,
        subscription: NewSubscription,
        now_ms: i64,
    ) -> Result<Subscription, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let total = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM event_subscriptions")
            .fetch_one(&mut *tx)
            .await
            .map_err(store_error)?;
        if total >= i64::try_from(MAX_TOTAL_SUBSCRIPTIONS).unwrap_or(i64::MAX) {
            return Err(StoreError::TotalCapacity);
        }
        let thread_total = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM event_subscriptions WHERE thread_id = ?",
        )
        .bind(subscription.thread_id.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_error)?;
        if thread_total >= i64::try_from(MAX_SUBSCRIPTIONS_PER_THREAD).unwrap_or(i64::MAX) {
            return Err(StoreError::ThreadCapacity);
        }

        let id = Uuid::now_v7();
        let (source, event_types_json, label_filters_json) = match &subscription.filter {
            Some(filter) => (
                Some(filter.source.as_str()),
                encode(&filter.event_types)?,
                encode(&filter.labels)?,
            ),
            None => (None, "[]".to_string(), "{}".to_string()),
        };
        let cursor_sequence = subscription
            .source_cursor
            .as_ref()
            .map(|cursor| cursor.sequence.to_string());
        let cursor_value = subscription
            .source_cursor
            .as_ref()
            .and_then(|cursor| cursor.value.as_deref());
        let heartbeat_interval_ms = subscription
            .heartbeat
            .as_ref()
            .map(|heartbeat| heartbeat.interval_ms);
        let next_heartbeat_at_ms = subscription
            .heartbeat
            .as_ref()
            .map(|heartbeat| heartbeat.first_deadline(now_ms));
        sqlx::query(
            "INSERT INTO event_subscriptions (
                id, thread_id, source, event_types_json, label_filters_json,
                cursor_sequence, cursor_value, heartbeat_interval_ms,
                next_heartbeat_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(subscription.thread_id.to_string())
        .bind(source)
        .bind(event_types_json)
        .bind(label_filters_json)
        .bind(cursor_sequence)
        .bind(cursor_value)
        .bind(heartbeat_interval_ms)
        .bind(next_heartbeat_at_ms)
        .bind(now_ms)
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(Subscription {
            id,
            thread_id: subscription.thread_id,
            filter: subscription.filter,
            source_cursor: subscription.source_cursor,
            heartbeat_interval_ms,
            next_heartbeat_at_ms,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        })
    }

    async fn list(&self, query: ListSubscriptionsQuery) -> Result<SubscriptionPage, StoreError> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT id, thread_id, source, event_types_json, label_filters_json,
                    cursor_sequence, cursor_value, heartbeat_interval_ms,
                    next_heartbeat_at_ms, created_at_ms, updated_at_ms
             FROM event_subscriptions",
        );
        if let Some(thread_id) = query.thread_id {
            builder
                .push(" WHERE thread_id = ")
                .push_bind(thread_id.to_string());
        }
        builder
            .push(" ORDER BY created_at_ms, id LIMIT ")
            .push_bind(i64::try_from(query.limit.saturating_add(1)).unwrap_or(i64::MAX))
            .push(" OFFSET ")
            .push_bind(i64::try_from(query.offset).unwrap_or(i64::MAX));
        let rows = builder
            .build()
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(store_error)?;
        let mut data = rows
            .into_iter()
            .map(subscription_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let next_offset = (data.len() > query.limit).then(|| {
            data.truncate(query.limit);
            query.offset.saturating_add(query.limit)
        });
        Ok(SubscriptionPage { data, next_offset })
    }

    async fn cancel(&self, id: Uuid) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        sqlx::query("DELETE FROM event_subscription_pending_wakes WHERE subscription_id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        let cancelled = sqlx::query("DELETE FROM event_subscriptions WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(store_error)?
            .rows_affected()
            != 0;
        tx.commit().await.map_err(store_error)?;
        Ok(cancelled)
    }

    async fn publish(
        &self,
        event: PublishedEvent,
        now_ms: i64,
    ) -> Result<PublishEventOutcome, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let rows = sqlx::query(
            "SELECT id, thread_id, event_types_json, label_filters_json, cursor_sequence
             FROM event_subscriptions WHERE source = ? ORDER BY created_at_ms, id",
        )
        .bind(&event.source)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        let mut accepted = Vec::new();
        let mut ignored = Vec::new();
        let mut affected = HashSet::new();
        for row in rows {
            let id = parse_uuid(row.try_get("id").map_err(store_error)?)?;
            let thread_id = parse_thread_id(row.try_get("thread_id").map_err(store_error)?)?;
            let filter = EventFilter {
                source: event.source.clone(),
                event_types: decode(row.try_get("event_types_json").map_err(store_error)?)?,
                labels: decode(row.try_get("label_filters_json").map_err(store_error)?)?,
            };
            if !filter.matches(&event) {
                continue;
            }
            let previous = row
                .try_get::<Option<String>, _>("cursor_sequence")
                .map_err(store_error)?
                .map(|value| value.parse::<u64>().map_err(|_| StoreError::InvalidData))
                .transpose()?;
            if previous.is_some_and(|sequence| event.cursor.sequence <= sequence) {
                ignored.push(id);
                continue;
            }
            sqlx::query(
                "UPDATE event_subscriptions
                 SET cursor_sequence = ?, cursor_value = ?, updated_at_ms = ?
                 WHERE id = ?",
            )
            .bind(event.cursor.sequence.to_string())
            .bind(event.cursor.value.as_deref())
            .bind(now_ms)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
            let revision = next_revision(&mut tx).await?;
            upsert_event_wake(&mut tx, id, revision, &event, now_ms).await?;
            accepted.push(id);
            affected.insert(thread_id);
        }
        tx.commit().await.map_err(store_error)?;
        Ok(PublishEventOutcome {
            accepted_subscription_ids: accepted,
            ignored_subscription_ids: ignored,
            affected_thread_ids: sorted_thread_ids(affected),
        })
    }

    async fn trigger(
        &self,
        subscription_ids: Vec<Uuid>,
        now_ms: i64,
    ) -> Result<TriggerOutcome, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT id, thread_id FROM event_subscriptions WHERE id IN (",
        );
        let mut separated = builder.separated(", ");
        for id in &subscription_ids {
            separated.push_bind(id.to_string());
        }
        separated.push_unseparated(") ORDER BY id");
        let rows = builder
            .build()
            .fetch_all(&mut *tx)
            .await
            .map_err(store_error)?;
        let mut found = BTreeMap::new();
        let mut affected = HashSet::new();
        for row in rows {
            let id = parse_uuid(row.try_get("id").map_err(store_error)?)?;
            let thread_id = parse_thread_id(row.try_get("thread_id").map_err(store_error)?)?;
            found.insert(id, thread_id);
        }
        for (id, thread_id) in &found {
            let revision = next_revision(&mut tx).await?;
            upsert_reason_wake(
                &mut tx,
                *id,
                revision,
                WakeReason::Manual,
                /*heartbeat_due_at_ms*/ None,
                now_ms,
            )
            .await?;
            affected.insert(*thread_id);
        }
        tx.commit().await.map_err(store_error)?;
        Ok(TriggerOutcome {
            triggered_subscription_ids: found.keys().copied().collect(),
            missing_subscription_ids: subscription_ids
                .into_iter()
                .filter(|id| !found.contains_key(id))
                .collect(),
            affected_thread_ids: sorted_thread_ids(affected),
        })
    }

    async fn collect_due_heartbeats(&self, now_ms: i64) -> Result<Vec<ThreadId>, StoreError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let rows = sqlx::query(
            "SELECT id, thread_id, heartbeat_interval_ms, next_heartbeat_at_ms
             FROM event_subscriptions
             WHERE next_heartbeat_at_ms IS NOT NULL AND next_heartbeat_at_ms <= ?
             ORDER BY next_heartbeat_at_ms, id",
        )
        .bind(now_ms)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_error)?;
        let mut affected = HashSet::new();
        for row in rows {
            let id = parse_uuid(row.try_get("id").map_err(store_error)?)?;
            let thread_id = parse_thread_id(row.try_get("thread_id").map_err(store_error)?)?;
            let interval_ms = row
                .try_get::<i64, _>("heartbeat_interval_ms")
                .map_err(store_error)?;
            let due_at_ms = row
                .try_get::<i64, _>("next_heartbeat_at_ms")
                .map_err(store_error)?;
            let next_at_ms = next_heartbeat_after(due_at_ms, interval_ms, now_ms);
            sqlx::query(
                "UPDATE event_subscriptions
                 SET next_heartbeat_at_ms = ?, updated_at_ms = ? WHERE id = ?",
            )
            .bind(next_at_ms)
            .bind(now_ms)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
            let revision = next_revision(&mut tx).await?;
            upsert_reason_wake(
                &mut tx,
                id,
                revision,
                WakeReason::Heartbeat,
                Some(due_at_ms),
                now_ms,
            )
            .await?;
            affected.insert(thread_id);
        }
        tx.commit().await.map_err(store_error)?;
        Ok(sorted_thread_ids(affected))
    }

    async fn next_heartbeat_deadline(&self) -> Result<Option<i64>, StoreError> {
        sqlx::query_scalar("SELECT MIN(next_heartbeat_at_ms) FROM event_subscriptions")
            .fetch_one(self.pool.as_ref())
            .await
            .map_err(store_error)
    }

    async fn pending_thread_ids(&self) -> Result<Vec<ThreadId>, StoreError> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT subscriptions.thread_id
             FROM event_subscription_pending_wakes AS pending
             JOIN event_subscriptions AS subscriptions ON subscriptions.id = pending.subscription_id
             ORDER BY subscriptions.thread_id",
        )
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(store_error)?;
        rows.into_iter().map(parse_thread_id).collect()
    }

    async fn pending_wake(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<PendingWakeBatch>, StoreError> {
        let rows = sqlx::query(
            "SELECT pending.subscription_id, pending.revision,
                    pending.event_pending, pending.heartbeat_pending, pending.manual_pending,
                    pending.event_id, pending.event_source, pending.event_type,
                    pending.event_sequence, pending.event_cursor, pending.event_labels_json,
                    pending.event_occurred_at_ms, pending.event_count,
                    pending.heartbeat_due_at_ms
             FROM event_subscription_pending_wakes AS pending
             JOIN event_subscriptions AS subscriptions ON subscriptions.id = pending.subscription_id
             WHERE subscriptions.thread_id = ?
             ORDER BY pending.subscription_id",
        )
        .bind(thread_id.to_string())
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(store_error)?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut through_revision = 0;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let revision = row.try_get::<i64, _>("revision").map_err(store_error)?;
            through_revision = through_revision.max(revision);
            let reasons = wake_reasons(
                row.try_get("event_pending").map_err(store_error)?,
                row.try_get("heartbeat_pending").map_err(store_error)?,
                row.try_get("manual_pending").map_err(store_error)?,
            );
            let event = event_metadata_from_row(&row)?;
            items.push(WakeItem {
                subscription_id: parse_uuid(row.try_get("subscription_id").map_err(store_error)?)?,
                reasons,
                event,
                heartbeat_due_at_ms: row.try_get("heartbeat_due_at_ms").map_err(store_error)?,
            });
        }
        Ok(Some(PendingWakeBatch {
            wake: WakeBatch { thread_id, items },
            through_revision,
        }))
    }

    async fn acknowledge_wake(
        &self,
        thread_id: ThreadId,
        through_revision: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "DELETE FROM event_subscription_pending_wakes
             WHERE revision <= ? AND subscription_id IN (
                 SELECT id FROM event_subscriptions WHERE thread_id = ?
             )",
        )
        .bind(through_revision)
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await
        .map_err(store_error)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "event_subscriptions_tests.rs"]
mod tests;
