use crate::RepositoryId;
use crate::SafeRepositoryLabel;
use crate::UsageDetailListQuery;
use crate::UsageDetailRecord;
use crate::UsagePage;
use crate::UsageStore;
use crate::UsageStoreError;
use sqlx::Row;

use super::detail_query_support::cursor_at;
use super::detail_query_support::cursor_id;
use super::detail_query_support::fetch_limit;
use super::detail_query_support::page_from_rows;

pub(crate) async fn list_repository_identities(
    store: &UsageStore,
    query: &UsageDetailListQuery,
) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
    let rows = sqlx::query(
        r#"
        SELECT id, identity_source, safe_display_label, created_at_ms FROM repositories
        WHERE (? IS NULL OR id = ?)
          AND (? IS NULL OR created_at_ms >= ?)
          AND (? IS NULL OR created_at_ms < ?)
          AND (? IS NULL OR created_at_ms < ? OR (created_at_ms = ? AND id > ?))
        ORDER BY created_at_ms DESC, id ASC LIMIT ?
        "#,
    )
    .bind(query.repository_id.as_ref().map(RepositoryId::as_str))
    .bind(query.repository_id.as_ref().map(RepositoryId::as_str))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_at(query))
    .bind(cursor_id(query))
    .bind(fetch_limit(query)?)
    .fetch_all(&store.pool)
    .await
    .map_err(UsageStoreError::Database)?;
    page_from_rows(&rows, query, "created_at_ms", "id", |row| {
        Ok(UsageDetailRecord::RepositoryIdentity {
            id: RepositoryId::new(row.get::<String, _>("id"))
                .map_err(|_| UsageStoreError::InvalidFact)?
                .as_str()
                .to_string(),
            identity_source: fixed(
                row.get("identity_source"),
                &["origin", "git_common_dir", "workspace"],
            )?,
            safe_display_label: SafeRepositoryLabel::new(
                row.get::<String, _>("safe_display_label"),
            )
            .map_err(|_| UsageStoreError::InvalidFact)?
            .as_str()
            .to_string(),
            created_at_ms: row.get("created_at_ms"),
        })
    })
}

fn fixed(value: String, allowed: &[&str]) -> Result<String, UsageStoreError> {
    allowed
        .contains(&value.as_str())
        .then_some(value)
        .ok_or(UsageStoreError::InvalidFact)
}
