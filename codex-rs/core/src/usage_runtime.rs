use crate::model_context_estimate::ModelContextEstimate;
use codex_api::ProviderResponseStatus;
use codex_api::ProviderUsageObservation;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ResponseItem;
use codex_protocol::provider_usage::ProviderTokenCount;
use codex_protocol::provider_usage::ProviderUsage;
use codex_usage::AccountAttributionSnapshot;
use codex_usage::AccountAuthMode;
use codex_usage::AccountProfileRef;
use codex_usage::Activity;
use codex_usage::ActivityState;
use codex_usage::AgentId;
use codex_usage::AgentRoleKind;
use codex_usage::AttributionProvenance;
use codex_usage::ClientOrigin;
use codex_usage::CoverageScopeKind;
use codex_usage::CoverageState;
use codex_usage::ErrorCategory;
use codex_usage::FactEventId;
use codex_usage::MeasurementProvenance;
use codex_usage::ModelName;
use codex_usage::ModelRequestId;
use codex_usage::NewAgent;
use codex_usage::NewCoverageEvent;
use codex_usage::NewModelRequest;
use codex_usage::NewModelRequestContext;
use codex_usage::NewOperation;
use codex_usage::NewThread;
use codex_usage::NewTokenObservation;
use codex_usage::NewTurn;
use codex_usage::OperationId;
use codex_usage::OperationKind;
use codex_usage::Phase;
use codex_usage::ProcessId;
use codex_usage::ProviderKind;
use codex_usage::RepositoryBucket;
use codex_usage::TerminalOperation;
use codex_usage::TerminalStatus;
use codex_usage::ThreadId;
use codex_usage::ThreadSourceKind;
use codex_usage::TokenCategoryPath;
use codex_usage::TokenObservationSource;
use codex_usage::TokenUnit;
use codex_usage::TransportKind;
use codex_usage::TurnId;
use codex_usage::UsageStore;
use codex_usage::UsageStoreError;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock as StdOnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::Semaphore;

const SAFE_UNAVAILABLE: &str = "usage accounting is unavailable; model operation was not started";

pub(crate) struct UsageRuntime {
    codex_home: PathBuf,
    process_id: ProcessId,
    process_started_at_ms: i64,
    store: OnceCell<Arc<UsageStore>>,
    faulted: AtomicBool,
    fault_generation: AtomicU64,
    fault_recovery_allowed: AtomicBool,
    faulted_operations: std::sync::Mutex<HashSet<OperationId>>,
    recovery_gate: Semaphore,
    entity_times: Mutex<HashMap<String, i64>>,
    repository_state: repository::TurnRepositoryState,
    tool_state: tool::ToolRuntimeState,
}

impl fmt::Debug for UsageRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("UsageRuntime([private])")
    }
}

pub(crate) struct ModelAttemptContext<'a> {
    pub(crate) thread_id: &'a str,
    pub(crate) parent_thread_id: Option<&'a str>,
    pub(crate) turn_id: Option<&'a str>,
    pub(crate) delegated: bool,
    pub(crate) provider: ProviderKind,
    pub(crate) model: &'a str,
    pub(crate) transport: &'a str,
    pub(crate) client_origin: &'a str,
    pub(crate) account: AccountAttributionSnapshot,
    pub(crate) repositories: Vec<RepositoryCandidate>,
    pub(crate) attempt_number: u32,
    pub(crate) retry_of_operation_id: Option<OperationId>,
    pub(crate) retry_slot: Arc<std::sync::Mutex<Option<OperationId>>>,
    pub(crate) context_estimate: Option<ModelContextEstimate>,
}

pub(crate) struct UsageRequestChain {
    retry_slot: Arc<std::sync::Mutex<Option<OperationId>>>,
    next_attempt_number: AtomicU32,
}

pub(crate) struct UsageRequestAttempt {
    pub(crate) attempt_number: u32,
    pub(crate) retry_of_operation_id: Option<OperationId>,
    pub(crate) retry_slot: Arc<std::sync::Mutex<Option<OperationId>>>,
}

