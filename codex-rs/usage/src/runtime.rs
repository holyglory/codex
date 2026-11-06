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

type Publishers = BTreeMap<PathBuf, Weak<PublisherSignal>>;
static PUBLISHERS: OnceLock<Mutex<Publishers>> = OnceLock::new();

#[derive(Debug)]
struct PublisherSignal {
    wake: mpsc::SyncSender<()>,
}

/// Lifetime handle for the process's independent publisher for one Codex home.
/// Clones share one worker. Dropping the last handle closes its wake channel;
/// the worker finishes its current bounded publication and exits.
#[derive(Clone, Debug)]
pub struct UsagePublisherRuntime {
    _signal: Arc<PublisherSignal>,
}

impl UsagePublisherRuntime {
    /// Start without waiting for recovery, publication or any report query.
    /// Multiple thread managers using the same local store share one worker.
    pub fn start(codex_home: impl Into<PathBuf>) -> Result<Self> {
        let root = codex_home.into().join(USAGE_DIRECTORY_NAME);
        let mut publishers = PUBLISHERS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| StorageError::InvalidData("publisher registry lock poisoned".into()))?;
        publishers.retain(|_, publisher| publisher.strong_count() != 0);
        if let Some(signal) = publishers.get(&root).and_then(Weak::upgrade) {
            return Ok(Self { _signal: signal });
        }
        let (wake, receiver) = mpsc::sync_channel(1);
        let signal = Arc::new(PublisherSignal { wake });
        let worker_root = root.clone();
        std::thread::Builder::new()
            .name("codex-usage-publisher".into())
            .spawn(move || publish_loop(worker_root, receiver))?;
        let _ = signal.wake.try_send(());
        publishers.insert(root, Arc::downgrade(&signal));
        Ok(Self { _signal: signal })
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
        .and_then(Weak::upgrade)
    {
        let _ = signal.wake.try_send(());
    }
}

fn publish_loop(root: PathBuf, receiver: mpsc::Receiver<()>) {
    let mut had_failure = false;
    let mut catch_up = false;
    loop {
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
        let result = UsagePublisher::open_at(&root)
            .and_then(|mut publisher| publisher.publish_batch(PUBLICATION_LIMITS));
        match result {
            Ok(manifest) => {
                catch_up = crate::UsageJournal::new(root.join("events.jsonl"))
                    .checkpoint()
                    .is_ok_and(|end| end.sequence > manifest.watermark);
                had_failure = false;
            }
            Err(_) => {
                catch_up = false;
                if !had_failure {
                    tracing::warn!(
                        "usage snapshot publication failed; durable journal remains available"
                    );
                    had_failure = true;
                }
            }
        }
    }
}
