use super::*;
use codex_usage::ApprovalOutcome;
use codex_usage::ApprovalProvenance;
use codex_usage::NewToolApprovalEvent;
use codex_usage::NewToolInvocation;
use codex_usage::ObservationTiming;
use codex_usage::OperationFamily;
use codex_usage::ToolExecutionGroupId;
use codex_usage::ToolExecutionRole;
use codex_usage::ToolInvocationId;
use codex_usage::ToolKind;
use codex_usage::ToolName as UsageToolName;

#[derive(Clone, Copy, Eq, PartialEq)]
struct DeclaredActivity {
    phase: Phase,
    activity: Activity,
    rework_of_operation_id: Option<OperationId>,
}

#[derive(Default)]
struct ActivityDeclaration {
    active: Option<DeclaredActivity>,
    staged: Option<DeclaredActivity>,
    last_heartbeat_event_id: Option<FactEventId>,
    parent_inheritance_blocked: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UsageActivityRelation {
    #[default]
    NewWork,
    ReworkPrevious,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MissingReworkTarget;

#[derive(Clone)]
struct ActiveToolOperation {
    operation_id: OperationId,
}

#[derive(Default)]
pub(super) struct ToolRuntimeState {
    activities: Mutex<HashMap<String, ActivityDeclaration>>,
    latest_models: Mutex<HashMap<String, OperationId>>,
    active_tools: Mutex<HashMap<String, ToolInvocationId>>,
    active_operations: Mutex<HashMap<String, ActiveToolOperation>>,
    approval_terminals: Mutex<HashMap<String, TerminalStatus>>,
    retries: Mutex<HashMap<String, OperationId>>,
}

#[derive(Clone, Copy)]
pub(crate) struct UsageToolDescriptor {
    pub(crate) kind: &'static str,
    pub(crate) safe_name: &'static str,
    pub(crate) family: &'static str,
    pub(crate) activity_control: bool,
    pub(crate) activity_state: ActivityState,
}

pub(crate) struct ToolAttemptContext<'a> {
    pub(crate) thread_id: &'a str,
    pub(crate) parent_thread_id: Option<&'a str>,
    pub(crate) turn_id: Option<&'a str>,
    pub(crate) delegated: bool,
    pub(crate) call_id: &'a str,
    pub(crate) cancellation_token: tokio_util::sync::CancellationToken,
    pub(crate) descriptor: UsageToolDescriptor,
    pub(crate) execution_group_id: Option<ToolExecutionGroupId>,
    pub(crate) execution_role: ToolExecutionRole,
    pub(crate) account: AccountAttributionSnapshot,
    pub(crate) repositories: Vec<RepositoryCandidate>,
}

pub(crate) struct UsageToolAttempt {
    runtime: Arc<UsageRuntime>,
    operation_id: OperationId,
    tool_invocation_id: ToolInvocationId,
    key: String,
    source_event_id: FactEventId,
    started: Instant,
    finished: AtomicBool,
    cancellation_token: tokio_util::sync::CancellationToken,
    repository_bucket: RepositoryBucket,
}

pub(crate) struct UsageWaitSpan {
    runtime: Arc<UsageRuntime>,
    id: codex_usage::ActivitySpanId,
    operation_id: OperationId,
    finished: AtomicBool,
    event_gate: Arc<tokio::sync::Semaphore>,
}

impl UsageRuntime {
    pub(super) async fn begin_tool_attempt_once(
        self: &Arc<Self>,
        context: &ToolAttemptContext<'_>,
    ) -> Result<UsageToolAttempt, CodexErr> {
        if context.call_id.len() > 512 {
            return Err(unavailable());
        }
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
        let proposed_thread_at = self
            .write_required(store.thread_created_at(&thread_id).await)?
            .unwrap_or(
                self.entity_created_at(&format!("thread:{}", context.thread_id))
                    .await,
            );
        let thread_created_at = self
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
                    created_at_ms: proposed_thread_at,
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
        let agent_created_at = self
            .write_required(store.agent_created_at(&agent_id).await)?
            .unwrap_or(thread_created_at);
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
                created_at_ms: agent_created_at,
            },
        )
        .await?;
        if let Some(turn_id) = &turn_id {
            let turn_created_at = self
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
                    account: context.account.clone(),
                    created_at_ms: turn_created_at,
                },
            )
            .await?;
        }
        let key = operation_key(context.thread_id, context.turn_id, context.call_id);
        let (phase, activity, provenance) = if context.descriptor.activity_control {
            (
                Phase::Unattributed,
                Activity::AccountingOverhead,
                AttributionProvenance::DeterministicClassification,
            )
        } else {
            self.current_tool_activity(context.thread_id).await
        };
        let parent_operation_id = self
            .tool_state
            .latest_models
            .lock()
            .await
            .get(&agent_activity_key(context.thread_id))
            .copied();
        let retry_of_operation_id = self.tool_state.retries.lock().await.remove(&key);
        let operation_id = OperationId::new();
        let tool_invocation_id = ToolInvocationId::new();
        let started_at_ms = now_ms();
        let repository_resolution = self
            .resolve_repositories(
                &store,
                context.thread_id,
                context.turn_id,
                &context.repositories,
            )
            .await?;
        let operation_kind = if context.descriptor.activity_control {
            OperationKind::ActivityControl
        } else {
            OperationKind::LocalTool
        };
        self.write_required_for(
            operation_id,
            store
                .begin_operation(&NewOperation {
                    id: operation_id,
                    process_id: self.process_id,
                    thread_id: Some(thread_id),
                    turn_id,
                    agent_id: Some(agent_id),
                    parent_operation_id,
                    retry_of_operation_id,
                    rework_of_operation_id: None,
                    kind: operation_kind,
                    started_at_ms,
                    phase,
                    activity,
                    activity_state: context.descriptor.activity_state,
                    attribution_provenance: provenance,
                })
                .await,
        )?;
        self.write_required_for(
            operation_id,
            store
                .record_tool_invocation(&NewToolInvocation {
                    id: tool_invocation_id,
                    operation_id,
                    operation_kind,
                    tool_kind: ToolKind::new(context.descriptor.kind)
                        .map_err(|_| self.reject_invalid_metadata())?,
                    safe_tool_name: UsageToolName::new(context.descriptor.safe_name)
                        .map_err(|_| self.reject_invalid_metadata())?,
                    operation_family: OperationFamily::new(context.descriptor.family)
                        .map_err(|_| self.reject_invalid_metadata())?,
                    observation_timing: ObservationTiming::new("before_execution")
                        .map_err(|_| self.reject_invalid_metadata())?,
                    covering_model_request_id: None,
                    execution_group_id: context.execution_group_id,
                    execution_role: context.execution_role,
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
        self.write_required_for(
            operation_id,
            store
                .record_coverage(&NewCoverageEvent {
                    event_id: FactEventId::new(),
                    operation_id: Some(operation_id),
                    scope_kind: CoverageScopeKind::new("tool_attempt")
                        .map_err(|_| self.reject_invalid_metadata())?,
                    state: CoverageState::CaptureStarted,
                    reason_code: None,
                    occurred_at_ms: started_at_ms,
                })
                .await,
        )?;
        self.tool_state
            .active_tools
            .lock()
            .await
            .insert(key.clone(), tool_invocation_id);
        self.tool_state
            .active_operations
            .lock()
            .await
            .insert(key.clone(), ActiveToolOperation { operation_id });
        Ok(UsageToolAttempt {
            runtime: Arc::clone(self),
            operation_id,
            tool_invocation_id,
            key,
            source_event_id: FactEventId::new(),
            started: Instant::now(),
            finished: AtomicBool::new(false),
            cancellation_token: context.cancellation_token.clone(),
            repository_bucket: repository_resolution.bucket,
        })
    }

    pub(crate) async fn record_active_tool_approval(
        &self,
        thread_id: &str,
        turn_id: Option<&str>,
        call_id: &str,
        outcome: ApprovalOutcome,
        provenance: ApprovalProvenance,
    ) {
        let key = operation_key(thread_id, turn_id, call_id);
        let Some(tool_invocation_id) = self.tool_state.active_tools.lock().await.get(&key).copied()
        else {
            return;
        };
        let operation_id = self
            .tool_state
            .active_operations
            .lock()
            .await
            .get(&key)
            .map(|active| active.operation_id);
        let Some(store) = self.store.get() else {
            self.latch_fault_with_operation(operation_id, /*recovery_allowed*/ true);
            return;
        };
        if let Err(error) = store
            .record_tool_approval(&NewToolApprovalEvent {
                event_id: FactEventId::new(),
                tool_invocation_id,
                outcome,
                provenance,
                occurred_at_ms: now_ms(),
            })
            .await
        {
            self.latch_write_failure("tool_approval", operation_id, error);
        }
        let terminal = match outcome {
            ApprovalOutcome::Denied => Some(TerminalStatus::Denied),
            ApprovalOutcome::TimedOut => Some(TerminalStatus::TimedOut),
            ApprovalOutcome::Cancelled => Some(TerminalStatus::Cancelled),
            ApprovalOutcome::NotRequired | ApprovalOutcome::Approved => None,
        };
        if let Some(terminal) = terminal {
            self.tool_state
                .approval_terminals
                .lock()
                .await
                .insert(key, terminal);
        }
    }

    pub(crate) async fn begin_active_tool_wait(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: Option<&str>,
        call_id: &str,
        state: ActivityState,
    ) -> Result<Option<UsageWaitSpan>, CodexErr> {
        let key = operation_key(thread_id, turn_id, call_id);
        let Some(active) = self
            .tool_state
            .active_operations
            .lock()
            .await
            .get(&key)
            .cloned()
        else {
            return Ok(None);
        };
        let store = self.store().await?;
        let id = codex_usage::ActivitySpanId::new();
        self.write_required_for(
            active.operation_id,
            store
                .begin_activity_span(&codex_usage::NewActivitySpan {
                    id,
                    operation_id: active.operation_id,
                    activity_state: state,
                    started_at_ms: now_ms(),
                })
                .await,
        )?;
        Ok(Some(UsageWaitSpan {
            runtime: Arc::clone(self),
            id,
            operation_id: active.operation_id,
            finished: AtomicBool::new(false),
            event_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        }))
    }

    pub(crate) async fn stage_activity(
        &self,
        thread_id: &str,
        phase: Phase,
        activity: Activity,
        relation: UsageActivityRelation,
    ) -> Result<(), MissingReworkTarget> {
        let rework_of_operation_id = match relation {
            UsageActivityRelation::NewWork => None,
            UsageActivityRelation::ReworkPrevious => Some(
                self.tool_state
                    .latest_models
                    .lock()
                    .await
                    .get(&agent_activity_key(thread_id))
                    .copied()
                    .ok_or(MissingReworkTarget)?,
            ),
        };
        let mut activities = self.tool_state.activities.lock().await;
        let declaration = activities.entry(agent_activity_key(thread_id)).or_default();
        declaration.staged = Some(DeclaredActivity {
            phase,
            activity,
            rework_of_operation_id,
        });
        declaration.parent_inheritance_blocked = false;
        Ok(())
    }

    pub(crate) async fn heartbeat_activity(&self, thread_id: &str) {
        let key = agent_activity_key(thread_id);
        let (active, supersedes_event_id) = {
            let declarations = self.tool_state.activities.lock().await;
            let Some(declaration) = declarations.get(&key) else {
                return;
            };
            let Some(active) = declaration.active else {
                return;
            };
            (active, declaration.last_heartbeat_event_id)
        };
        let operation_id = self
            .tool_state
            .latest_models
            .lock()
            .await
            .get(&key)
            .copied();
        let (Some(operation_id), Some(store)) = (operation_id, self.store.get()) else {
            return;
        };
        let event_id = FactEventId::new();
        if let Err(error) = store
            .record_classification(&codex_usage::NewClassificationEvent {
                event_id,
                operation_id,
                phase: active.phase,
                activity: active.activity,
                activity_state: ActivityState::ModelActive,
                provenance: AttributionProvenance::AgentDeclared,
                supersedes_event_id,
                occurred_at_ms: now_ms(),
            })
            .await
        {
            self.latch_write_failure("activity_heartbeat", Some(operation_id), error);
            return;
        }
        let mut declarations = self.tool_state.activities.lock().await;
        if let Some(declaration) = declarations.get_mut(&key)
            && declaration.active == Some(active)
            && declaration.last_heartbeat_event_id == supersedes_event_id
        {
            declaration.last_heartbeat_event_id = Some(event_id);
        }
    }

    pub(crate) async fn end_activity(&self, thread_id: &str) {
        let mut declarations = self.tool_state.activities.lock().await;
        *declarations
            .entry(agent_activity_key(thread_id))
            .or_default() = ActivityDeclaration {
            parent_inheritance_blocked: true,
            ..ActivityDeclaration::default()
        };
    }

    pub(super) async fn activate_model_activity(
        &self,
        thread_id: &str,
        parent_thread_id: Option<&str>,
    ) -> (Phase, Activity, AttributionProvenance, Option<OperationId>) {
        let mut declarations = self.tool_state.activities.lock().await;
        let key = agent_activity_key(thread_id);
        let inherit_parent = {
            let declaration = declarations.entry(key.clone()).or_default();
            promote_staged_activity(declaration);
            declaration.active.is_none() && !declaration.parent_inheritance_blocked
        };
        if inherit_parent
            && let Some(parent_key) = parent_thread_id
                .filter(|parent_thread_id| *parent_thread_id != thread_id)
                .map(agent_activity_key)
            && let Some(parent) = declarations.get_mut(&parent_key)
        {
            promote_staged_activity(parent);
            if let Some(mut inherited) = parent.active {
                inherited.rework_of_operation_id = None;
                let declaration = declarations.entry(key.clone()).or_default();
                declaration.active = Some(inherited);
                declaration.last_heartbeat_event_id = None;
            }
        }
        let declaration = declarations.entry(key).or_default();
        let (phase, activity, provenance) = classification(declaration.active);
        let rework_of_operation_id = declaration
            .active
            .and_then(|activity| activity.rework_of_operation_id);
        if let Some(active) = declaration.active.as_mut() {
            active.rework_of_operation_id = None;
        }
        (phase, activity, provenance, rework_of_operation_id)
    }

    async fn current_tool_activity(
        &self,
        thread_id: &str,
    ) -> (Phase, Activity, AttributionProvenance) {
        let declarations = self.tool_state.activities.lock().await;
        classification(
            declarations
                .get(&agent_activity_key(thread_id))
                .and_then(|declaration| declaration.active),
        )
    }

    pub(super) async fn note_latest_model_operation(
        &self,
        thread_id: &str,
        operation_id: OperationId,
    ) {
        self.tool_state
            .latest_models
            .lock()
            .await
            .insert(agent_activity_key(thread_id), operation_id);
    }
}

fn promote_staged_activity(declaration: &mut ActivityDeclaration) {
    if let Some(staged) = declaration.staged.take() {
        declaration.active = Some(staged);
        declaration.last_heartbeat_event_id = None;
    }
}

impl UsageToolAttempt {
    pub(crate) async fn record_provider_usage(&self, usage: &ProviderUsage) {
        let Some(store) = self.runtime.store.get() else {
            self.runtime.latch_operation_fault(self.operation_id);
            return;
        };
        let coverage = if usage.categories_complete() {
            CoverageState::Complete
        } else {
            CoverageState::Partial
        };
        let observed_at_ms = now_ms();
        for (path, count) in provider_counts(usage) {
            let (token_count, state) = match count {
                ProviderTokenCount::Absent => continue,
                ProviderTokenCount::Value(count) => (Some(count), coverage),
                ProviderTokenCount::Null => (None, CoverageState::Unknown),
                ProviderTokenCount::Invalid => (None, CoverageState::Partial),
            };
            let Ok(category_path) = TokenCategoryPath::new(path) else {
                self.runtime.latch_operation_fault(self.operation_id);
                return;
            };
            if let Err(error) = store
                .record_token_observation(&NewTokenObservation {
                    id: FactEventId::new(),
                    source_event_id: self.source_event_id,
                    source: TokenObservationSource::ToolInvocation(self.tool_invocation_id),
                    category_path,
                    token_count,
                    unit: TokenUnit::Tokens,
                    measurement_provenance: MeasurementProvenance::ProviderReported,
                    coverage_state: state,
                    repository_bucket: self.repository_bucket.clone(),
                    observed_at_ms,
                })
                .await
            {
                self.runtime.latch_write_failure(
                    "tool_token_observation",
                    Some(self.operation_id),
                    error,
                );
                return;
            }
        }
    }

    pub(crate) async fn finish(&self, status: TerminalStatus, error: Option<ErrorCategory>) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        self.runtime
            .finish_tool(
                self.operation_id,
                self.key.clone(),
                status,
                error,
                u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            )
            .await;
    }

    pub(crate) async fn finish_error(
        &self,
        cancelled: bool,
        typed_outcome: Option<codex_tools::UsageTerminalOutcome>,
    ) {
        let approval_terminal = self
            .runtime
            .tool_state
            .approval_terminals
            .lock()
            .await
            .get(&self.key)
            .copied();
        let typed_status = typed_outcome.map(|outcome| match outcome.status {
            codex_tools::UsageTerminalStatus::Completed => TerminalStatus::Completed,
            codex_tools::UsageTerminalStatus::Failed => TerminalStatus::Failed,
            codex_tools::UsageTerminalStatus::Denied => TerminalStatus::Denied,
            codex_tools::UsageTerminalStatus::TimedOut => TerminalStatus::TimedOut,
            codex_tools::UsageTerminalStatus::Cancelled => TerminalStatus::Cancelled,
        });
        let status = approval_terminal
            .or_else(|| cancelled.then_some(TerminalStatus::Cancelled))
            .or(typed_status)
            .unwrap_or(TerminalStatus::Failed);
        let typed_error = typed_outcome
            .and_then(|outcome| outcome.error_category)
            .map(|category| match category {
                codex_tools::UsageTerminalErrorCategory::Tool => ErrorCategory::Tool,
                codex_tools::UsageTerminalErrorCategory::Timeout => ErrorCategory::Timeout,
                codex_tools::UsageTerminalErrorCategory::Cancelled => ErrorCategory::Cancelled,
                codex_tools::UsageTerminalErrorCategory::Provider => ErrorCategory::Provider,
            });
        let error = typed_error.unwrap_or(match status {
            TerminalStatus::Cancelled => ErrorCategory::Cancelled,
            TerminalStatus::TimedOut => ErrorCategory::Timeout,
            TerminalStatus::Denied => ErrorCategory::Tool,
            TerminalStatus::Completed
            | TerminalStatus::Incomplete
            | TerminalStatus::Failed
            | TerminalStatus::Interrupted => ErrorCategory::Tool,
        });
        self.finish(status, Some(error)).await;
    }
}

