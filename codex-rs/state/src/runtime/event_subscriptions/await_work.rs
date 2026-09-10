use super::SqliteEventSubscriptionStore;
use super::storage::decode;
use super::storage::next_revision;
use super::storage::store_error;
use super::storage::upsert_event_wake;
use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::StoreError;
use codex_event_subscriptions::WakeItem;
use codex_protocol::ThreadId;
use sqlx::Row;
use uuid::Uuid;

impl SqliteEventSubscriptionStore {
    pub async fn coordinator_subscriptions(
        &self,
    ) -> Result<Vec<codex_event_subscriptions::Subscription>, StoreError> {
        let rows=sqlx::query("SELECT * FROM event_subscriptions WHERE source = 'devcoordinator' ORDER BY created_at_ms, id LIMIT 4096")
            .fetch_all(self.pool.as_ref()).await.map_err(store_error)?;
        rows.into_iter()
            .map(super::storage::subscription_from_row)
            .collect()
    }

    pub async fn await_source_change(&self) {
        self.source_changed.notified().await;
    }

    pub async fn replay_subscription_events(
        &self,
        subscription_id: Uuid,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let subscription = sqlx::query("SELECT source, event_types_json, label_filters_json, cursor_sequence FROM event_subscriptions WHERE id = ?")
            .bind(subscription_id.to_string()).fetch_one(&mut *transaction).await.map_err(store_error)?;
        let Some(source) = subscription
            .try_get::<Option<String>, _>("source")
            .map_err(store_error)?
        else {
            return Ok(());
        };
        let Some(cursor) = subscription
            .try_get::<Option<String>, _>("cursor_sequence")
            .map_err(store_error)?
        else {
            return Ok(());
        };
        let cursor = cursor.parse::<u64>().map_err(|_| StoreError::InvalidData)?;
        let filter = EventFilter {
            source: source.clone(),
            event_types: decode(
                subscription
                    .try_get("event_types_json")
                    .map_err(store_error)?,
            )?,
            labels: decode(
                subscription
                    .try_get("label_filters_json")
                    .map_err(store_error)?,
            )?,
        };
        let events = sqlx::query_scalar::<_,String>("SELECT event_json FROM project_event_observations WHERE source = ? ORDER BY ordinal LIMIT 4096")
            .bind(source).fetch_all(&mut *transaction).await.map_err(store_error)?;
        if cursor > 0 && events.is_empty() && filter.source != "devcoordinator" {
            return Err(StoreError::Unavailable("event cursor history is unavailable; refresh the publisher's status before waiting".into()));
        }
        let mut newest = cursor;
        for event in events {
            let event: PublishedEvent =
                serde_json::from_str(&event).map_err(|_| StoreError::InvalidData)?;
            if event.cursor.sequence > newest && filter.matches(&event) {
                let revision = next_revision(&mut transaction).await?;
                upsert_event_wake(&mut transaction, subscription_id, revision, &event, now_ms)
                    .await?;
                newest = event.cursor.sequence;
            }
        }
        if newest > cursor {
            sqlx::query("UPDATE event_subscriptions SET cursor_sequence = ?, updated_at_ms = ? WHERE id = ?")
                .bind(newest.to_string()).bind(now_ms).bind(subscription_id.to_string()).execute(&mut *transaction).await.map_err(store_error)?;
        }
        transaction.commit().await.map_err(store_error)?;
        self.event_changed.notify_waiters();
        Ok(())
    }

    pub async fn await_subscription(
        &self,
        thread_id: ThreadId,
        subscription_id: Uuid,
    ) -> Result<WakeItem, StoreError> {
        loop {
            let changed = self.event_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(pending) = self.pending_wake(thread_id).await?
                && let Some(item) = pending
                    .wake
                    .items
                    .into_iter()
                    .find(|item| item.subscription_id == subscription_id)
            {
                return Ok(item);
            }
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM event_subscriptions WHERE id = ? AND thread_id = ?)",
            )
            .bind(subscription_id.to_string())
            .bind(thread_id.to_string())
            .fetch_one(self.pool.as_ref())
            .await
            .map_err(store_error)?;
            if !exists {
                return Err(StoreError::Unavailable("event wait was cancelled".into()));
            }
            changed.await;
        }
    }
}
