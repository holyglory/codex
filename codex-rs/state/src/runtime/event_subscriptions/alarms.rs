use super::SqliteEventSubscriptionStore;
use super::storage::next_revision;
use super::storage::parse_thread_id;
use super::storage::parse_uuid;
use super::storage::store_error;
use super::storage::upsert_event_wake;
use codex_event_subscriptions::Alarm;
use codex_event_subscriptions::AlarmPage;
use codex_event_subscriptions::AlarmSpec;
use codex_event_subscriptions::AlarmState;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::StoreError;
use codex_protocol::ThreadId;
use sqlx::Row;
use std::collections::BTreeMap;
use uuid::Uuid;

impl SqliteEventSubscriptionStore {
    pub async fn set_alarm(
        &self,
        thread: ThreadId,
        spec: AlarmSpec,
        now: i64,
    ) -> Result<Alarm, StoreError> {
        spec.validate(now).map_err(|_| StoreError::InvalidData)?;
        let encoded = serde_json::to_string(&spec).map_err(|_| StoreError::InvalidData)?;
        let scope = spec.scope_key();
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        if let Some(row) =
            sqlx::query("SELECT * FROM alarms WHERE thread_id=? AND scope_key=? AND dedupe_key=?")
                .bind(thread.to_string())
                .bind(&scope)
                .bind(&spec.dedupe_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_error)?
        {
            let existing = alarm_from_row(row)?;
            return Ok(existing);
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM event_subscriptions WHERE thread_id=?")
                .bind(thread.to_string())
                .fetch_one(&mut *tx)
                .await
                .map_err(store_error)?;
        if count >= codex_event_subscriptions::MAX_SUBSCRIPTIONS_PER_THREAD as i64 {
            return Err(StoreError::ThreadCapacity);
        }
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event_subscriptions")
            .fetch_one(&mut *tx)
            .await
            .map_err(store_error)?;
        if total >= codex_event_subscriptions::MAX_TOTAL_SUBSCRIPTIONS as i64 {
            return Err(StoreError::TotalCapacity);
        }
        let work: i64 = sqlx::query_scalar("SELECT accumulated_ms + CASE WHEN active_count>0 THEN MAX(0,?-observed_at_ms) ELSE 0 END FROM alarm_work WHERE thread_id=?")
            .bind(now).bind(thread.to_string()).fetch_optional(&mut *tx).await.map_err(store_error)?.unwrap_or(0);
        let target = spec
            .active_work_ms
            .map(|ms| work.checked_add(ms).ok_or(StoreError::InvalidData))
            .transpose()?;
        let mut at = spec
            .absolute_at_ms
            .or_else(|| spec.relative_ms.and_then(|ms| now.checked_add(ms)));
        if let Some(trigger) = &spec.operation_result
            && let Some(encoded) = sqlx::query_scalar::<_, String>("SELECT result_json FROM alarm_operation_results WHERE operation_id=? AND thread_id=?")
                .bind(&trigger.operation_id).bind(thread.to_string()).fetch_optional(&mut *tx).await.map_err(store_error)? {
                let result = serde_json::from_str(&encoded).map_err(|_| StoreError::InvalidData)?;
                if trigger.matches(&result) { at = Some(now); }
            }
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO event_subscriptions(id,thread_id,source,event_types_json,label_filters_json,created_at_ms,updated_at_ms) VALUES(?,?,'codex.alarm','[\"alarm.due\"]',?, ?,?)")
            .bind(id.to_string()).bind(thread.to_string()).bind(serde_json::json!({"alarm_id":id}).to_string()).bind(now).bind(now)
            .execute(&mut *tx).await.map_err(store_error)?;
        sqlx::query("INSERT INTO alarms(id,thread_id,scope_key,dedupe_key,spec_json,state,created_at_ms,due_at_ms,active_target_ms,expires_at_ms) VALUES(?,?,?,?,?,'armed',?,?,?,?)")
            .bind(id.to_string()).bind(thread.to_string()).bind(scope).bind(&spec.dedupe_key).bind(encoded).bind(now).bind(at).bind(target).bind(spec.expires_at_ms)
            .execute(&mut *tx).await.map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        self.deadline_changed.notify_one();
        Ok(Alarm {
            id,
            thread_id: thread,
            spec,
            state: AlarmState::Armed,
            created_at_ms: now,
            due_at_ms: at,
            delivered_at_ms: None,
            acknowledged_at_ms: None,
        })
    }

    pub async fn alarm_status(
        &self,
        thread: ThreadId,
        id: Uuid,
    ) -> Result<Option<Alarm>, StoreError> {
        sqlx::query("SELECT * FROM alarms WHERE thread_id=? AND id=?")
            .bind(thread.to_string())
            .bind(id.to_string())
            .fetch_optional(self.pool.as_ref())
            .await
            .map_err(store_error)?
            .map(alarm_from_row)
            .transpose()
    }

    pub async fn list_alarms(
        &self,
        thread: ThreadId,
        offset: usize,
        limit: usize,
    ) -> Result<AlarmPage, StoreError> {
        if !(1..=10).contains(&limit) || offset > 100_000 {
            return Err(StoreError::InvalidData);
        }
        let rows = sqlx::query(
            "SELECT * FROM alarms WHERE thread_id=? ORDER BY created_at_ms,id LIMIT ? OFFSET ?",
        )
        .bind(thread.to_string())
        .bind((limit + 1) as i64)
        .bind(offset as i64)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(store_error)?;
        let more = rows.len() > limit;
        Ok(AlarmPage {
            data: rows
                .into_iter()
                .take(limit)
                .map(alarm_from_row)
                .collect::<Result<_, _>>()?,
            next_offset: more.then_some(offset + limit),
        })
    }

    pub async fn acknowledge_alarm(
        &self,
        thread: ThreadId,
        id: Uuid,
        now: i64,
    ) -> Result<Alarm, StoreError> {
        self.finish_alarm(thread, id, AlarmState::Acknowledged, now)
            .await
    }

    pub async fn cancel_alarm(
        &self,
        thread: ThreadId,
        id: Uuid,
        now: i64,
    ) -> Result<Alarm, StoreError> {
        self.finish_alarm(thread, id, AlarmState::Cancelled, now)
            .await
    }

    async fn finish_alarm(
        &self,
        thread: ThreadId,
        id: Uuid,
        state: AlarmState,
        now: i64,
    ) -> Result<Alarm, StoreError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let row = sqlx::query("SELECT * FROM alarms WHERE thread_id=? AND id=?")
            .bind(thread.to_string())
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_error)?
            .ok_or(StoreError::InvalidData)?;
        let mut alarm = alarm_from_row(row)?;
        if alarm.state == state {
            return Ok(alarm);
        }
        if matches!(
            alarm.state,
            AlarmState::Acknowledged | AlarmState::Cancelled | AlarmState::Expired
        ) || state == AlarmState::Acknowledged && alarm.state != AlarmState::Delivered
        {
            return Err(StoreError::InvalidData);
        }
        let status = if state == AlarmState::Acknowledged {
            "acknowledged"
        } else {
            "cancelled"
        };
        sqlx::query("UPDATE alarms SET state=?,acknowledged_at_ms=? WHERE id=?")
            .bind(status)
            .bind((state == AlarmState::Acknowledged).then_some(now))
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        sqlx::query("DELETE FROM event_subscriptions WHERE id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        alarm.state = state;
        alarm.acknowledged_at_ms = (alarm.state == AlarmState::Acknowledged).then_some(now);
        self.deadline_changed.notify_one();
        Ok(alarm)
    }

