use crate::AccountAuthMode;
use crate::AccountProfileRef;
use crate::Activity;
use crate::ActivityState;
use crate::AgentRoleKind;
use crate::AttributionProvenance;
use crate::ClientOrigin;
use crate::ErrorCategory;
use crate::ModelName;
use crate::ObservationTiming;
use crate::OperationFamily;
use crate::OperationKind;
use crate::Phase;
use crate::ProviderKind;
use crate::RepositoryId;
use crate::TerminalStatus;
use crate::ThreadId;
use crate::ThreadSourceKind;
use crate::ToolKind;
use crate::ToolName;
use crate::TransportKind;
use crate::UsageDetailKind;
use crate::UsageDetailListQuery;
use crate::UsageDetailRecord;
use crate::UsageModelRequestDetail;
use crate::UsageOperationDetail;
use crate::UsagePage;
use crate::UsageStore;
use crate::UsageStoreError;
use crate::UsageToolDetail;
use sqlx::Row;

use super::detail_query_support::agent;
use super::detail_query_support::cursor_at;
use super::detail_query_support::cursor_id;
use super::detail_query_support::display_account;
use super::detail_query_support::fetch_limit;
use super::detail_query_support::model_request;
use super::detail_query_support::operation;
use super::detail_query_support::optional_enum;
use super::detail_query_support::optional_uuid;
use super::detail_query_support::page_from_rows;
use super::detail_query_support::positive;
use super::detail_query_support::required_enum;
use super::detail_query_support::thread;
use super::detail_query_support::tool_invocation;
use super::detail_query_support::turn;
use super::detail_query_support::uuid;

