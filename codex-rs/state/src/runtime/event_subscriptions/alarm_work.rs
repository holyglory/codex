use super::SqliteEventSubscriptionStore;
use super::storage::store_error;
use codex_event_subscriptions::AlarmSpec;
use codex_event_subscriptions::AlarmWorkEvent;
use codex_event_subscriptions::EventSubscriptionStore;
use codex_event_subscriptions::OperationResult;
use codex_event_subscriptions::StoreError;
use codex_protocol::ThreadId;
use sqlx::Row;

impl SqliteEventSubscriptionStore {
    pub async fn observe_alarm_work(
        &self,
        operation_id: &str,
        event: AlarmWorkEvent,
        now: i64,
    ) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let thread = match &event {
            AlarmWorkEvent::Start { thread_id, .. } => thread_id.to_string(),
            AlarmWorkEvent::Wait | AlarmWorkEvent::Resume | AlarmWorkEvent::Finish => {
                let thread: Option<String> = sqlx::query_scalar(
                    "SELECT thread_id FROM alarm_work_operations WHERE operation_id=?",
                )
                .bind(operation_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_error)?;
                let Some(thread) = thread else {
                    return Ok(());
                };
                thread
            }
        };
        sqlx::query("INSERT INTO alarm_work(thread_id,observed_at_ms) VALUES(?,?) ON CONFLICT(thread_id) DO UPDATE SET accumulated_ms=accumulated_ms+CASE WHEN active_count>0 THEN MAX(0,excluded.observed_at_ms-observed_at_ms) ELSE 0 END,observed_at_ms=MAX(observed_at_ms,excluded.observed_at_ms)")
            .bind(&thread).bind(now).execute(&mut *tx).await.map_err(store_error)?;
        match event {
            AlarmWorkEvent::Start { eligible, .. } => {
                sqlx::query("INSERT INTO alarm_work_operations(operation_id,thread_id,eligible) VALUES(?,?,?) ON CONFLICT DO NOTHING").bind(operation_id).bind(&thread).bind(eligible).execute(&mut *tx).await.map_err(store_error)?;
            }
            AlarmWorkEvent::Wait => {
                sqlx::query("UPDATE alarm_work_operations SET waits=waits+1 WHERE operation_id=?")
                    .bind(operation_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_error)?;
            }
            AlarmWorkEvent::Resume => {
                sqlx::query(
                    "UPDATE alarm_work_operations SET waits=MAX(0,waits-1) WHERE operation_id=?",
                )
                .bind(operation_id)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
            }
            AlarmWorkEvent::Finish => {
                sqlx::query("DELETE FROM alarm_work_operations WHERE operation_id=?")
                    .bind(operation_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_error)?;
            }
        }
        sqlx::query("UPDATE alarm_work SET active_count=(SELECT COUNT(*) FROM alarm_work_operations WHERE thread_id=? AND eligible=1 AND waits=0) WHERE thread_id=?").bind(&thread).bind(&thread).execute(&mut *tx).await.map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        self.deadline_changed.notify_one();
        Ok(())
    }

    pub async fn observe_alarm_result(
        &self,
        thread: ThreadId,
        result: &OperationResult,
        now: i64,
    ) -> Result<(), StoreError> {
        result.validate().map_err(|_| StoreError::InvalidData)?;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let encoded = serde_json::to_string(result).map_err(|_| StoreError::InvalidData)?;
        sqlx::query("INSERT INTO alarm_operation_results(operation_id,thread_id,result_json,observed_at_ms) VALUES(?,?,?,?) ON CONFLICT DO NOTHING")
            .bind(&result.operation_id).bind(thread.to_string()).bind(&encoded).bind(now).execute(&mut *tx).await.map_err(store_error)?;
        let stored: String = sqlx::query_scalar(
            "SELECT result_json FROM alarm_operation_results WHERE operation_id=? AND thread_id=?",
        )
        .bind(&result.operation_id)
        .bind(thread.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_error)?
        .ok_or(StoreError::InvalidData)?;
        if stored != encoded {
            return Err(StoreError::InvalidData);
        }
        let revision = sqlx::query_scalar::<_, i64>(
            "SELECT rowid FROM alarm_operation_results WHERE thread_id=? AND operation_id=?",
        )
        .bind(thread.to_string())
        .bind(&result.operation_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_error)?;
        let rows =
            sqlx::query("SELECT id,spec_json FROM alarms WHERE thread_id=? AND state='armed'")
                .bind(thread.to_string())
                .fetch_all(&mut *tx)
                .await
                .map_err(store_error)?;
        for row in rows {
            let spec: AlarmSpec =
                serde_json::from_str(row.try_get("spec_json").map_err(store_error)?)
                    .map_err(|_| StoreError::InvalidData)?;
            if spec
                .operation_result
                .as_ref()
                .is_some_and(|trigger| trigger.matches(result))
            {
                sqlx::query("UPDATE alarms SET due_at_ms=? WHERE id=? AND due_at_ms IS NULL")
                    .bind(now)
                    .bind(row.try_get::<String, _>("id").map_err(store_error)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_error)?;
            }
        }
        sqlx::query("DELETE FROM alarm_operation_results WHERE sequence NOT IN (SELECT sequence FROM alarm_operation_results ORDER BY sequence DESC LIMIT 4096)").execute(&mut *tx).await.map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        let mut labels = std::collections::BTreeMap::from([
            ("operation_id".into(), result.operation_id.clone()),
            ("tool_name".into(), result.tool_name.clone()),
            (
                "outcome_class".into(),
                serde_json::to_value(&result.outcome_class)
                    .map_err(|_| StoreError::InvalidData)?
                    .as_str()
                    .ok_or(StoreError::InvalidData)?
                    .to_owned(),
            ),
        ]);
        if let Some(code) = &result.result_code {
            labels.insert("result_code".into(), code.clone());
        }
        self.publish(
            codex_event_subscriptions::PublishedEvent {
                id: result.operation_id.clone(),
                source: "codex.operation".into(),
                event_type: "tool_finished".into(),
                labels,
                cursor: codex_event_subscriptions::SourceCursor {
                    sequence: revision as u64,
                    value: None,
                },
                occurred_at_ms: now,
            },
            now,
        )
        .await?;
        self.deadline_changed.notify_one();
        Ok(())
    }
}
