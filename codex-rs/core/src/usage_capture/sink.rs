use super::work_binding::BoundaryTracker;
use super::work_binding::PendingWork;
use super::work_binding::UsageWorkAction;
use super::work_binding::UsageWorkBoundaryState;
use super::work_binding::UsageWorkBoundaryStatus;
use super::work_binding::UsageWorkReceipt;
use super::work_binding::boundary_status;
use codex_usage::UsageRecord;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::task::JoinHandle;

const MAX_PENDING_RECORDS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Persistence {
    Recorded,
    Pending,
    Omitted,
}

struct PendingRecord {
    record: UsageRecord,
    receipt: Option<tokio::sync::oneshot::Sender<Persistence>>,
}

// Keep the frequent observation inline, without adding an allocation per capture.
#[allow(clippy::large_enum_variant)]
enum PendingCapture {
    Record(PendingRecord),
    Work(PendingWork),
}

pub(super) struct CaptureSink {
    home: PathBuf,
    pending: Mutex<VecDeque<PendingCapture>>,
    gap: Mutex<Option<UsageRecord>>,
    loss: Mutex<CoverageLoss>,
    dropped: AtomicU64,
    writing: AtomicBool,
    deferred: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    flushing: tokio::sync::Semaphore,
    boundaries: Mutex<BoundaryTracker>,
}

#[derive(Default)]
struct CoverageLoss {
    count: u64,
    thread_id: Option<String>,
    parent_thread_id: Option<String>,
}

impl CaptureSink {
    pub(super) fn new(home: &Path) -> Arc<Self> {
        Arc::new(Self {
            home: home.to_path_buf(),
            pending: Mutex::default(),
            gap: Mutex::default(),
            loss: Mutex::default(),
            dropped: AtomicU64::new(0),
            writing: AtomicBool::new(false),
            deferred: AtomicBool::new(false),
            tasks: Mutex::default(),
            flushing: tokio::sync::Semaphore::new(/*permits*/ 1),
            boundaries: Mutex::default(),
        })
    }

    pub(super) fn submit(self: &Arc<Self>, record: UsageRecord) {
        self.enqueue(PendingCapture::Record(PendingRecord {
            record,
            receipt: None,
        }));
    }

    pub(super) async fn submit_confirmed(self: &Arc<Self>, record: UsageRecord) -> Persistence {
        let (receipt, mut received) = tokio::sync::oneshot::channel();
        self.enqueue(PendingCapture::Record(PendingRecord {
            record,
            receipt: Some(receipt),
        }));
        self.flush().await;
        received.try_recv().unwrap_or(Persistence::Pending)
    }