impl UsageStore {
    pub async fn list_details(
        &self,
        kind: UsageDetailKind,
        query: &UsageDetailListQuery,
        account_label: impl Fn(&AccountProfileRef) -> String + Copy,
    ) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
        match kind {
            UsageDetailKind::Processes => self.detail_processes(query).await,
            UsageDetailKind::Threads => self.detail_threads(query).await,
            UsageDetailKind::Turns => self.detail_turns(query, account_label).await,
            UsageDetailKind::Agents => self.detail_agents(query).await,
            UsageDetailKind::Operations => self.detail_operations(query, account_label).await,
            _ => super::detail_query_facts::list_fact_details(self, kind, query).await,
        }
    }

    async fn detail_processes(
        &self,
        query: &UsageDetailListQuery,
    ) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT id, started_at_ms FROM process_instances
            WHERE (? IS NULL OR started_at_ms >= ?)
              AND (? IS NULL OR started_at_ms < ?)
              AND (? IS NULL OR started_at_ms < ? OR (started_at_ms = ? AND id > ?))
            ORDER BY started_at_ms DESC, id ASC LIMIT ?
            "#,
        )
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_id(query))
        .bind(fetch_limit(query)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        page_from_rows(&rows, query, "started_at_ms", "id", |row| {
            Ok(UsageDetailRecord::Process {
                id: uuid(row.get("id"))?,
                started_at_ms: row.get("started_at_ms"),
            })
        })
    }

    async fn detail_threads(
        &self,
        query: &UsageDetailListQuery,
    ) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT id, parent_thread_id, source_kind, created_at_ms FROM threads
            WHERE (? IS NULL OR id = ?)
              AND (? IS NULL OR created_at_ms >= ?)
              AND (? IS NULL OR created_at_ms < ?)
              AND (? IS NULL OR created_at_ms < ? OR (created_at_ms = ? AND id > ?))
            ORDER BY created_at_ms DESC, id ASC LIMIT ?
            "#,
        )
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_id(query))
        .bind(fetch_limit(query)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        page_from_rows(&rows, query, "created_at_ms", "id", |row| {
            Ok(UsageDetailRecord::Thread {
                id: thread(row.get("id"))?,
                parent_thread_id: row
                    .get::<Option<String>, _>("parent_thread_id")
                    .map(thread)
                    .transpose()?,
                source_kind: ThreadSourceKind::new(row.get::<String, _>("source_kind"))
                    .map_err(|_| UsageStoreError::InvalidFact)?
                    .as_str()
                    .to_string(),
                created_at_ms: row.get("created_at_ms"),
            })
        })
    }

    async fn detail_turns(
        &self,
        query: &UsageDetailListQuery,
        account_label: impl Fn(&AccountProfileRef) -> String + Copy,
    ) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT id, thread_id, account_profile_ref, account_auth_mode, created_at_ms
            FROM turns
            WHERE (? IS NULL OR thread_id = ?)
              AND (? IS NULL OR account_profile_ref = ?)
              AND (? IS NULL OR created_at_ms >= ?)
              AND (? IS NULL OR created_at_ms < ?)
              AND (? IS NULL OR created_at_ms < ? OR (created_at_ms = ? AND id > ?))
            ORDER BY created_at_ms DESC, id ASC LIMIT ?
            "#,
        )
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(
            query
                .account_profile_ref
                .as_ref()
                .map(AccountProfileRef::as_str),
        )
        .bind(
            query
                .account_profile_ref
                .as_ref()
                .map(AccountProfileRef::as_str),
        )
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_id(query))
        .bind(fetch_limit(query)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        page_from_rows(&rows, query, "created_at_ms", "id", |row| {
            Ok(UsageDetailRecord::Turn {
                id: turn(row.get("id"))?,
                thread_id: thread(row.get("thread_id"))?,
                account: display_account(row.get("account_profile_ref"), account_label)?,
                account_auth_mode: optional_enum(
                    row.get("account_auth_mode"),
                    AccountAuthMode::parse,
                    AccountAuthMode::as_str,
                )?,
                created_at_ms: row.get("created_at_ms"),
            })
        })
    }

    async fn detail_agents(
        &self,
        query: &UsageDetailListQuery,
    ) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
        let rows = sqlx::query(
            r#"
            SELECT id, thread_id, parent_agent_id, role_kind, created_at_ms FROM agents
            WHERE (? IS NULL OR thread_id = ?)
              AND (? IS NULL OR created_at_ms >= ?)
              AND (? IS NULL OR created_at_ms < ?)
              AND (? IS NULL OR created_at_ms < ? OR (created_at_ms = ? AND id > ?))
            ORDER BY created_at_ms DESC, id ASC LIMIT ?
            "#,
        )
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_id(query))
        .bind(fetch_limit(query)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        page_from_rows(&rows, query, "created_at_ms", "id", |row| {
            Ok(UsageDetailRecord::Agent {
                id: agent(row.get("id"))?,
                thread_id: thread(row.get("thread_id"))?,
                parent_agent_id: row
                    .get::<Option<String>, _>("parent_agent_id")
                    .map(agent)
                    .transpose()?,
                role_kind: AgentRoleKind::new(row.get::<String, _>("role_kind"))
                    .map_err(|_| UsageStoreError::InvalidFact)?
                    .as_str()
                    .to_string(),
                created_at_ms: row.get("created_at_ms"),
            })
        })
    }

    async fn detail_operations(
        &self,
        query: &UsageDetailListQuery,
        account_label: impl Fn(&AccountProfileRef) -> String + Copy,
    ) -> Result<UsagePage<UsageDetailRecord>, UsageStoreError> {
        let repository = match &query.repository_id {
            Some(id) => Some(self.canonical_repository_id(id).await?),
            None => None,
        };
        let rows = sqlx::query(
            r#"
            SELECT operation.*, terminal.event_id AS terminal_event_id,
                   terminal.event_kind AS terminal_status,
                   terminal.occurred_at_ms AS completed_at_ms,
                   terminal.duration_ns, terminal.error_category,
                   request.id AS request_id, request.provider_kind, request.model,
                   request.transport_kind, request.attempt_number,
                   COALESCE(request.account_profile_ref,
                            parent_request.account_profile_ref,
                            turn.account_profile_ref) AS effective_account,
                   COALESCE(request.account_auth_mode,
                            parent_request.account_auth_mode,
                            turn.account_auth_mode) AS effective_auth_mode,
                   request.client_origin,
                   tool.id AS tool_id, tool.tool_kind, tool.safe_tool_name,
                   tool.operation_family, tool.observation_timing,
                   tool.covering_model_request_id
            FROM operations AS operation
            LEFT JOIN operation_events AS terminal
              ON terminal.operation_id = operation.id AND terminal.terminal = 1
            LEFT JOIN model_requests AS request ON request.operation_id = operation.id
            LEFT JOIN model_requests AS parent_request
              ON parent_request.operation_id = operation.parent_operation_id
            LEFT JOIN tool_invocations AS tool ON tool.operation_id = operation.id
            LEFT JOIN turns AS turn ON turn.id = operation.turn_id
            WHERE (? IS NULL OR operation.thread_id = ?)
              AND (? IS NULL OR COALESCE(request.account_profile_ref,
                                         parent_request.account_profile_ref,
                                         turn.account_profile_ref) = ?)
              AND (? IS NULL OR operation.started_at_ms >= ?)
              AND (? IS NULL OR operation.started_at_ms < ?)
              AND (? IS NULL OR EXISTS (
                  WITH RECURSIVE family(id) AS (
                      SELECT ? UNION ALL SELECT merge.source_repository_id FROM family
                      JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
                  )
                  SELECT 1 FROM repository_attributions AS attribution
                  WHERE attribution.operation_id = operation.id
                    AND attribution.repository_id IN (SELECT id FROM family)
              ))
              AND (? IS NULL OR operation.started_at_ms < ?
                   OR (operation.started_at_ms = ? AND operation.id > ?))
            ORDER BY operation.started_at_ms DESC, operation.id ASC LIMIT ?
            "#,
        )
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.account_profile_ref.as_ref().map(AccountProfileRef::as_str))
        .bind(query.account_profile_ref.as_ref().map(AccountProfileRef::as_str))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(repository.as_ref().map(RepositoryId::as_str))
        .bind(repository.as_ref().map(RepositoryId::as_str))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_at(query))
        .bind(cursor_id(query))
        .bind(fetch_limit(query)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        page_from_rows(&rows, query, "started_at_ms", "id", |row| {
            operation_record(row, account_label)
        })
    }
}

