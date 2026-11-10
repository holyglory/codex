//! Short-lived immutable usage views. The actual reader, including its bounded
//! journal tail and snapshot file pin, is retained across scope queries.
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_usage::UsageReader;

use super::local_usage_bindings::Bindings;

const RETENTION: Duration = Duration::from_secs(5);
const MAX_VIEWS: usize = 4;
const MAX_RESPONSES: usize = 8;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct ReadView {
    home: PathBuf,
    created: Instant,
    pub(super) captured_at_ms: u64,
    pub(super) watermark: String,
    pub(super) reader: UsageReader,
    pub(super) sidecar: Option<Bindings>,
    responses: Mutex<VecDeque<(Vec<u8>, LocalUsageSummaryResponse, usize)>>,
}

#[derive(Default)]
struct Cache {
    views: VecDeque<Arc<ReadView>>,
    cleanup_running: bool,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

#[derive(Debug)]
pub(super) enum ViewError {
    Expired,
    Unavailable,
}

pub(super) fn open(home: &Path, expected: Option<&str>) -> Result<Arc<ReadView>, ViewError> {
    let cache = CACHE.get_or_init(|| Mutex::new(Cache::default()));
    if let Some(expected) = expected {
        let mut cache = cache.lock().map_err(|_| ViewError::Unavailable)?;
        cache
            .views
            .retain(|view| view.created.elapsed() < RETENTION);
        return cache
            .views
            .iter()
            .find(|view| view.home == home && view.watermark == expected)
            .cloned()
            .ok_or(ViewError::Expired);
    }
    let sidecar = Bindings::read(home).map_err(|_| ViewError::Unavailable)?;
    let reader = UsageReader::open(home).map_err(|_| ViewError::Unavailable)?;
    let captured_at_ms =
        super::local_usage_processor::now_ms().map_err(|_| ViewError::Unavailable)?;
    static SERIAL: AtomicU64 = AtomicU64::new(1);
    static SESSION: OnceLock<u64> = OnceLock::new();
    let session = SESSION.get_or_init(|| captured_at_ms);
    let manifest = reader.manifest();
    let watermark = format!(
        "usage-v2:s{}-{session}:g{}:w{}:j{}:{}",
        SERIAL.fetch_add(1, Ordering::Relaxed),
        manifest.snapshot_generation,
        manifest.watermark,
        reader.journal_watermark(),
        sidecar
            .as_ref()
            .map_or("absent", |source| source.revision.as_str())
    );
    let view = Arc::new(ReadView {
        home: home.to_owned(),
        created: Instant::now(),
        captured_at_ms,
        watermark,
        reader,
        sidecar,
        responses: Mutex::new(VecDeque::new()),
    });
    let handle = tokio::runtime::Handle::try_current().map_err(|_| ViewError::Unavailable)?;
    let start_cleanup = {
        let mut cache = cache.lock().map_err(|_| ViewError::Unavailable)?;
        cache
            .views
            .retain(|view| view.created.elapsed() < RETENTION);
        while cache.views.len() >= MAX_VIEWS {
            cache.views.pop_front();
        }
        cache.views.push_back(Arc::clone(&view));
        if cache.cleanup_running {
            false
        } else {
            cache.cleanup_running = true;
            true
        }
    };
    if start_cleanup {
        // One cache-owned timer releases idle pins at expiry. It performs no
        // source I/O, capture, review, or capacity scheduling.
        handle.spawn(async {
            loop {
                let next = {
                    let Ok(mut cache) = CACHE.get_or_init(|| Mutex::new(Cache::default())).lock()
                    else {
                        return;
                    };
                    cache
                        .views
                        .retain(|view| view.created.elapsed() < RETENTION);
                    if let Some(view) = cache.views.front() {
                        view.created + RETENTION
                    } else {
                        cache.cleanup_running = false;
                        return;
                    }
                };
                tokio::time::sleep_until(tokio::time::Instant::from_std(next)).await;
            }
        });
    }
    Ok(view)
}

fn response_key(params: &LocalUsageSummaryParams) -> Result<Vec<u8>, ()> {
    let mut params = params.clone();
    params.expected_snapshot = None;
    serde_json::to_vec(&params).map_err(|_| ())
}

impl ReadView {
    pub(super) fn response(
        &self,
        params: &LocalUsageSummaryParams,
    ) -> Result<Option<LocalUsageSummaryResponse>, ()> {
        let key = response_key(params)?;
        let mut entries = self.responses.lock().map_err(|_| ())?;
        let Some(index) = entries
            .iter()
            .position(|(candidate, _, _)| candidate == &key)
        else {
            return Ok(None);
        };
        let entry = entries.remove(index).ok_or(())?;
        let response = entry.1.clone();
        entries.push_back(entry);
        Ok(Some(response))
    }

    pub(super) fn remember(
        &self,
        params: &LocalUsageSummaryParams,
        response: &LocalUsageSummaryResponse,
        encoded_bytes: usize,
    ) -> Result<(), ()> {
        let key = response_key(params)?;
        let size = key.len().checked_add(encoded_bytes).ok_or(())?;
        if size > MAX_RESPONSE_BYTES {
            return Ok(());
        }
        let mut entries = self.responses.lock().map_err(|_| ())?;
        while !entries.is_empty()
            && (entries.len() >= MAX_RESPONSES
                || entries
                    .iter()
                    .map(|entry| entry.2)
                    .sum::<usize>()
                    .saturating_add(size)
                    > MAX_RESPONSE_BYTES)
        {
            entries.pop_front();
        }
        entries.push_back((key, response.clone(), size));
        Ok(())
    }
}