impl Drop for UsageToolAttempt {
    fn drop(&mut self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let runtime = Arc::clone(&self.runtime);
        let operation_id = self.operation_id;
        let key = self.key.clone();
        let duration_ns = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let cancelled = self.cancellation_token.is_cancelled();
        drop(handle.spawn(async move {
            runtime
                .finish_tool(
                    operation_id,
                    key,
                    if cancelled {
                        TerminalStatus::Cancelled
                    } else {
                        TerminalStatus::Interrupted
                    },
                    Some(if cancelled {
                        ErrorCategory::Cancelled
                    } else {
                        ErrorCategory::Tool
                    }),
                    duration_ns,
                )
                .await;
        }));
    }
}

impl UsageWaitSpan {
    pub(crate) async fn heartbeat(&self) {
        let Ok(_event_permit) = self.event_gate.acquire().await else {
            self.runtime.latch_operation_fault(self.operation_id);
            return;
        };
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        self.record_event(codex_usage::ActivitySpanEventKind::Heartbeat)
            .await;
    }

    pub(crate) async fn finish(&self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(_event_permit) = self.event_gate.acquire().await else {
            self.runtime.latch_operation_fault(self.operation_id);
            return;
        };
        self.record_event(codex_usage::ActivitySpanEventKind::Ended)
            .await;
    }

