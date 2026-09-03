use crate::CoverageState;
use crate::ErrorCategory;
use crate::OperationId;
use crate::ProcessId;
use crate::TerminalStatus;
use crate::UsageStore;
use crate::UsageStoreError;
use uuid::Uuid;

const RECOVERY_SCOPE: &str = "runtime_recovery";
const WRITE_FAILURE_REASON: &str = "write_failure";

impl UsageStore {
    /// Proves that writes work again and preserves a partial-coverage fact for the failure.
    ///
    /// Only the supplied operations owned by this process can be interrupted. Other concurrent
    /// operations remain untouched. The first statement obtains SQLite's write lock so the
    /// recovery snapshot cannot become stale before its terminal facts are committed.
    pub async fn recover_after_write_failure(
        &self,
        process_id: ProcessId,
        affected_operations: &[OperationId],
        occurred_at_ms: i64,
    ) -> Result<u64, UsageStoreError> {
        let mut transaction = self.pool.begin().await.map_err(UsageStoreError::Database)?;
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
        .execute(&mut *transaction)
        .await
        .map_err(UsageStoreError::Database)?;

        let mut interrupted = 0_u64;
        for operation_id in affected_operations {
            let result = sqlx::query(
                r#"
                INSERT INTO operation_events(
                    event_id, operation_id, event_kind, terminal, occurred_at_ms,
                    duration_ns, error_category
                )
                SELECT ?, operation.id, ?, 1, ?, NULL, ?
                FROM operations AS operation
                WHERE operation.id = ? AND operation.process_id = ?
                  AND NOT EXISTS (
                      SELECT 1 FROM operation_events AS event
                      WHERE event.operation_id = operation.id AND event.terminal = 1
                  )
                "#,
            )
            .bind(Uuid::now_v7().to_string())
            .bind(TerminalStatus::Interrupted.as_str())
            .bind(occurred_at_ms)
            .bind(ErrorCategory::Unavailable.as_str())
            .bind(operation_id.as_string())
            .bind(process_id.as_string())
            .execute(&mut *transaction)
            .await
            .map_err(UsageStoreError::Database)?;
            if result.rows_affected() == 0 {
                continue;
            }
            interrupted = interrupted
                .checked_add(1)
                .ok_or(UsageStoreError::AggregateOverflow)?;
            sqlx::query(
                r#"
                INSERT INTO coverage_events(
                    event_id, operation_id, scope_kind, coverage_state,
                    reason_code, occurred_at_ms
                ) VALUES (?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(Uuid::now_v7().to_string())
            .bind(operation_id.as_string())
            .bind(RECOVERY_SCOPE)
            .bind(CoverageState::Partial.as_str())
            .bind(WRITE_FAILURE_REASON)
            .bind(occurred_at_ms)
            .execute(&mut *transaction)
            .await
            .map_err(UsageStoreError::Database)?;
        }

        sqlx::query(
            r#"
            INSERT INTO coverage_events(
                event_id, operation_id, scope_kind, coverage_state, reason_code, occurred_at_ms
            ) VALUES (?, NULL, ?, ?, ?, ?)
            "#,
        )
        .bind(Uuid::now_v7().to_string())
        .bind(RECOVERY_SCOPE)
        .bind(CoverageState::Partial.as_str())
        .bind(WRITE_FAILURE_REASON)
        .bind(occurred_at_ms)
        .execute(&mut *transaction)
        .await
        .map_err(UsageStoreError::Database)?;

        transaction
            .commit()
            .await
            .map_err(UsageStoreError::Database)?;
        Ok(interrupted)
    }
}

#[cfg(all(test, unix))]
#[path = "recovery_tests.rs"]
mod tests;
