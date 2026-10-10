use super::*;
use codex_usage::ObservationTiming;
use codex_usage::OperationFamily;
use codex_usage::ToolKind;
use codex_usage::ToolName as UsageToolName;

#[path = "usage_runtime_buffer_model.rs"]
mod model;
#[path = "usage_runtime_buffer_tool.rs"]
mod tool;

pub(super) use model::PendingModelAttempt;
pub(super) use tool::PendingToolAttempt;

pub(super) const PENDING_USAGE_CAPACITY: usize = 256;

#[derive(Clone)]
pub(super) enum PendingUsageRecord {
    Model(Arc<PendingModelAttempt>),
    Tool(Arc<PendingToolAttempt>),
}

impl PendingUsageRecord {
    fn same_record(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Model(left), Self::Model(right)) => Arc::ptr_eq(left, right),
            (Self::Tool(left), Self::Tool(right)) => Arc::ptr_eq(left, right),
            (Self::Model(_), Self::Tool(_)) | (Self::Tool(_), Self::Model(_)) => false,
        }
    }

    async fn replay(&self, store: &UsageStore) -> Result<bool, UsageStoreError> {
        match self {
            Self::Model(record) => record.replay(store).await,
            Self::Tool(record) => record.replay(store).await,
        }
    }

    fn increment_replay_failures(&self) -> u32 {
        match self {
            Self::Model(record) => record.replay_failures.fetch_add(1, Ordering::AcqRel) + 1,
            Self::Tool(record) => record.replay_failures.fetch_add(1, Ordering::AcqRel) + 1,
        }
    }
}

#[derive(Clone)]
struct PendingBaseFacts {
    thread: NewThread,
    turn: Option<NewTurn>,
    agent: NewAgent,
    operation: NewOperation,
}

impl UsageRuntime {
    pub(super) async fn enqueue_pending(&self, record: PendingUsageRecord) {
        let mut pending = self.pending_usage.lock().await;
        if pending.iter().any(|existing| existing.same_record(&record)) {
            return;
        }
        let dropped = if pending.len() == PENDING_USAGE_CAPACITY {
            pending.pop_front().is_some()
        } else {
            false
        };
        pending.push_back(record);
        let pending_records = pending.len();
        drop(pending);
        if dropped {
            tracing::warn!(
                pending_records,
                "usage accounting retry cache reached capacity; the oldest record was dropped and work will continue"
            );
        } else {
            tracing::warn!(
                pending_records,
                "usage accounting is unavailable; work will continue with a pending in-memory record"
            );
        }
    }

    pub(super) async fn flush_pending_usage(&self) {
        if self.pending_usage.lock().await.is_empty() && !self.faulted.load(Ordering::Acquire) {
            return;
        }
        let Ok(_permit) = self.pending_flush_gate.try_acquire() else {
            return;
        };
        if self.faulted.load(Ordering::Acquire)
            && (!self.fault_recovery_allowed.load(Ordering::Acquire)
                || self.recover_after_write_failure().await.is_err())
        {
            let pending_records = self.pending_usage.lock().await.len();
            tracing::warn!(
                pending_records,
                "usage accounting retry deferred; work will continue"
            );
            return;
        }
        if self.pending_usage.lock().await.is_empty() {
            return;
        }
        let store = match self.store().await {
            Ok(store) => store,
            Err(_) => return,
        };
        let records = self
            .pending_usage
            .lock()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let mut replayed = 0_usize;
        for record in records {
            match record.replay(&store).await {
                Ok(false) => continue,
                Ok(true) => {
                    self.pending_usage
                        .lock()
                        .await
                        .retain(|queued| !queued.same_record(&record));
                    replayed = replayed.saturating_add(1);
                }
                Err(error) => {
                    let retry = record.increment_replay_failures();
                    self.latch_write_failure("pending_usage_replay", None, error);
                    tracing::warn!(retry, "usage accounting retry failed; work will continue");
                    break;
                }
            }
        }
        if replayed > 0 {
            let pending_records = self.pending_usage.lock().await.len();
            tracing::info!(
                replayed_records = replayed,
                pending_records,
                "usage accounting pending records recovered"
            );
        }
    }
}

async fn replay_base(store: &UsageStore, facts: &PendingBaseFacts) -> Result<(), UsageStoreError> {
    let mut thread = facts.thread.clone();
    match store.ensure_thread(&thread).await {
        Ok(()) => {}
        Err(UsageStoreError::FactConflict) => {
            thread.created_at_ms = store
                .thread_created_at(&thread.id)
                .await?
                .ok_or(UsageStoreError::FactConflict)?;
            store.ensure_thread(&thread).await?;
        }
        Err(error) => return Err(error),
    }
    if let Some(mut turn) = facts.turn.clone() {
        match store.ensure_turn(&turn).await {
            Ok(()) => {}
            Err(UsageStoreError::FactConflict) => {
                turn.created_at_ms = store
                    .turn_created_at(&turn.id)
                    .await?
                    .ok_or(UsageStoreError::FactConflict)?;
                if !store.turn_identity_matches(&turn).await? {
                    return Err(UsageStoreError::FactConflict);
                }
            }
            Err(error) => return Err(error),
        }
    }
    let mut agent = facts.agent.clone();
    match store.ensure_agent(&agent).await {
        Ok(()) => {}
        Err(UsageStoreError::FactConflict) => {
            agent.created_at_ms = store
                .agent_created_at(&agent.id)
                .await?
                .ok_or(UsageStoreError::FactConflict)?;
            store.ensure_agent(&agent).await?;
        }
        Err(error) => return Err(error),
    }
    store.begin_operation(&facts.operation).await
}

fn safe_thread_id(value: &str) -> ThreadId {
    ThreadId::new(value).unwrap_or_else(|_| bounded_identifier(OperationId::new().as_string()))
}

fn safe_agent_id(value: &str) -> AgentId {
    AgentId::new(value).unwrap_or_else(|_| bounded_identifier(OperationId::new().as_string()))
}

fn bounded_identifier<T>(value: impl Into<String>) -> T
where
    T: BoundedIdentifier,
{
    T::from_bounded(value.into())
}

trait BoundedIdentifier: Sized {
    fn from_bounded(value: String) -> Self;
}

macro_rules! bounded_identifier_impl {
    ($($kind:ty),+ $(,)?) => {
        $(impl BoundedIdentifier for $kind {
            fn from_bounded(value: String) -> Self {
                <$kind>::new(value).expect("static usage identifier must be valid")
            }
        })+
    };
}

bounded_identifier_impl!(
    AgentId,
    AgentRoleKind,
    ClientOrigin,
    CoverageScopeKind,
    ModelName,
    ObservationTiming,
    OperationFamily,
    ThreadId,
    ThreadSourceKind,
    ToolKind,
    TransportKind,
    UsageToolName,
);