    async fn record_event(&self, kind: codex_usage::ActivitySpanEventKind) {
        let Some(store) = self.runtime.store.get() else {
            self.runtime.latch_operation_fault(self.operation_id);
            return;
        };
        if let Err(error) = store
            .record_activity_span_event(&codex_usage::NewActivitySpanEvent {
                event_id: FactEventId::new(),
                activity_span_id: self.id,
                kind,
                occurred_at_ms: now_ms(),
            })
            .await
        {
            self.runtime
                .latch_write_failure("tool_wait_event", Some(self.operation_id), error);
        }
    }
}

impl Drop for UsageWaitSpan {
    fn drop(&mut self) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.runtime.latch_operation_fault(self.operation_id);
            return;
        };
        let runtime = Arc::clone(&self.runtime);
        let activity_span_id = self.id;
        let operation_id = self.operation_id;
        let event_gate = Arc::clone(&self.event_gate);
        drop(handle.spawn(async move {
            let Ok(_event_permit) = event_gate.acquire().await else {
                runtime.latch_operation_fault(operation_id);
                return;
            };
            let Some(store) = runtime.store.get() else {
                runtime.latch_operation_fault(operation_id);
                return;
            };
            if let Err(error) = store
                .record_activity_span_event(&codex_usage::NewActivitySpanEvent {
                    event_id: FactEventId::new(),
                    activity_span_id,
                    kind: codex_usage::ActivitySpanEventKind::Ended,
                    occurred_at_ms: now_ms(),
                })
                .await
            {
                runtime.latch_write_failure("tool_wait_drop", Some(operation_id), error);
            }
        }));
    }
}

