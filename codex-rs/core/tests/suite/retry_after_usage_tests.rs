//! Cancellation coverage shares the existing public retry journey fixture.
use super::*;
use anyhow::Context as _;
use pretty_assertions::assert_eq;
use std::sync::Arc;

/// Cancellation records elapsed backoff without inventing a second provider attempt.
#[test_case::test_case(0, "stream_retry"; "sampling_wait")]
#[test_case::test_case(1, "http_retry"; "http_wait")]
#[tokio::test(flavor = "current_thread")]
async fn cancelled_retry_wait_records_partial_elapsed_time(
    request_max_retries: u64,
    reason: &str,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(/*s*/ 503)
                .insert_header("Retry-After", "60")
                .set_body_json(json!({"error":{"code":"server_is_overloaded"}})),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(move |config| {
            config.model_provider.request_max_retries = Some(request_max_retries);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;
    submit_user_input(&test, "cancel the advised retry").await?;
    telemetry.next_retry().await;
    tokio::time::sleep(Duration::from_millis(25)).await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    test.codex.shutdown_and_wait().await?;
    let facts = UsageJournal::new(
        test.home
            .path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()?;
    assert_eq!(response_mock.requests().len(), 1);
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.event.as_deref() == Some("model_request_started"))
            .count(),
        1
    );
    let waits = facts
        .iter()
        .filter(|fact| fact.event.as_deref() == Some("retry_wait_finished"))
        .collect::<Vec<_>>();
    assert_eq!(
        waits
            .iter()
            .map(|fact| (fact.tool.as_deref(), fact.outcome))
            .collect::<Vec<_>>(),
        vec![(Some(reason), Some(UsageOutcome::Cancelled))]
    );
    let wait = waits.first().context("cancelled retry wait")?;
    assert!(
        wait.duration_ms
            .is_some_and(|duration| duration > 0 && duration < 60_000)
    );
    assert!(wait.retry_of_operation_id.is_some());
    Ok(())
}

/// An interrupted request before headers has one cancelled attempt and no invented retry.
#[tokio::test(flavor = "current_thread")]
async fn cancelled_http_attempt_before_headers_finishes_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let observed = Arc::new(tokio::sync::Notify::new());
    let request_observed = Arc::clone(&observed);
    let response_mock = Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |_: &wiremock::Request| {
            request_observed.notify_one();
            responses::sse_response(responses::sse(vec![responses::ev_completed("too-late")]))
                .set_delay(Duration::from_secs(60))
        })
        .expect(/*r*/ 1)
        .mount_as_scoped(&server)
        .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    submit_user_input(&test, "cancel the pending request").await?;
    tokio::time::timeout(Duration::from_secs(10), observed.notified()).await?;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    test.codex.shutdown_and_wait().await?;
    let facts = UsageJournal::new(
        test.home
            .path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()?;
    let attempts = facts
        .iter()
        .filter(|fact| fact.event.as_deref() == Some("model_request_finished"))
        .collect::<Vec<_>>();
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == "/v1/responses"
                && request.method.as_str() == "POST")
            .count(),
        1
    );
    drop(response_mock);
    assert_eq!(
        attempts
            .iter()
            .map(|fact| (
                fact.attempt_number,
                fact.outcome,
                fact.response_id.as_deref(),
                fact.retry_of_operation_id.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![(Some(1), Some(UsageOutcome::Cancelled), None, None)]
    );
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.event.as_deref() == Some("retry_wait_started"))
            .count(),
        0
    );
    Ok(())
}

/// An interrupted WebSocket backoff records only the warmup and failed send that reached the server.
#[tokio::test(flavor = "current_thread")]
async fn cancelled_websocket_retry_wait_records_actual_sends() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_websocket_server(vec![vec![
        vec![responses::ev_response_created("prewarm"), responses::ev_completed("prewarm")],
        vec![json!({"type":"error","status":429,"error":{
            "type":"rate_limit_error","code":"rate_limit_exceeded","message":"Please wait.","headers":{"retry-after":60}
        }})],
    ]]).await;
    let test = test_codex()
        .with_config(|config| {
            config.model_catalog = Some(bundled_models_response().expect("bundled models"));
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_websocket_server(&server)
        .await?;
    let warmup = server
        .wait_for_request(/*connection_index*/ 0, /*request_index*/ 0)
        .await;
    assert_eq!(warmup.body_json()["generate"].as_bool(), Some(false));
    submit_user_input(&test, "cancel the websocket retry wait").await?;
    let retry = telemetry.next_retry().await;
    assert_eq!(
        (retry.layer.as_str(), retry.operation.as_str()),
        ("stream", "sampling")
    );
    tokio::time::sleep(Duration::from_millis(25)).await;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    test.codex.shutdown_and_wait().await?;
    assert_eq!(
        server
            .connections()
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        vec![2]
    );
    let facts = UsageJournal::new(
        test.home
            .path()
            .join(codex_usage::USAGE_DIRECTORY_NAME)
            .join("events.jsonl"),
    )
    .records()?;
    let starts = facts
        .iter()
        .filter(|fact| {
            matches!(
                fact.event.as_deref(),
                Some("model_request_started" | "model_warmup_started")
            )
        })
        .collect::<Vec<_>>();
    let terminals = facts
        .iter()
        .filter(|fact| {
            matches!(
                fact.event.as_deref(),
                Some("model_request_finished" | "model_warmup_finished")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        terminals
            .iter()
            .map(|fact| (
                fact.operation_id.as_deref(),
                fact.attempt_number,
                fact.outcome,
                fact.retry_of_operation_id.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                starts[0].operation_id.as_deref(),
                Some(1),
                Some(UsageOutcome::Completed),
                None
            ),
            (
                starts[1].operation_id.as_deref(),
                Some(1),
                Some(UsageOutcome::Failed),
                None
            ),
        ]
    );
    let waits = facts
        .iter()
        .filter(|fact| fact.event.as_deref() == Some("retry_wait_finished"))
        .collect::<Vec<_>>();
    assert_eq!(
        waits
            .iter()
            .map(|fact| (
                fact.tool.as_deref(),
                fact.outcome,
                fact.retry_of_operation_id.as_deref()
            ))
            .collect::<Vec<_>>(),
        vec![(
            Some("stream_retry"),
            Some(UsageOutcome::Cancelled),
            starts[1].operation_id.as_deref()
        )]
    );
    assert!(
        waits[0]
            .duration_ms
            .is_some_and(|duration| duration > 0 && duration < 60_000)
    );
    assert!(
        facts
            .iter()
            .filter(|fact| fact.event.as_deref() == Some("token_observation"))
            .all(|fact| fact.provider_total_tokens.is_none_or(|total| total == 0))
    );
    server.shutdown().await;
    Ok(())
}
