use super::*;

impl SqliteEventSubscriptionStore {
    pub async fn event_route_exists(
        &self,
        thread: ThreadId,
        filter: &EventFilter,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM event_subscriptions WHERE thread_id=? AND source=? AND event_types_json=? AND label_filters_json=?)")
            .bind(thread.to_string()).bind(&filter.source).bind(encode(&filter.event_types)?).bind(encode(&filter.labels)?)
            .fetch_one(self.pool.as_ref()).await.map_err(store_error)
    }

    pub fn set_alarm_delivery_available(&self, available: bool) {
        self.alarm_delivery_available
            .store(available, std::sync::atomic::Ordering::Release);
    }
    pub fn alarm_delivery_available(&self) -> bool {
        self.alarm_delivery_available
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Advance an external route only after its derived reminder is durably stored.
    pub async fn advance_event_route(&self, id: Uuid, sequence: u64) -> Result<(), StoreError> {
        sqlx::query("UPDATE event_subscriptions SET cursor_sequence=? WHERE id=? AND (cursor_sequence IS NULL OR CAST(cursor_sequence AS INTEGER)<?)")
            .bind(sequence.to_string()).bind(id.to_string()).bind(sequence as i64).execute(self.pool.as_ref()).await.map_err(store_error)?;
        Ok(())
    }

    /// Idempotently attach a content-free event route before advertising a capability.
    pub async fn ensure_event_route(
        &self,
        thread: ThreadId,
        filter: EventFilter,
        now: i64,
    ) -> Result<(), StoreError> {
        filter.validate().map_err(|_| StoreError::InvalidData)?;
        let events = encode(&filter.event_types)?;
        let labels = encode(&filter.labels)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM event_subscriptions WHERE thread_id=? AND source=? AND event_types_json=? AND label_filters_json=?)")
            .bind(thread.to_string()).bind(&filter.source).bind(&events).bind(&labels).fetch_one(&mut *tx).await.map_err(store_error)?;
        if exists {
            return Ok(());
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM event_subscriptions WHERE thread_id=?")
                .bind(thread.to_string())
                .fetch_one(&mut *tx)
                .await
                .map_err(store_error)?;
        if count >= MAX_SUBSCRIPTIONS_PER_THREAD as i64 {
            return Err(StoreError::ThreadCapacity);
        }
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_subscriptions")
            .fetch_one(&mut *tx)
            .await
            .map_err(store_error)?;
        if total >= MAX_TOTAL_SUBSCRIPTIONS as i64 {
            return Err(StoreError::TotalCapacity);
        }
        sqlx::query("INSERT INTO event_subscriptions(id,thread_id,source,event_types_json,label_filters_json,created_at_ms,updated_at_ms) VALUES(?,?,?,?,?,?,?)")
            .bind(Uuid::now_v7().to_string()).bind(thread.to_string()).bind(filter.source).bind(events).bind(labels).bind(now).bind(now).execute(&mut *tx).await.map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        self.source_changed.notify_one();
        Ok(())
    }
}
