use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionCellExecutionLimits;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeSessionProviderFuture;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::StartedCell;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(super) fn wait_timer_armed_provider(
    host_program: PathBuf,
) -> (Arc<dyn CodeModeSessionProvider>, oneshot::Receiver<()>) {
    let (timer_armed_tx, timer_armed_rx) = oneshot::channel();
    let provider = WaitTimerArmedProvider {
        inner: Arc::new(ProcessOwnedCodeModeSessionProvider::with_host_program(
            host_program,
        )),
        timer_armed: Arc::new(Mutex::new(Some(timer_armed_tx))),
    };
    (Arc::new(provider), timer_armed_rx)
}

struct WaitTimerArmedProvider {
    inner: Arc<dyn CodeModeSessionProvider>,
    timer_armed: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl CodeModeSessionProvider for WaitTimerArmedProvider {
    fn availability(&self) -> Result<(), String> {
        self.inner.availability()
    }

    fn create_session(&self) -> CodeModeSessionProviderFuture<'_> {
        let timer_armed = Arc::clone(&self.timer_armed);
        Box::pin(async move {
            let inner = self.inner.create_session().await?;
            Ok(Arc::new(WaitTimerArmedSession { inner, timer_armed }) as Arc<dyn CodeModeSession>)
        })
    }

    fn create_session_with_limits<'a>(
        &'a self,
        limits: CodeModeSessionCellExecutionLimits,
    ) -> CodeModeSessionProviderFuture<'a> {
        let timer_armed = Arc::clone(&self.timer_armed);
        Box::pin(async move {
            let inner = self.inner.create_session_with_limits(limits).await?;
            Ok(Arc::new(WaitTimerArmedSession { inner, timer_armed }) as Arc<dyn CodeModeSession>)
        })
    }
}

struct WaitTimerArmedSession {
    inner: Arc<dyn CodeModeSession>,
    timer_armed: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl CodeModeSession for WaitTimerArmedSession {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
        delegate: Arc<dyn CodeModeSessionDelegate>,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        self.inner.execute(request, delegate, preempt)
    }

    fn wait<'a>(
        &'a self,
        request: WaitRequest,
        preempt: Option<CancellationToken>,
    ) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        // The current host may reject a stale cell before polling the transport future. Signal
        // entry before constructing that future so the fixture proves dispatch reached the wait
        // path without depending on a particular host-side polling implementation.
        if let Some(timer_armed) = self
            .timer_armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = timer_armed.send(());
        }
        let wait = self.inner.wait(request, preempt);
        Box::pin(wait)
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.inner.terminate(cell_id)
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        self.inner.shutdown()
    }
}