    pub(super) async fn submit_work(
        self: &Arc<Self>,
        thread_id: String,
        action: UsageWorkAction,
    ) -> Result<UsageWorkReceipt, String> {
        action.validate()?;
        let (receipt, received) = tokio::sync::oneshot::channel();
        self.enqueue(PendingCapture::Work(PendingWork {
            thread_id,
            action,
            receipt,
        }));
        // Accounting cannot stall model/tool progress on a blocked filesystem.
        tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 2), received)
            .await
            .map_err(|_| {
                "usage_work persistence pending; inspect status before retrying".to_string()
            })?
            .map_err(|_| "usage_work capture unavailable".to_string())?
    }

    pub(super) async fn submit_work_for_turn(
        self: &Arc<Self>,
        thread_id: String,
        action: UsageWorkAction,
        boundary_id: Option<String>,
        boundary_state: Option<UsageWorkBoundaryState>,
    ) -> Result<UsageWorkReceipt, String> {
        let mut receipt = self.submit_work(thread_id.clone(), action).await?;
        if let Some(boundary_id) = boundary_id {
            receipt.boundary = Some(
                self.boundary_status(thread_id, boundary_id, boundary_state)
                    .await?,
            );
        }
        Ok(receipt)
    }

    pub(super) async fn boundary_status(
        self: &Arc<Self>,
        thread_id: String,
        boundary_id: String,
        explicit_state: Option<UsageWorkBoundaryState>,
    ) -> Result<UsageWorkBoundaryStatus, String> {
        // A boundary must observe all earlier operation starts and declarations. The
        // flush is bounded by the same worker coordination used by ordinary controls.
        self.flush().await;
        if explicit_state == Some(UsageWorkBoundaryState::Clear) {
            let current =
                codex_usage::read_current_work_binding(&self.home, &thread_id).map_err(|_| {
                    "usage_work storage unavailable; inspect status before retrying".to_string()
                })?;
            if let Some(current) = current.filter(|value| value.update.native_project_id.is_some())
            {
                self.submit_work(
                    thread_id.clone(),
                    UsageWorkAction::Clear {
                        expected_revision: current.update.transition_sequence,
                    },
                )
                .await?;
            }
        }
        let home = self.home.clone();
        let tracker = &self.boundaries;
        boundary_status(&home, tracker, thread_id, boundary_id, explicit_state)
    }

    pub(super) async fn inherit_work(
        self: &Arc<Self>,
        parent_thread_id: String,
        child_thread_id: String,
    ) -> Result<Option<UsageWorkReceipt>, String> {
        self.flush().await;
        let parent = codex_usage::read_current_work_binding(&self.home, &parent_thread_id)
            .map_err(|_| {
                "usage_work storage unavailable; inspect status before retrying".to_string()
            })?;
        let Some(parent) = parent else {
            return Ok(None);
        };
        let Some(native_project_id) = parent.update.native_project_id else {
            return Ok(None);
        };
        // A host or child-side declaration made before inheritance is authoritative for
        // the child. Inheritance must never replace that explicit selection.
        if codex_usage::read_current_work_binding(&self.home, &child_thread_id)
            .map_err(|_| {
                "usage_work storage unavailable; inspect status before retrying".to_string()
            })?
            .is_some()
        {
            return Ok(None);
        }
        let result = self
            .submit_work(
                child_thread_id,
                UsageWorkAction::Set {
                    native_project_id,
                    workstream_id: parent.update.workstream_id,
                    outcome_id: parent.update.outcome_id,
                    expected_revision: 0,
                },
            )
            .await?;
        Ok(Some(result))
    }

    fn enqueue(self: &Arc<Self>, item: PendingCapture) {
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.len() == MAX_PENDING_RECORDS {
                match item {
                    PendingCapture::Record(mut record) => {
                        self.note_loss(&record.record);
                        if let Some(receipt) = record.receipt.take() {
                            let _ = receipt.send(Persistence::Omitted);
                        }
                    }
                    PendingCapture::Work(work) => {
                        let _ = work.receipt.send(Err(
                            "usage_work capture queue is full; retry after recovery".into(),
                        ));
                    }
                }
            } else {
                pending.push_back(item);
            }
        }
        self.schedule();
    }

    fn note_loss(&self, record: &UsageRecord) {
        let mut loss = self
            .loss
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if loss.count == 0 {
            loss.thread_id = record.thread_id.clone();
            loss.parent_thread_id = record.parent_thread_id.clone();
        } else if loss.thread_id != record.thread_id
            || loss.parent_thread_id != record.parent_thread_id
        {
            loss.thread_id = None;
            loss.parent_thread_id = None;
        }
        loss.count += 1;
        if self.dropped.fetch_add(1, Ordering::AcqRel) == 0 {
            tracing::warn!("local usage capture cache is full; coverage is incomplete");
        }
    }

    fn schedule(self: &Arc<Self>) {
        // Registration and flush share this lock so a flush cannot miss an active writer.
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.writing.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.writing.store(false, Ordering::Release);
            tracing::warn!("local usage capture runtime is unavailable");
            return;
        };
        let sink = Arc::clone(self);
        let task = runtime.spawn_blocking(move || sink.write_pending());
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    fn pending_gap(&self) -> Option<UsageRecord> {
        let mut gap = self
            .gap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut loss = self
            .loss
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if loss.count > 0 && gap.is_none() {
            let loss = std::mem::take(&mut *loss);
            let id = format!("coverage:{}", uuid::Uuid::new_v4());
            let mut record = UsageRecord::new_millis(
                id,
                super::now_ms(),
                /*input_tokens*/ 0,
                /*output_tokens*/ 0,
            );
            record.thread_id = loss.thread_id;
            record.parent_thread_id = loss.parent_thread_id;
            record.event = Some("coverage_gap".to_string());
            record.classification = Some("runtime_observed".to_string());
            record.coverage = Some("incomplete".to_string());
            record.missing_observations = Some(loss.count);
            *gap = Some(record);
        }
        gap.clone()
    }

    fn write_pending(self: Arc<Self>) {
        let mut failed = false;
        loop {
            let gap = self.pending_gap();
            let mut records = Vec::new();
            let work = {
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while matches!(pending.front(), Some(PendingCapture::Record(_))) {
                    if let Some(PendingCapture::Record(record)) = pending.pop_front() {
                        records.push(record);
                    }
                }
                if records.is_empty() && gap.is_none() {
                    pending.pop_front()
                } else {
                    None
                }
            };
            if let Some(PendingCapture::Work(work)) = work {
                work.execute(&self.home);
                continue;
            }
            if records.is_empty() && gap.is_none() {
                break;
            }
            let batch = gap
                .iter()
                .cloned()
                .chain(records.iter().map(|pending| pending.record.clone()))
                .collect();
            if codex_usage::capture_records(&self.home, batch).is_err() {
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut retry = VecDeque::from(records);
                // A failed prefix cannot acknowledge or replay a later switch.
                // Preserve all ordinary observations under the existing retry bound.
                for item in pending.drain(..) {
                    match item {
                        PendingCapture::Record(record) => retry.push_back(record),
                        PendingCapture::Work(work) => {
                            let _ = work.receipt.send(Err(
                                "usage_work capture prefix is pending; retry after recovery".into(),
                            ));
                        }
                    }
                }
                let retained = MAX_PENDING_RECORDS.min(retry.len());
                for mut record in retry.drain(retained..) {
                    self.note_loss(&record.record);
                    if let Some(receipt) = record.receipt.take() {
                        let _ = receipt.send(Persistence::Omitted);
                    }
                }
                for record in &mut retry {
                    if let Some(receipt) = record.receipt.take() {
                        let _ = receipt.send(Persistence::Pending);
                    }
                }
                *pending = retry.into_iter().map(PendingCapture::Record).collect();
                if !self.deferred.swap(true, Ordering::AcqRel) {
                    tracing::warn!("local usage capture is deferred; coverage is incomplete");
                }
                failed = true;
                break;
            }
            for record in &mut records {
                if let Some(receipt) = record.receipt.take() {
                    let _ = receipt.send(Persistence::Recorded);
                }
            }
            if self.deferred.swap(false, Ordering::AcqRel) {
                tracing::info!("local usage capture recovered");
            }
            if let Some(gap) = gap {
                self.dropped.fetch_sub(
                    gap.missing_observations.unwrap_or_default(),
                    Ordering::AcqRel,
                );
                *self
                    .gap
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            }
        }
        self.writing.store(false, Ordering::Release);
        let pending = !self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        if !failed && (pending || self.dropped.load(Ordering::Acquire) > 0) {
            self.schedule();
        }
    }

    pub(super) async fn flush(self: &Arc<Self>) {
        let Ok(_flushing) = self.flushing.acquire().await else {
            tracing::warn!("local usage flush gate is unavailable");
            return;
        };
        self.schedule();
        loop {
            let tasks = std::mem::take(
                &mut *self
                    .tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if tasks.is_empty() {
                break;
            }
            for task in tasks {
                let _ = task.await;
            }
        }
    }
}

#[cfg(test)]
#[path = "sink_tests.rs"]
mod tests;

impl Drop for CaptureSink {
    fn drop(&mut self) {
        if !self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            || self.dropped.load(Ordering::Acquire) > 0
        {
            tracing::warn!("local usage capture ended with incomplete coverage");
        }
    }
}
