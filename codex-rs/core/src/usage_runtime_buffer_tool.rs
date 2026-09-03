use super::super::*;
use super::PendingBaseFacts;
use super::PendingUsageRecord;
use super::bounded_identifier;
use super::replay_base;
use super::safe_agent_id;
use super::safe_thread_id;
use crate::usage_runtime::tool::ActiveToolOperation;
use crate::usage_runtime::tool::ToolAttemptContext;
use crate::usage_runtime::tool::UsageToolAttempt;
use codex_usage::NewToolApprovalEvent;
use codex_usage::NewToolInvocation;
use codex_usage::ObservationTiming;
use codex_usage::OperationFamily;
use codex_usage::ToolInvocationId;
use codex_usage::ToolKind;
use codex_usage::ToolName as UsageToolName;

struct PendingToolState {
    base: PendingBaseFacts,
    invocation: NewToolInvocation,
    approvals: Vec<NewToolApprovalEvent>,
    tokens: Vec<NewTokenObservation>,
    coverage: Vec<NewCoverageEvent>,
    terminal: Option<TerminalOperation>,
}

pub(in super::super) struct PendingToolFinish {
    pub(in super::super) terminal: TerminalOperation,
    pub(in super::super) attempt_coverage: NewCoverageEvent,
}

pub(in super::super) struct PendingToolAttempt {
    state: std::sync::Mutex<PendingToolState>,
    pub(super) replay_failures: AtomicU32,
}

impl PendingToolAttempt {
    pub(in super::super) fn new(
        thread: NewThread,
        turn: Option<NewTurn>,
        agent: NewAgent,
        operation: NewOperation,
        invocation: NewToolInvocation,
        capture_started: NewCoverageEvent,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(PendingToolState {
                base: PendingBaseFacts {
                    thread,
                    turn,
                    agent,
                    operation,
                },
                invocation,
                approvals: Vec::new(),
                tokens: Vec::new(),
                coverage: vec![capture_started],
                terminal: None,
            }),
            replay_failures: AtomicU32::new(0),
        })
    }

    pub(in super::super) fn record_approval(&self, event: NewToolApprovalEvent) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .approvals
            .push(event);
    }

    pub(in super::super) fn record_provider_usage(
        &self,
        source_event_id: FactEventId,
        usage: &ProviderUsage,
        repository_bucket: RepositoryBucket,
        observed_at_ms: i64,
    ) -> Vec<NewTokenObservation> {
        let coverage = if usage.categories_complete() {
            CoverageState::Complete
        } else {
            CoverageState::Partial
        };
        let tool_invocation_id = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invocation
            .id;
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
                            stage = "pending_tool_token_category",
                            "usage accounting metadata was skipped; work will continue"
                        );
                        return None;
                    }
                };
                Some(NewTokenObservation {
                    id: FactEventId::new(),
                    source_event_id,
                    source: TokenObservationSource::ToolInvocation(tool_invocation_id),
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
    ) -> Option<PendingToolFinish> {
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
        let attempt_coverage = NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(operation_id),
            scope_kind: bounded_identifier("tool_attempt"),
            state: CoverageState::Partial,
            reason_code: None,
            occurred_at_ms,
        };
        state.terminal = Some(terminal.clone());
        state.coverage.push(attempt_coverage.clone());
        Some(PendingToolFinish {
            terminal,
            attempt_coverage,
        })
    }

    pub(super) async fn replay(&self, store: &UsageStore) -> Result<bool, UsageStoreError> {
        let Some(snapshot) = self.snapshot_if_finished() else {
            return Ok(false);
        };
        replay_base(store, &snapshot.base).await?;
        store.record_tool_invocation(&snapshot.invocation).await?;
        for approval in &snapshot.approvals {
            store.record_tool_approval(approval).await?;
        }
        for token in &snapshot.tokens {
            store.record_token_observation(token).await?;
        }
        if let Some(terminal) = &snapshot.terminal {
            store.finish_operation(terminal).await?;
        }
        for coverage in &snapshot.coverage {
            store.record_coverage(coverage).await?;
        }
        Ok(true)
    }

    fn snapshot_if_finished(&self) -> Option<PendingToolState> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.terminal.as_ref()?;
        Some(PendingToolState {
            base: state.base.clone(),
            invocation: state.invocation.clone(),
            approvals: state.approvals.clone(),
            tokens: state.tokens.clone(),
            coverage: state.coverage.clone(),
            terminal: state.terminal.clone(),
        })
    }
}

impl UsageRuntime {
    pub(in super::super) async fn begin_buffered_tool_attempt(
        self: &Arc<Self>,
        context: &ToolAttemptContext<'_>,
    ) -> UsageToolAttempt {
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
        let tool_invocation_id = ToolInvocationId::new();
        let operation_kind = if context.descriptor.activity_control {
            OperationKind::ActivityControl
        } else {
            OperationKind::LocalTool
        };
        let capture_started = NewCoverageEvent {
            event_id: FactEventId::new(),
            operation_id: Some(operation_id),
            scope_kind: bounded_identifier("tool_attempt"),
            state: CoverageState::CaptureStarted,
            reason_code: None,
            occurred_at_ms: started_at_ms,
        };
        let pending = PendingToolAttempt::new(
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
                retry_of_operation_id: None,
                rework_of_operation_id: None,
                kind: operation_kind,
                started_at_ms,
                phase: Phase::Unattributed,
                activity: if context.descriptor.activity_control {
                    Activity::AccountingOverhead
                } else {
                    Activity::Unknown
                },
                activity_state: context.descriptor.activity_state,
                attribution_provenance: AttributionProvenance::Unknown,
            },
            NewToolInvocation {
                id: tool_invocation_id,
                operation_id,
                operation_kind,
                tool_kind: ToolKind::new(context.descriptor.kind)
                    .unwrap_or_else(|_| bounded_identifier("unknown")),
                safe_tool_name: UsageToolName::new(context.descriptor.safe_name)
                    .unwrap_or_else(|_| bounded_identifier("unknown")),
                operation_family: OperationFamily::new(context.descriptor.family)
                    .unwrap_or_else(|_| bounded_identifier("unknown")),
                observation_timing: ObservationTiming::new("before_execution")
                    .unwrap_or_else(|_| bounded_identifier("unknown")),
                covering_model_request_id: None,
                execution_group_id: context.execution_group_id,
                execution_role: context.execution_role,
            },
            capture_started,
        );
        self.enqueue_pending(PendingUsageRecord::Tool(Arc::clone(&pending)))
            .await;
        let key = crate::usage_runtime::tool::operation_key(
            context.thread_id,
            context.turn_id,
            context.call_id,
        );
        self.tool_state
            .active_tools
            .lock()
            .await
            .insert(key.clone(), tool_invocation_id);
        self.tool_state.active_operations.lock().await.insert(
            key.clone(),
            ActiveToolOperation {
                operation_id,
                pending: Arc::clone(&pending),
                durable: false,
            },
        );
        UsageToolAttempt {
            runtime: Arc::clone(self),
            operation_id,
            key,
            source_event_id: FactEventId::new(),
            started: Instant::now(),
            finished: AtomicBool::new(false),
            cancellation_token: context.cancellation_token.clone(),
            repository_bucket: RepositoryBucket::Unknown,
            pending,
            durable: false,
        }
    }
}