impl UsageRequestChain {
    pub(crate) fn new() -> Self {
        Self {
            retry_slot: Arc::new(std::sync::Mutex::new(None)),
            next_attempt_number: AtomicU32::new(1),
        }
    }

    pub(crate) fn next_attempt(&self) -> Result<UsageRequestAttempt, CodexErr> {
        let attempt_number = self
            .next_attempt_number
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| unavailable())?;
        let retry_of_operation_id = self
            .retry_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Ok(UsageRequestAttempt {
            attempt_number,
            retry_of_operation_id,
            retry_slot: Arc::clone(&self.retry_slot),
        })
    }
}

pub(crate) struct UsageAttempt {
    runtime: Arc<UsageRuntime>,
    operation_id: OperationId,
    model_request_id: ModelRequestId,
    thread_id: ThreadId,
    turn_id: Option<TurnId>,
    agent_id: AgentId,
    phase: Phase,
    attribution_provenance: AttributionProvenance,
    fallback_source_event_id: FactEventId,
    retry_slot: Arc<std::sync::Mutex<Option<OperationId>>>,
    started: Instant,
    finished: AtomicBool,
    saw_provider_usage: AtomicBool,
    partial_provider_usage: AtomicBool,
    provider_observed_at_ms: StdOnceLock<i64>,
    saw_usage_activity: AtomicBool,
    saw_mixed_activity_output: AtomicBool,
    hosted_seen: std::sync::Mutex<HashSet<String>>,
    repository_ids: Vec<codex_usage::RepositoryId>,
    repository_bucket: RepositoryBucket,
}

