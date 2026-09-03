use crate::repository::RepositoryHmacKey;
use crate::repository::load_or_create_repository_key;
use crate::types::DoctorReport;
use crate::types::ErrorCategory;
use crate::types::NewOperation;
use crate::types::OperationId;
use crate::types::ProcessId;
use crate::types::TAXONOMY_VERSION;
use crate::types::TerminalOperation;
use crate::types::TerminalStatus;
use codex_private_storage::ensure_private_directory;
use codex_private_storage::ensure_private_file;
use codex_private_storage::verify_private_file;
use codex_state::SqlitePoolProfile;
use codex_state::open_sqlite_pool;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use std::path::Path;
use thiserror::Error;
use uuid::Uuid;

// Keep migrations embedded so every runtime surface applies the same schema before capture.
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, Error)]
pub enum UsageStoreError {
    #[error("usage database is unavailable")]
    Database(#[source] sqlx::Error),
    #[error("usage database migration failed")]
    Migration(#[source] sqlx::migrate::MigrateError),
    #[error("usage operation conflicts with an existing record")]
    OperationConflict,
    #[error("usage process conflicts with an existing record")]
    ProcessConflict,
    #[error("usage process has a conflicting terminal result")]
    ProcessEventConflict,
    #[error("usage operation has a conflicting terminal result")]
    TerminalConflict,
    #[error("usage fact conflicts with an existing record")]
    FactConflict,
    #[error("usage fact violates a measurement or relationship invariant")]
    InvalidFact,
    #[error("usage duration exceeds the supported database range")]
    DurationOutOfRange,
    #[error("usage token count exceeds the supported database range")]
    TokenCountOutOfRange,
    #[error("usage database returned an out-of-range aggregate")]
    DatabaseValueOutOfRange,
    #[error("usage database filesystem setup failed")]
    Filesystem(#[source] std::io::Error),
    #[error("private usage storage is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("repository identity key is missing for existing usage history")]
    RepositoryKeyMissing,
    #[error("repository identity key is corrupt")]
    RepositoryKeyCorrupt,
    #[error("repository merge history contains a cycle")]
    RepositoryMergeCycle,
    #[error("repository key was committed but temporary-file cleanup failed")]
    RepositoryKeyCommittedCleanupUncertain(#[source] std::io::Error),
    #[error("repository key was committed but directory synchronization failed")]
    RepositoryKeyCommittedSyncUncertain(#[source] std::io::Error),
    #[error("usage aggregate exceeds the supported numeric range")]
    AggregateOverflow,
    #[error("usage task tree exceeds the supported query bound")]
    TaskTreeTooLarge,
}

impl UsageStoreError {
    /// Returns whether retrying after a bounded health check can safely restore capture.
    pub fn recovery_may_succeed(&self) -> bool {
        matches!(
            self,
            Self::Database(_)
                | Self::Filesystem(_)
                | Self::RepositoryKeyCommittedCleanupUncertain(_)
        )
    }
}

pub struct UsageStore {
    pub(crate) pool: SqlitePool,
    pub(crate) repository_key: RepositoryHmacKey,
}

impl UsageStore {
    pub async fn operation_links(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<crate::types::OperationLinks>, UsageStoreError> {
        let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT retry_of_operation_id, rework_of_operation_id FROM operations WHERE id = ?",
        )
        .bind(operation_id.as_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        row.map(|(retry, rework)| {
            Ok(crate::types::OperationLinks {
                retry_of_operation_id: retry
                    .as_deref()
                    .map(|value| {
                        OperationId::from_string(value).ok_or(UsageStoreError::InvalidFact)
                    })
                    .transpose()?,
                rework_of_operation_id: rework
                    .as_deref()
                    .map(|value| {
                        OperationId::from_string(value).ok_or(UsageStoreError::InvalidFact)
                    })
                    .transpose()?,
            })
        })
        .transpose()
    }

    pub async fn open(codex_home: &Path) -> Result<Self, UsageStoreError> {
        let usage_dir = codex_home.join("usage");
        ensure_private_directory(&usage_dir).map_err(private_storage_error)?;
        let database_path = usage_dir.join("usage.sqlite3");
        let pool = open_sqlite_pool(&database_path, SqlitePoolProfile::DurableEvents)
            .await
            .map_err(UsageStoreError::Database)?;
        ensure_private_file(&database_path).map_err(private_storage_error)?;
        if let Err(error) = MIGRATOR.run(&pool).await {
            pool.close().await;
            return Err(UsageStoreError::Migration(error));
        }
        let _ = crate::report_cache::ensure(&pool).await;
        let repository_key = match load_or_create_repository_key(&usage_dir, &pool).await {
            Ok(repository_key) => repository_key,
            Err(error) => {
                pool.close().await;
                return Err(error);
            }
        };
        verify_sqlite_files(&database_path)?;
        Ok(Self {
            pool,
            repository_key,
        })
    }

    pub async fn register_process(
        &self,
        process_id: &ProcessId,
        os_pid: u32,
        started_at_ms: i64,
    ) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO process_instances(id, os_pid, started_at_ms)
            VALUES (?, ?, ?)
            ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(process_id.as_string())
        .bind(i64::from(os_pid))
        .bind(started_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !self
                .process_matches(process_id, os_pid, started_at_ms)
                .await?
        {
            return Err(UsageStoreError::ProcessConflict);
        }
        Ok(())
    }

    pub async fn heartbeat_process(
        &self,
        process_id: &ProcessId,
        occurred_at_ms: i64,
    ) -> Result<(), UsageStoreError> {
        sqlx::query(
            r#"
            INSERT INTO process_events(event_id, process_id, event_kind, occurred_at_ms)
            VALUES (?, ?, 'heartbeat', ?)
            ON CONFLICT(process_id, event_kind, occurred_at_ms) DO NOTHING
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(process_id.as_string())
        .bind(occurred_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(())
    }

    pub async fn finish_process(
        &self,
        process_id: &ProcessId,
        occurred_at_ms: i64,
    ) -> Result<(), UsageStoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO process_events(event_id, process_id, event_kind, occurred_at_ms)
            VALUES (?, ?, 'ended', ?)
            ON CONFLICT(process_id) WHERE event_kind = 'ended' DO NOTHING
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(process_id.as_string())
        .bind(occurred_at_ms)
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0
            && !self.process_end_matches(process_id, occurred_at_ms).await?
        {
            return Err(UsageStoreError::ProcessEventConflict);
        }
        Ok(())
    }

    pub async fn begin_operation(&self, operation: &NewOperation) -> Result<(), UsageStoreError> {
        let operation_id = operation.id.as_string();
        let result = sqlx::query(
            r#"
            INSERT INTO operations(
                id, process_id, thread_id, turn_id, agent_id,
                parent_operation_id, retry_of_operation_id, rework_of_operation_id,
                operation_kind, started_at_ms, taxonomy_version, phase, activity,
                activity_state, attribution_provenance
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(&operation_id)
        .bind(operation.process_id.as_string())
        .bind(
            operation
                .thread_id
                .as_ref()
                .map(crate::types::ThreadId::as_str),
        )
        .bind(operation.turn_id.as_ref().map(crate::types::TurnId::as_str))
        .bind(
            operation
                .agent_id
                .as_ref()
                .map(crate::types::AgentId::as_str),
        )
        .bind(
            operation
                .parent_operation_id
                .map(crate::types::OperationId::as_string),
        )
        .bind(
            operation
                .retry_of_operation_id
                .map(crate::types::OperationId::as_string),
        )
        .bind(
            operation
                .rework_of_operation_id
                .map(crate::types::OperationId::as_string),
        )
        .bind(operation.kind.as_str())
        .bind(operation.started_at_ms)
        .bind(TAXONOMY_VERSION)
        .bind(operation.phase.as_str())
        .bind(operation.activity.as_str())
        .bind(operation.activity_state.as_str())
        .bind(operation.attribution_provenance.as_str())
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 && !self.operation_matches(operation).await? {
            return Err(UsageStoreError::OperationConflict);
        }
        Ok(())
    }

    pub async fn finish_operation(
        &self,
        terminal: &TerminalOperation,
    ) -> Result<(), UsageStoreError> {
        let duration_ns =
            i64::try_from(terminal.duration_ns).map_err(|_| UsageStoreError::DurationOutOfRange)?;
        let operation_id = terminal.operation_id.as_string();
        let result = sqlx::query(
            r#"
            INSERT INTO operation_events(
                event_id, operation_id, event_kind, terminal, occurred_at_ms,
                duration_ns, error_category
            ) VALUES (?, ?, ?, 1, ?, ?, ?)
            ON CONFLICT(operation_id) WHERE terminal = 1 DO NOTHING
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&operation_id)
        .bind(terminal.status.as_str())
        .bind(terminal.occurred_at_ms)
        .bind(duration_ns)
        .bind(
            terminal
                .error_category
                .map(crate::types::ErrorCategory::as_str),
        )
        .execute(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if result.rows_affected() == 0 && !self.terminal_matches_or_recovered(terminal).await? {
            return Err(UsageStoreError::TerminalConflict);
        }
        Ok(())
    }

    pub async fn doctor(&self) -> Result<DoctorReport, UsageStoreError> {
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
        let migration_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
        let incomplete_operations = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*) FROM operations AS operation
            WHERE NOT EXISTS (
                SELECT 1 FROM operation_events AS event
                WHERE event.operation_id = operation.id AND event.terminal = 1
            )
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(DoctorReport {
            integrity,
            migration_count: migration_count as u64,
            incomplete_operations: incomplete_operations as u64,
        })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    async fn operation_matches(&self, operation: &NewOperation) -> Result<bool, UsageStoreError> {
        let row = sqlx::query(
            r#"
            SELECT process_id, thread_id, turn_id, agent_id, parent_operation_id,
                   retry_of_operation_id, rework_of_operation_id, operation_kind,
                   started_at_ms, taxonomy_version, phase, activity, activity_state,
                   attribution_provenance
            FROM operations WHERE id = ?
            "#,
        )
        .bind(operation.id.as_string())
        .fetch_one(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let parent_operation_id = operation
            .parent_operation_id
            .map(crate::types::OperationId::as_string);
        let retry_of_operation_id = operation
            .retry_of_operation_id
            .map(crate::types::OperationId::as_string);
        let rework_of_operation_id = operation
            .rework_of_operation_id
            .map(crate::types::OperationId::as_string);
        Ok(
            row.get::<String, _>("process_id") == operation.process_id.as_string()
                && row.get::<Option<String>, _>("thread_id").as_deref()
                    == operation
                        .thread_id
                        .as_ref()
                        .map(crate::types::ThreadId::as_str)
                && row.get::<Option<String>, _>("turn_id").as_deref()
                    == operation.turn_id.as_ref().map(crate::types::TurnId::as_str)
                && row.get::<Option<String>, _>("agent_id").as_deref()
                    == operation
                        .agent_id
                        .as_ref()
                        .map(crate::types::AgentId::as_str)
                && row.get::<Option<String>, _>("parent_operation_id") == parent_operation_id
                && row.get::<Option<String>, _>("retry_of_operation_id") == retry_of_operation_id
                && row.get::<Option<String>, _>("rework_of_operation_id") == rework_of_operation_id
                && row.get::<String, _>("operation_kind") == operation.kind.as_str()
                && row.get::<i64, _>("started_at_ms") == operation.started_at_ms
                && row.get::<i64, _>("taxonomy_version") == TAXONOMY_VERSION
                && row.get::<String, _>("phase") == operation.phase.as_str()
                && row.get::<String, _>("activity") == operation.activity.as_str()
                && row.get::<String, _>("activity_state") == operation.activity_state.as_str()
                && row.get::<String, _>("attribution_provenance")
                    == operation.attribution_provenance.as_str(),
        )
    }

    async fn process_matches(
        &self,
        process_id: &ProcessId,
        os_pid: u32,
        started_at_ms: i64,
    ) -> Result<bool, UsageStoreError> {
        let row = sqlx::query("SELECT os_pid, started_at_ms FROM process_instances WHERE id = ?")
            .bind(process_id.as_string())
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)?;
        Ok(row.get::<i64, _>("os_pid") == i64::from(os_pid)
            && row.get::<i64, _>("started_at_ms") == started_at_ms)
    }

    async fn process_end_matches(
        &self,
        process_id: &ProcessId,
        occurred_at_ms: i64,
    ) -> Result<bool, UsageStoreError> {
        let stored_at = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT occurred_at_ms FROM process_events
            WHERE process_id = ? AND event_kind = 'ended'
            "#,
        )
        .bind(process_id.as_string())
        .fetch_one(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(stored_at == occurred_at_ms)
    }

    async fn terminal_matches_or_recovered(
        &self,
        terminal: &TerminalOperation,
    ) -> Result<bool, UsageStoreError> {
        let row = sqlx::query(
            r#"
            SELECT event_kind, occurred_at_ms, duration_ns, error_category
            FROM operation_events WHERE operation_id = ? AND terminal = 1
            "#,
        )
        .bind(terminal.operation_id.as_string())
        .fetch_one(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let expected_error = terminal
            .error_category
            .map(crate::types::ErrorCategory::as_str);
        let stored_kind = row.get::<String, _>("event_kind");
        let stored_duration = row.get::<Option<i64>, _>("duration_ns");
        let stored_error = row.get::<Option<String>, _>("error_category");
        let recovered = stored_kind == TerminalStatus::Interrupted.as_str()
            && stored_duration.is_none()
            && stored_error.as_deref() == Some(ErrorCategory::Unavailable.as_str());
        Ok(recovered
            || (stored_kind == terminal.status.as_str()
                && row.get::<i64, _>("occurred_at_ms") == terminal.occurred_at_ms
                && stored_duration == Some(terminal.duration_ns as i64)
                && stored_error.as_deref() == expected_error))
    }
}

fn private_storage_error(error: codex_private_storage::PrivateStorageError) -> UsageStoreError {
    UsageStoreError::Filesystem(std::io::Error::other(error))
}

fn verify_sqlite_files(database_path: &Path) -> Result<(), UsageStoreError> {
    verify_private_file(database_path).map_err(private_storage_error)?;
    let Some(file_name) = database_path.file_name().and_then(std::ffi::OsStr::to_str) else {
        return Err(UsageStoreError::Filesystem(std::io::Error::other(
            "usage database filename is invalid",
        )));
    };
    for suffix in ["-wal", "-shm"] {
        let sidecar = database_path.with_file_name(format!("{file_name}{suffix}"));
        if sidecar.exists() {
            verify_private_file(&sidecar).map_err(private_storage_error)?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
