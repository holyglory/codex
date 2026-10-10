use crate::store::UsageStore;
use crate::store::UsageStoreError;
use crate::types::AccountAttributionSnapshot;
use crate::types::AgentId;
use crate::types::AgentRoleKind;
use crate::types::ThreadId;
use crate::types::ThreadSourceKind;
use crate::types::TurnId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewThread {
    pub id: ThreadId,
    pub parent_thread_id: Option<ThreadId>,
    pub source_kind: ThreadSourceKind,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewTurn {
    pub id: TurnId,
    pub thread_id: ThreadId,
    /// Account observed when the turn is first created. Individual model requests retain their
    /// own factual account when automatic failover changes profiles within the turn.
    pub account: AccountAttributionSnapshot,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAgent {
    pub id: AgentId,
    pub thread_id: ThreadId,
    pub parent_agent_id: Option<AgentId>,
    pub role_kind: AgentRoleKind,
    pub created_at_ms: i64,
}

impl UsageStore {
    pub async fn thread_created_at(&self, id: &ThreadId) -> Result<Option<i64>, UsageStoreError> {
        sqlx::query_scalar("SELECT created_at_ms FROM threads WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(UsageStoreError::Database)
    }

    pub async fn turn_created_at(&self, id: &TurnId) -> Result<Option<i64>, UsageStoreError> {
        sqlx::query_scalar("SELECT created_at_ms FROM turns WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(UsageStoreError::Database)
    }

    pub async fn turn_identity_matches(&self, fact: &NewTurn) -> Result<bool, UsageStoreError> {
        sqlx::query_scalar(
            r#"
            SELECT EXISTS(SELECT 1 FROM turns
                WHERE id = ? AND thread_id = ? AND created_at_ms = ?)
            "#,
        )
        .bind(fact.id.as_str())
        .bind(fact.thread_id.as_str())
        .bind(fact.created_at_ms)
        .fetch_one(&self.pool)
        .await
        .map_err(UsageStoreError::Database)
    }

    pub async fn agent_created_at(&self, id: &AgentId) -> Result<Option<i64>, UsageStoreError> {
        sqlx::query_scalar("SELECT created_at_ms FROM agents WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(UsageStoreError::Database)
    }

    pub async fn ensure_thread(&self, fact: &NewThread) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO threads(id, parent_thread_id, source_kind, created_at_ms)
            VALUES (?, ?, ?, ?) ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(fact.id.as_str())
        .bind(fact.parent_thread_id.as_ref().map(ThreadId::as_str))
        .bind(fact.source_kind.as_str())
        .bind(fact.created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS(SELECT 1 FROM threads
                    WHERE id = ? AND parent_thread_id IS ?
                      AND source_kind = ? AND created_at_ms = ?)
                "#,
            )
            .bind(fact.id.as_str())
            .bind(fact.parent_thread_id.as_ref().map(ThreadId::as_str))
            .bind(fact.source_kind.as_str())
            .bind(fact.created_at_ms)
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
        {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn ensure_turn(&self, fact: &NewTurn) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO turns(
                id, thread_id, account_profile_ref, account_auth_mode, created_at_ms
            ) VALUES (?, ?, ?, ?, ?) ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(fact.id.as_str())
        .bind(fact.thread_id.as_str())
        .bind(
            fact.account
                .profile_ref()
                .map(crate::types::AccountProfileRef::as_str),
        )
        .bind(
            fact.account
                .auth_mode()
                .map(crate::types::AccountAuthMode::as_str),
        )
        .bind(fact.created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS(SELECT 1 FROM turns
                    WHERE id = ? AND thread_id = ? AND account_profile_ref IS ?
                      AND account_auth_mode IS ?
                      AND created_at_ms = ?)
                "#,
            )
            .bind(fact.id.as_str())
            .bind(fact.thread_id.as_str())
            .bind(
                fact.account
                    .profile_ref()
                    .map(crate::types::AccountProfileRef::as_str),
            )
            .bind(
                fact.account
                    .auth_mode()
                    .map(crate::types::AccountAuthMode::as_str),
            )
            .bind(fact.created_at_ms)
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
        {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }

    pub async fn ensure_agent(&self, fact: &NewAgent) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO agents(id, thread_id, parent_agent_id, role_kind, created_at_ms)
            VALUES (?, ?, ?, ?, ?) ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(fact.id.as_str())
        .bind(fact.thread_id.as_str())
        .bind(fact.parent_agent_id.as_ref().map(AgentId::as_str))
        .bind(fact.role_kind.as_str())
        .bind(fact.created_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS(SELECT 1 FROM agents
                    WHERE id = ? AND thread_id = ? AND parent_agent_id IS ?
                      AND role_kind = ? AND created_at_ms = ?)
                "#,
            )
            .bind(fact.id.as_str())
            .bind(fact.thread_id.as_str())
            .bind(fact.parent_agent_id.as_ref().map(AgentId::as_str))
            .bind(fact.role_kind.as_str())
            .bind(fact.created_at_ms)
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?
        {
            return Err(UsageStoreError::FactConflict);
        }
        Ok(())
    }
}
