use codex_event_subscriptions::ProjectAutomation;
use codex_event_subscriptions::ScopedWakePolicy;
use codex_event_subscriptions::StoreError;
use codex_event_subscriptions::ThreadWakePolicy;
use codex_event_subscriptions::WakeItem;
use codex_event_subscriptions::WakeLifecycle;
use codex_event_subscriptions::WakePolicyChange;
use codex_event_subscriptions::WakeScope;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::SqliteConnection;

use super::SqliteEventSubscriptionStore;
use super::storage::encode;
use super::storage::next_revision;
use super::storage::store_error;

impl SqliteEventSubscriptionStore {
    pub async fn project_review_background_allowed(
        &self,
        owner: ThreadId,
        project_id: &str,
    ) -> Result<bool, StoreError> {
        let subscription: String = sqlx::query_scalar(
            "SELECT subscription_id FROM project_automations WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_one(self.pool.as_ref())
        .await
        .map_err(store_error)?;
        let subscription_id =
            uuid::Uuid::parse_str(&subscription).map_err(|_| StoreError::InvalidData)?;
        let policy = self.read_wake_policy(owner).await?;
        Ok(policy.allows_scopes(&[
            WakeScope::ProjectReview {
                project_id: project_id.into(),
            },
            WakeScope::Subscription { subscription_id },
            WakeScope::Thread,
        ]))
    }

    pub async fn read_wake_policy(
        &self,
        thread_id: ThreadId,
    ) -> Result<ThreadWakePolicy, StoreError> {
        let mut connection = self.pool.acquire().await.map_err(store_error)?;
        read_policy(&mut connection, thread_id).await
    }

    pub async fn set_wake_policy(
        &self,
        change: WakePolicyChange,
    ) -> Result<ThreadWakePolicy, StoreError> {
        let key = change.scope.key()?;
        if change.authorization_ref.is_empty()
            || change.authorization_ref.len() > 256
            || change.authorization_ref.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidData);
        }
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let current = read_policy(&mut tx, change.thread_id).await?;
        if current.revision != change.expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: change.expected_revision,
                actual: current.revision,
            });
        }
        match &change.scope {
            WakeScope::Thread => {}
            WakeScope::Subscription { subscription_id } => {
                let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM event_subscriptions WHERE id = ? AND thread_id = ?)")
                    .bind(subscription_id.to_string()).bind(change.thread_id.to_string()).fetch_one(&mut *tx).await.map_err(store_error)?;
                if !valid {
                    return Err(StoreError::InvalidData);
                }
            }
            WakeScope::ProjectDelivery { project_id, .. }
            | WakeScope::ProjectReview { project_id } => {
                let json: Option<String> = sqlx::query_scalar(
                    "SELECT state_json FROM project_automations WHERE project_id = ?",
                )
                .bind(project_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_error)?;
                let project: ProjectAutomation =
                    serde_json::from_str(&json.ok_or(StoreError::InvalidData)?)
                        .map_err(|_| StoreError::InvalidData)?;
                if project.owner_thread_id != change.thread_id {
                    return Err(StoreError::InvalidData);
                }
                if let WakeScope::ProjectDelivery { target, .. } = &change.scope
                    && !project.delivery.contains_key(target)
                {
                    return Err(StoreError::InvalidData);
                }
            }
        }
        if current.policies.len() >= 256
            && !current
                .policies
                .iter()
                .any(|entry| entry.scope == change.scope)
        {
            return Err(StoreError::ThreadCapacity);
        }
        let revision = next_revision(&mut tx).await?;
        sqlx::query("INSERT INTO thread_wake_policy_state(thread_id, revision) VALUES (?, ?) ON CONFLICT(thread_id) DO UPDATE SET revision = excluded.revision")
            .bind(change.thread_id.to_string()).bind(revision).execute(&mut *tx).await.map_err(store_error)?;
        sqlx::query("INSERT INTO thread_wake_policies(thread_id, scope_json, policy_json, revision, authorization_ref) VALUES (?, ?, ?, ?, ?) ON CONFLICT(thread_id, scope_json) DO UPDATE SET policy_json = excluded.policy_json, revision = excluded.revision, authorization_ref = excluded.authorization_ref")
            .bind(change.thread_id.to_string()).bind(key).bind(encode(&change.policy)?).bind(revision).bind(change.authorization_ref)
            .execute(&mut *tx).await.map_err(store_error)?;
        let result = read_policy(&mut tx, change.thread_id).await?;
        tx.commit().await.map_err(store_error)?;
        self.project_changed.notify_one();
        Ok(result)
    }

    pub async fn record_wake_lifecycle(
        &self,
        thread_id: ThreadId,
        event: WakeLifecycle,
    ) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let revision = next_revision(&mut tx).await?;
        sqlx::query("INSERT INTO thread_wake_policy_state(thread_id, revision) VALUES (?, ?) ON CONFLICT(thread_id) DO UPDATE SET revision = excluded.revision")
            .bind(thread_id.to_string()).bind(revision).execute(&mut *tx).await.map_err(store_error)?;
        let query = match event {
            WakeLifecycle::UserStarted => {
                "UPDATE thread_wake_policy_state SET resumed_revision = ? WHERE thread_id = ?"
            }
            WakeLifecycle::UserStopped => {
                "UPDATE thread_wake_policy_state SET stopped_revision = ? WHERE thread_id = ?"
            }
        };
        sqlx::query(query)
            .bind(revision)
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(store_error)?;
        if event == WakeLifecycle::UserStopped {
            // Stop cancels pending immediate triggers, preserving timed/event alarms.
            sqlx::query("UPDATE event_subscription_pending_wakes SET manual_pending = 0 WHERE subscription_id IN (SELECT id FROM event_subscriptions WHERE thread_id = ?)")
                .bind(thread_id.to_string()).execute(&mut *tx).await.map_err(store_error)?;
            sqlx::query("DELETE FROM event_subscription_pending_wakes WHERE event_pending = 0 AND heartbeat_pending = 0 AND manual_pending = 0 AND subscription_id IN (SELECT id FROM event_subscriptions WHERE thread_id = ?)")
                .bind(thread_id.to_string()).execute(&mut *tx).await.map_err(store_error)?;
            // An interrupted review remains the same job and worker, pending a permitted resume.
            sqlx::query("UPDATE project_automations SET state_json = json_set(state_json, '$.review.notified', json('false')), next_deadline_at_ms = CASE WHEN next_deadline_at_ms IS NULL THEN json_extract(state_json, '$.review.dueAtMs') ELSE MIN(next_deadline_at_ms, json_extract(state_json, '$.review.dueAtMs')) END WHERE json_extract(state_json, '$.ownerThreadId') = ? AND json_extract(state_json, '$.paused') = 0 AND json_extract(state_json, '$.completed') = 0 AND json_type(state_json, '$.review') = 'object' AND json_extract(state_json, '$.review.decisionRef') IS NULL")
                .bind(thread_id.to_string()).execute(&mut *tx).await.map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        self.project_changed.notify_one();
        Ok(())
    }

    pub async fn manual_wake_pending(
        &self,
        thread_id: ThreadId,
        subscription_id: uuid::Uuid,
    ) -> Result<bool, StoreError> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM event_subscription_pending_wakes AS pending JOIN event_subscriptions AS subscription ON subscription.id = pending.subscription_id WHERE subscription.thread_id = ? AND pending.subscription_id = ? AND pending.manual_pending = 1)")
            .bind(thread_id.to_string()).bind(subscription_id.to_string()).fetch_one(self.pool.as_ref()).await.map_err(store_error)
    }

    pub async fn complete_delivery(
        &self,
        thread_id: ThreadId,
        through_revision: i64,
        delivered: &[WakeItem],
        discarded: &[WakeItem],
    ) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        for item in delivered {
            if let Some(event) = &item.event
                && event.source == "codex.project"
                && event.cursor.sequence <= through_revision as u64
                && let Some(project_id) = event.labels.get("project")
            {
                let json: Option<String> = sqlx::query_scalar(
                    "SELECT state_json FROM project_automations WHERE project_id = ?",
                )
                .bind(project_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_error)?;
                if let Some(json) = json {
                    let mut project: ProjectAutomation =
                        serde_json::from_str(&json).map_err(|_| StoreError::InvalidData)?;
                    if project.owner_thread_id == thread_id {
                        for job in project.review.iter_mut().chain(
                            project
                                .delivery
                                .values_mut()
                                .filter_map(|target| target.job.as_mut()),
                        ) {
                            if job.id.to_string() == event.id {
                                job.notification_delivered = true;
                            }
                        }
                        sqlx::query(
                            "UPDATE project_automations SET state_json = ? WHERE project_id = ?",
                        )
                        .bind(encode(&project)?)
                        .bind(project_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(store_error)?;
                    }
                }
            }
        }
        for item in delivered.iter().chain(discarded) {
            sqlx::query("DELETE FROM event_subscription_pending_wakes WHERE subscription_id = ? AND revision <= ? AND event_id IS ? AND subscription_id IN (SELECT id FROM event_subscriptions WHERE thread_id = ?)")
                .bind(item.subscription_id.to_string()).bind(through_revision).bind(item.event.as_ref().map(|event| &event.id)).bind(thread_id.to_string())
                .execute(&mut *tx).await.map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

async fn read_policy(
    connection: &mut SqliteConnection,
    thread_id: ThreadId,
) -> Result<ThreadWakePolicy, StoreError> {
    let state = sqlx::query("SELECT revision, stopped_revision, resumed_revision FROM thread_wake_policy_state WHERE thread_id = ?")
        .bind(thread_id.to_string()).fetch_optional(&mut *connection).await.map_err(store_error)?;
    let Some(state) = state else {
        return Ok(ThreadWakePolicy::default());
    };
    let rows = sqlx::query("SELECT scope_json, policy_json, revision, authorization_ref FROM thread_wake_policies WHERE thread_id = ? ORDER BY scope_json")
        .bind(thread_id.to_string()).fetch_all(connection).await.map_err(store_error)?;
    let mut policies = Vec::with_capacity(rows.len());
    for row in rows {
        policies.push(ScopedWakePolicy {
            scope: serde_json::from_str(row.try_get("scope_json").map_err(store_error)?)
                .map_err(|_| StoreError::InvalidData)?,
            policy: serde_json::from_str(row.try_get("policy_json").map_err(store_error)?)
                .map_err(|_| StoreError::InvalidData)?,
            revision: row.try_get("revision").map_err(store_error)?,
            authorization_ref: row.try_get("authorization_ref").map_err(store_error)?,
        });
    }
    Ok(ThreadWakePolicy {
        revision: state.try_get("revision").map_err(store_error)?,
        stopped_revision: state.try_get("stopped_revision").map_err(store_error)?,
        resumed_revision: state.try_get("resumed_revision").map_err(store_error)?,
        policies,
    })
}

#[cfg(test)]
#[path = "wake_policy_tests.rs"]
mod tests;
