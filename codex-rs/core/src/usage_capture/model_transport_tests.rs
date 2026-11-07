//! Force both worker/interceptor orderings without scheduler-dependent test hooks.
use super::*;
use crate::usage_capture::model::CaptureRequests;
use crate::usage_capture::sink::CaptureSink;
use codex_extension_api::ModelRequestContributor;
use codex_extension_api::ModelRequestInput;
use pretty_assertions::assert_eq;

fn context(home: &std::path::Path) -> Arc<CaptureContext> {
    Arc::new(CaptureContext {
        sink: CaptureSink::new(home),
        thread_id: "thread".into(),
        parent_thread_id: None,
        turn_id: Some("turn".into()),
        account_id: None,
        repository_id: None,
        activity: Arc::default(),
        tool_operations: Mutex::default(),
    })
}

#[tokio::test]
async fn queued_start_preserves_cancelled_stream_ownership() {
    for ordering in ["start_before_stream", "start_after_stream"] {
        let home = tempfile::tempdir().expect("home");
        let context = context(home.path());
        let store = ExtensionData::new("request");
        let interceptor = CaptureRequests(Arc::clone(&context))
            .request(ModelRequestInput {
                kind: ModelRequestKind::Generation,
                thread_id: "thread",
                model: "model",
                request_store: &store,
                client_metadata: &mut None,
            })
            .expect("capture interceptor");
        let observer = interceptor.request_telemetry().expect("transport observer");
        if ordering == "start_before_stream" {
            observer.on_request_start(/*attempt*/ 0);
        }
        let stream = interceptor.intercept(Box::pin(futures::stream::pending()));
        if ordering == "start_after_stream" {
            observer.on_request_start(/*attempt*/ 0);
        }
        drop(stream);
        drop(observer);
        context.sink.flush().await;
        let facts = codex_usage::UsageJournal::new(
            home.path()
                .join(codex_usage::USAGE_DIRECTORY_NAME)
                .join("events.jsonl"),
        )
        .records()
        .expect("persisted cancellation");
        assert_eq!(
            facts
                .iter()
                .filter(|fact| fact.event.as_deref() == Some("model_request_finished"))
                .map(|fact| (fact.attempt_number, fact.outcome))
                .collect::<Vec<_>>(),
            vec![(Some(1), Some(UsageOutcome::Cancelled))],
            "{ordering}"
        );
    }
}

#[tokio::test]
async fn warmup_and_generation_keep_independent_lineage_in_one_host_store() {
    let home = tempfile::tempdir().expect("home");
    let context = context(home.path());
    let store = ExtensionData::new("shared-host-metadata");
    for kind in [
        ModelRequestKind::Warmup,
        ModelRequestKind::Generation,
        ModelRequestKind::Generation,
    ] {
        let interceptor = CaptureRequests(Arc::clone(&context))
            .request(ModelRequestInput {
                kind,
                thread_id: "thread",
                model: "model",
                request_store: &store,
                client_metadata: &mut None,
            })
            .expect("capture interceptor");
        let observer = interceptor.request_telemetry().expect("transport observer");
        observer.on_request_start(/*attempt*/ 0);
        drop(interceptor.intercept(Box::pin(futures::stream::pending())));
        drop(observer);
    }
    context.sink.flush().await;
    let facts = codex_usage::UsageJournal::new(
        home.path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()
    .expect("persisted lineage");
    let starts = facts
        .iter()
        .filter(|fact| {
            matches!(
                fact.event.as_deref(),
                Some("model_request_started" | "model_warmup_started")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        starts
            .iter()
            .map(|fact| (fact.attempt_number, fact.retry_of_operation_id.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (Some(1), None),
            (Some(1), None),
            (Some(2), starts[1].operation_id.as_deref())
        ]
    );
}
