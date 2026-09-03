mod export;
mod mapping;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::outgoing_message::OutgoingMessageSender;
use codex_account_registry::RegistryStore;
use codex_account_registry::RegistryStoreError;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LocalUsageActivityKind;
use codex_app_server_protocol::LocalUsageActivityListParams;
use codex_app_server_protocol::LocalUsageActivityListResponse;
use codex_app_server_protocol::LocalUsageClassificationCorrectParams;
use codex_app_server_protocol::LocalUsageClassificationCorrectResponse;
use codex_app_server_protocol::LocalUsageEventKind;
use codex_app_server_protocol::LocalUsageEventListParams;
use codex_app_server_protocol::LocalUsageEventListResponse;
use codex_app_server_protocol::LocalUsageExportCreateParams;
use codex_app_server_protocol::LocalUsageExportCreateResponse;
use codex_app_server_protocol::LocalUsagePhase;
use codex_app_server_protocol::LocalUsageReport;
use codex_app_server_protocol::LocalUsageRepository;
use codex_app_server_protocol::LocalUsageRepositoryListParams;
use codex_app_server_protocol::LocalUsageRepositoryListResponse;
use codex_app_server_protocol::LocalUsageRepositoryMergeParams;
use codex_app_server_protocol::LocalUsageRepositoryMergeResponse;
use codex_app_server_protocol::LocalUsageRepositoryReadParams;
use codex_app_server_protocol::LocalUsageRepositoryReadResponse;
use codex_app_server_protocol::LocalUsageRepositoryUpdateParams;
use codex_app_server_protocol::LocalUsageRepositoryUpdateResponse;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_app_server_protocol::LocalUsageThread;
use codex_app_server_protocol::LocalUsageThreadReadParams;
use codex_app_server_protocol::LocalUsageThreadReadResponse;
use codex_app_server_protocol::LocalUsageToolListParams;
use codex_app_server_protocol::LocalUsageToolListResponse;
use codex_app_server_protocol::LocalUsageUpdatedNotification;
use codex_app_server_protocol::ServerNotification;
use codex_usage::AccountProfileRef;
use codex_usage::Activity;
use codex_usage::AgentId;
use codex_usage::FactEventId;
use codex_usage::Phase;
use codex_usage::RepositoryId;
use codex_usage::SafeRepositoryLabel;
use codex_usage::ThreadId;
use codex_usage::UsageActivityListQuery;
use codex_usage::UsageEventKind;
use codex_usage::UsageEventListQuery;
use codex_usage::UsagePageCursor;
use codex_usage::UsagePageRequest;
use codex_usage::UsageRepositoryRecord;
use codex_usage::UsageStore;
use codex_usage::UsageStoreError;
use codex_usage::UsageSummaryQuery;
use codex_usage::UsageThreadRecord;
use codex_usage::UsageToolListQuery;
use codex_usage::UtcTimeRange;
use codex_usage::redacted_account_profile_label;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::OnceCell;
use uuid::Uuid;

const DEFAULT_PAGE_LIMIT: u32 = 50;
const MAX_PAGE_LIMIT: u32 = 100;
const MAX_CURSOR_BYTES: usize = 320;

#[derive(Clone)]
pub(crate) struct LocalUsageRequestProcessor {
    codex_home: PathBuf,
    store: Arc<OnceCell<Arc<UsageStore>>>,
    outgoing: Arc<OutgoingMessageSender>,
    generation: Arc<AtomicU64>,
}

