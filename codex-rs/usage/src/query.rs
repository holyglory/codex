use crate::AccountProfileRef;
use crate::Activity;
use crate::ActivityState;
use crate::AgentId;
use crate::AttributionProvenance;
use crate::CoverageState;
use crate::FactEventId;
use crate::NewClassificationEvent;
use crate::OperationFamily;
use crate::OperationId;
use crate::Phase;
use crate::RepositoryId;
use crate::SafeRepositoryLabel;
use crate::TerminalStatus;
use crate::ThreadId;
use crate::ToolInvocationId;
use crate::ToolName;
use crate::UsageActivityListQuery;
use crate::UsageActivityRecord;
use crate::UsageEventKind;
use crate::UsageEventListQuery;
use crate::UsageEventProvenance;
use crate::UsageEventRecord;
use crate::UsagePage;
use crate::UsagePageCursor;
use crate::UsagePageRequest;
use crate::UsageRepositoryRecord;
use crate::UsageStore;
use crate::UsageStoreError;
use crate::UsageThreadRecord;
use crate::UsageToolListQuery;
use crate::UsageToolRecord;
use sqlx::Row;
use std::collections::HashSet;

const MAX_THREAD_REPOSITORIES: usize = 256;

impl UsageStore {
    pub async fn list_repositories(
        &self,
        page: &UsagePageRequest,
    ) -> Result<UsagePage<UsageRepositoryRecord>, UsageStoreError> {
        let cursor_at = page.cursor.as_ref().map(UsagePageCursor::occurred_at_ms);
        let cursor_id = page.cursor.as_ref().map(UsagePageCursor::id);
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE canonical(id) AS (
                SELECT repository.id FROM repositories AS repository
                WHERE NOT EXISTS (
                    SELECT 1 FROM repository_merge_events AS merge
                    WHERE merge.source_repository_id = repository.id
                )
            ), family(canonical_id, member_id) AS (
                SELECT id, id FROM canonical
                UNION ALL
                SELECT family.canonical_id, merge.source_repository_id
                FROM family
                JOIN repository_merge_events AS merge
                  ON merge.target_repository_id = family.member_id
            ), canonical_rows(id, created_at_ms) AS (
                SELECT family.canonical_id, MIN(repository.created_at_ms)
                FROM family
                JOIN repositories AS repository ON repository.id = family.member_id
                GROUP BY family.canonical_id
            ), updates(id, occurred_at_ms) AS (
                SELECT family.canonical_id, repository.created_at_ms
                FROM family
                JOIN repositories AS repository ON repository.id = family.member_id
                UNION ALL
                SELECT family.canonical_id, seen.occurred_at_ms
                FROM family
                JOIN repository_seen_events AS seen ON seen.repository_id = family.member_id
                UNION ALL
                SELECT family.canonical_id, alias.occurred_at_ms
                FROM family
                JOIN repository_alias_events AS alias ON alias.repository_id = family.member_id
                UNION ALL
                SELECT family.canonical_id, merge.occurred_at_ms
                FROM family
                JOIN repository_merge_events AS merge
                  ON merge.source_repository_id = family.member_id
            ), repository_rows(id, created_at_ms, updated_at_ms) AS (
                SELECT canonical_rows.id, canonical_rows.created_at_ms,
                       MAX(updates.occurred_at_ms)
                FROM canonical_rows
                JOIN updates ON updates.id = canonical_rows.id
                GROUP BY canonical_rows.id, canonical_rows.created_at_ms
            )
            SELECT id, created_at_ms, updated_at_ms FROM repository_rows
            WHERE (? IS NULL OR updated_at_ms < ? OR (updated_at_ms = ? AND id > ?))
            ORDER BY updated_at_ms DESC, id ASC
            LIMIT ?
            "#,
        )
        .bind(cursor_at)
        .bind(cursor_at)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(page_fetch_limit(page.limit)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;

        let mut data = Vec::with_capacity(rows.len().min(page.limit as usize));
        for row in rows.iter().take(page.limit as usize) {
            let id = repository_id(row.get("id"))?;
            let label = validated_label(self.repository_display_label(&id).await?)?;
            data.push(UsageRepositoryRecord {
                id,
                label,
                created_at_ms: row.get("created_at_ms"),
                updated_at_ms: row.get("updated_at_ms"),
            });
        }
        let next_cursor = next_cursor(&rows, page.limit, "updated_at_ms", "id")?;
        Ok(UsagePage { data, next_cursor })
    }

