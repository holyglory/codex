use codex_event_subscriptions::AutomationJobKind;
use codex_event_subscriptions::ProjectAutomation;
use codex_event_subscriptions::ProjectAutomationCommand;
use codex_event_subscriptions::PublishedEvent;
use codex_event_subscriptions::SourceCursor;
use codex_event_subscriptions::StoreError;
use codex_protocol::ThreadId;
use sqlx::Row;
use std::collections::BTreeMap;
use uuid::Uuid;

use super::SqliteEventSubscriptionStore;
use super::storage::next_revision;
use super::storage::parse_uuid;
use super::storage::upsert_event_wake;

fn store_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Unavailable(error.to_string())
}

const MAX_PROJECTS: i64 = 1024;

enum ProjectCommandOrigin {
    Requested,
    Enrollment { parent_thread_id: Option<ThreadId> },
}

impl SqliteEventSubscriptionStore {
    pub(super) async fn detach_project_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<(), StoreError> {
        let observed_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(store_error)?
            .as_millis();
        let observed_at_ms = i64::try_from(observed_at_ms).map_err(store_error)?;
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let rows = sqlx::query("SELECT project_id, subscription_id, state_json FROM project_automations ORDER BY project_id LIMIT 1024")
            .fetch_all(&mut *transaction).await.map_err(store_error)?;
        for row in rows {
            let project_id: String = row.try_get("project_id").map_err(store_error)?;
            let subscription_id: String = row.try_get("subscription_id").map_err(store_error)?;
            let mut project: ProjectAutomation = serde_json::from_str(
                &row.try_get::<String, _>("state_json")
                    .map_err(store_error)?,
            )
            .map_err(store_error)?;
            if project.threads.remove(&thread_id.to_string()).is_none() {
                continue;
            }
            project.thread_workstreams.remove(&thread_id.to_string());
            if project.owner_thread_id == thread_id {
                if let Some(next_owner) = project.threads.keys().next() {
                    project.owner_thread_id =
                        ThreadId::from_string(next_owner).map_err(store_error)?;
                    sqlx::query("UPDATE event_subscriptions SET thread_id = ? WHERE id = ?")
                        .bind(project.owner_thread_id.to_string())
                        .bind(&subscription_id)
                        .execute(&mut *transaction)
                        .await
                        .map_err(store_error)?;
                } else {
                    project.paused = true;
                    sqlx::query(
                        "DELETE FROM event_subscription_pending_wakes WHERE subscription_id = ?",
                    )
                    .bind(&subscription_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(store_error)?;
                }
            }
            project.revision += 1;
            sqlx::query("UPDATE project_automations SET revision = ?, next_deadline_at_ms = ?, state_json = ? WHERE project_id = ?")
                .bind(project.revision as i64).bind(project.next_deadline()).bind(serde_json::to_string(&project).map_err(store_error)?).bind(&project_id)
                .execute(&mut *transaction).await.map_err(store_error)?;
            sqlx::query("INSERT INTO project_automation_history (project_id, revision, at_ms, command_json) VALUES (?, ?, ?, ?)")
                .bind(&project_id).bind(project.revision as i64).bind(observed_at_ms)
                .bind(serde_json::json!({"action":"detach_deleted_task","thread_id":thread_id}).to_string())
                .execute(&mut *transaction).await.map_err(store_error)?;
        }
        transaction.commit().await.map_err(store_error)?;
        self.project_changed.notify_one();
        Ok(())
    }

    pub(super) async fn restore_unfinished_project_jobs(&self) -> Result<(), StoreError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let rows = sqlx::query(
            "SELECT project_id, state_json FROM project_automations ORDER BY project_id LIMIT 1024",
        )
        .fetch_all(&mut *transaction)
        .await
        .map_err(store_error)?;
        for row in rows {
            let project_id: String = row.try_get("project_id").map_err(store_error)?;
            let mut project: ProjectAutomation = serde_json::from_str(
                &row.try_get::<String, _>("state_json")
                    .map_err(store_error)?,
            )
            .map_err(store_error)?;
            if let Some(job) = &mut project.review {
                job.notified = false;
            }
            for target in project.delivery.values_mut() {
                if let Some(job) = &mut target.job {
                    job.notified = false;
                }
            }
            sqlx::query("UPDATE project_automations SET next_deadline_at_ms = ?, state_json = ? WHERE project_id = ?")
                .bind(project.next_deadline()).bind(serde_json::to_string(&project).map_err(store_error)?).bind(project_id)
                .execute(&mut *transaction).await.map_err(store_error)?;
        }
        transaction.commit().await.map_err(store_error)?;
        Ok(())
    }

