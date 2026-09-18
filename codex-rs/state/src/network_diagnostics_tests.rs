use super::*;
use crate::StateRuntime;
use crate::log_db;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::test]
async fn network_queue_overflow_is_reported_in_retained_evidence() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::new_for_testing(AbsolutePathBuf::try_from(dir.path())?);
    let sink = NetworkSink::start(sqlite.clone());
    // This current-thread test does not yield until the independent queue is full.
    for index in 0..4098 {
        sink.record(NetworkDiagnostic {
            id: 0,
            timestamp_ms: index,
            thread_id: Some("overflow-fixture".to_string()),
            turn_id: None,
            event: "failure-fixture".to_string(),
            details: BTreeMap::new(),
        });
    }
    sink.flush().await;
    let first = query(
        &sqlite,
        NetworkQuery {
            before_id: Some(2),
            limit: 1,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        first.data[0]
            .details
            .get("dropped_records_before_this_event"),
        Some(&json!(2))
    );
    let latest = query(
        &sqlite,
        NetworkQuery {
            limit: 1,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(latest.data[0].id, 4096);
    Ok(())
}

#[tokio::test]
async fn incident_history_survives_log_pruning_and_reopening_without_payloads() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::new_for_testing(AbsolutePathBuf::try_from(dir.path())?);
    let runtime = StateRuntime::init(sqlite.clone(), "test".to_string()).await?;
    let layer = log_db::start(runtime.clone());
    let guard = tracing_subscriber::registry()
        .with(layer.clone().with_filter(log_db::default_filter()))
        .set_default();
    tracing::info_span!("turn", thread_id = "task-network", turn_id = "turn-network", model = "test-model").in_scope(|| {
        tracing::warn!(target: "codex.network_diagnostics", event = "websocket_close", close_code = 1008u64,
            close_reason = "policy violation token=short", origin = "https://api.example.test", authorization = "private-header", payload = "private-prompt");
        tracing::event!(target: "codex_otel.trace_safe", tracing::Level::INFO, event.name = "codex.api_request", http.response.status_code = 503u64,
            error.message = "http 503", auth.request_id = "req-failure", attempt = 2u64, user.email = "private-email");
        tracing::warn!(target: "codex_core::responses_retry", retries = 2u64, max_retries = 5u64, sampling_error = "private-body", "retrying");
        tracing::warn!(target: "codex_core::client", "falling back to HTTP");
        tracing::event!(target: "codex_otel.trace_safe", tracing::Level::INFO, event.name = "codex.sse_event", event.kind = "response.completed");
        tracing::event!(target: "codex_otel.trace_safe", tracing::Level::INFO, event.name = "codex.user_prompt", message = "private-prompt");
        for _ in 0..1200 { tracing::info!("routine activity"); }
    });
    layer.flush().await;
    drop(guard);
    drop(layer);
    let retained_logs = runtime.query_feedback_logs("task-network").await?;
    assert!(!String::from_utf8(retained_logs)?.contains("retrying"));
    runtime.close().await;
    drop(runtime);

    // A new reader after the runtime closes does not depend on task memory or debug retention.
    let page = query(
        &sqlite,
        NetworkQuery {
            thread_id: Some("task-network".to_string()),
            limit: 100,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        page.data
            .iter()
            .map(|row| (
                row.event.as_str(),
                row.thread_id.as_deref(),
                row.turn_id.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                "codex.sse_event",
                Some("task-network"),
                Some("turn-network")
            ),
            (
                "fallback_to_http",
                Some("task-network"),
                Some("turn-network")
            ),
            ("retry", Some("task-network"), Some("turn-network")),
            (
                "codex.api_request",
                Some("task-network"),
                Some("turn-network")
            ),
            (
                "websocket_close",
                Some("task-network"),
                Some("turn-network")
            ),
        ]
    );
    assert_eq!(
        page.data[4].details.get("close_reason"),
        Some(&json!("policy violation token=[REDACTED_SECRET]"))
    );
    assert_eq!(
        page.data[3].details.get("request_id"),
        Some(&json!("req-failure"))
    );
    assert_eq!(
        page.data[4].details.get("origin"),
        Some(&json!("https://api.example.test"))
    );
    let incidents = query(
        &sqlite,
        NetworkQuery {
            incidents_only: true,
            limit: 100,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        incidents
            .data
            .iter()
            .map(|row| row.event.as_str())
            .collect::<Vec<_>>(),
        vec![
            "fallback_to_http",
            "retry",
            "codex.api_request",
            "websocket_close"
        ]
    );
    let encoded = serde_json::to_string(&page)?;
    for secret in [
        "short",
        "private-header",
        "private-prompt",
        "private-email",
        "private-body",
    ] {
        assert!(
            !encoded.contains(secret),
            "unexpected private value: {secret}"
        );
    }
    let first = query(
        &sqlite,
        NetworkQuery {
            limit: 2,
            ..Default::default()
        },
    )
    .await?;
    let second = query(
        &sqlite,
        NetworkQuery {
            before_id: first.next_before_id,
            limit: 100,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        first
            .data
            .into_iter()
            .chain(second.data)
            .collect::<Vec<_>>(),
        page.data
    );
    let absent = query(
        &sqlite,
        NetworkQuery {
            thread_id: Some("another-task".to_string()),
            limit: 10,
            ..Default::default()
        },
    )
    .await?;
    assert!(absent.data.is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.path().join(DATABASE_FILENAME))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    Ok(())
}

#[tokio::test]
async fn unknown_events_and_upstream_error_bodies_are_not_retained() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let sqlite = SqliteConfig::new_for_testing(AbsolutePathBuf::try_from(dir.path())?);
    let runtime = StateRuntime::init(sqlite.clone(), "test".to_string()).await?;
    let layer = log_db::start(runtime.clone());
    let guard = tracing_subscriber::registry()
        .with(layer.clone().with_filter(log_db::default_filter()))
        .set_default();
    tracing::event!(target: "codex_otel.trace_safe", tracing::Level::INFO, event.name = "codex.sse_event", event.kind = "response.failed", error.message = "{private response body}");
    tracing::event!(target: "codex_otel.trace_safe", tracing::Level::INFO, event.name = "codex.tool_result", message = "private tool output");
    tracing::event!(target: "unrelated", tracing::Level::INFO, event.name = "codex.api_request", message = "not a network event");
    layer.flush().await;
    drop(guard);
    let page = query(
        &sqlite,
        NetworkQuery {
            limit: 20,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.data[0].details.get("failed"), Some(&json!(true)));
    assert!(!serde_json::to_string(&page)?.contains("private"));
    drop(layer);
    runtime.close().await;
    Ok(())
}
