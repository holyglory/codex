use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::mpsc;
use std::time::Duration;

use crate::JournalReadLimits;
use crate::Result;
use crate::StorageError;
use crate::USAGE_DIRECTORY_NAME;
use crate::UsagePublisher;

const PUBLICATION_LIMITS: JournalReadLimits = JournalReadLimits {
    max_bytes: 16 * 1024 * 1024,
    max_entries: 16_384,
};

struct PublisherRegistration {
    owner: Weak<PublisherOwner>,
    signal: Weak<PublisherSignal>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaintenanceKind {
    PublicationRecovery,
    Retention,
}

struct MaintenanceRequest {
    kind: MaintenanceKind,
    completion: mpsc::SyncSender<()>,
}

type Publishers = BTreeMap<PathBuf, PublisherRegistration>;
static PUBLISHERS: OnceLock<Mutex<Publishers>> = OnceLock::new();

#[derive(Debug)]
struct PublisherSignal {
    wake: Mutex<Option<mpsc::SyncSender<()>>>,
}

impl PublisherSignal {
    fn notify(&self) {
        if let Ok(wake) = self.wake.try_lock()
            && let Some(wake) = wake.as_ref()
        {
            let _ = wake.try_send(());
        }
    }
}

#[derive(Debug)]
struct PublisherOwner {
    signal: Arc<PublisherSignal>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Drop for PublisherOwner {
    fn drop(&mut self) {
        drop(
            self.signal
                .wake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        // Never leave DuckDB work running while the host process tears down
        // its native libraries. Notification handles do not own these joins,
        // so capture remains independent of publication and report reads.
        for worker in self
            .workers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            if worker.join().is_err() {
                tracing::warn!("usage worker ended unexpectedly during shutdown");
            }
        }
    }
}

/// Lifetime handle for the process's independent publisher for one Codex home.
/// Clones share one worker. Dropping the last handle closes its wake channel;
/// the workers finish their bounded work and join before shutdown returns.
#[derive(Clone, Debug)]
pub struct UsagePublisherRuntime {
    _owner: Arc<PublisherOwner>,
}

impl UsagePublisherRuntime {
    /// Start without waiting for recovery, publication or any report query.
    /// Multiple thread managers using the same local store share one worker.
    pub fn start(codex_home: impl Into<PathBuf>) -> Result<Self> {
        Self::start_with_retention(codex_home, crate::RetentionPolicy::default())
    }