    pub async fn project_status(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectAutomation>, StoreError> {
        let json = sqlx::query_scalar::<_, String>(
            "SELECT state_json FROM project_automations WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(store_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(store_error))
            .transpose()
    }

    pub async fn project_command(
        &self,
        project_id: &str,
        thread_id: ThreadId,
        expected_revision: Option<u64>,
        command: ProjectAutomationCommand,
        now_ms: i64,
    ) -> Result<ProjectAutomation, StoreError> {
        self.apply_project_command(
            project_id,
            thread_id,
            expected_revision,
            command,
            now_ms,
            ProjectCommandOrigin::Requested,
        )
        .await
    }

    pub async fn project_enroll_thread(
        &self,
        project_id: &str,
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
        now_ms: i64,
    ) -> Result<ProjectAutomation, StoreError> {
        self.apply_project_command(
            project_id,
            thread_id,
            None,
            ProjectAutomationCommand::Bind {
                purpose: codex_event_subscriptions::WorkPurpose::Analysis,
                workstream: None,
            },
            now_ms,
            ProjectCommandOrigin::Enrollment { parent_thread_id },
        )
        .await
    }

    async fn apply_project_command(
        &self,
        project_id: &str,
        thread_id: ThreadId,
        expected_revision: Option<u64>,
        mut command: ProjectAutomationCommand,
        now_ms: i64,
        origin: ProjectCommandOrigin,
    ) -> Result<ProjectAutomation, StoreError> {
        if project_id.is_empty() || project_id.len() > 256 || project_id.contains('\0') {
            return Err(store_error("invalid project identity"));
        }
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let row = sqlx::query(
            "SELECT state_json, subscription_id FROM project_automations WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?;
        let (mut project, subscription_id) = match row {
            Some(row) => (
                serde_json::from_str::<ProjectAutomation>(
                    &row.try_get::<String, _>("state_json")
                        .map_err(store_error)?,
                )
                .map_err(store_error)?,
                parse_uuid(row.try_get("subscription_id").map_err(store_error)?)?,
            ),
            None => {
                if !matches!(command, ProjectAutomationCommand::Bind { .. }) {
                    return Err(store_error(
                        "project is not enrolled; bind its purpose first",
                    ));
                }
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project_automations")
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(store_error)?;
                if count >= MAX_PROJECTS {
                    return Err(StoreError::TotalCapacity);
                }
                (
                    ProjectAutomation::new(project_id.to_owned(), thread_id, now_ms),
                    Uuid::now_v7(),
                )
            }
        };
        let is_review_worker: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM project_review_workers WHERE worker_thread_id = ?)",
        )
        .bind(thread_id.to_string())
        .fetch_one(&mut *transaction)
        .await
        .map_err(store_error)?;
        let mut inherited_from = None;
        if let ProjectCommandOrigin::Enrollment { parent_thread_id } = origin {
            if project.paused || project.completed || is_review_worker {
                transaction.commit().await.map_err(store_error)?;
                return Ok(project);
            }
            if project.threads.contains_key(&thread_id.to_string()) {
                project.last_activity_at_ms = project.last_activity_at_ms.max(now_ms);
                sqlx::query("UPDATE project_automations SET state_json = ? WHERE project_id = ?")
                    .bind(serde_json::to_string(&project).map_err(store_error)?)
                    .bind(project_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(store_error)?;
                transaction.commit().await.map_err(store_error)?;
                return Ok(project);
            }
            if let Some(parent) = parent_thread_id.map(|parent| parent.to_string())
                && let Some(purpose) = project.threads.get(&parent).copied()
            {
                command = ProjectAutomationCommand::Bind {
                    purpose,
                    workstream: project.thread_workstreams.get(&parent).cloned(),
                };
                if let Some(outcome) = project.thread_outcomes.get(&parent).cloned() {
                    project
                        .thread_outcomes
                        .insert(thread_id.to_string(), outcome);
                }
                if let Some(experiment) = project.thread_experiments.get(&parent).cloned() {
                    project
                        .thread_experiments
                        .insert(thread_id.to_string(), experiment);
                }
                inherited_from = Some(parent);
            }
        }
        if let Some(revision) = expected_revision
            && revision != project.revision
        {
            return Err(store_error(
                "project revision changed; read current status before retrying",
            ));
        }
        if expected_revision.is_none()
            && !matches!(
                command,
                ProjectAutomationCommand::Bind { .. } | ProjectAutomationCommand::Status
            )
        {
            return Err(store_error("project mutations require expected_revision"));
        }
        let mut history = serde_json::to_value(&command).map_err(store_error)?;
        if let Some(parent) = inherited_from {
            history["inheritedFromThreadId"] = serde_json::json!(parent);
            history["inheritedOutcomeId"] =
                serde_json::json!(project.thread_outcomes.get(&thread_id.to_string()));
            history["inheritedExperimentRef"] =
                serde_json::json!(project.thread_experiments.get(&thread_id.to_string()));
        }
        let serialized = serde_json::to_string(&history).map_err(store_error)?;
        let invalidates_wake = matches!(
            command,
            ProjectAutomationCommand::Postpone { .. }
                | ProjectAutomationCommand::Pause { .. }
                | ProjectAutomationCommand::RecordDelivery { .. }
                | ProjectAutomationCommand::Complete { .. }
                | ProjectAutomationCommand::Transfer { .. }
        );
        let prior_activity = project.last_activity_at_ms;
        project
            .apply(thread_id, command, now_ms)
            .map_err(store_error)?;
        if is_review_worker {
            project.last_activity_at_ms = prior_activity;
        }
        if invalidates_wake {
            let removed = sqlx::query(
                "DELETE FROM event_subscription_pending_wakes WHERE subscription_id = ?",
            )
            .bind(subscription_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(store_error)?
            .rows_affected();
            if removed > 0 {
                if let Some(job) = &mut project.review {
                    job.notified = false;
                }
                for target in project.delivery.values_mut() {
                    if let Some(job) = &mut target.job {
                        job.notified = false;
                    }
                }
            }
        }
        sqlx::query("INSERT INTO event_subscriptions (id, thread_id, source, event_types_json, label_filters_json, created_at_ms, updated_at_ms) VALUES (?, ?, 'codex.project', '[\"project_work_due\"]', '{}', ?, ?) ON CONFLICT(id) DO UPDATE SET thread_id = excluded.thread_id, updated_at_ms = excluded.updated_at_ms")
            .bind(subscription_id.to_string()).bind(project.owner_thread_id.to_string()).bind(now_ms).bind(now_ms)
            .execute(&mut *transaction).await.map_err(store_error)?;
        sqlx::query("INSERT INTO project_automations (project_id, subscription_id, revision, next_deadline_at_ms, state_json) VALUES (?, ?, ?, ?, ?) ON CONFLICT(project_id) DO UPDATE SET revision = excluded.revision, next_deadline_at_ms = excluded.next_deadline_at_ms, state_json = excluded.state_json")
            .bind(project_id).bind(subscription_id.to_string()).bind(project.revision as i64)
            .bind(project.next_deadline()).bind(serde_json::to_string(&project).map_err(store_error)?)
            .execute(&mut *transaction).await.map_err(store_error)?;
        sqlx::query("INSERT INTO project_automation_history (project_id, revision, at_ms, command_json) VALUES (?, ?, ?, ?)")
            .bind(project_id).bind(project.revision as i64).bind(now_ms).bind(serialized)
            .execute(&mut *transaction).await.map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        self.project_changed.notify_one();
        Ok(project)
    }

    pub async fn project_activity(
        &self,
        project_id: &str,
        thread_id: ThreadId,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let json = sqlx::query_scalar::<_, String>("SELECT state_json FROM project_automations WHERE project_id = ? AND NOT EXISTS (SELECT 1 FROM project_review_workers WHERE worker_thread_id = ?)")
            .bind(project_id).bind(thread_id.to_string()).fetch_optional(&mut *transaction).await.map_err(store_error)?;
        if let Some(json) = json {
            let mut project: ProjectAutomation =
                serde_json::from_str(&json).map_err(store_error)?;
            if !project.paused
                && !project.completed
                && project.threads.contains_key(&thread_id.to_string())
            {
                project.last_activity_at_ms = project.last_activity_at_ms.max(now_ms);
                sqlx::query("UPDATE project_automations SET state_json = ? WHERE project_id = ?")
                    .bind(serde_json::to_string(&project).map_err(store_error)?)
                    .bind(project_id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(store_error)?;
            }
        }
        transaction.commit().await.map_err(store_error)?;
        Ok(())
    }

    pub(super) async fn collect_project_deadlines(
        &self,
        now_ms: i64,
    ) -> Result<Vec<ThreadId>, StoreError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let rows = sqlx::query("SELECT project_id, subscription_id, state_json FROM project_automations WHERE next_deadline_at_ms <= ? ORDER BY next_deadline_at_ms LIMIT 1024")
            .bind(now_ms).fetch_all(&mut *transaction).await.map_err(store_error)?;
        let mut affected = Vec::new();
        for row in rows {
            let project_id: String = row.try_get("project_id").map_err(store_error)?;
            let subscription_id = parse_uuid(row.try_get("subscription_id").map_err(store_error)?)?;
            let mut project: ProjectAutomation = serde_json::from_str(
                &row.try_get::<String, _>("state_json")
                    .map_err(store_error)?,
            )
            .map_err(store_error)?;
            let jobs = project.collect_due(now_ms);
            if !jobs.is_empty() {
                let revision = next_revision(&mut transaction).await?;
                let event_type = if jobs
                    .iter()
                    .any(|job| job.kind == AutomationJobKind::DeliveryRecovery)
                {
                    "delivery_recovery_due"
                } else if jobs
                    .iter()
                    .any(|job| job.kind == AutomationJobKind::Delivery)
                {
                    "delivery_due"
                } else {
                    "performance_review_due"
                };
                let event = PublishedEvent {
                    id: jobs[0].id.to_string(),
                    source: "codex.project".into(),
                    event_type: event_type.into(),
                    cursor: SourceCursor {
                        sequence: revision as u64,
                        value: None,
                    },
                    labels: BTreeMap::from([("project".into(), project_id.clone())]),
                    occurred_at_ms: now_ms,
                };
                upsert_event_wake(&mut transaction, subscription_id, revision, &event, now_ms)
                    .await?;
                affected.push(project.owner_thread_id);
            }
            sqlx::query("UPDATE project_automations SET next_deadline_at_ms = ?, state_json = ? WHERE project_id = ?")
                .bind(project.next_deadline()).bind(serde_json::to_string(&project).map_err(store_error)?).bind(project_id)
                .execute(&mut *transaction).await.map_err(store_error)?;
        }
        transaction.commit().await.map_err(store_error)?;
        Ok(affected)
    }
}

#[cfg(test)]
#[path = "project_automation_tests.rs"]
mod tests;
