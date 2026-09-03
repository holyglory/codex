use super::super::*;
use super::PendingBaseFacts;
use super::PendingUsageRecord;
use super::bounded_identifier;
use super::replay_base;
use super::safe_agent_id;
use super::safe_thread_id;
use codex_usage::NewClassificationEvent;

struct PendingModelState {
    base: PendingBaseFacts,
    request: NewModelRequest,
    context: Option<NewModelRequestContext>,
    tokens: Vec<NewTokenObservation>,
    classifications: Vec<NewClassificationEvent>,
    coverage: Vec<NewCoverageEvent>,
    terminal: Option<TerminalOperation>,
}

pub(in super::super) struct PendingModelFinish {
    pub(in super::super) terminal: TerminalOperation,
    pub(in super::super) classification: Option<NewClassificationEvent>,
    pub(in super::super) marginal_coverage: Option<NewCoverageEvent>,
    pub(in super::super) attempt_coverage: NewCoverageEvent,
}

pub(in super::super) struct PendingModelAttempt {
    state: std::sync::Mutex<PendingModelState>,
    pub(super) replay_failures: AtomicU32,
}

impl PendingModelAttempt {
    pub(in super::super) fn new(
        thread: NewThread,
        turn: Option<NewTurn>,
        agent: NewAgent,
        operation: NewOperation,
        request: NewModelRequest,
        context: Option<NewModelRequestContext>,
        capture_started: NewCoverageEvent,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(PendingModelState {
                base: PendingBaseFacts {
                    thread,
                    turn,
                    agent,
                    operation,
                },
                request,
                context,
                tokens: Vec::new(),
                classifications: Vec::new(),
                coverage: vec![capture_started],
                terminal: None,
            }),
            replay_failures: AtomicU32::new(0),
        })
    }

    pub(in super::super) fn record_provider_usage(
        &self,
        source_event_id: FactEventId,
        model_request_id: ModelRequestId,
        usage: &ProviderUsage,
        repository_bucket: RepositoryBucket,
        observed_at_ms: i64,
    ) -> Vec<NewTokenObservation> {
        let coverage = if usage.categories_complete() {
            CoverageState::Complete
        } else {
            CoverageState::Partial
        };
        let observations = provider_counts(usage)
            .into_iter()
            .filter_map(|(path, count)| {
                let (token_count, coverage_state) = match count {
                    ProviderTokenCount::Absent => return None,
                    ProviderTokenCount::Value(count) => (Some(count), coverage),
                    ProviderTokenCount::Null => (None, CoverageState::Unknown),
                    ProviderTokenCount::Invalid => (None, CoverageState::Partial),
                };
                let category_path = match TokenCategoryPath::new(path) {
                    Ok(category_path) => category_path,
                    Err(_) => {
                        tracing::warn!(
                            stage = "pending_model_token_category",
                            "usage accounting metadata was skipped; work will continue"
                        );
                        return None;
                    }
                };
                Some(NewTokenObservation {
                    id: FactEventId::new(),
                    source_event_id,
                    source: TokenObservationSource::ModelRequest(model_request_id),
                    category_path,
                    token_count,
                    unit: TokenUnit::Tokens,
                    measurement_provenance: MeasurementProvenance::ProviderReported,
                    coverage_state,
                    repository_bucket: repository_bucket.clone(),
                    observed_at_ms,
                })
            })
            .collect::<Vec<_>>();
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tokens
            .extend(observations.iter().cloned());
        observations
    }

    pub(in super::super) fn finish(
        &self,
        status: TerminalStatus,
        error: Option<ErrorCategory>,
        duration_ns: u64,
        saw_provider_usage: bool,
        saw_usage_activity: bool,
        saw_mixed_activity_output: bool,
    ) -> Option<PendingModelFinish> {
        let occurred_at_ms = now_ms();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal.is_some() {
            return None;
        }
        let operation_id = state.base.operation.id;
        let terminal = TerminalOperation {
            operation_id,
            status,
            occurred_at_ms,
            duration_ns,
            error_category: error,
        };
        let classification = saw_usage_activity.then(|| NewClassificationEvent {
            event_id: FactEventId::new(),
            operation_id,
            phase: Phase::Unattributed,
            activity: if saw_mixed_activity_output {
                Activity::Mixed
            } else {
                Activity::AccountingOverhead
            },
            activity_state: ActivityState::ModelActive,
            provenance: AttributionProvenance::DeterministicClassification,
            supersedes_event_id: None,
            occurred_at_ms,
        });
        let marginal_coverage = saw_usage_activity.then(|| NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(operation_id),
            scope_kind: bounded_identifier("usage_activity_schema_marginal"),
            state: CoverageState::Unknown,
            reason_code: None,
            occurred_at_ms,
        });
        let attempt_coverage = NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(operation_id),
            scope_kind: bounded_identifier("model_attempt"),
            state: match status {
                TerminalStatus::Completed if !saw_provider_usage => CoverageState::Unknown,
                TerminalStatus::Completed
                | TerminalStatus::Incomplete
                | TerminalStatus::Failed
                | TerminalStatus::Denied
                | TerminalStatus::TimedOut
                | TerminalStatus::Cancelled
                | TerminalStatus::Interrupted => CoverageState::Partial,
            },
            reason_code: None,
            occurred_at_ms,
        };
        state.terminal = Some(terminal.clone());
        state.classifications.extend(classification.iter().cloned());
        state.coverage.extend(marginal_coverage.iter().cloned());
        state.coverage.push(attempt_coverage.clone());
        Some(PendingModelFinish {
            terminal,
            classification,
            marginal_coverage,
            attempt_coverage,
        })
    }

    pub(super) async fn replay(&self, store: &UsageStore) -> Result<bool, UsageStoreError> {
        let Some(snapshot) = self.snapshot_if_finished() else {
            return Ok(false);
        };
        replay_base(store, &snapshot.base).await?;
        store.record_model_request(&snapshot.request).await?;
        if let Some(context) = &snapshot.context {
            store.record_model_request_context(context).await?;
        }
        for token in &snapshot.tokens {
            store.record_token_observation(token).await?;
        }
        if let Some(terminal) = &snapshot.terminal {
            store.finish_operation(terminal).await?;
        }
        for classification in &snapshot.classifications {
            store.record_classification(classification).await?;
        }
        for coverage in &snapshot.coverage {
            store.record_coverage(coverage).await?;
        }
        Ok(true)
    }

    fn snapshot_if_finished(&self) -> Option<PendingModelState> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.terminal.as_ref()?;
        Some(PendingModelState {
            base: state.base.clone(),
            request: state.request.clone(),
            context: state.context.clone(),
            tokens: state.tokens.clone(),
            classifications: state.classifications.clone(),
            coverage: state.coverage.clone(),
            terminal: state.terminal.clone(),
        })
    }
}