impl UsageRuntime {
    pub(crate) fn new(codex_home: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            codex_home,
            process_id: ProcessId::new(),
            process_started_at_ms: now_ms(),
            store: OnceCell::new(),
            faulted: AtomicBool::new(false),
            fault_generation: AtomicU64::new(0),
            fault_recovery_allowed: AtomicBool::new(true),
            faulted_operations: std::sync::Mutex::new(HashSet::new()),
            recovery_gate: Semaphore::new(1),
            entity_times: Mutex::new(HashMap::new()),
            repository_state: repository::TurnRepositoryState::default(),
            tool_state: tool::ToolRuntimeState::default(),
        })
    }

    async fn begin_model_attempt_once(
        self: &Arc<Self>,
        context: &ModelAttemptContext<'_>,
    ) -> Result<UsageAttempt, CodexErr> {
        let store = self.store().await?;
        let thread_id = ThreadId::new(context.thread_id).map_err(|_| unavailable())?;
        let requested_parent_thread_id = context
            .parent_thread_id
            .map(ThreadId::new)
            .transpose()
            .map_err(|_| unavailable())?;
        let parent_thread_id = match requested_parent_thread_id {
            Some(parent_thread_id)
                if self
                    .write_required(store.thread_created_at(&parent_thread_id).await)?
                    .is_some() =>
            {
                Some(parent_thread_id)
            }
            Some(_) | None => None,
        };
        let turn_id = context
            .turn_id
            .map(TurnId::new)
            .transpose()
            .map_err(|_| unavailable())?;
        let agent_id = AgentId::new(context.thread_id).map_err(|_| unavailable())?;
        let model = model_name(context.model).ok_or_else(|| self.reject_invalid_metadata())?;
        let transport_kind =
            TransportKind::new(context.transport).map_err(|_| self.reject_invalid_metadata())?;
        let client_origin =
            ClientOrigin::new(context.client_origin).map_err(|_| self.reject_invalid_metadata())?;
        let account = context.account.clone();
        let created_at_ms = self
            .write_required(store.thread_created_at(&thread_id).await)?
            .unwrap_or(
                self.entity_created_at(&format!("thread:{}", context.thread_id))
                    .await,
            );
        let created_at_ms = self
            .ensure_thread(
                &store,
                NewThread {
                    id: thread_id.clone(),
                    parent_thread_id: parent_thread_id.clone(),
                    source_kind: ThreadSourceKind::new(if context.delegated {
                        "delegated"
                    } else {
                        "root"
                    })
                    .map_err(|_| unavailable())?,
                    created_at_ms,
                },
            )
            .await?;
        let parent_agent_id = match parent_thread_id.as_ref() {
            Some(parent_thread_id) => {
                let parent_agent_id =
                    AgentId::new(parent_thread_id.as_str()).map_err(|_| unavailable())?;
                self.write_required(store.agent_created_at(&parent_agent_id).await)?
                    .map(|_| parent_agent_id)
            }
            None => None,
        };
        let agent_created_at_ms = self
            .write_required(store.agent_created_at(&agent_id).await)?
            .unwrap_or(created_at_ms);
        self.ensure_agent(
            &store,
            NewAgent {
                id: agent_id.clone(),
                thread_id: thread_id.clone(),
                parent_agent_id,
                role_kind: AgentRoleKind::new(if context.delegated {
                    "delegated"
                } else {
                    "root"
                })
                .map_err(|_| unavailable())?,
                created_at_ms: agent_created_at_ms,
            },
        )
        .await?;
        if let Some(turn_id) = &turn_id {
            let turn_created_at_ms = self
                .write_required(store.turn_created_at(turn_id).await)?
                .unwrap_or(
                    self.entity_created_at(&format!("turn:{}", turn_id.as_str()))
                        .await,
                );
            self.ensure_turn(
                &store,
                NewTurn {
                    id: turn_id.clone(),
                    thread_id: thread_id.clone(),
                    account: account.clone(),
                    created_at_ms: turn_created_at_ms,
                },
            )
            .await?;
        }

        let operation_id = OperationId::new();
        let started_at_ms = now_ms();
        let repository_resolution = self
            .resolve_repositories(
                &store,
                context.thread_id,
                context.turn_id,
                &context.repositories,
            )
            .await?;
        let (phase, activity, attribution_provenance, rework_of_operation_id) = self
            .activate_model_activity(
                context.thread_id,
                parent_thread_id.as_ref().map(ThreadId::as_str),
            )
            .await;
        self.write_required_for(
            operation_id,
            store
                .begin_operation(&NewOperation {
                    id: operation_id,
                    process_id: self.process_id,
                    thread_id: Some(thread_id.clone()),
                    turn_id: turn_id.clone(),
                    agent_id: Some(agent_id.clone()),
                    parent_operation_id: None,
                    retry_of_operation_id: context.retry_of_operation_id,
                    rework_of_operation_id,
                    kind: OperationKind::ModelRequest,
                    started_at_ms,
                    phase,
                    activity,
                    activity_state: ActivityState::ModelActive,
                    attribution_provenance,
                })
                .await,
        )?;
        self.record_repository_resolution(
            &store,
            operation_id,
            &repository_resolution,
            started_at_ms,
        )
        .await?;
        let model_request_id = ModelRequestId::new();
        self.write_required_for(
            operation_id,
            store
                .record_model_request(&NewModelRequest {
                    id: model_request_id,
                    operation_id,
                    provider_kind: context.provider.clone(),
                    model,
                    transport_kind,
                    attempt_number: context.attempt_number,
                    account,
                    client_origin,
                })
                .await,
        )?;
        if let Some(estimate) = context.context_estimate {
            self.write_required_for(
                operation_id,
                store
                    .record_model_request_context(&NewModelRequestContext {
                        model_request_id,
                        policy_estimated_tokens: estimate.policy_tokens,
                        conversation_estimated_tokens: estimate.conversation_tokens,
                        tool_output_estimated_tokens: estimate.tool_output_tokens,
                        observed_at_ms: started_at_ms,
                    })
                    .await,
            )?;
        }
        self.write_required_for(
            operation_id,
            store
                .record_coverage(&NewCoverageEvent {
                    event_id: FactEventId::new(),
                    operation_id: Some(operation_id),
                    scope_kind: CoverageScopeKind::new("model_attempt")
                        .map_err(|_| unavailable())?,
                    state: CoverageState::CaptureStarted,
                    reason_code: None,
                    occurred_at_ms: started_at_ms,
                })
                .await,
        )?;
        self.note_latest_model_operation(context.thread_id, operation_id)
            .await;
        Ok(UsageAttempt {
            runtime: Arc::clone(self),
            operation_id,
            model_request_id,
            thread_id,
            turn_id,
            agent_id,
            phase,
            attribution_provenance,
            fallback_source_event_id: FactEventId::new(),
            retry_slot: Arc::clone(&context.retry_slot),
            started: Instant::now(),
            finished: AtomicBool::new(false),
            saw_provider_usage: AtomicBool::new(false),
            partial_provider_usage: AtomicBool::new(false),
            provider_observed_at_ms: StdOnceLock::new(),
            saw_usage_activity: AtomicBool::new(false),
            saw_mixed_activity_output: AtomicBool::new(false),
            hosted_seen: std::sync::Mutex::new(HashSet::new()),
            repository_ids: repository_resolution.ids,
            repository_bucket: repository_resolution.bucket,
        })
    }

    async fn store(&self) -> Result<Arc<UsageStore>, CodexErr> {
        self.store
            .get_or_try_init(|| async {
                let store =
                    Arc::new(UsageStore::open(&self.codex_home).await.map_err(|error| {
                        self.latch_write_failure("store_open", None, error);
                        unavailable()
                    })?);
                store
                    .register_process(
                        &self.process_id,
                        std::process::id(),
                        self.process_started_at_ms,
                    )
                    .await
                    .map_err(|error| {
                        self.latch_write_failure("process_registration", None, error);
                        unavailable()
                    })?;
                Ok(store)
            })
            .await
            .map(Arc::clone)
    }

    async fn entity_created_at(&self, key: &str) -> i64 {
        *self
            .entity_times
            .lock()
            .await
            .entry(key.to_string())
            .or_insert_with(now_ms)
    }

    async fn ensure_thread(
        &self,
        store: &UsageStore,
        mut fact: NewThread,
    ) -> Result<i64, CodexErr> {
        match store.ensure_thread(&fact).await {
            Ok(()) => Ok(fact.created_at_ms),
            Err(UsageStoreError::FactConflict) => {
                fact.created_at_ms = self
                    .write_required(store.thread_created_at(&fact.id).await)?
                    .ok_or_else(|| self.reject_invalid_metadata())?;
                self.write_required(store.ensure_thread(&fact).await)?;
                Ok(fact.created_at_ms)
            }
            Err(error) => self.write_required(Err(error)),
        }
    }

    async fn ensure_turn(&self, store: &UsageStore, mut fact: NewTurn) -> Result<(), CodexErr> {
        match store.ensure_turn(&fact).await {
            Ok(()) => Ok(()),
            Err(UsageStoreError::FactConflict) => {
                fact.created_at_ms = self
                    .write_required(store.turn_created_at(&fact.id).await)?
                    .ok_or_else(|| self.reject_invalid_metadata())?;
                if self.write_required(store.turn_identity_matches(&fact).await)? {
                    return Ok(());
                }
                self.write_required(store.ensure_turn(&fact).await)
            }
            Err(error) => self.write_required(Err(error)),
        }
    }

    async fn ensure_agent(&self, store: &UsageStore, mut fact: NewAgent) -> Result<(), CodexErr> {
        match store.ensure_agent(&fact).await {
            Ok(()) => Ok(()),
            Err(UsageStoreError::FactConflict) => {
                fact.created_at_ms = self
                    .write_required(store.agent_created_at(&fact.id).await)?
                    .ok_or_else(|| self.reject_invalid_metadata())?;
                self.write_required(store.ensure_agent(&fact).await)
            }
            Err(error) => self.write_required(Err(error)),
        }
    }

    fn write_required<T>(
        &self,
        result: Result<T, codex_usage::UsageStoreError>,
    ) -> Result<T, CodexErr> {
        result.map_err(|error| {
            self.latch_write_failure("required_write", None, error);
            unavailable()
        })
    }

    fn write_required_for<T>(
        &self,
        operation_id: OperationId,
        result: Result<T, codex_usage::UsageStoreError>,
    ) -> Result<T, CodexErr> {
        result.map_err(|error| {
            self.latch_write_failure("required_write", Some(operation_id), error);
            unavailable()
        })
    }

    fn latch_write_failure(
        &self,
        stage: &'static str,
        operation_id: Option<OperationId>,
        error: UsageStoreError,
    ) {
        tracing::warn!(stage, error = %error, "usage accounting write failed");
        self.latch_fault_with_operation(operation_id, error.recovery_may_succeed());
    }

    fn latch_operation_fault(&self, operation_id: OperationId) {
        self.latch_fault_with_operation(Some(operation_id), /*recovery_allowed*/ true);
    }

    fn latch_fault_with_operation(
        &self,
        operation_id: Option<OperationId>,
        recovery_allowed: bool,
    ) {
        if let Some(operation_id) = operation_id {
            self.faulted_operations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(operation_id);
        }
        if !recovery_allowed {
            self.fault_recovery_allowed.store(false, Ordering::Release);
        }
        self.fault_generation.fetch_add(1, Ordering::AcqRel);
        self.faulted.store(true, Ordering::Release);
    }

    pub(crate) fn reject_invalid_metadata(&self) -> CodexErr {
        unavailable()
    }
}

