use codex_event_subscriptions::ProjectAutomation;
use codex_event_subscriptions::ProjectIdentityCandidates;
use codex_event_subscriptions::ProjectIdentityKind;
use codex_event_subscriptions::StoreError;

use super::SqliteEventSubscriptionStore;
use super::storage::store_error;

pub(super) async fn canonical_key(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    project_id: &str,
) -> Result<String, StoreError> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT canonical_project_id FROM project_identity_aliases WHERE alias_id = ?",
    )
    .bind(project_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_error)?
    .unwrap_or_else(|| project_id.to_owned()))
}

impl SqliteEventSubscriptionStore {
    pub async fn resolve_project_identity(
        &self,
        candidates: &ProjectIdentityCandidates,
        now_ms: i64,
    ) -> Result<String, StoreError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        let canonical = candidates.canonical.project_id.clone();
        for alias in &candidates.aliases {
            if alias.project_id == canonical {
                continue;
            }
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM project_automations WHERE project_id = ?)",
            )
            .bind(&alias.project_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_error)?;
            if exists && !project_exists(&mut tx, &canonical).await? {
                rename_project(&mut tx, &alias.project_id, &canonical).await?;
            } else if exists && project_exists(&mut tx, &canonical).await? {
                merge_project(&mut tx, &alias.project_id, &canonical).await?;
            }
            sqlx::query("INSERT INTO project_identity_aliases(alias_id,canonical_project_id,alias_kind,canonical_identity_id,created_at_ms,last_seen_at_ms) VALUES(?,?,?,?,?,?) ON CONFLICT(alias_id) DO UPDATE SET canonical_project_id=excluded.canonical_project_id,last_seen_at_ms=excluded.last_seen_at_ms")
                .bind(&alias.project_id).bind(&canonical).bind(kind(alias.kind)).bind(&canonical).bind(now_ms).bind(now_ms)
                .execute(&mut *tx).await.map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)?;
        self.project_changed.notify_one();
        Ok(canonical)
    }

    pub async fn register_project_identity_aliases(
        &self,
        candidates: &ProjectIdentityCandidates,
        canonical: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(store_error)?;
        for alias in &candidates.aliases {
            sqlx::query("INSERT INTO project_identity_aliases(alias_id,canonical_project_id,alias_kind,canonical_identity_id,created_at_ms,last_seen_at_ms) VALUES(?,?,?,?,?,?) ON CONFLICT(alias_id) DO UPDATE SET canonical_project_id=excluded.canonical_project_id,last_seen_at_ms=excluded.last_seen_at_ms")
                .bind(&alias.project_id).bind(canonical).bind(kind(alias.kind)).bind(canonical).bind(now_ms).bind(now_ms).execute(&mut *tx).await.map_err(store_error)?;
        }
        tx.commit().await.map_err(store_error)
    }
}

fn kind(kind: ProjectIdentityKind) -> &'static str {
    match kind {
        ProjectIdentityKind::GitCommonDirectory => "git_common_directory",
        ProjectIdentityKind::WorkspacePath => "workspace_path",
    }
}

async fn project_exists(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
) -> Result<bool, StoreError> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM project_automations WHERE project_id = ?)")
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(store_error)
}

async fn rename_project(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    from: &str,
    to: &str,
) -> Result<(), StoreError> {
    sqlx::query("UPDATE project_automations SET project_id=?, state_json=json_set(state_json,'$.projectId',?) WHERE project_id=?").bind(to).bind(to).bind(from).execute(&mut **tx).await.map_err(store_error)?;
    sqlx::query("UPDATE project_automation_history SET project_id=? WHERE project_id=?")
        .bind(to)
        .bind(from)
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    Ok(())
}

async fn merge_project(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    from: &str,
    to: &str,
) -> Result<(), StoreError> {
    let source: String =
        sqlx::query_scalar("SELECT state_json FROM project_automations WHERE project_id=?")
            .bind(from)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_error)?;
    let target: String =
        sqlx::query_scalar("SELECT state_json FROM project_automations WHERE project_id=?")
            .bind(to)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_error)?;
    let mut source: ProjectAutomation =
        serde_json::from_str(&source).map_err(|_| StoreError::InvalidData)?;
    let mut target: ProjectAutomation =
        serde_json::from_str(&target).map_err(|_| StoreError::InvalidData)?;
    for (id, purpose) in std::mem::take(&mut source.threads).into_iter() {
        target.threads.entry(id).or_insert(purpose);
    }
    for (id, value) in std::mem::take(&mut source.thread_workstreams).into_iter() {
        target.thread_workstreams.entry(id).or_insert(value);
    }
    for (id, value) in std::mem::take(&mut source.thread_outcomes).into_iter() {
        target.thread_outcomes.entry(id).or_insert(value);
    }
    for (id, value) in std::mem::take(&mut source.thread_experiments).into_iter() {
        target.thread_experiments.entry(id).or_insert(value);
    }
    target.started_at_ms = target.started_at_ms.min(source.started_at_ms);
    target.last_activity_at_ms = target.last_activity_at_ms.max(source.last_activity_at_ms);
    target.review = target.review.or(source.review);
    target.revision = target.revision.max(source.revision).saturating_add(1);
    let json = serde_json::to_string(&target).map_err(|_| StoreError::InvalidData)?;
    sqlx::query("UPDATE project_automations SET revision=?,next_deadline_at_ms=?,state_json=? WHERE project_id=?").bind(target.revision as i64).bind(target.next_deadline()).bind(json).bind(to).execute(&mut **tx).await.map_err(store_error)?;
    let source_sub: String =
        sqlx::query_scalar("SELECT subscription_id FROM project_automations WHERE project_id=?")
            .bind(from)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_error)?;
    let target_sub: String =
        sqlx::query_scalar("SELECT subscription_id FROM project_automations WHERE project_id=?")
            .bind(to)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_error)?;
    sqlx::query("DELETE FROM event_subscription_pending_wakes WHERE subscription_id=?")
        .bind(&target_sub)
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    sqlx::query(
        "UPDATE event_subscription_pending_wakes SET subscription_id=? WHERE subscription_id=?",
    )
    .bind(&target_sub)
    .bind(&source_sub)
    .execute(&mut **tx)
    .await
    .map_err(store_error)?;
    sqlx::query("DELETE FROM project_automations WHERE project_id=?")
        .bind(from)
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    sqlx::query("DELETE FROM event_subscriptions WHERE id=?")
        .bind(source_sub)
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    sqlx::query("UPDATE project_automation_history SET project_id=? WHERE project_id=?")
        .bind(to)
        .bind(from)
        .execute(&mut **tx)
        .await
        .map_err(store_error)?;
    Ok(())
}
