use super::*;
use crate::provider_usage::ProviderResponseStatus;
use crate::provider_usage::ProviderUsage;
use crate::provider_usage::ProviderUsageObservation;
use assert_matches::assert_matches;
use codex_client::TransportError;
use futures::TryStreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::io::ReaderStream;

async fn collect_terminal_event(event: Value) -> Vec<Result<ResponseEvent, ApiError>> {
    collect_terminal_events(vec![event]).await
}

async fn collect_terminal_events(events: Vec<Value>) -> Vec<Result<ResponseEvent, ApiError>> {
    let body = events
        .into_iter()
        .map(|event| {
            let kind = event["type"].as_str().expect("event type");
            format!("event: {kind}\ndata: {event}\n\n")
        })
        .collect::<String>();
    let stream = ReaderStream::new(std::io::Cursor::new(body))
        .map_err(|error| TransportError::Network(error.to_string()));
    let (tx, mut rx) = mpsc::channel(4);
    tokio::spawn(process_sse(
        Box::pin(stream),
        tx,
        Duration::from_secs(1),
        /*telemetry*/ None,
    ));

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

fn provider_usage(value: Value) -> ProviderUsage {
    serde_json::from_value(value).expect("valid provider usage fixture")
}

#[tokio::test]
async fn completed_surfaces_provider_usage_without_changing_legacy_token_usage() {
    let usage = json!({
        "input_tokens": 10,
        "input_tokens_details": {
            "cached_tokens": 0,
            "image_tokens": 3
        },
        "output_tokens": 5,
        "output_tokens_details": null,
        "total_tokens": 15
    });
    let events = collect_terminal_event(json!({
        "type": "response.completed",
        "response": {"id": "resp-1", "usage": usage}
    }))
    .await;
    let expected_usage_metadata = ResponseUsageMetadata {
        amount: None,
        metadata: Some(usage.clone()),
    };

    assert_eq!(events.len(), 2);
    assert_matches!(
        &events[0],
        Ok(ResponseEvent::ProviderUsage(observation))
            if observation == &ProviderUsageObservation::new(
                ProviderResponseStatus::Completed,
                ProviderSourceEventKey::from_provider_response_id("resp-1"),
                provider_usage(usage.clone())
            )
    );
    assert_matches!(
        &events[1],
        Ok(ResponseEvent::Completed {
            response_id,
            token_usage: Some(token_usage),
            usage_metadata: Some(usage_metadata),
            end_turn: None,
        }) if response_id == "resp-1"
            && usage_metadata == &expected_usage_metadata
            && token_usage == &TokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
            total_tokens: 15,
            codex_rollout_budget_units: None,
        }
    );
}

#[tokio::test]
async fn failed_surfaces_provider_usage_before_the_existing_error() {
    let usage = json!({
        "input_tokens": 7,
        "output_tokens": 0,
        "total_tokens": 7
    });
    let events = collect_terminal_event(json!({
        "type": "response.failed",
        "response": {
            "id": "resp-failed",
            "usage": usage,
            "error": {"code": "insufficient_quota", "message": "redacted fixture"}
        }
    }))
    .await;

    assert_eq!(events.len(), 2);
    assert_matches!(
        &events[0],
        Ok(ResponseEvent::ProviderUsage(observation))
            if observation == &ProviderUsageObservation::new(
                ProviderResponseStatus::Failed,
                ProviderSourceEventKey::from_provider_response_id("resp-failed"),
                provider_usage(usage)
            )
    );
    assert_matches!(&events[1], Err(ApiError::QuotaExceeded));
}

#[tokio::test]
async fn incomplete_surfaces_provider_usage_before_the_existing_error() {
    let usage = json!({
        "input_tokens": 11,
        "output_tokens": 2,
        "output_tokens_details": {"reasoning_tokens": 1},
        "total_tokens": 13
    });
    let events = collect_terminal_event(json!({
        "type": "response.incomplete",
        "response": {
            "id": "resp-incomplete",
            "usage": usage,
            "incomplete_details": {"reason": "max_output_tokens"}
        }
    }))
    .await;

    assert_eq!(events.len(), 2);
    assert_matches!(
        &events[0],
        Ok(ResponseEvent::ProviderUsage(observation))
            if observation == &ProviderUsageObservation::new(
                ProviderResponseStatus::Incomplete,
                ProviderSourceEventKey::from_provider_response_id("resp-incomplete"),
                provider_usage(usage)
            )
    );
    assert_matches!(events[1], Err(ApiError::Stream(_)));
}

#[tokio::test]
async fn terminal_replay_is_suppressed_after_the_first_event() {
    let terminal = json!({
        "type": "response.completed",
        "response": {
            "id": "resp-replay",
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        }
    });
    let events = collect_terminal_events(vec![terminal.clone(), terminal]).await;

    assert_eq!(events.len(), 2);
    assert_matches!(events[0], Ok(ResponseEvent::ProviderUsage(_)));
    assert_matches!(events[1], Ok(ResponseEvent::Completed { .. }));
}

#[tokio::test]
async fn missing_response_id_keeps_usage_without_inventing_a_source_key() {
    let events = collect_terminal_event(json!({
        "type": "response.incomplete",
        "response": {
            "usage": {"input_tokens": 1},
            "incomplete_details": {"reason": "max_output_tokens"}
        }
    }))
    .await;

    assert_matches!(
        &events[0],
        Ok(ResponseEvent::ProviderUsage(observation))
            if observation.source_event_key().is_none()
    );
    assert_matches!(events[1], Err(ApiError::Stream(_)));
}

#[tokio::test]
async fn absent_or_null_usage_does_not_emit_an_observation() {
    for response in [
        json!({"id": "resp-absent"}),
        json!({"id": "resp-null", "usage": null}),
    ] {
        let events = collect_terminal_event(json!({
            "type": "response.completed",
            "response": response
        }))
        .await;
        assert_eq!(events.len(), 1);
        assert_matches!(events[0], Ok(ResponseEvent::Completed { .. }));
    }
}
