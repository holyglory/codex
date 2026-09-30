//! Drops one real server commit acknowledgment without dropping its durable state.
use super::*;
use futures::SinkExt;
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio_tungstenite::tungstenite::Message;

pub(super) struct LostCommitProxy {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<std::thread::JoinHandle<Result<()>>>,
    dropped: Arc<AtomicBool>,
}

impl LostCommitProxy {
    pub(super) fn start(daemon: &TestDaemon) -> Result<Self> {
        let status = daemon.lifecycle("version")?;
        let socket = PathBuf::from(status["socketPath"].as_str().context("daemon socket")?);
        let upstream = std::fs::read_link(&socket)?;
        std::fs::remove_file(&socket)?;
        let listener = std::os::unix::net::UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&dropped);
        let (stop, mut stopping) = tokio::sync::oneshot::channel();
        let task = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()?.block_on(async move {
                let listener = tokio::net::UnixListener::from_std(listener)?;
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        _ = &mut stopping => return Ok(()),
                        completed = connections.join_next(), if !connections.is_empty() => {
                            if let Some(result) = completed { result??; }
                        }
                        accepted = listener.accept() => {
                            let (stream, _) = accepted?;
                            let upstream = upstream.clone();
                            let socket = socket.clone();
                            let observed = Arc::clone(&observed);
                            connections.spawn(async move {
                                let mut uri = String::new();
                                let mut downstream = tokio_tungstenite::accept_hdr_async(stream, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                                    uri = format!("ws://localhost{}", request.uri());
                                    Ok(response)
                                }).await?;
                                let (mut upstream, _) = tokio_tungstenite::client_async(&uri, tokio::net::UnixStream::connect(upstream).await?).await?;
                                loop {
                                    tokio::select! {
                                        message = downstream.next() => {
                                            let Some(Ok(message)) = message else { return Ok::<_, anyhow::Error>(()); };
                                            if upstream.send(message).await.is_err() { return Ok(()); }
                                        }
                                        message = upstream.next() => {
                                            let Some(Ok(message)) = message else { return Ok(()); };
                                            if let Message::Text(text) = &message
                                                && serde_json::from_str::<Value>(text).is_ok_and(|value| value["type"] == "committed") {
                                                std::fs::remove_file(&socket)?;
                                                observed.store(true, Ordering::Release);
                                                // The candidate now owns publication of a new socket.
                                                // Drop this frame and connection after the real commit.
                                                return Ok(());
                                            }
                                            if downstream.send(message).await.is_err() { return Ok(()); }
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
            })
        });
        Ok(Self {
            stop: Some(stop),
            task: Some(task),
            dropped,
        })
    }

    pub(super) fn dropped_receipt(&self) -> bool {
        self.dropped.load(Ordering::Acquire)
    }
}

impl Drop for LostCommitProxy {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