fn operation_record(
    row: &sqlx::sqlite::SqliteRow,
    account_label: impl Fn(&AccountProfileRef) -> String + Copy,
) -> Result<UsageDetailRecord, UsageStoreError> {
    let request_id = row.get::<Option<String>, _>("request_id");
    let tool_id = row.get::<Option<String>, _>("tool_id");
    let duration_ns = row
        .get::<Option<i64>, _>("duration_ns")
        .map(u64::try_from)
        .transpose()
        .map_err(|_| UsageStoreError::InvalidFact)?;
    Ok(UsageDetailRecord::Operation(Box::new(
        UsageOperationDetail {
            id: operation(row.get("id"))?,
            process_id: uuid(row.get("process_id"))?,
            thread_id: row
                .get::<Option<String>, _>("thread_id")
                .map(thread)
                .transpose()?,
            turn_id: row
                .get::<Option<String>, _>("turn_id")
                .map(turn)
                .transpose()?,
            agent_id: row
                .get::<Option<String>, _>("agent_id")
                .map(agent)
                .transpose()?,
            parent_operation_id: optional_uuid(row.get("parent_operation_id"))?,
            retry_of_operation_id: optional_uuid(row.get("retry_of_operation_id"))?,
            rework_of_operation_id: optional_uuid(row.get("rework_of_operation_id"))?,
            operation_kind: required_enum(
                row.get("operation_kind"),
                OperationKind::parse,
                OperationKind::as_str,
            )?,
            started_at_ms: row.get("started_at_ms"),
            taxonomy_version: positive(row.get("taxonomy_version"))?,
            phase: required_enum(row.get("phase"), Phase::parse, Phase::as_str)?,
            activity: required_enum(row.get("activity"), Activity::parse, Activity::as_str)?,
            activity_state: required_enum(
                row.get("activity_state"),
                ActivityState::parse,
                ActivityState::as_str,
            )?,
            attribution_provenance: required_enum(
                row.get("attribution_provenance"),
                AttributionProvenance::parse,
                AttributionProvenance::as_str,
            )?,
            account: display_account(row.get("effective_account"), account_label)?,
            account_auth_mode: optional_enum(
                row.get("effective_auth_mode"),
                AccountAuthMode::parse,
                AccountAuthMode::as_str,
            )?,
            terminal_event_id: optional_uuid(row.get("terminal_event_id"))?,
            terminal_status: optional_enum(
                row.get("terminal_status"),
                TerminalStatus::parse,
                TerminalStatus::as_str,
            )?,
            completed_at_ms: row.get("completed_at_ms"),
            duration_ns,
            error_category: optional_enum(
                row.get("error_category"),
                ErrorCategory::parse,
                ErrorCategory::as_str,
            )?,
            model_request: request_id
                .map(|id| {
                    Ok(UsageModelRequestDetail {
                        id: model_request(id)?,
                        provider: ProviderKind::new(row.get::<String, _>("provider_kind"))
                            .map_err(|_| UsageStoreError::InvalidFact)?
                            .as_str()
                            .to_string(),
                        model: ModelName::new(row.get::<String, _>("model"))
                            .map_err(|_| UsageStoreError::InvalidFact)?
                            .as_str()
                            .to_string(),
                        transport: TransportKind::new(row.get::<String, _>("transport_kind"))
                            .map_err(|_| UsageStoreError::InvalidFact)?
                            .as_str()
                            .to_string(),
                        attempt_number: row
                            .get::<i64, _>("attempt_number")
                            .try_into()
                            .map_err(|_| UsageStoreError::InvalidFact)?,
                        account: display_account(row.get("effective_account"), account_label)?,
                        account_auth_mode: optional_enum(
                            row.get("effective_auth_mode"),
                            AccountAuthMode::parse,
                            AccountAuthMode::as_str,
                        )?,
                        client_origin: ClientOrigin::new(row.get::<String, _>("client_origin"))
                            .map_err(|_| UsageStoreError::InvalidFact)?
                            .as_str()
                            .to_string(),
                    })
                })
                .transpose()?,
            tool: tool_id
                .map(|id| {
                    Ok(UsageToolDetail {
                        id: tool_invocation(id)?,
                        tool_kind: ToolKind::new(row.get::<String, _>("tool_kind"))
                            .map_err(|_| UsageStoreError::InvalidFact)?
                            .as_str()
                            .to_string(),
                        safe_tool_name: ToolName::new(row.get::<String, _>("safe_tool_name"))
                            .map_err(|_| UsageStoreError::InvalidFact)?
                            .as_str()
                            .to_string(),
                        operation_family: OperationFamily::new(
                            row.get::<String, _>("operation_family"),
                        )
                        .map_err(|_| UsageStoreError::InvalidFact)?
                        .as_str()
                        .to_string(),
                        observation_timing: ObservationTiming::new(
                            row.get::<String, _>("observation_timing"),
                        )
                        .map_err(|_| UsageStoreError::InvalidFact)?
                        .as_str()
                        .to_string(),
                        covering_model_request_id: row
                            .get::<Option<String>, _>("covering_model_request_id")
                            .map(model_request)
                            .transpose()?,
                    })
                })
                .transpose()?,
        },
    )))
}

#[cfg(test)]
#[path = "detail_query_tests.rs"]
mod tests;
