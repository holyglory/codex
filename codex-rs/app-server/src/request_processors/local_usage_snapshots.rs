//! Short-lived immutable usage views. The actual reader, including its bounded
//! journal tail and snapshot file pin, is retained across scope queries.
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_usage::UsageReader;

use super::local_usage_bindings::Bindings;
use super::local_usage_leases::Lease;
use super::local_usage_leases::LeaseCache;
pub(super) use super::local_usage_leases::LeaseError as ViewError;

const RETENTION: Duration = Duration::from_secs(5);
const MAX_VIEWS: usize = 4;
const MAX_RESPONSES: usize = 8;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(super) struct ReadView {
    pub(super) captured_at_ms: u64,
    pub(super) watermark: String,
    pub(super) reader: UsageReader,
    pub(super) sidecar: Option<Bindings>,
    responses: Mutex<VecDeque<(Vec<u8>, LocalUsageSummaryResponse, usize)>>,
}

static CACHE: LazyLock<Arc<LeaseCache<ReadView>>> =
    LazyLock::new(|| LeaseCache::new(MAX_VIEWS, RETENTION));

pub(super) fn open(home: &Path, expected: Option<&str>) -> Result<Lease<ReadView>, ViewError> {
    if let Some(expected) = expected {
        return CACHE.existing(home, expected);
    }
    let reservation = CACHE.reserve()?;
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
    let view = ReadView {
        captured_at_ms,
        watermark: watermark.clone(),
        reader,
        sidecar,
        responses: Mutex::new(VecDeque::new()),
    };
    reservation.insert(home.to_owned(), watermark, view)
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