    pub(super) async fn collect_alarm_deadlines(
        &self,
        now: i64,
    ) -> Result<Vec<ThreadId>, StoreError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        sqlx::query("DELETE FROM event_subscriptions WHERE id IN (SELECT id FROM alarms WHERE state IN ('armed','due','delivered') AND expires_at_ms<=?)")
            .bind(now).execute(&mut *tx).await.map_err(store_error)?;
        sqlx::query("UPDATE alarms SET state='expired' WHERE state IN ('armed','due','delivered') AND expires_at_ms<=?")
            .bind(now).execute(&mut *tx).await.map_err(store_error)?;
        let rows=sqlx::query("SELECT a.* FROM alarms a LEFT JOIN alarm_work w ON w.thread_id=a.thread_id WHERE a.state='armed' AND (a.due_at_ms<=? OR a.active_target_ms<=COALESCE(w.accumulated_ms,0)+CASE WHEN w.active_count>0 THEN MAX(0,?-w.observed_at_ms) ELSE 0 END)")
            .bind(now).bind(now).fetch_all(&mut *tx).await.map_err(store_error)?;
        let mut affected = Vec::new();
        for row in rows {
            let alarm = alarm_from_row(row)?;
            let revision = next_revision(&mut tx).await?;
            let event = PublishedEvent {
                id: alarm.id.to_string(),
                source: "codex.alarm".into(),
                event_type: "alarm.due".into(),
                cursor: SourceCursor {
                    sequence: revision as u64,
                    value: None,
                },
                labels: BTreeMap::from([("alarm_id".into(), alarm.id.to_string())]),
                occurred_at_ms: now,
            };
            upsert_event_wake(&mut tx, alarm.id, revision, &event, now).await?;
            sqlx::query("UPDATE alarms SET state='due',due_at_ms=COALESCE(due_at_ms,?) WHERE id=?")
                .bind(now)
                .bind(alarm.id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
            affected.push(alarm.thread_id);
        }
        tx.commit().await.map_err(store_error)?;
        Ok(affected)
    }

    pub(super) async fn restore_alarm_runtime(&self) -> Result<(), StoreError> {
        // Never count daemon downtime or infer work after a crash. Settled time is retained.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        sqlx::query("DELETE FROM alarm_work_operations")
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        sqlx::query("UPDATE alarm_work SET active_count=0")
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        sqlx::query(
            "DELETE FROM event_subscription_pending_wakes WHERE event_source='codex.project'",
        )
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        sqlx::query("UPDATE project_automations SET next_deadline_at_ms=NULL")
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

fn alarm_from_row(row: sqlx::sqlite::SqliteRow) -> Result<Alarm, StoreError> {
    let spec: String = row.try_get("spec_json").map_err(store_error)?;
    let state: String = row.try_get("state").map_err(store_error)?;
    Ok(Alarm {
        id: parse_uuid(row.try_get("id").map_err(store_error)?)?,
        thread_id: parse_thread_id(row.try_get("thread_id").map_err(store_error)?)?,
        spec: serde_json::from_str(&spec).map_err(|_| StoreError::InvalidData)?,
        state: serde_json::from_value(serde_json::Value::String(state))
            .map_err(|_| StoreError::InvalidData)?,
        created_at_ms: row.try_get("created_at_ms").map_err(store_error)?,
        due_at_ms: row.try_get("due_at_ms").map_err(store_error)?,
        delivered_at_ms: row.try_get("delivered_at_ms").map_err(store_error)?,
        acknowledged_at_ms: row.try_get("acknowledged_at_ms").map_err(store_error)?,
    })
}
