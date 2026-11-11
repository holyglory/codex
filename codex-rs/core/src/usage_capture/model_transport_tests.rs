//! Force both worker/interceptor orderings without scheduler-dependent test hooks.
use super::*;
use crate::usage_capture::model::CaptureRequests;
use crate::usage_capture::model::ResponseOperations;
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
        tool_operations: Arc::default(),
    })
}

#[tokio::test]
async fn queued_start_preserves_cancelled_stream_ownership() {
    for ordering in ["start_before_stream", "start_after_stream"] {
        let home = tempfile::tempdir().expect("home");
        let context = context(home.path());
        let store = ExtensionData::new("request");
        let interceptor = CaptureRequests(Arc::clone(&context), Arc::default())
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
        let interceptor = CaptureRequests(Arc::clone(&context), Arc::default())
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

#[tokio::test]
async fn response_links_preserve_origin_and_leave_conflicts_and_overflow_unknown() {
    use codex_extension_api::ResponseEvent;
    use codex_usage::UsageRecord;
    use futures::StreamExt;

    let home = tempfile::tempdir().expect("home");
    let responses = Arc::new(ResponseOperations::default());
    let store = ExtensionData::new("request");
    let token = |response: &str| {
        UsageRecord::new(
            format!("thread:{response}"),
            /*observed_at*/ 1,
            /*input_tokens*/ 7,
            /*output_tokens*/ 3,
        )
        .with_operation(response)
        .with_account("current-account")
    };
    let mut first = context(home.path());
    Arc::get_mut(&mut first).expect("unique context").account_id = Some("origin-account".into());
    let mut retargeted = context(home.path());
    Arc::get_mut(&mut retargeted)
        .expect("unique context")
        .account_id = Some("retargeted-account".into());
    let mut in_flight = Vec::new();
    for (context, ids) in [
        (&first, vec!["original".to_string(), "conflict".to_string()]),
        (
            &retargeted,
            vec!["next".to_string(), "conflict".to_string()],
        ),
        (&retargeted, vec!["conflict".to_string()]),
        (
            &retargeted,
            (0..256).map(|index| format!("filler-{index}")).collect(),
        ),
    ] {
        let interceptor = CaptureRequests(Arc::clone(context), Arc::clone(&responses))
            .request(ModelRequestInput {
                kind: ModelRequestKind::Generation,
                thread_id: "thread",
                model: "model",
                request_store: &store,
                client_metadata: &mut None,
            })
            .expect("interceptor");
        let observer = interceptor.request_telemetry().expect("observer");
        observer.on_request_start(/*attempt*/ 0);
        let count = ids.len();
        let events = ids.into_iter().map(|id| {
            Ok(ResponseEvent::Created {
                response_id: Some(id),
            })
        });
        let mut stream = interceptor.intercept(Box::pin(
            futures::stream::iter(events).chain(futures::stream::pending()),
        ));
        for _ in 0..count {
            assert!(stream.next().await.is_some());
        }
        in_flight.push((stream, observer));
    }
    first.sink.flush().await;
    retargeted.sink.flush().await;
    let facts = codex_usage::UsageJournal::new(
        home.path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()
    .expect("captured operations");
    let original = facts
        .iter()
        .find(|fact| {
            fact.event.as_deref() == Some("model_request_started")
                && fact.account_id.as_deref() == Some("origin-account")
        })
        .expect("origin request");
    let mut original_token = token("original");
    let mut expected = original_token.clone();
    expected.operation_id = original.operation_id.clone();
    expected.account_id = original.account_id.clone();
    responses.annotate_token_record(&mut original_token);
    assert_eq!(original_token, expected);
    let next = facts
        .iter()
        .find(|fact| {
            fact.event.as_deref() == Some("model_request_started") && fact.attempt_number == Some(2)
        })
        .expect("retargeted request");
    let mut next_token = token("next");
    let mut expected = next_token.clone();
    expected.operation_id = next.operation_id.clone();
    expected.account_id = next.account_id.clone();
    responses.annotate_token_record(&mut next_token);
    assert_eq!(next_token, expected);
    for response in ["conflict", "missing", "filler-255"] {
        let mut unknown = token(response);
        let mut expected = unknown.clone();
        expected.operation_id = None;
        expected.account_id = None;
        responses.annotate_token_record(&mut unknown);
        assert_eq!(unknown, expected, "{response}");
    }
    drop(in_flight);
    first.sink.flush().await;
    retargeted.sink.flush().await;
}

#[tokio::test]
async fn sequential_response_usage_and_empty_completions_reuse_bounded_links() {
    use codex_extension_api::ResponseEvent;
    use codex_protocol::protocol::TokenUsage;
    use codex_usage::UsageRecord;
    use futures::StreamExt;

    let home = tempfile::tempdir().expect("home");
    let context = context(home.path());
    let responses = Arc::new(ResponseOperations::default());
    let store = ExtensionData::new("request");
    let mut tokens = Vec::new();
    for index in 0..300 {
        for has_usage in [true, false] {
            let response_id = format!("response-{index}-{has_usage}");
            let interceptor = CaptureRequests(Arc::clone(&context), Arc::clone(&responses))
                .request(ModelRequestInput {
                    kind: ModelRequestKind::Generation,
                    thread_id: "thread",
                    model: "model",
                    request_store: &store,
                    client_metadata: &mut None,
                })
                .expect("interceptor");
            let observer = interceptor.request_telemetry().expect("observer");
            observer.on_request_start(/*attempt*/ 0);
            let mut stream = interceptor.intercept(Box::pin(futures::stream::iter([
                Ok(ResponseEvent::Created {
                    response_id: Some(response_id.clone()),
                }),
                Ok(ResponseEvent::Completed {
                    response_id: response_id.clone(),
                    token_usage: has_usage.then(TokenUsage::default),
                    usage_metadata: None,
                    end_turn: None,
                }),
            ])));
            assert!(stream.next().await.is_some());
            assert!(stream.next().await.is_some());
            if has_usage {
                let mut token = UsageRecord::new(
                    format!("thread:{response_id}"),
                    /*observed_at*/ 1,
                    /*input_tokens*/ 7,
                    /*output_tokens*/ 3,
                )
                .with_operation(&response_id);
                responses.annotate_token_record(&mut token);
                assert!(
                    token
                        .operation_id
                        .as_ref()
                        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
                    "{response_id}"
                );
                tokens.push(token);
            }
            context.sink.flush().await;
        }
    }
    let facts = codex_usage::UsageJournal::new(
        home.path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()
    .expect("captured requests");
    assert_eq!(tokens.len(), 300);
    assert!(
        tokens
            .iter()
            .all(|token| facts.iter().any(|fact| fact.event.as_deref()
                == Some("model_request_started")
                && fact.operation_id == token.operation_id))
    );
}