impl LocalUsageRequestProcessor {
    pub(crate) fn new(codex_home: PathBuf, outgoing: Arc<OutgoingMessageSender>) -> Self {
        Self {
            codex_home,
            store: Arc::new(OnceCell::new()),
            outgoing,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) async fn summary(
        &self,
        params: LocalUsageSummaryParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        Ok(Some(
            self.summary_response(
                &store,
                SummaryFilters {
                    repository_key: params.repository_key,
                    thread_id: params.thread_id,
                    account_id: params.account_id,
                    from_at: params.from_at,
                    to_at: params.to_at,
                },
            )
            .await?
            .into(),
        ))
    }

    pub(crate) async fn thread_read(
        &self,
        params: LocalUsageThreadReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let id = thread_id(params.thread_id)?;
        let record = store
            .read_thread(&id)
            .await
            .map_err(store_error)?
            .ok_or_else(resource_not_found)?;
        let summary = store
            .usage_summary_query(UsageSummaryQuery {
                thread_id: Some(id),
                repository_id: None,
                account_profile_ref: None,
                time_range: None,
            })
            .await
            .map_err(store_error)?;
        let (aggregate, token_categories) = mapping::summary(&summary).map_err(store_error)?;
        let account = record
            .account_profile_ref
            .as_ref()
            .map(|account| self.account_label(account))
            .transpose()?;
        let report = self.report(&store, &summary, account).await?;
        Ok(Some(
            LocalUsageThreadReadResponse {
                thread: api_thread(record, aggregate),
                token_categories,
                report,
            }
            .into(),
        ))
    }

    pub(crate) async fn repository_list(
        &self,
        params: LocalUsageRepositoryListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let page = page("repository", params.cursor, params.limit)?;
        let records = store.list_repositories(&page).await.map_err(store_error)?;
        let mut data = Vec::with_capacity(records.data.len());
        for record in records.data {
            data.push(self.api_repository(&store, record).await?);
        }
        Ok(Some(
            LocalUsageRepositoryListResponse {
                data,
                next_cursor: records
                    .next_cursor
                    .map(|cursor| encode_cursor("repository", &cursor)),
            }
            .into(),
        ))
    }

    pub(crate) async fn repository_read(
        &self,
        params: LocalUsageRepositoryReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let id = repository_id(params.repository_key)?;
        let record = store
            .read_repository(&id)
            .await
            .map_err(store_error)?
            .ok_or_else(resource_not_found)?;
        let summary = store
            .usage_summary_query(UsageSummaryQuery {
                thread_id: None,
                repository_id: Some(record.id.clone()),
                account_profile_ref: None,
                time_range: None,
            })
            .await
            .map_err(store_error)?;
        let (aggregate, token_categories) = mapping::summary(&summary).map_err(store_error)?;
        let report = self.report(&store, &summary, /*account*/ None).await?;
        Ok(Some(
            LocalUsageRepositoryReadResponse {
                repository: api_repository_record(record, aggregate),
                token_categories,
                report,
            }
            .into(),
        ))
    }

    pub(crate) async fn repository_update(
        &self,
        params: LocalUsageRepositoryUpdateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let id = repository_id(params.repository_key)?;
        let canonical = store
            .read_repository(&id)
            .await
            .map_err(store_error)?
            .ok_or_else(resource_not_found)?
            .id;
        let label = SafeRepositoryLabel::new(params.label)
            .map_err(|_| invalid_params("repository label is invalid"))?;
        store
            .append_repository_alias(FactEventId::new(), &canonical, &label, now_ms())
            .await
            .map_err(store_error)?;
        let record = store
            .read_repository(&canonical)
            .await
            .map_err(store_error)?
            .ok_or_else(resource_not_found)?;
        let repository = self.api_repository(&store, record).await?;
        self.notify(
            /*thread_id*/ None,
            Some(repository.repository_key.clone()),
        )
        .await;
        Ok(Some(
            LocalUsageRepositoryUpdateResponse { repository }.into(),
        ))
    }

    pub(crate) async fn repository_merge(
        &self,
        params: LocalUsageRepositoryMergeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let source =
            canonical_repository(&store, repository_id(params.source_repository_key)?).await?;
        let target =
            canonical_repository(&store, repository_id(params.target_repository_key)?).await?;
        if source == target {
            return Err(invalid_params("repository merge would create a cycle"));
        }
        store
            .append_repository_merge(FactEventId::new(), &source, &target, now_ms())
            .await
            .map_err(store_error)?;
        let record = store
            .read_repository(&target)
            .await
            .map_err(store_error)?
            .ok_or_else(resource_not_found)?;
        let repository = self.api_repository(&store, record).await?;
        self.notify(
            /*thread_id*/ None,
            Some(repository.repository_key.clone()),
        )
        .await;
        Ok(Some(
            LocalUsageRepositoryMergeResponse { repository }.into(),
        ))
    }

    pub(crate) async fn tool_list(
        &self,
        params: LocalUsageToolListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let page = store
            .list_tools(&UsageToolListQuery {
                page: page("tool", params.cursor, params.limit)?,
                time_range: time_range(params.from_at, params.to_at)?,
                thread_id: params.thread_id.map(thread_id).transpose()?,
                repository_id: params.repository_key.map(repository_id).transpose()?,
            })
            .await
            .map_err(store_error)?;
        Ok(Some(
            LocalUsageToolListResponse {
                data: page.data.into_iter().map(mapping::tool).collect(),
                next_cursor: page
                    .next_cursor
                    .map(|cursor| encode_cursor("tool", &cursor)),
            }
            .into(),
        ))
    }

    pub(crate) async fn activity_list(
        &self,
        params: LocalUsageActivityListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let page = store
            .list_activities(&UsageActivityListQuery {
                page: page("activity", params.cursor, params.limit)?,
                time_range: time_range(params.from_at, params.to_at)?,
                thread_id: params.thread_id.map(thread_id).transpose()?,
                agent_id: params
                    .agent_id
                    .map(|id| AgentId::new(id).map_err(|_| invalid_identifier()))
                    .transpose()?,
            })
            .await
            .map_err(store_error)?;
        Ok(Some(
            LocalUsageActivityListResponse {
                data: page.data.into_iter().map(mapping::activity).collect(),
                next_cursor: page
                    .next_cursor
                    .map(|cursor| encode_cursor("activity", &cursor)),
            }
            .into(),
        ))
    }

    pub(crate) async fn event_list(
        &self,
        params: LocalUsageEventListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let page = store
            .list_events(&UsageEventListQuery {
                page: page("event", params.cursor, params.limit)?,
                time_range: time_range(params.from_at, params.to_at)?,
                thread_id: params.thread_id.map(thread_id).transpose()?,
                repository_id: params.repository_key.map(repository_id).transpose()?,
                kind: params.kind.map(usage_event_kind),
            })
            .await
            .map_err(store_error)?;
        Ok(Some(
            LocalUsageEventListResponse {
                data: page.data.into_iter().map(mapping::event).collect(),
                next_cursor: page
                    .next_cursor
                    .map(|cursor| encode_cursor("event", &cursor)),
            }
            .into(),
        ))
    }

    pub(crate) async fn classification_correct(
        &self,
        params: LocalUsageClassificationCorrectParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let target = FactEventId::from_string(&params.event_id).ok_or_else(invalid_identifier)?;
        let event = store
            .correct_classification(
                target,
                usage_phase(params.phase),
                usage_activity(params.activity),
                now_ms(),
            )
            .await
            .map_err(store_error)?
            .ok_or_else(resource_not_found)?;
        let event = mapping::event(event);
        self.notify(event.thread_id.clone(), event.repository_key.clone())
            .await;
        Ok(Some(
            LocalUsageClassificationCorrectResponse { event }.into(),
        ))
    }

    pub(crate) async fn export_create(
        &self,
        params: LocalUsageExportCreateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.store().await?;
        let created_at = now_seconds();
        let response = self
            .summary_response(
                &store,
                SummaryFilters {
                    repository_key: params.repository_key,
                    thread_id: params.thread_id,
                    account_id: None,
                    from_at: params.from_at,
                    to_at: params.to_at,
                },
            )
            .await?;
        let exported =
            export::create(&params.output_path, params.format, &response).map_err(|error| {
                if error.committed() {
                    internal_error("local usage export was committed but durability is uncertain")
                } else if error.invalid_destination() {
                    invalid_params("local usage export destination is invalid or unavailable")
                } else {
                    internal_error("local usage export could not be created")
                }
            })?;
        Ok(Some(
            LocalUsageExportCreateResponse {
                export_id: Uuid::now_v7().to_string(),
                created_at,
                file_name: exported.file_name,
            }
            .into(),
        ))
    }

    async fn store(&self) -> Result<Arc<UsageStore>, JSONRPCErrorError> {
        self.store
            .get_or_try_init(|| async {
                UsageStore::open(&self.codex_home)
                    .await
                    .map(Arc::new)
                    .map_err(store_error)
            })
            .await
            .map(Arc::clone)
    }

    async fn summary_response(
        &self,
        store: &UsageStore,
        filters: SummaryFilters,
    ) -> Result<LocalUsageSummaryResponse, JSONRPCErrorError> {
        let account_profile_ref = filters
            .account_id
            .map(|id| AccountProfileRef::new(id).map_err(|_| invalid_identifier()))
            .transpose()?;
        let account = account_profile_ref
            .as_ref()
            .map(|account| self.account_label(account))
            .transpose()?;
        let summary = store
            .usage_summary_query(UsageSummaryQuery {
                thread_id: filters.thread_id.map(thread_id).transpose()?,
                repository_id: filters.repository_key.map(repository_id).transpose()?,
                account_profile_ref,
                time_range: time_range(filters.from_at, filters.to_at)?,
            })
            .await
            .map_err(store_error)?;
        let (aggregate, token_categories) = mapping::summary(&summary).map_err(store_error)?;
        let report = self.report(store, &summary, account).await?;
        Ok(LocalUsageSummaryResponse {
            aggregate,
            token_categories,
            report,
            generated_at: now_seconds(),
        })
    }

    async fn report(
        &self,
        store: &UsageStore,
        summary: &codex_usage::UsageSummary,
        account: Option<String>,
    ) -> Result<LocalUsageReport, JSONRPCErrorError> {
        let mut report = mapping::report(summary, account);
        let mut labels = BTreeMap::<String, String>::new();
        for tokens in &mut report.provider_tokens {
            let label = if let Some(label) = labels.get(&tokens.repository_bucket) {
                label.clone()
            } else {
                let label = repository_bucket_label(store, &tokens.repository_bucket).await?;
                labels.insert(tokens.repository_bucket.clone(), label.clone());
                label
            };
            tokens.repository_label = Some(label);
        }
        Ok(report)
    }

    fn account_label(&self, account: &AccountProfileRef) -> Result<String, JSONRPCErrorError> {
        let registry = match RegistryStore::new(&self.codex_home).read() {
            Ok(registry) => Some(registry),
            Err(RegistryStoreError::NotFound) => None,
            Err(_) => return Err(internal_error("local account registry is unavailable")),
        };
        Ok(registry
            .and_then(|registry| {
                registry
                    .accounts
                    .into_iter()
                    .find(|metadata| metadata.id.as_str() == account.as_str())
            })
            .map_or_else(
                || redacted_account_profile_label(account),
                |metadata| metadata.alias.to_string(),
            ))
    }

    async fn api_repository(
        &self,
        store: &UsageStore,
        record: UsageRepositoryRecord,
    ) -> Result<LocalUsageRepository, JSONRPCErrorError> {
        let summary = store
            .usage_summary_query(UsageSummaryQuery {
                thread_id: None,
                repository_id: Some(record.id.clone()),
                account_profile_ref: None,
                time_range: None,
            })
            .await
            .map_err(store_error)?;
        let (aggregate, _) = mapping::summary(&summary).map_err(store_error)?;
        Ok(api_repository_record(record, aggregate))
    }

    async fn notify(&self, thread_id: Option<String>, repository_key: Option<String>) {
        let previous = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                Some(generation.saturating_add(1))
            })
            .unwrap_or(u64::MAX);
        self.outgoing
            .send_server_notification(ServerNotification::LocalUsageUpdated(
                LocalUsageUpdatedNotification {
                    generation: previous.saturating_add(1),
                    updated_at: now_seconds(),
                    thread_id,
                    repository_key,
                },
            ))
            .await;
    }
}