    pub async fn read_repository(
        &self,
        id: &RepositoryId,
    ) -> Result<Option<UsageRepositoryRecord>, UsageStoreError> {
        if !self.repository_exists(id).await? {
            return Ok(None);
        }
        let canonical = self.canonical_repository_id(id).await?;
        let row = sqlx::query(
            r#"
            WITH RECURSIVE family(id) AS (
                SELECT ?
                UNION ALL
                SELECT merge.source_repository_id
                FROM family
                JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
            ), updates(occurred_at_ms) AS (
                SELECT repository.created_at_ms FROM repositories AS repository
                WHERE repository.id IN (SELECT id FROM family)
                UNION ALL SELECT seen.occurred_at_ms FROM repository_seen_events AS seen
                WHERE seen.repository_id IN (SELECT id FROM family)
                UNION ALL SELECT alias.occurred_at_ms FROM repository_alias_events AS alias
                WHERE alias.repository_id IN (SELECT id FROM family)
                UNION ALL SELECT merge.occurred_at_ms FROM repository_merge_events AS merge
                WHERE merge.source_repository_id IN (SELECT id FROM family)
            )
            SELECT MIN(repository.created_at_ms) AS created_at_ms,
                   (SELECT MAX(occurred_at_ms) FROM updates) AS updated_at_ms
            FROM repositories AS repository
            WHERE repository.id IN (SELECT id FROM family)
            "#,
        )
        .bind(canonical.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let label = validated_label(self.repository_display_label(&canonical).await?)?;
        Ok(Some(UsageRepositoryRecord {
            id: canonical,
            label,
            created_at_ms: row.get("created_at_ms"),
            updated_at_ms: row.get("updated_at_ms"),
        }))
    }

    pub async fn read_thread(
        &self,
        id: &ThreadId,
    ) -> Result<Option<UsageThreadRecord>, UsageStoreError> {
        let row = sqlx::query(
            r#"
            SELECT thread.created_at_ms,
                   MAX(
                       thread.created_at_ms,
                       COALESCE((SELECT MAX(event.occurred_at_ms)
                                 FROM thread_events AS event
                                 WHERE event.thread_id = thread.id), thread.created_at_ms),
                       COALESCE((SELECT MAX(operation.started_at_ms)
                                 FROM operations AS operation
                                 WHERE operation.thread_id = thread.id), thread.created_at_ms),
                       COALESCE((SELECT MAX(terminal.occurred_at_ms)
                                 FROM operation_events AS terminal
                                 JOIN operations AS operation
                                   ON operation.id = terminal.operation_id
                                 WHERE operation.thread_id = thread.id), thread.created_at_ms),
                       COALESCE((SELECT MAX(token.observed_at_ms)
                                 FROM token_observations AS token
                                 LEFT JOIN model_requests AS request
                                   ON request.id = token.model_request_id
                                 LEFT JOIN tool_invocations AS tool
                                   ON tool.id = token.tool_invocation_id
                                 JOIN operations AS operation
                                   ON operation.id = COALESCE(request.operation_id, tool.operation_id)
                                 WHERE operation.thread_id = thread.id), thread.created_at_ms)
                   ) AS updated_at_ms
            FROM threads AS thread
            WHERE thread.id = ?
            "#,
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let repository_rows = sqlx::query_scalar::<_, String>(
            r#"
            SELECT DISTINCT attribution.repository_id
            FROM repository_attributions AS attribution
            JOIN operations AS operation ON operation.id = attribution.operation_id
            WHERE operation.thread_id = ? AND attribution.repository_id IS NOT NULL
            LIMIT ?
            "#,
        )
        .bind(id.as_str())
        .bind(i64::try_from(MAX_THREAD_REPOSITORIES + 1).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        if repository_rows.len() > MAX_THREAD_REPOSITORIES {
            return Err(UsageStoreError::DatabaseValueOutOfRange);
        }
        let mut repository_ids = Vec::new();
        let mut seen = HashSet::new();
        for raw in repository_rows {
            let canonical = self.canonical_repository_id(&repository_id(raw)?).await?;
            if seen.insert(canonical.as_str().to_string()) {
                repository_ids.push(canonical);
            }
        }
        repository_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let account_profile_ref = sqlx::query_scalar::<_, String>(
            r#"
            SELECT turn.account_profile_ref FROM turns AS turn
            WHERE turn.thread_id = ? AND turn.account_profile_ref IS NOT NULL
            ORDER BY turn.created_at_ms DESC, turn.id DESC LIMIT 1
            "#,
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?
        .map(AccountProfileRef::new)
        .transpose()
        .map_err(|_| UsageStoreError::InvalidFact)?;
        Ok(Some(UsageThreadRecord {
            id: id.clone(),
            repository_ids,
            account_profile_ref,
            created_at_ms: row.get("created_at_ms"),
            updated_at_ms: row.get("updated_at_ms"),
        }))
    }

    pub async fn list_tools(
        &self,
        query: &UsageToolListQuery,
    ) -> Result<UsagePage<UsageToolRecord>, UsageStoreError> {
        let cursor_at = query
            .page
            .cursor
            .as_ref()
            .map(UsagePageCursor::occurred_at_ms);
        let cursor_id = query.page.cursor.as_ref().map(UsagePageCursor::id);
        let repository = match &query.repository_id {
            Some(repository) => Some(self.canonical_repository_id(repository).await?),
            None => None,
        };
        let repository = repository.as_ref().map(RepositoryId::as_str);
        let rows = sqlx::query(
            r#"
            SELECT tool.id, tool.operation_id, operation.thread_id,
                   tool.safe_tool_name, tool.operation_family, operation.started_at_ms,
                   terminal.occurred_at_ms, terminal.event_kind,
                   COALESCE(classification.provenance, operation.attribution_provenance) AS provenance
            FROM tool_invocations AS tool
            JOIN operations AS operation ON operation.id = tool.operation_id
            LEFT JOIN operation_events AS terminal
              ON terminal.operation_id = operation.id AND terminal.terminal = 1
            LEFT JOIN effective_classification_events AS classification
              ON classification.operation_id = operation.id
            WHERE operation.thread_id IS NOT NULL
              AND (? IS NULL OR operation.thread_id = ?)
              AND (? IS NULL OR operation.started_at_ms >= ?)
              AND (? IS NULL OR operation.started_at_ms < ?)
              AND (? IS NULL OR EXISTS (
                  WITH RECURSIVE family(id) AS (
                      SELECT ? UNION ALL
                      SELECT merge.source_repository_id FROM family
                      JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
                  )
                  SELECT 1 FROM repository_attributions AS attribution
                  WHERE attribution.operation_id = operation.id
                    AND attribution.repository_id IN (SELECT id FROM family)
              ))
              AND (? IS NULL OR operation.started_at_ms < ?
                   OR (operation.started_at_ms = ? AND tool.id > ?))
            ORDER BY operation.started_at_ms DESC, tool.id ASC
            LIMIT ?
            "#,
        )
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(repository)
        .bind(repository)
        .bind(cursor_at)
        .bind(cursor_at)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(page_fetch_limit(query.page.limit)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let mut data = Vec::with_capacity(rows.len().min(query.page.limit as usize));
        for row in rows.iter().take(query.page.limit as usize) {
            let operation_id: String = row.get("operation_id");
            data.push(UsageToolRecord {
                id: ToolInvocationId::from_string(row.get("id"))
                    .ok_or(UsageStoreError::InvalidFact)?,
                thread_id: ThreadId::new(row.get::<String, _>("thread_id"))
                    .map_err(|_| UsageStoreError::InvalidFact)?,
                repository_id: self.operation_repository_id(&operation_id).await?,
                tool_name: ToolName::new(row.get::<String, _>("safe_tool_name"))
                    .map_err(|_| UsageStoreError::InvalidFact)?,
                operation_family: OperationFamily::new(row.get::<String, _>("operation_family"))
                    .map_err(|_| UsageStoreError::InvalidFact)?,
                started_at_ms: row.get("started_at_ms"),
                completed_at_ms: row.get("occurred_at_ms"),
                status: parse_optional(
                    row.get::<Option<String>, _>("event_kind"),
                    TerminalStatus::parse,
                )?,
                provenance: AttributionProvenance::parse(row.get("provenance"))
                    .ok_or(UsageStoreError::InvalidFact)?,
            });
        }
        let next_cursor = next_cursor(&rows, query.page.limit, "started_at_ms", "id")?;
        Ok(UsagePage { data, next_cursor })
    }

    pub async fn list_activities(
        &self,
        query: &UsageActivityListQuery,
    ) -> Result<UsagePage<UsageActivityRecord>, UsageStoreError> {
        let cursor_at = query
            .page
            .cursor
            .as_ref()
            .map(UsagePageCursor::occurred_at_ms);
        let cursor_id = query.page.cursor.as_ref().map(UsagePageCursor::id);
        let rows = sqlx::query(
            r#"
            SELECT operation.id, operation.thread_id, operation.agent_id,
                   COALESCE(classification.phase, operation.phase) AS phase,
                   COALESCE(classification.activity, operation.activity) AS activity,
                   COALESCE(classification.activity_state, operation.activity_state) AS activity_state,
                   COALESCE(classification.provenance, operation.attribution_provenance) AS provenance,
                   operation.started_at_ms, terminal.occurred_at_ms
            FROM operations AS operation
            LEFT JOIN operation_events AS terminal
              ON terminal.operation_id = operation.id AND terminal.terminal = 1
            LEFT JOIN effective_classification_events AS classification
              ON classification.operation_id = operation.id
            WHERE operation.thread_id IS NOT NULL AND operation.agent_id IS NOT NULL
              AND (? IS NULL OR operation.thread_id = ?)
              AND (? IS NULL OR operation.agent_id = ?)
              AND (? IS NULL OR operation.started_at_ms >= ?)
              AND (? IS NULL OR operation.started_at_ms < ?)
              AND (? IS NULL OR operation.started_at_ms < ?
                   OR (operation.started_at_ms = ? AND operation.id > ?))
            ORDER BY operation.started_at_ms DESC, operation.id ASC
            LIMIT ?
            "#,
        )
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.agent_id.as_ref().map(AgentId::as_str))
        .bind(query.agent_id.as_ref().map(AgentId::as_str))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(cursor_at)
        .bind(cursor_at)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(page_fetch_limit(query.page.limit)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let data = rows
            .iter()
            .take(query.page.limit as usize)
            .map(activity_record)
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = next_cursor(&rows, query.page.limit, "started_at_ms", "id")?;
        Ok(UsagePage { data, next_cursor })
    }

    pub async fn list_events(
        &self,
        query: &UsageEventListQuery,
    ) -> Result<UsagePage<UsageEventRecord>, UsageStoreError> {
        let cursor_at = query
            .page
            .cursor
            .as_ref()
            .map(UsagePageCursor::occurred_at_ms);
        let cursor_id = query.page.cursor.as_ref().map(UsagePageCursor::id);
        let repository = match &query.repository_id {
            Some(repository) => Some(self.canonical_repository_id(repository).await?),
            None => None,
        };
        let repository = repository.as_ref().map(RepositoryId::as_str);
        let kind = query.kind.map(UsageEventKind::as_str);
        let rows = sqlx::query(
            r#"
            WITH usage_events(event_id, operation_id, thread_id, occurred_at_ms, kind, provenance, coverage_state) AS (
                SELECT operation.id, operation.id, operation.thread_id, operation.started_at_ms,
                       CASE operation.operation_kind
                           WHEN 'model_request' THEN 'model_request_started'
                           WHEN 'activity_control' THEN 'activity_changed'
                           ELSE 'tool_started'
                       END,
                       operation.attribution_provenance,
                       COALESCE((SELECT coverage.coverage_state FROM coverage_events AS coverage
                                 WHERE coverage.operation_id = operation.id
                                 ORDER BY coverage.occurred_at_ms DESC, coverage.event_id DESC LIMIT 1),
                                'unknown')
                FROM operations AS operation
                UNION ALL
                SELECT terminal.event_id, operation.id, operation.thread_id, terminal.occurred_at_ms,
                       CASE operation.operation_kind
                           WHEN 'model_request' THEN 'model_request_completed'
                           WHEN 'activity_control' THEN 'activity_changed'
                           ELSE 'tool_completed'
                       END,
                       operation.attribution_provenance,
                       COALESCE((SELECT coverage.coverage_state FROM coverage_events AS coverage
                                 WHERE coverage.operation_id = operation.id
                                 ORDER BY coverage.occurred_at_ms DESC, coverage.event_id DESC LIMIT 1),
                                'unknown')
                FROM operation_events AS terminal
                JOIN operations AS operation ON operation.id = terminal.operation_id
                WHERE terminal.terminal = 1
                UNION ALL
                SELECT classification.event_id, operation.id, operation.thread_id,
                       classification.occurred_at_ms,
                       CASE classification.provenance WHEN 'user_corrected'
                           THEN 'classification_corrected' ELSE 'activity_changed' END,
                       classification.provenance,
                       COALESCE((SELECT coverage.coverage_state FROM coverage_events AS coverage
                                 WHERE coverage.operation_id = operation.id
                                 ORDER BY coverage.occurred_at_ms DESC, coverage.event_id DESC LIMIT 1),
                                'unknown')
                FROM classification_events AS classification
                JOIN operations AS operation ON operation.id = classification.operation_id
                UNION ALL
                SELECT coverage.event_id, coverage.operation_id, operation.thread_id,
                       coverage.occurred_at_ms, 'coverage_gap', 'runtime_observed',
                       coverage.coverage_state
                FROM coverage_events AS coverage
                LEFT JOIN operations AS operation ON operation.id = coverage.operation_id
                WHERE coverage.coverage_state NOT IN ('capture_started', 'complete')
            )
            SELECT event_id, operation_id, thread_id, occurred_at_ms, kind, provenance, coverage_state
            FROM usage_events AS event
            WHERE (? IS NULL OR event.thread_id = ?)
              AND (? IS NULL OR event.occurred_at_ms >= ?)
              AND (? IS NULL OR event.occurred_at_ms < ?)
              AND (? IS NULL OR event.kind = ?)
              AND (? IS NULL OR EXISTS (
                  WITH RECURSIVE family(id) AS (
                      SELECT ? UNION ALL
                      SELECT merge.source_repository_id FROM family
                      JOIN repository_merge_events AS merge ON merge.target_repository_id = family.id
                  )
                  SELECT 1 FROM repository_attributions AS attribution
                  WHERE attribution.operation_id = event.operation_id
                    AND attribution.repository_id IN (SELECT id FROM family)
              ))
              AND (? IS NULL OR event.occurred_at_ms < ?
                   OR (event.occurred_at_ms = ? AND event.event_id > ?))
            ORDER BY event.occurred_at_ms DESC, event.event_id ASC
            LIMIT ?
            "#,
        )
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.thread_id.as_ref().map(ThreadId::as_str))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::start_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(query.time_range.map(crate::UtcTimeRange::end_ms))
        .bind(kind)
        .bind(kind)
        .bind(repository)
        .bind(repository)
        .bind(cursor_at)
        .bind(cursor_at)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(page_fetch_limit(query.page.limit)?)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let mut data = Vec::with_capacity(rows.len().min(query.page.limit as usize));
        for row in rows.iter().take(query.page.limit as usize) {
            data.push(self.event_record(row).await?);
        }
        let next_cursor = next_cursor(&rows, query.page.limit, "occurred_at_ms", "event_id")?;
        Ok(UsagePage { data, next_cursor })
    }

    pub async fn correct_classification(
        &self,
        target_event_id: FactEventId,
        phase: Phase,
        activity: Activity,
        occurred_at_ms: i64,
    ) -> Result<Option<UsageEventRecord>, UsageStoreError> {
        let target = target_event_id.as_string();
        let operation_id = sqlx::query_scalar::<_, String>(
            r#"
            SELECT operation_id FROM (
                SELECT id AS event_id, id AS operation_id FROM operations
                UNION ALL SELECT event_id, operation_id FROM operation_events
                UNION ALL SELECT event_id, operation_id FROM classification_events
                UNION ALL SELECT event_id, operation_id FROM coverage_events
            ) WHERE event_id = ? LIMIT 1
            "#,
        )
        .bind(&target)
        .fetch_optional(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let Some(operation_id) = operation_id else {
            return Ok(None);
        };
        let current = sqlx::query(
            r#"
            SELECT operation.activity_state,
                   classification.event_id,
                   classification.activity_state AS corrected_state
            FROM operations AS operation
            LEFT JOIN effective_classification_events AS classification
              ON classification.operation_id = operation.id
            WHERE operation.id = ?
            ORDER BY classification.occurred_at_ms DESC, classification.event_id DESC
            LIMIT 1
            "#,
        )
        .bind(&operation_id)
        .fetch_one(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let state = current
            .get::<Option<String>, _>("corrected_state")
            .unwrap_or_else(|| current.get("activity_state"));
        let state = ActivityState::parse(&state).ok_or(UsageStoreError::InvalidFact)?;
        let supersedes_event_id = current
            .get::<Option<String>, _>("event_id")
            .map(|id| FactEventId::from_string(&id).ok_or(UsageStoreError::InvalidFact))
            .transpose()?;
        let event_id = FactEventId::new();
        self.record_classification(&NewClassificationEvent {
            event_id,
            operation_id: OperationId::from_string(&operation_id)
                .ok_or(UsageStoreError::InvalidFact)?,
            phase,
            activity,
            activity_state: state,
            provenance: AttributionProvenance::UserCorrected,
            supersedes_event_id,
            occurred_at_ms,
        })
        .await?;
        let row = sqlx::query(
            r#"
            SELECT operation.thread_id,
                   COALESCE((SELECT coverage.coverage_state FROM coverage_events AS coverage
                             WHERE coverage.operation_id = operation.id
                             ORDER BY coverage.occurred_at_ms DESC, coverage.event_id DESC LIMIT 1),
                            'unknown') AS coverage_state
            FROM operations AS operation WHERE operation.id = ?
            "#,
        )
        .bind(&operation_id)
        .fetch_one(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        Ok(Some(UsageEventRecord {
            id: event_id,
            thread_id: row
                .get::<Option<String>, _>("thread_id")
                .map(ThreadId::new)
                .transpose()
                .map_err(|_| UsageStoreError::InvalidFact)?,
            repository_id: self.operation_repository_id(&operation_id).await?,
            occurred_at_ms,
            kind: UsageEventKind::ClassificationCorrected,
            provenance: UsageEventProvenance::UserCorrected,
            coverage: CoverageState::parse(row.get("coverage_state"))
                .ok_or(UsageStoreError::InvalidFact)?,
        }))
    }

    pub async fn repository_exists(&self, id: &RepositoryId) -> Result<bool, UsageStoreError> {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM repositories WHERE id = ?)")
            .bind(id.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(UsageStoreError::Database)
    }

    async fn operation_repository_id(
        &self,
        operation_id: &str,
    ) -> Result<Option<RepositoryId>, UsageStoreError> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT repository_id FROM repository_attributions WHERE operation_id = ? AND repository_id IS NOT NULL LIMIT 2",
        )
        .bind(operation_id)
        .fetch_all(&self.pool)
        .await
        .map_err(UsageStoreError::Database)?;
        let mut canonical = HashSet::new();
        let mut selected = None;
        for row in rows {
            let id = self.canonical_repository_id(&repository_id(row)?).await?;
            if canonical.insert(id.as_str().to_string()) {
                if selected.is_some() {
                    return Ok(None);
                }
                selected = Some(id);
            }
        }
        Ok(selected)
    }

    async fn event_record(
        &self,
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<UsageEventRecord, UsageStoreError> {
        let operation_id: Option<String> = row.get("operation_id");
        Ok(UsageEventRecord {
            id: FactEventId::from_string(row.get("event_id"))
                .ok_or(UsageStoreError::InvalidFact)?,
            thread_id: row
                .get::<Option<String>, _>("thread_id")
                .map(ThreadId::new)
                .transpose()
                .map_err(|_| UsageStoreError::InvalidFact)?,
            repository_id: match operation_id {
                Some(operation_id) => self.operation_repository_id(&operation_id).await?,
                None => None,
            },
            occurred_at_ms: row.get("occurred_at_ms"),
            kind: UsageEventKind::parse(row.get("kind")).ok_or(UsageStoreError::InvalidFact)?,
            provenance: UsageEventProvenance::parse(row.get("provenance"))
                .ok_or(UsageStoreError::InvalidFact)?,
            coverage: CoverageState::parse(row.get("coverage_state"))
                .ok_or(UsageStoreError::InvalidFact)?,
        })
    }
}

fn activity_record(row: &sqlx::sqlite::SqliteRow) -> Result<UsageActivityRecord, UsageStoreError> {
    Ok(UsageActivityRecord {
        id: OperationId::from_string(row.get("id")).ok_or(UsageStoreError::InvalidFact)?,
        thread_id: ThreadId::new(row.get::<String, _>("thread_id"))
            .map_err(|_| UsageStoreError::InvalidFact)?,
        agent_id: AgentId::new(row.get::<String, _>("agent_id"))
            .map_err(|_| UsageStoreError::InvalidFact)?,
        phase: Phase::parse(row.get("phase")).ok_or(UsageStoreError::InvalidFact)?,
        activity: Activity::parse(row.get("activity")).ok_or(UsageStoreError::InvalidFact)?,
        state: ActivityState::parse(row.get("activity_state"))
            .ok_or(UsageStoreError::InvalidFact)?,
        started_at_ms: row.get("started_at_ms"),
        ended_at_ms: row.get("occurred_at_ms"),
        provenance: AttributionProvenance::parse(row.get("provenance"))
            .ok_or(UsageStoreError::InvalidFact)?,
    })
}

fn page_fetch_limit(limit: u32) -> Result<i64, UsageStoreError> {
    i64::from(limit)
        .checked_add(1)
        .ok_or(UsageStoreError::DatabaseValueOutOfRange)
}

fn next_cursor(
    rows: &[sqlx::sqlite::SqliteRow],
    limit: u32,
    timestamp_column: &str,
    id_column: &str,
) -> Result<Option<UsagePageCursor>, UsageStoreError> {
    if rows.len() <= limit as usize {
        return Ok(None);
    }
    let row = rows
        .get(limit.saturating_sub(1) as usize)
        .ok_or(UsageStoreError::DatabaseValueOutOfRange)?;
    UsagePageCursor::new(row.get(timestamp_column), row.get::<String, _>(id_column))
        .ok_or(UsageStoreError::InvalidFact)
        .map(Some)
}

fn repository_id(value: String) -> Result<RepositoryId, UsageStoreError> {
    RepositoryId::new(value).map_err(|_| UsageStoreError::InvalidFact)
}

fn validated_label(value: String) -> Result<String, UsageStoreError> {
    SafeRepositoryLabel::new(value)
        .map(|label| label.as_str().to_string())
        .map_err(|_| UsageStoreError::InvalidFact)
}

fn parse_optional<T>(
    value: Option<String>,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<Option<T>, UsageStoreError> {
    value
        .map(|value| parse(&value).ok_or(UsageStoreError::InvalidFact))
        .transpose()
}