impl UsageAttempt {
    pub(crate) async fn observe_response_item(&self, item: &ResponseItem) {
        self.observe_activity_boundary(item);
        self.record_hosted_tool(item).await;
    }

    fn observe_activity_boundary(&self, item: &ResponseItem) {
        match item {
            ResponseItem::FunctionCall {
                name, namespace, ..
            } if name == "usage_activity"
                && namespace
                    .as_deref()
                    .is_none_or(|namespace| namespace == "functions") =>
            {
                self.saw_usage_activity.store(true, Ordering::Release);
            }
            ResponseItem::Reasoning { .. } => {}
            ResponseItem::Message { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. } => {
                self.saw_mixed_activity_output
                    .store(true, Ordering::Release);
            }
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => {}
        }
    }

    pub(crate) async fn record_provider_usage(&self, observation: &ProviderUsageObservation) {
        self.record_provider_usage_parts(
            observation
                .source_event_key()
                .map(codex_api::ProviderSourceEventKey::as_bytes),
            observation.usage(),
        )
        .await;
    }

    async fn record_provider_usage_parts(
        &self,
        source_event_key: Option<&[u8; 32]>,
        usage: &ProviderUsage,
    ) {
        self.saw_provider_usage.store(true, Ordering::Release);
        let source_event_id = source_event_key
            .map(FactEventId::from_provider_source_key)
            .unwrap_or(self.fallback_source_event_id);
        let coverage = if usage.categories_complete() {
            CoverageState::Complete
        } else {
            self.partial_provider_usage.store(true, Ordering::Release);
            CoverageState::Partial
        };
        let Some(store) = self.runtime.store.get() else {
            self.runtime.latch_operation_fault(self.operation_id);
            return;
        };
        for (path, count) in provider_counts(usage) {
            let (token_count, state) = match count {
                ProviderTokenCount::Absent => continue,
                ProviderTokenCount::Value(count) => (Some(count), coverage),
                ProviderTokenCount::Null => (None, CoverageState::Unknown),
                ProviderTokenCount::Invalid => (None, CoverageState::Partial),
            };
            if let Err(error) = store
                .record_token_observation(&NewTokenObservation {
                    id: FactEventId::new(),
                    source_event_id,
                    source: TokenObservationSource::ModelRequest(self.model_request_id),
                    category_path: match TokenCategoryPath::new(path) {
                        Ok(path) => path,
                        Err(_) => {
                            self.runtime.latch_operation_fault(self.operation_id);
                            return;
                        }
                    },
                    token_count,
                    unit: TokenUnit::Tokens,
                    measurement_provenance: MeasurementProvenance::ProviderReported,
                    coverage_state: state,
                    repository_bucket: self.repository_bucket.clone(),
                    observed_at_ms: *self.provider_observed_at_ms.get_or_init(now_ms),
                })
                .await
            {
                self.runtime.latch_write_failure(
                    "model_token_observation",
                    Some(self.operation_id),
                    error,
                );
                return;
            }
        }
    }