struct SummaryFilters {
    repository_key: Option<String>,
    thread_id: Option<String>,
    account_id: Option<String>,
    from_at: Option<i64>,
    to_at: Option<i64>,
}

fn api_thread(
    record: UsageThreadRecord,
    aggregate: codex_app_server_protocol::LocalUsageAggregate,
) -> LocalUsageThread {
    LocalUsageThread {
        thread_id: record.id.as_str().to_string(),
        repository_keys: record
            .repository_ids
            .into_iter()
            .map(|id| id.as_str().to_string())
            .collect(),
        account_id: record
            .account_profile_ref
            .map(|account| account.as_str().to_string()),
        started_at: mapping::seconds(record.created_at_ms),
        updated_at: mapping::seconds(record.updated_at_ms),
        aggregate,
    }
}

fn api_repository_record(
    record: UsageRepositoryRecord,
    aggregate: codex_app_server_protocol::LocalUsageAggregate,
) -> LocalUsageRepository {
    LocalUsageRepository {
        repository_key: record.id.as_str().to_string(),
        label: record.label,
        created_at: mapping::seconds(record.created_at_ms),
        updated_at: mapping::seconds(record.updated_at_ms),
        aggregate,
    }
}

async fn canonical_repository(
    store: &UsageStore,
    id: RepositoryId,
) -> Result<RepositoryId, JSONRPCErrorError> {
    store
        .read_repository(&id)
        .await
        .map_err(store_error)?
        .map(|record| record.id)
        .ok_or_else(resource_not_found)
}

