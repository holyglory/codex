use super::SqliteEventSubscriptionStore;
use super::storage::*;
use codex_event_subscriptions::EventFilter;
use codex_event_subscriptions::PublishEventOutcome;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::StoreError;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use std::collections::HashSet;
use uuid::Uuid;

enum AttentionTarget {
    MatchingLabels,
    Subscription(Uuid),
}

impl SqliteEventSubscriptionStore {
    pub(super) async fn publish_source_attention(
        &self,
        event: PublishedEvent,
        now_ms: i64,
    ) -> Result<PublishEventOutcome, StoreError> {
        self.publish_attention(AttentionTarget::MatchingLabels, event, now_ms)
            .await
    }

    pub async fn publish_source_attention_to(
        &self,
        subscription_id: Uuid,
        event: PublishedEvent,
        now_ms: i64,
    ) -> Result<PublishEventOutcome, StoreError> {
        if event.source != "devcoordinator"
            || !matches!(
                event.event_type.as_str(),
                "source.unavailable" | "source.cursor_stale"
            )
        {
            return Err(StoreError::InvalidData);
        }
        event.validate().map_err(|_| StoreError::InvalidData)?;
        self.publish_attention(
            AttentionTarget::Subscription(subscription_id),
            event,
            now_ms,
        )
        .await
    }

    async fn publish_attention(
        &self,
        target: AttentionTarget,
        event: PublishedEvent,
        now_ms: i64,
    ) -> Result<PublishEventOutcome, StoreError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT id, thread_id, event_types_json, label_filters_json, cursor_sequence FROM event_subscriptions WHERE source = ",
        );
        query.push_bind(&event.source);
        if let AttentionTarget::Subscription(id) = target {
            query.push(" AND id = ").push_bind(id.to_string());
        }
        query.push(" ORDER BY created_at_ms, id");
        let rows = query
            .build()
            .fetch_all(&mut *transaction)
            .await
            .map_err(store_error)?;
        let mut accepted = Vec::new();
        let mut ignored = Vec::new();
        let mut affected = HashSet::new();
        for row in rows {
            let id = parse_uuid(row.try_get("id").map_err(store_error)?)?;
            let filter = EventFilter {
                source: event.source.clone(),
                event_types: decode(row.try_get("event_types_json").map_err(store_error)?)?,
                labels: decode(row.try_get("label_filters_json").map_err(store_error)?)?,
            };
            if !filter.matches(&event) {
                continue;
            }
            let has_result:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM event_subscription_pending_wakes WHERE subscription_id = ? AND event_pending = 1 AND event_type NOT IN ('source.unavailable','source.cursor_stale'))")
                .bind(id.to_string()).fetch_one(&mut *transaction).await.map_err(store_error)?;
            if has_result {
                ignored.push(id);
                continue;
            }
            let mut attention = event.clone();
            attention.cursor.sequence = row
                .try_get::<Option<String>, _>("cursor_sequence")
                .map_err(store_error)?
                .map(|value| value.parse::<u64>().map_err(|_| StoreError::InvalidData))
                .transpose()?
                .unwrap_or(0);
            attention.cursor.value = None;
            let revision = next_revision(&mut transaction).await?;
            upsert_event_wake(&mut transaction, id, revision, &attention, now_ms).await?;
            accepted.push(id);
            affected.insert(parse_thread_id(
                row.try_get("thread_id").map_err(store_error)?,
            )?);
        }
        transaction.commit().await.map_err(store_error)?;
        self.event_changed.notify_waiters();
        Ok(PublishEventOutcome {
            accepted_subscription_ids: accepted,
            ignored_subscription_ids: ignored,
            affected_thread_ids: sorted_thread_ids(affected),
        })
    }
}
