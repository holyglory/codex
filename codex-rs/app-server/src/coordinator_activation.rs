//! A client activation signal, consumed into the existing event bridge.
//! This directory contains no clocks, review state, or completion receipts.
use super::*;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_utils_absolute_path::AbsolutePathBuf;

pub(super) struct Activation {
    directory: AbsolutePathBuf,
    receiver: codex_file_watcher::Receiver,
    _subscriber: FileWatcherSubscriber,
    _registration: WatchRegistration,
    _watcher: Arc<FileWatcher>,
}
impl Activation {
    pub(super) fn new(home: &AbsolutePathBuf, watcher: Arc<FileWatcher>) -> Self {
        let directory = home.join("alarm-activations");
        let (subscriber, receiver) = watcher.add_subscriber();
        let registration = subscriber.register_paths(vec![WatchPath {
            path: directory.to_path_buf(),
            recursive: true,
        }]);
        Self {
            directory,
            receiver,
            _subscriber: subscriber,
            _registration: registration,
            _watcher: watcher,
        }
    }
    pub(super) async fn changed(&mut self) {
        let _ = self.receiver.recv().await;
    }
    pub(super) async fn drain(
        &self,
        store: &SqliteEventSubscriptionStore,
    ) -> Result<(), SourceError> {
        let mut entries = match tokio::fs::read_dir(self.directory.as_path()).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(SourceError::Unavailable),
        };
        for _ in 0..codex_event_subscriptions::MAX_TOTAL_SUBSCRIPTIONS {
            let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|_| SourceError::Unavailable)?
            else {
                break;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(thread) = codex_protocol::ThreadId::from_string(&name) else {
                continue;
            };
            if !entry
                .file_type()
                .await
                .map_err(|_| SourceError::Unavailable)?
                .is_file()
            {
                continue;
            }
            let file = match tokio::fs::File::open(entry.path()).await {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(SourceError::Unavailable),
            };
            let mut bytes = Vec::new();
            file.take(64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| SourceError::Unavailable)?;
            if bytes != b"codex.alarm-route.v1\n" {
                continue;
            }
            store
                .ensure_event_route(
                    thread,
                    codex_event_subscriptions::EventFilter {
                        source: "devcoordinator".into(),
                        event_types: std::collections::BTreeSet::from([
                            "review.reminder".into(),
                            "source.unavailable".into(),
                            "source.cursor_stale".into(),
                        ]),
                        labels: BTreeMap::from([("owner_thread_id".into(), thread.to_string())]),
                    },
                    chrono::Utc::now().timestamp_millis(),
                )
                .await
                .map_err(|_| SourceError::Unavailable)?;
            match tokio::fs::remove_file(entry.path()).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(SourceError::Unavailable),
            }
        }
        Ok(())
    }
}

pub(super) async fn changed(activation: &mut Option<Activation>) {
    if let Some(activation) = activation {
        activation.changed().await;
    } else {
        std::future::pending::<()>().await;
    }
}