async fn repository_bucket_label(
    store: &UsageStore,
    repository_bucket: &str,
) -> Result<String, JSONRPCErrorError> {
    match repository_bucket {
        "multi_repo" => Ok("multiple repositories".to_string()),
        "unknown" => Ok("unknown repository".to_string()),
        value => {
            let Some(id) = RepositoryId::new(value.to_string()).ok() else {
                return Ok("collected repository".to_string());
            };
            Ok(store
                .read_repository(&id)
                .await
                .map_err(store_error)?
                .map_or_else(|| "collected repository".to_string(), |record| record.label))
        }
    }
}

fn page(
    kind: &'static str,
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<UsagePageRequest, JSONRPCErrorError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
        return Err(invalid_params("limit must be between 1 and 100"));
    }
    let cursor = cursor
        .map(|cursor| parse_cursor(kind, &cursor))
        .transpose()?;
    Ok(UsagePageRequest { cursor, limit })
}

fn parse_cursor(kind: &str, cursor: &str) -> Result<UsagePageCursor, JSONRPCErrorError> {
    if cursor.len() > MAX_CURSOR_BYTES || cursor.chars().any(char::is_control) {
        return Err(invalid_params("cursor is invalid"));
    }
    let mut fields = cursor.split('|');
    let valid_version = fields.next() == Some("v1");
    let valid_kind = fields.next() == Some(kind);
    let occurred_at_ms = fields.next().and_then(|value| value.parse::<i64>().ok());
    let id = fields.next();
    if !valid_version || !valid_kind || fields.next().is_some() {
        return Err(invalid_params("cursor is invalid"));
    }
    UsagePageCursor::new(
        occurred_at_ms.ok_or_else(|| invalid_params("cursor is invalid"))?,
        id.ok_or_else(|| invalid_params("cursor is invalid"))?,
    )
    .ok_or_else(|| invalid_params("cursor is invalid"))
}

