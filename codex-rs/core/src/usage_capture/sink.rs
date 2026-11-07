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

pub(super) struct CaptureSink {
    home: PathBuf,
    pending: Mutex<VecDeque<PendingRecord>>,
    gap: Mutex<Option<UsageRecord>>,
    loss: Mutex<CoverageLoss>,
    dropped: AtomicU64,
    writing: AtomicBool,
    deferred: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    flushing: tokio::sync::Semaphore,
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
        })
    }

    pub(super) fn submit(self: &Arc<Self>, record: UsageRecord) {
        self.enqueue(PendingRecord {
            record,
            receipt: None,
        });
    }

    pub(super) async fn submit_confirmed(self: &Arc<Self>, record: UsageRecord) -> Persistence {
        let (receipt, mut received) = tokio::sync::oneshot::channel();
        self.enqueue(PendingRecord {
            record,
            receipt: Some(receipt),
        });
        self.flush().await;
        received.try_recv().unwrap_or(Persistence::Pending)
    }

    fn enqueue(self: &Arc<Self>, mut pending_record: PendingRecord) {
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.len() == MAX_PENDING_RECORDS {
                self.note_loss(&pending_record.record);
                if let Some(receipt) = pending_record.receipt.take() {
                    let _ = receipt.send(Persistence::Omitted);
                }
            } else {
                pending.push_back(pending_record);
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
            let mut records = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain(..)
                .collect::<Vec<_>>();
            let gap = self.pending_gap();
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
                retry.append(&mut pending);
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
                *pending = retry;
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