impl UsageRuntime {
    pub(in super::super) async fn begin_buffered_model_attempt(
        self: &Arc<Self>,
        context: &ModelAttemptContext<'_>,
    ) -> UsageAttempt {
        let started_at_ms = now_ms();
        let thread_id = safe_thread_id(context.thread_id);
        let parent_thread_id = context
            .parent_thread_id
            .and_then(|value| ThreadId::new(value).ok());
        let parent_agent_id = parent_thread_id
            .as_ref()
            .and_then(|id| AgentId::new(id.as_str()).ok());
        let turn_id = context.turn_id.and_then(|value| TurnId::new(value).ok());
        let agent_id = safe_agent_id(thread_id.as_str());
        let operation_id = OperationId::new();
        let model_request_id = ModelRequestId::new();
        let capture_started = NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(operation_id),
            scope_kind: bounded_identifier("model_attempt"),
            state: CoverageState::CaptureStarted,
            reason_code: None,
            occurred_at_ms: started_at_ms,
        };
        let pending = PendingModelAttempt::new(
            NewThread {
                id: thread_id.clone(),
                parent_thread_id,
                source_kind: bounded_identifier(if context.delegated {
                    "delegated"
                } else {
                    "root"
                }),
                created_at_ms: started_at_ms,
            },
            turn_id.clone().map(|id| NewTurn {
                id,
                thread_id: thread_id.clone(),
                account: context.account.clone(),
                created_at_ms: started_at_ms,
            }),
            NewAgent {
                id: agent_id.clone(),
                thread_id: thread_id.clone(),
                parent_agent_id,
                role_kind: bounded_identifier(if context.delegated {
                    "delegated"
                } else {
                    "root"
                }),
                created_at_ms: started_at_ms,
            },
            NewOperation {
                id: operation_id,
                process_id: self.process_id,
                thread_id: Some(thread_id.clone()),
                turn_id: turn_id.clone(),
                agent_id: Some(agent_id.clone()),
                parent_operation_id: None,
                retry_of_operation_id: context.retry_of_operation_id,
                rework_of_operation_id: None,
                kind: OperationKind::ModelRequest,
                started_at_ms,
                phase: Phase::Unattributed,
                activity: Activity::Unknown,
                activity_state: ActivityState::ModelActive,
                attribution_provenance: AttributionProvenance::Unknown,
            },
            NewModelRequest {
                id: model_request_id,
                operation_id,
                provider_kind: context.provider.clone(),
                model: ModelName::new(context.model)
                    .unwrap_or_else(|_| bounded_identifier("unknown")),
                transport_kind: TransportKind::new(context.transport)
                    .unwrap_or_else(|_| bounded_identifier("unknown")),
                attempt_number: context.attempt_number,
                account: context.account.clone(),
                client_origin: ClientOrigin::new(context.client_origin)
                    .unwrap_or_else(|_| bounded_identifier("unknown")),
            },
            context
                .context_estimate
                .map(|estimate| NewModelRequestContext {
                    model_request_id,
                    policy_estimated_tokens: estimate.policy_tokens,
                    conversation_estimated_tokens: estimate.conversation_tokens,
                    tool_output_estimated_tokens: estimate.tool_output_tokens,
                    observed_at_ms: started_at_ms,
                }),
            capture_started,
        );
        self.enqueue_pending(PendingUsageRecord::Model(Arc::clone(&pending)))
            .await;
        UsageAttempt {
            runtime: Arc::clone(self),
            operation_id,
            model_request_id,
            thread_id,
            turn_id,
            agent_id,
            phase: Phase::Unattributed,
            attribution_provenance: AttributionProvenance::Unknown,
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
            repository_ids: Vec::new(),
            repository_bucket: RepositoryBucket::Unknown,
            pending,
            durable: false,
        }
    }
}
