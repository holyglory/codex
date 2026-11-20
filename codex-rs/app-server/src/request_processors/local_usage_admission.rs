//! Bound independent local-usage reads and share identical in-flight requests.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio::sync::watch;

use crate::error_code::internal_error;

type ReadResult = Result<LocalUsageSummaryResponse, JSONRPCErrorError>;
type RequestKey = (PathBuf, Vec<u8>);

struct Admission {
    active: Arc<Semaphore>,
    admitted: Arc<Semaphore>,
    requests: Mutex<BTreeMap<RequestKey, watch::Receiver<Option<ReadResult>>>>,
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            active: Arc::new(Semaphore::new(2)),
            admitted: Arc::new(Semaphore::new(10)),
            requests: Mutex::new(BTreeMap::new()),
        }
    }
}

static HEADLINES: LazyLock<Arc<Admission>> = LazyLock::new(Arc::default);
static RICH: LazyLock<Arc<Admission>> = LazyLock::new(Arc::default);

pub(super) async fn summary(home: PathBuf, params: LocalUsageSummaryParams) -> ReadResult {
    let headline =
        !params.include_review && params.record_limit == 0 && params.operation_limit == 0;
    let admission = Arc::clone(if headline { &HEADLINES } else { &RICH });
    let key = (
        home.clone(),
        serde_json::to_vec(&params).map_err(|_| unavailable("request_unavailable"))?,
    );
    admission
        .read(key, move || {
            if headline {
                super::local_usage_headline::read(home, params)
            } else {
                super::local_usage_processor::read(home, params)
            }
        })
        .await
}

impl Admission {
    async fn read(
        self: Arc<Self>,
        key: RequestKey,
        read: impl FnOnce() -> ReadResult + Send + 'static,
    ) -> ReadResult {
        let mut receiver = {
            let mut requests = self
                .requests
                .lock()
                .map_err(|_| unavailable("reader_unavailable"))?;
            if let Some(receiver) = requests.get(&key) {
                receiver.clone()
            } else {
                let admitted = Arc::clone(&self.admitted)
                    .try_acquire_owned()
                    .map_err(|_| unavailable("reader_busy"))?;
                let (sender, receiver) = watch::channel(None);
                requests.insert(key.clone(), receiver.clone());
                let admission = Arc::clone(&self);
                tokio::spawn(async move {
                    let _admitted = admitted;
                    // The timeout bounds queue waiting, never an admitted source read.
                    let result = match tokio::time::timeout(
                        Duration::from_secs(5),
                        Arc::clone(&admission.active).acquire_owned(),
                    )
                    .await
                    {
                        Ok(Ok(active)) => tokio::task::spawn_blocking(move || {
                            let _active = active;
                            read()
                        })
                        .await
                        .unwrap_or_else(|_| Err(unavailable("reader_unavailable"))),
                        Ok(Err(_)) | Err(_) => Err(unavailable("reader_busy")),
                    };
                    sender.send_replace(Some(result));
                    if let Ok(mut requests) = admission.requests.lock() {
                        requests.remove(&key);
                    }
                });
                receiver
            }
        };
        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }
            receiver
                .changed()
                .await
                .map_err(|_| unavailable("reader_unavailable"))?;
        }
    }
}

fn unavailable(reason: &str) -> JSONRPCErrorError {
    let mut error = internal_error("local usage reader unavailable");
    error.data = Some(
        json!({"source":"usage-v2","coverage":"unavailable","reason":reason,"retryable":true}),
    );
    error
}

#[cfg(test)]
#[path = "local_usage_admission_tests.rs"]
mod tests;