    pub(crate) async fn finish_provider(&self, status: ProviderResponseStatus) {
        let (terminal, error) = match status {
            ProviderResponseStatus::Completed => (TerminalStatus::Completed, None),
            ProviderResponseStatus::Failed => {
                (TerminalStatus::Failed, Some(ErrorCategory::Provider))
            }
            ProviderResponseStatus::Incomplete => {
                (TerminalStatus::Incomplete, Some(ErrorCategory::Provider))
            }
        };
        self.finish(terminal, error).await;
    }

    pub(crate) async fn finish(&self, status: TerminalStatus, error: Option<ErrorCategory>) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(store) = self.runtime.store.get() else {
            self.runtime.latch_operation_fault(self.operation_id);
            return;
        };
        let duration_ns = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if let Err(error) = store
            .finish_operation(&TerminalOperation {
                operation_id: self.operation_id,
                status,
                occurred_at_ms: now_ms(),
                duration_ns,
                error_category: error,
            })
            .await
        {
            self.runtime.latch_write_failure(
                "model_operation_terminal",
                Some(self.operation_id),
                error,
            );
            return;
        }
        if self.saw_usage_activity.load(Ordering::Acquire) {
            let activity = if self.saw_mixed_activity_output.load(Ordering::Acquire) {
                Activity::Mixed
            } else {
                Activity::AccountingOverhead
            };
            if let Err(error) = store
                .record_classification(&codex_usage::NewClassificationEvent {
                    event_id: FactEventId::new(),
                    operation_id: self.operation_id,
                    phase: Phase::Unattributed,
                    activity,
                    activity_state: ActivityState::ModelActive,
                    provenance: AttributionProvenance::DeterministicClassification,
                    supersedes_event_id: None,
                    occurred_at_ms: now_ms(),
                })
                .await
            {
                self.runtime.latch_write_failure(
                    "model_activity_classification",
                    Some(self.operation_id),
                    error,
                );
            }
            if let Err(error) = store
                .record_coverage(&NewCoverageEvent {
                    event_id: FactEventId::new(),
                    operation_id: Some(self.operation_id),
                    scope_kind: match CoverageScopeKind::new("usage_activity_schema_marginal") {
                        Ok(scope) => scope,
                        Err(_) => {
                            self.runtime.latch_operation_fault(self.operation_id);
                            return;
                        }
                    },
                    state: CoverageState::Unknown,
                    reason_code: None,
                    occurred_at_ms: now_ms(),
                })
                .await
            {
                self.runtime.latch_write_failure(
                    "model_activity_coverage",
                    Some(self.operation_id),
                    error,
                );
            }
        }
        let coverage_state = match status {
            TerminalStatus::Completed if !self.saw_provider_usage.load(Ordering::Acquire) => {
                CoverageState::Unknown
            }
            // Account and repository attribution are intentionally unknown until the router and
            // repository stage are wired, so D1 cannot claim complete attempt coverage.
            TerminalStatus::Completed => CoverageState::Partial,
            TerminalStatus::Incomplete
            | TerminalStatus::Failed
            | TerminalStatus::Denied
            | TerminalStatus::TimedOut
            | TerminalStatus::Cancelled
            | TerminalStatus::Interrupted => CoverageState::Partial,
        };
        if let Err(error) = store
            .record_coverage(&NewCoverageEvent {
                event_id: FactEventId::new(),
                operation_id: Some(self.operation_id),
                scope_kind: match CoverageScopeKind::new("model_attempt") {
                    Ok(scope) => scope,
                    Err(_) => {
                        self.runtime.latch_operation_fault(self.operation_id);
                        return;
                    }
                },
                state: coverage_state,
                reason_code: None,
                occurred_at_ms: now_ms(),
            })
            .await
        {
            self.runtime.latch_write_failure(
                "model_attempt_coverage",
                Some(self.operation_id),
                error,
            );
        }
        if !matches!(status, TerminalStatus::Completed) {
            *self
                .retry_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(self.operation_id);
        }
    }
}