    /// Configure detail retention for this process's shared background worker.
    pub fn start_with_retention(
        codex_home: impl Into<PathBuf>,
        policy: crate::RetentionPolicy,
    ) -> Result<Self> {
        policy.validate()?;
        let root = codex_home.into().join(USAGE_DIRECTORY_NAME);
        let mut publishers = PUBLISHERS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| StorageError::InvalidData("publisher registry lock poisoned".into()))?;
        publishers.retain(|_, publisher| publisher.owner.strong_count() != 0);
        if let Some(owner) = publishers
            .get(&root)
            .and_then(|entry| entry.owner.upgrade())
        {
            return Ok(Self { _owner: owner });
        }
        let (wake, receiver) = mpsc::sync_channel(1);
        let signal = Arc::new(PublisherSignal {
            wake: Mutex::new(Some(wake)),
        });
        let owner = Arc::new(PublisherOwner {
            signal: Arc::clone(&signal),
            workers: Mutex::default(),
        });
        let (warm, ready) = mpsc::channel();
        let warm_root = root.clone();
        let warm_signal = Arc::downgrade(&owner);
        let warm_worker = std::thread::Builder::new()
            .name("codex-usage-startup-warmup".into())
            .spawn(move || {
                if ready.recv().is_ok() {
                    prewarm_recent_chats(warm_root, warm_signal);
                }
            })?;
        let worker_root = root.clone();
        let (maintenance, maintain) = mpsc::sync_channel::<MaintenanceRequest>(/*bound*/ 1);
        let maintenance_root = root.clone();
        let maintenance_worker = std::thread::Builder::new()
            .name("codex-usage-retention".into())
            .spawn(move || {
                let mut retention_maintenance = crate::retention::Maintenance::default();
                while let Ok(request) = maintain.recv() {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(i64::MAX as u128) as i64;
                    let result = match request.kind {
                        MaintenanceKind::PublicationRecovery => retention_maintenance
                            .resume_publication(&maintenance_root)
                            .map(|_| ()),
                        MaintenanceKind::Retention => crate::retention::maintain(
                            &maintenance_root,
                            policy,
                            now_ms,
                            &mut retention_maintenance,
                        ),
                    };
                    if result.is_err() {
                        tracing::debug!("usage maintenance deferred after failure");
                    }
                    let _ = request.completion.send(());
                }
            })?;
        let publisher_worker = std::thread::Builder::new()
            .name("codex-usage-publisher".into())
            .spawn(move || publish_loop(worker_root, receiver, warm, maintenance))?;
        *owner
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            // The publisher owns both wake senders. Join it first so the
            // maintenance and warmup receivers observe channel closure during
            // shutdown instead of waiting on a sender held by the publisher.
            vec![publisher_worker, maintenance_worker, warm_worker];
        signal.notify();
        publishers.insert(
            root,
            PublisherRegistration {
                owner: Arc::downgrade(&owner),
                signal: Arc::downgrade(&signal),
            },
        );
        Ok(Self { _owner: owner })
    }
}

/// A wake is advisory; durable append success never depends on a publisher.
pub(crate) fn notify_publisher(codex_home: &Path) {
    let Some(publishers) = PUBLISHERS.get() else {
        return;
    };
    let Ok(publishers) = publishers.try_lock() else {
        return;
    };
    if let Some(signal) = publishers
        .get(&codex_home.join(USAGE_DIRECTORY_NAME))
        .and_then(|entry| entry.signal.upgrade())
    {
        signal.notify();
    }
}

fn publish_loop(
    root: PathBuf,
    receiver: mpsc::Receiver<()>,
    warm: mpsc::Sender<()>,
    maintenance: mpsc::SyncSender<MaintenanceRequest>,
) {
    let mut had_failure = false;
    let mut catch_up = false;
    let mut warm = Some(warm);
    let mut handoff: Option<(MaintenanceKind, mpsc::Receiver<()>)> = None;
    let mut shutdown_requested = false;
    let mut pending_wake = false;
    // Keep ordinary startup/prewarming on its established periodic cadence;
    // publication errors request maintenance immediately below.
    let mut next_maintenance = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some((kind, completion)) = handoff.as_ref() {
            let kind = *kind;
            match completion.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                    handoff = None;
                    catch_up = true;
                    if matches!(kind, MaintenanceKind::Retention) {
                        next_maintenance = std::time::Instant::now() + Duration::from_secs(60);
                    }
                    if shutdown_requested && pending_wake {
                        let _ = UsagePublisher::open_at(&root)
                            .and_then(|mut publisher| publisher.publish_batch(PUBLICATION_LIMITS));
                        return;
                    }
                    continue;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    match receiver.try_recv() {
                        Ok(()) => {
                            pending_wake = true;
                            std::thread::sleep(Duration::from_millis(/*millis*/ 10));
                            continue;
                        }
                        Err(mpsc::TryRecvError::Empty) => {
                            std::thread::sleep(Duration::from_millis(/*millis*/ 10));
                            continue;
                        }
                        Err(mpsc::TryRecvError::Disconnected) => {
                            shutdown_requested = true;
                            pending_wake = true;
                            continue;
                        }
                    }
                }
            }
        }
        // Polling while catching up observes channel closure between batches;
        // continuous capture cannot keep a dropped runtime alive indefinitely.
        let wait = if catch_up {
            Duration::ZERO
        } else {
            Duration::from_secs(1)
        };
        match receiver.recv_timeout(wait) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let maintenance_due = std::time::Instant::now() >= next_maintenance;
        let result = UsagePublisher::open_at(&root)
            .and_then(|mut publisher| publisher.publish_batch(PUBLICATION_LIMITS));
        let mut maintenance_requested = maintenance_due.then_some(MaintenanceKind::Retention);
        match result {
            Ok(manifest) => {
                catch_up = crate::UsageJournal::new(root.join("events.jsonl"))
                    .checkpoint()
                    .is_ok_and(|end| end.sequence > manifest.watermark);
                // The worker warms immutable metadata after publication. Reads
                // can always fall back independently if warming fails.
                let _ = crate::ManifestStore::new(root.join("manifest.json")).load_shared();
                let _ = crate::review_projection::load_review_projection_shared(&root, &manifest);
                if !catch_up && let Some(warm) = warm.take() {
                    let _ = warm.send(());
                }
                had_failure = false;
            }
            Err(error) => {
                catch_up = false;
                // Only a full operation index can use the short recovery
                // handoff. Other failures retain full maintenance, including
                // retention and reclamation, before publication retries.
                maintenance_requested = Some(match error {
                    StorageError::OperationIndexAtCapacity => MaintenanceKind::PublicationRecovery,
                    StorageError::Io(_)
                    | StorageError::Json(_)
                    | StorageError::InvalidData(_)
                    | StorageError::UnsupportedFormat(_)
                    | StorageError::Columnar(_) => MaintenanceKind::Retention,
                });
                if !had_failure {
                    tracing::warn!(
                        "usage snapshot publication failed; durable journal remains available"
                    );
                    had_failure = true;
                }
            }
        }
        if let Some(kind) = maintenance_requested {
            // Queue only after publication has released its lock, then wait
            // for one recovery step or a scheduled retention pass. Recovery
            // releases publication as soon as one merge frees run capacity.
            // Capture and report readers remain independent of this handoff.
            let (completion, completed) = mpsc::sync_channel(/*bound*/ 0);
            let mut request = MaintenanceRequest { kind, completion };
            loop {
                match maintenance.try_send(request) {
                    Ok(()) => {
                        handoff = Some((kind, completed));
                        break;
                    }
                    Err(mpsc::TrySendError::Full(requested)) => {
                        request = requested;
                        match receiver.try_recv() {
                            Ok(()) => pending_wake = true,
                            Err(mpsc::TryRecvError::Empty) => {}
                            Err(mpsc::TryRecvError::Disconnected) => {
                                shutdown_requested = true;
                                pending_wake = true;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(/*millis*/ 10));
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        catch_up = true;
                        if matches!(kind, MaintenanceKind::Retention) {
                            next_maintenance = std::time::Instant::now() + Duration::from_secs(60);
                        }
                        break;
                    }
                }
            }
        }
    }
}

fn prewarm_recent_chats(root: PathBuf, signal: Weak<PublisherOwner>) {
    let Ok(reader) = crate::UsageReader::open_at(root) else {
        tracing::debug!("usage startup warmup could not pin a snapshot");
        return;
    };
    // Retain only ten candidates while walking metadata; no history query,
    // thread enumeration from the app, or user-visible request is required.
    let mut recent = std::collections::BTreeSet::new();
    for (thread, at) in &reader.manifest().thread_last_activity_at_ms {
        recent.insert((*at, thread.as_str()));
        if recent.len() > 10 {
            recent.pop_first();
        }
    }
    for (_, thread) in recent.into_iter().rev() {
        if signal.strong_count() == 0 {
            break;
        }
        let filter = crate::UsageFilter::default().for_thread(thread);
        if reader.summary(crate::TimeRange::all(), &filter).is_err() {
            tracing::debug!("usage startup warmup skipped an unavailable summary");
        }
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