fn encode_cursor(kind: &str, cursor: &UsagePageCursor) -> String {
    format!("v1|{kind}|{}|{}", cursor.occurred_at_ms(), cursor.id())
}

fn time_range(
    from_at: Option<i64>,
    to_at: Option<i64>,
) -> Result<Option<UtcTimeRange>, JSONRPCErrorError> {
    if from_at.is_none() && to_at.is_none() {
        return Ok(None);
    }
    let start_ms = match from_at {
        Some(value) => value
            .checked_mul(1_000)
            .ok_or_else(|| invalid_params("time range is invalid"))?,
        None => i64::MIN,
    };
    let end_ms = match to_at {
        Some(value) => value
            .checked_mul(1_000)
            .ok_or_else(|| invalid_params("time range is invalid"))?,
        None => i64::MAX,
    };
    UtcTimeRange::new(start_ms, end_ms)
        .map(Some)
        .map_err(|_| invalid_params("time range is invalid"))
}

fn thread_id(value: String) -> Result<ThreadId, JSONRPCErrorError> {
    ThreadId::new(value).map_err(|_| invalid_identifier())
}

fn repository_id(value: String) -> Result<RepositoryId, JSONRPCErrorError> {
    RepositoryId::new(value).map_err(|_| invalid_identifier())
}

fn invalid_identifier() -> JSONRPCErrorError {
    invalid_params("local usage identifier is invalid")
}

fn resource_not_found() -> JSONRPCErrorError {
    invalid_params("local usage resource was not found")
}

fn store_error(error: UsageStoreError) -> JSONRPCErrorError {
    match error {
        UsageStoreError::RepositoryMergeCycle => {
            invalid_params("repository merge would create a cycle")
        }
        UsageStoreError::InvalidFact => internal_error("local usage database is corrupt"),
        UsageStoreError::Database(_)
        | UsageStoreError::Migration(_)
        | UsageStoreError::Filesystem(_)
        | UsageStoreError::UnsupportedPlatform
        | UsageStoreError::RepositoryKeyMissing
        | UsageStoreError::RepositoryKeyCorrupt
        | UsageStoreError::RepositoryKeyCommittedCleanupUncertain(_)
        | UsageStoreError::RepositoryKeyCommittedSyncUncertain(_) => {
            internal_error("local usage database is unavailable")
        }
        UsageStoreError::OperationConflict
        | UsageStoreError::ProcessConflict
        | UsageStoreError::ProcessEventConflict
        | UsageStoreError::TerminalConflict
        | UsageStoreError::FactConflict
        | UsageStoreError::DurationOutOfRange
        | UsageStoreError::TokenCountOutOfRange
        | UsageStoreError::DatabaseValueOutOfRange
        | UsageStoreError::AggregateOverflow
        | UsageStoreError::TaskTreeTooLarge => {
            internal_error("local usage request could not be completed")
        }
    }
}