impl UsageRuntime {
    async fn finish_tool(
        &self,
        operation_id: OperationId,
        key: String,
        status: TerminalStatus,
        error: Option<ErrorCategory>,
        duration_ns: u64,
    ) {
        self.tool_state.active_tools.lock().await.remove(&key);
        self.tool_state.active_operations.lock().await.remove(&key);
        self.tool_state.approval_terminals.lock().await.remove(&key);
        let Some(store) = self.store.get() else {
            self.latch_operation_fault(operation_id);
            return;
        };
        if let Err(error) = store
            .finish_operation(&TerminalOperation {
                operation_id,
                status,
                occurred_at_ms: now_ms(),
                duration_ns,
                error_category: error,
            })
            .await
        {
            self.latch_write_failure("tool_operation_terminal", Some(operation_id), error);
            return;
        }
        if let Err(error) = store
            .record_coverage(&NewCoverageEvent {
                event_id: FactEventId::new(),
                operation_id: Some(operation_id),
                scope_kind: match CoverageScopeKind::new("tool_attempt") {
                    Ok(scope) => scope,
                    Err(_) => {
                        self.latch_operation_fault(operation_id);
                        return;
                    }
                },
                state: CoverageState::Partial,
                reason_code: None,
                occurred_at_ms: now_ms(),
            })
            .await
        {
            self.latch_write_failure("tool_attempt_coverage", Some(operation_id), error);
        }
        if !matches!(status, TerminalStatus::Completed) {
            self.tool_state
                .retries
                .lock()
                .await
                .insert(key, operation_id);
        }
    }
}

