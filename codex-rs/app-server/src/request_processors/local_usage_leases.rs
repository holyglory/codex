//! Bound snapshot pins without expiring active reads or evicting retained views.
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_app_server_protocol::JSONRPCErrorError;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::error_code::internal_error;
use crate::error_code::invalid_params;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum LeaseError {
    Expired,
    Busy,
    Unavailable,
}

pub(super) struct LeaseCache<T> {
    state: Mutex<State<T>>,
    changed: Notify,
    max_views: usize,
    retention: Duration,
}

struct Entry<T> {
    home: PathBuf,
    watermark: String,
    value: T,
}

struct Slot<T> {
    entry: Arc<Entry<T>>,
    active: usize,
    retained_until: Option<Instant>,
}

struct State<T> {
    slots: Vec<Slot<T>>,
    reservations: usize,
    cleanup_running: bool,
}

impl<T> State<T> {
    fn prune(&mut self) {
        let now = Instant::now();
        self.slots.retain(|slot| {
            slot.active != 0 || slot.retained_until.is_some_and(|until| until > now)
        });
    }
}

pub(super) struct Reservation<T: Send + Sync + 'static> {
    cache: Arc<LeaseCache<T>>,
    active: bool,
}

pub(super) struct Lease<T: Send + Sync + 'static> {
    cache: Arc<LeaseCache<T>>,
    entry: Arc<Entry<T>>,
}

impl<T: Send + Sync + 'static> LeaseCache<T> {
    pub(super) fn new(max_views: usize, retention: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                slots: Vec::new(),
                reservations: 0,
                cleanup_running: false,
            }),
            changed: Notify::new(),
            max_views,
            retention,
        })
    }

    pub(super) fn existing(
        self: &Arc<Self>,
        home: &Path,
        watermark: &str,
    ) -> Result<Lease<T>, LeaseError> {
        let mut state = self.state.lock().map_err(|_| LeaseError::Unavailable)?;
        state.prune();
        let slot = state
            .slots
            .iter_mut()
            .find(|slot| slot.entry.home == home && slot.entry.watermark == watermark)
            .ok_or(LeaseError::Expired)?;
        slot.active = slot.active.checked_add(1).ok_or(LeaseError::Unavailable)?;
        Ok(Lease {
            cache: Arc::clone(self),
            entry: Arc::clone(&slot.entry),
        })
    }

    /// Reserve before source I/O, so active creations also consume a cache slot.
    pub(super) fn reserve(self: &Arc<Self>) -> Result<Reservation<T>, LeaseError> {
        let mut state = self.state.lock().map_err(|_| LeaseError::Unavailable)?;
        state.prune();
        if state.slots.len() + state.reservations >= self.max_views {
            return Err(LeaseError::Busy);
        }
        state.reservations += 1;
        Ok(Reservation {
            cache: Arc::clone(self),
            active: true,
        })
    }

    async fn cleanup(self: Arc<Self>) {
        loop {
            let next = {
                let Ok(mut state) = self.state.lock() else {
                    return;
                };
                state.prune();
                if state.slots.is_empty() {
                    state.cleanup_running = false;
                    return;
                }
                state
                    .slots
                    .iter()
                    .filter(|slot| slot.active == 0)
                    .filter_map(|slot| slot.retained_until)
                    .min()
            };
            // This single cache-owned timer performs no source I/O or scheduling.
            // Active-only caches sleep until a lease completes or drops.
            match next {
                Some(until) => tokio::select! {
                    _ = tokio::time::sleep_until(until) => {},
                    _ = self.changed.notified() => {},
                },
                None => self.changed.notified().await,
            }
        }
    }
}

impl<T: Send + Sync + 'static> Reservation<T> {
    pub(super) fn insert(
        mut self,
        home: PathBuf,
        watermark: String,
        value: T,
    ) -> Result<Lease<T>, LeaseError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| LeaseError::Unavailable)?;
        let cache = Arc::clone(&self.cache);
        let entry = Arc::new(Entry {
            home,
            watermark,
            value,
        });
        let start_cleanup = {
            let mut state = cache.state.lock().map_err(|_| LeaseError::Unavailable)?;
            state.reservations -= 1;
            self.active = false;
            state.slots.push(Slot {
                entry: Arc::clone(&entry),
                active: 1,
                retained_until: None,
            });
            if state.cleanup_running {
                false
            } else {
                state.cleanup_running = true;
                true
            }
        };
        if start_cleanup {
            handle.spawn(Arc::clone(&cache).cleanup());
        }
        Ok(Lease { cache, entry })
    }
}

impl<T: Send + Sync + 'static> Drop for Reservation<T> {
    fn drop(&mut self) {
        if self.active
            && let Ok(mut state) = self.cache.state.lock()
        {
            state.reservations -= 1;
        }
        self.cache.changed.notify_one();
    }
}

impl<T: Send + Sync + 'static> Lease<T> {
    pub(super) fn retain_after_response(&self) -> Result<(), LeaseError> {
        let mut state = self
            .cache
            .state
            .lock()
            .map_err(|_| LeaseError::Unavailable)?;
        let slot = state
            .slots
            .iter_mut()
            .find(|slot| Arc::ptr_eq(&slot.entry, &self.entry))
            .ok_or(LeaseError::Unavailable)?;
        slot.retained_until = Some(Instant::now() + self.cache.retention);
        self.cache.changed.notify_one();
        Ok(())
    }
}

impl<T: Send + Sync + 'static> Deref for Lease<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.entry.value
    }
}

impl<T: Send + Sync + 'static> Drop for Lease<T> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.cache.state.lock()
            && let Some(slot) = state
                .slots
                .iter_mut()
                .find(|slot| Arc::ptr_eq(&slot.entry, &self.entry))
        {
            slot.active -= 1;
        }
        self.cache.changed.notify_one();
    }
}

pub(super) fn rpc_error(error: LeaseError) -> JSONRPCErrorError {
    let (message, reason) = match error {
        LeaseError::Expired => {
            return invalid_params("local usage snapshot changed; restart pagination");
        }
        LeaseError::Busy => (
            "local usage snapshot capacity is busy",
            "snapshot_capacity_busy",
        ),
        LeaseError::Unavailable => ("local usage reader unavailable", "reader_unavailable"),
    };
    let mut error = internal_error(message);
    error.data = Some(
        json!({"source":"usage-v2","coverage":"unavailable","reason":reason,"retryable":true}),
    );
    error
}

#[cfg(test)]
#[path = "local_usage_leases_tests.rs"]
mod tests;