fn provider_counts(usage: &ProviderUsage) -> Vec<(&str, ProviderTokenCount)> {
    let mut counts = vec![
        ("input_tokens", usage.input_tokens()),
        (
            "input_tokens_details.cached_tokens",
            usage.cached_input_tokens(),
        ),
        (
            "input_tokens_details.cache_write_tokens",
            usage.cache_write_input_tokens(),
        ),
        ("output_tokens", usage.output_tokens()),
        (
            "output_tokens_details.reasoning_tokens",
            usage.reasoning_output_tokens(),
        ),
        ("total_tokens", usage.total_tokens()),
    ];
    counts.extend(
        usage
            .additional_token_categories()
            .iter()
            .map(|(path, count)| (path.as_str(), *count)),
    );
    counts
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}

fn model_name(value: &str) -> Option<ModelName> {
    if let Ok(model) = ModelName::new(value) {
        return Some(model);
    }
    if value.len() > 512 {
        return None;
    }
    ModelName::new(format!(
        "opaque-{}",
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, value.as_bytes()).simple()
    ))
    .ok()
}

pub(crate) fn provider_kind(value: &str) -> Option<ProviderKind> {
    if let Ok(provider) = ProviderKind::new(value) {
        return Some(provider);
    }
    if value.len() > 512 {
        return None;
    }
    ProviderKind::new(format!(
        "opaque-{}",
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, value.as_bytes()).simple()
    ))
    .ok()
}