fn usage_phase(value: LocalUsagePhase) -> Phase {
    match value {
        LocalUsagePhase::Planning => Phase::Planning,
        LocalUsagePhase::Implementation => Phase::Implementation,
        LocalUsagePhase::Testing => Phase::Testing,
        LocalUsagePhase::Deployment => Phase::Deployment,
        LocalUsagePhase::Reporting => Phase::Reporting,
        LocalUsagePhase::Unattributed => Phase::Unattributed,
    }
}

fn usage_activity(value: LocalUsageActivityKind) -> Activity {
    match value {
        LocalUsageActivityKind::Requirements => Activity::Requirements,
        LocalUsageActivityKind::Specification => Activity::Specification,
        LocalUsageActivityKind::RepositoryAnalysis => Activity::RepositoryAnalysis,
        LocalUsageActivityKind::Research => Activity::Research,
        LocalUsageActivityKind::Diagnosis => Activity::Diagnosis,
        LocalUsageActivityKind::ArchitectureDesign => Activity::ArchitectureDesign,
        LocalUsageActivityKind::WorkPlanning => Activity::WorkPlanning,
        LocalUsageActivityKind::Coding => Activity::Coding,
        LocalUsageActivityKind::Configuration => Activity::Configuration,
        LocalUsageActivityKind::Refactoring => Activity::Refactoring,
        LocalUsageActivityKind::DependencyOrBuildChange => Activity::DependencyOrBuildChange,
        LocalUsageActivityKind::TestAuthoring => Activity::TestAuthoring,
        LocalUsageActivityKind::DocumentationAuthoring => Activity::DocumentationAuthoring,
        LocalUsageActivityKind::DataOrSchemaChange => Activity::DataOrSchemaChange,
        LocalUsageActivityKind::BuildValidation => Activity::BuildValidation,
        LocalUsageActivityKind::UnitTesting => Activity::UnitTesting,
        LocalUsageActivityKind::IntegrationTesting => Activity::IntegrationTesting,
        LocalUsageActivityKind::BrowserQa => Activity::BrowserQa,
        LocalUsageActivityKind::CompatibilityTesting => Activity::CompatibilityTesting,
        LocalUsageActivityKind::MigrationRehearsal => Activity::MigrationRehearsal,
        LocalUsageActivityKind::VerificationReview => Activity::VerificationReview,
        LocalUsageActivityKind::Packaging => Activity::Packaging,
        LocalUsageActivityKind::Deployment => Activity::Deployment,
        LocalUsageActivityKind::Rollback => Activity::Rollback,
        LocalUsageActivityKind::RuntimeOperations => Activity::RuntimeOperations,
        LocalUsageActivityKind::Monitoring => Activity::Monitoring,
        LocalUsageActivityKind::UserElaboration => Activity::UserElaboration,
        LocalUsageActivityKind::StatusUpdate => Activity::StatusUpdate,
        LocalUsageActivityKind::CompletionHandoff => Activity::CompletionHandoff,
        LocalUsageActivityKind::ReviewFeedback => Activity::ReviewFeedback,
        LocalUsageActivityKind::Coordination => Activity::Coordination,
        LocalUsageActivityKind::AccountingOverhead => Activity::AccountingOverhead,
        LocalUsageActivityKind::Mixed => Activity::Mixed,
        LocalUsageActivityKind::Unknown => Activity::Unknown,
    }
}

fn usage_event_kind(value: LocalUsageEventKind) -> UsageEventKind {
    match value {
        LocalUsageEventKind::ModelRequestStarted => UsageEventKind::ModelRequestStarted,
        LocalUsageEventKind::ModelRequestCompleted => UsageEventKind::ModelRequestCompleted,
        LocalUsageEventKind::ToolStarted => UsageEventKind::ToolStarted,
        LocalUsageEventKind::ToolCompleted => UsageEventKind::ToolCompleted,
        LocalUsageEventKind::ActivityChanged => UsageEventKind::ActivityChanged,
        LocalUsageEventKind::ClassificationCorrected => UsageEventKind::ClassificationCorrected,
        LocalUsageEventKind::CoverageGap => UsageEventKind::CoverageGap,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

fn now_seconds() -> i64 {
    now_ms().div_euclid(1_000)
}