impl UsageAttempt {
    pub(super) async fn record_hosted_tool(&self, item: &ResponseItem) {
        let (item_id, status, descriptor, activity) = match item {
            ResponseItem::WebSearchCall { id, status, .. } => (
                id.as_ref().map(codex_protocol::ResponseItemId::as_str),
                status.as_deref(),
                UsageToolDescriptor {
                    kind: "hosted",
                    safe_name: "web_search",
                    family: "research",
                    activity_control: false,
                    activity_state: ActivityState::ToolActive,
                },
                Activity::Research,
            ),
            ResponseItem::ImageGenerationCall { id, status, .. } => (
                id.as_ref().map(codex_protocol::ResponseItemId::as_str),
                Some(status.as_str()),
                UsageToolDescriptor {
                    kind: "hosted",
                    safe_name: "image_generation",
                    family: "generation",
                    activity_control: false,
                    activity_state: ActivityState::ToolActive,
                },
                Activity::UserElaboration,
            ),
            _ => return,
        };
        let dedupe_key = format!(
            "{}:{}",
            self.model_request_id.as_string(),
            item_id.unwrap_or(descriptor.safe_name)
        );
        if !self
            .hosted_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(dedupe_key.clone())
        {
            return;
        }
        let operation_id = stable_operation_id("hosted-operation", &dedupe_key);
        let Some(store) = self.runtime.store.get() else {
            self.runtime.latch_operation_fault(operation_id);
            return;
        };
        let tool_invocation_id = stable_tool_id("hosted-tool", &dedupe_key);
        let occurred_at_ms = now_ms();
        let terminal = hosted_terminal(status);
        let repository_resolution = repository::RepositoryResolution {
            ids: self.repository_ids.clone(),
            bucket: self.repository_bucket.clone(),
        };
        let writes = async {
            store
                .begin_operation(&NewOperation {
                    id: operation_id,
                    process_id: self.runtime.process_id,
                    thread_id: Some(self.thread_id.clone()),
                    turn_id: self.turn_id.clone(),
                    agent_id: Some(self.agent_id.clone()),
                    parent_operation_id: Some(self.operation_id),
                    retry_of_operation_id: None,
                    rework_of_operation_id: None,
                    kind: OperationKind::HostedTool,
                    started_at_ms: occurred_at_ms,
                    phase: self.phase,
                    activity,
                    activity_state: ActivityState::ToolActive,
                    attribution_provenance: self.attribution_provenance,
                })
                .await?;
            self.runtime
                .record_repository_resolution(
                    store,
                    operation_id,
                    &repository_resolution,
                    occurred_at_ms,
                )
                .await
                .map_err(|_| codex_usage::UsageStoreError::InvalidFact)?;
            store
                .record_tool_invocation(&NewToolInvocation {
                    id: tool_invocation_id,
                    operation_id,
                    operation_kind: OperationKind::HostedTool,
                    tool_kind: ToolKind::new(descriptor.kind)
                        .map_err(|_| codex_usage::UsageStoreError::InvalidFact)?,
                    safe_tool_name: UsageToolName::new(descriptor.safe_name)
                        .map_err(|_| codex_usage::UsageStoreError::InvalidFact)?,
                    operation_family: OperationFamily::new(descriptor.family)
                        .map_err(|_| codex_usage::UsageStoreError::InvalidFact)?,
                    observation_timing: ObservationTiming::new("observed_after_execution")
                        .map_err(|_| codex_usage::UsageStoreError::InvalidFact)?,
                    covering_model_request_id: Some(self.model_request_id),
                    execution_group_id: None,
                    execution_role: ToolExecutionRole::Standalone,
                })
                .await?;
            store
                .finish_operation(&TerminalOperation {
                    operation_id,
                    status: terminal,
                    occurred_at_ms,
                    duration_ns: 0,
                    error_category: (!matches!(terminal, TerminalStatus::Completed))
                        .then_some(ErrorCategory::Provider),
                })
                .await?;
            store
                .record_coverage(&NewCoverageEvent {
                    event_id: FactEventId::new(),
                    operation_id: Some(operation_id),
                    scope_kind: CoverageScopeKind::new("hosted_tool_tokens")
                        .map_err(|_| codex_usage::UsageStoreError::InvalidFact)?,
                    state: CoverageState::Unknown,
                    reason_code: None,
                    occurred_at_ms,
                })
                .await
        }
        .await;
        if let Err(error) = writes {
            self.runtime
                .latch_write_failure("hosted_tool_observation", Some(operation_id), error);
        }
    }
}