fn unavailable() -> CodexErr {
    CodexErr::Fatal(SAFE_UNAVAILABLE.to_string())
}

pub(crate) fn usage_account_snapshot(
    profile_ref: Option<&str>,
    auth_mode: Option<codex_protocol::auth::AuthMode>,
) -> Result<AccountAttributionSnapshot, CodexErr> {
    let profile_ref = profile_ref
        .map(AccountProfileRef::new)
        .transpose()
        .map_err(|_| unavailable())?;
    let auth_mode = auth_mode.map(|mode| match mode {
        codex_protocol::auth::AuthMode::ApiKey => AccountAuthMode::ApiKey,
        codex_protocol::auth::AuthMode::Chatgpt => AccountAuthMode::Chatgpt,
        codex_protocol::auth::AuthMode::ChatgptAuthTokens => AccountAuthMode::ChatgptAuthTokens,
        codex_protocol::auth::AuthMode::Headers => AccountAuthMode::Headers,
        codex_protocol::auth::AuthMode::AgentIdentity => AccountAuthMode::AgentIdentity,
        codex_protocol::auth::AuthMode::PersonalAccessToken => AccountAuthMode::PersonalAccessToken,
        codex_protocol::auth::AuthMode::BedrockApiKey => AccountAuthMode::BedrockApiKey,
        codex_protocol::auth::AuthMode::BedrockAccessKeys => AccountAuthMode::BedrockAccessKeys,
    });
    Ok(AccountAttributionSnapshot::new(profile_ref, auth_mode))
}

#[cfg(test)]
#[path = "usage_runtime_tests.rs"]
mod tests;

#[path = "usage_runtime_recovery.rs"]
mod recovery;
#[path = "usage_runtime_repository.rs"]
mod repository;
#[path = "usage_runtime_tool.rs"]
mod tool;
pub(crate) use repository::RepositoryCandidate;
pub(crate) use repository::repository_safe_label;
pub(crate) use tool::ToolAttemptContext;
pub(crate) use tool::UsageActivityRelation;
pub(crate) use tool::UsageToolDescriptor;
