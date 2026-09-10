use codex_event_subscriptions::ProjectAutomation;
use codex_event_subscriptions::StoreError;
use codex_protocol::ThreadId;
use uuid::Uuid;

use super::SqliteEventSubscriptionStore;
use super::storage::parse_thread_id;
use super::storage::store_error;

impl SqliteEventSubscriptionStore {
    pub async fn claim_project_review_worker(
        &self,
        project_id: &str,
        job_id: Uuid,
        candidate: ThreadId,
        now_ms: i64,
    ) -> Result<Option<ThreadId>, StoreError> {
        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let json = sqlx::query_scalar::<_, String>(
            "SELECT state_json FROM project_automations WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(store_error)?;
        let Some(json) = json else { return Ok(None) };
        let project: ProjectAutomation =
            serde_json::from_str(&json).map_err(|_| StoreError::InvalidData)?;
        if project.paused || project.review.as_ref().is_none_or(|job| job.id != job_id) {
            return Ok(None);
        }
        sqlx::query("INSERT INTO project_review_workers (project_id,job_id,worker_thread_id,claimed_at_ms) VALUES (?,?,?,?) ON CONFLICT(project_id,job_id) DO NOTHING")
            .bind(project_id).bind(job_id.to_string()).bind(candidate.to_string()).bind(now_ms)
            .execute(&mut *transaction).await.map_err(store_error)?;
        let selected=sqlx::query_scalar::<_,String>("SELECT worker_thread_id FROM project_review_workers WHERE project_id = ? AND job_id = ?")
            .bind(project_id).bind(job_id.to_string()).fetch_one(&mut *transaction).await.map_err(store_error)?;
        transaction.commit().await.map_err(store_error)?;
        parse_thread_id(selected).map(Some)
    }
}