fn classification(
    declaration: Option<DeclaredActivity>,
) -> (Phase, Activity, AttributionProvenance) {
    declaration.map_or(
        (
            Phase::Unattributed,
            Activity::Unknown,
            AttributionProvenance::Unknown,
        ),
        |declaration| {
            (
                declaration.phase,
                declaration.activity,
                AttributionProvenance::AgentDeclared,
            )
        },
    )
}

fn agent_activity_key(thread_id: &str) -> String {
    stable_key("agent_activity", &[thread_id])
}

pub(super) fn activity_key(thread_id: &str, turn_id: Option<&str>) -> String {
    stable_key("activity", &[thread_id, turn_id.unwrap_or("")])
}

fn operation_key(thread_id: &str, turn_id: Option<&str>, call_id: &str) -> String {
    stable_key("tool", &[thread_id, turn_id.unwrap_or(""), call_id])
}

fn stable_key(namespace: &str, values: &[&str]) -> String {
    let mut input = namespace.as_bytes().to_vec();
    for value in values {
        input.push(0);
        input.extend_from_slice(value.as_bytes());
    }
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, &input).to_string()
}

fn stable_operation_id(namespace: &str, value: &str) -> OperationId {
    OperationId::from_stable_key(stable_key(namespace, &[value]).as_bytes())
}

fn stable_tool_id(namespace: &str, value: &str) -> ToolInvocationId {
    ToolInvocationId::from_stable_key(stable_key(namespace, &[value]).as_bytes())
}

fn hosted_terminal(status: Option<&str>) -> TerminalStatus {
    match status {
        Some("completed") => TerminalStatus::Completed,
        Some("failed") => TerminalStatus::Failed,
        Some("cancelled") => TerminalStatus::Cancelled,
        Some("timed_out") => TerminalStatus::TimedOut,
        Some("incomplete") | Some("in_progress") | None | Some(_) => TerminalStatus::Incomplete,
    }
}
