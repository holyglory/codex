//! Prune diagnostic logs in the background immediately, then on a timer.
//! SQLite keeps freed pages for reuse.

use super::LOG_GLOBAL_ROW_LIMIT;
use super::StateRuntime;
use chrono::Utc;
use sqlx::Row;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration;

const LOG_RETENTION_SECONDS: i64 = 10 * 24 * 60 * 60; // 10 days
const LOG_DATABASE_BUDGET_BYTES: i64 = 64 * 1024 * 1024; // 64 MiB

impl StateRuntime {
    pub(super) fn start_periodic_logs_maintenance(self: &Arc<Self>, period: Duration) {
        let runtime = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(
                /*start*/ tokio::time::Instant::now(),
                /*period*/ period,
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut first_sweep = true;
            loop {
                // Hold no strong runtime reference while waiting.
                interval.tick().await;
                let Some(runtime) = runtime.upgrade() else {
                    break;
                };
                if runtime.logs_pool.is_closed() {
                    break;
                }
                if let Err(err) = prune_by_age_and_size(
                    &runtime.logs_pool,
                    Utc::now().timestamp(),
                    LOG_DATABASE_BUDGET_BYTES,
                )
                .await
                {
                    tracing::warn!("failed to prune diagnostic logs: {err}");
                }
                if first_sweep {
                    first_sweep = false;
                    // Preserve the startup checkpoint without waiting for readers or writers.
                    if let Err(err) = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                        .execute(runtime.logs_pool.as_ref())
                        .await
                    {
                        tracing::warn!("failed to checkpoint diagnostic logs: {err}");
                    }
                }
            }
        });
    }
}

async fn prune_by_age_and_size(
    pool: &SqlitePool,
    now: i64,
    budget_bytes: i64,
) -> anyhow::Result<()> {
    let mut retention_seconds = LOG_RETENTION_SECONDS;
    loop {
        sqlx::query("DELETE FROM logs WHERE ts < ?")
            .bind(now.saturating_sub(retention_seconds))
            .execute(pool)
            .await?;
        // Exclude free pages: SQLite reuses them without shrinking the file.
        let occupied_bytes: i64 = sqlx::query_scalar(
            "SELECT (page_count - freelist_count) * page_size \
             FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()",
        )
        .fetch_one(pool)
        .await?;
        if occupied_bytes <= budget_bytes {
            return Ok(());
        }
        if retention_seconds == 1 {
            prune_newest_rows_to_budget(pool, budget_bytes).await?;
            let _ = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                .execute(pool)
                .await;
            return Ok(());
        }
        // Keep the newest second even if it exceeds the budget.
        retention_seconds = (retention_seconds / 2).max(1);
    }
}

/// Enforce a global content cap after age pruning has reached the newest
/// second. Partition caps alone are insufficient when a process creates many
/// short-lived thread IDs; this removes the oldest rows across all partitions.
async fn prune_newest_rows_to_budget(pool: &SqlitePool, budget_bytes: i64) -> anyhow::Result<()> {
    let rows = sqlx::query("SELECT id, estimated_bytes FROM logs ORDER BY id DESC LIMIT ?")
        .bind(LOG_GLOBAL_ROW_LIMIT.saturating_add(1))
        .fetch_all(pool)
        .await?;
    let mut cumulative_bytes = 0_i64;
    let mut cutoff_id = None;
    for (index, row) in rows.iter().enumerate() {
        cumulative_bytes =
            cumulative_bytes.saturating_add(row.try_get::<i64, _>("estimated_bytes")?.max(0));
        if index > 0
            && (cumulative_bytes > budget_bytes
                || i64::try_from(index + 1).unwrap_or(i64::MAX) > LOG_GLOBAL_ROW_LIMIT)
        {
            cutoff_id = Some(row.try_get::<i64, _>("id")?);
            break;
        }
    }
    if let Some(cutoff_id) = cutoff_id {
        sqlx::query("DELETE FROM logs WHERE id <= ?")
            .bind(cutoff_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "logs_maintenance_tests.rs"]
mod tests;
