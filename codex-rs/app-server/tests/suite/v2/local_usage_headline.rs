use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::LocalUsageSummaryParams;
use codex_app_server_protocol::LocalUsageSummaryResponse;
use codex_app_server_protocol::RequestId;
use codex_usage::JournalReadLimits;
use codex_usage::UsageHeadlinePublisher;
use codex_usage::UsagePublisher;
use codex_usage::UsageReader;
use codex_usage::UsageRecord;
use codex_usage::UsageStore;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

pub(super) fn publish(root: &Path) -> Result<()> {
    if let Some(mut publisher) = UsageHeadlinePublisher::open_at(root)? {
        while !publisher.step()? {}
    }
    Ok(())
}

#[tokio::test]
async fn token_headline_preserves_exact_windows_unknowns_and_skips_rich_files() -> Result<()> {
    let home = TempDir::new()?;
    let mut store = UsageStore::open(home.path())?;
    store.append_record(super::fixture_fact(
        "outside-operation",
        "thread-a",
        "turn-a",
        "repo-a",
        -500,
        50,
    ))?;
    for (id, at_ms, input, output, provider_total) in [
        ("before", 2_000, 700, 0, Some(700)),
        ("selected", 2_001, 100, 20, Some(999)),
        ("zero", 2_002, 0, 0, Some(0)),
        ("unknown", 2_003, 9, 1, None),
        ("after", 2_004, 800, 0, Some(800)),
    ] {
        let mut fact = UsageRecord::new_millis(id, at_ms, input, output)
            .with_account("account-a")
            .with_repository("repo-a")
            .with_thread("thread-a");
        fact.event = Some("token_observation".into());
        fact.coverage = Some("recorded".into());
        fact.provider_total_tokens = provider_total;
        if id == "selected" {
            fact.cached_input_tokens = 80;
            fact.reasoning_output_tokens = 5;
        }
        store.append_record(fact)?;
    }
    store.append_record(
        UsageRecord::new_millis("other-account", 2_001, 900, 0)
            .with_account("account-b")
            .with_repository("repo-a")
            .with_thread("thread-a"),
    )?;
    UsagePublisher::open_at(store.root())?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 64,
    })?;
    publish(store.root())?;
    drop(store);
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .build_initialized_with_timeout(Duration::from_secs(20))
        .await?;
    let root = home.path().join("usage-v2");
    let publication = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(".snapshot.lock"))?;
    publication.lock()?;
    let manifest = UsageReader::open(home.path())?.manifest().clone();
    let projection: Value = serde_json::from_slice(&std::fs::read(
        root.join("segments").join(
            manifest
                .review_projection_file
                .context("fixture review descriptor")?,
        ),
    )?)?;
    let partition = projection["partitions"][0]["file"]
        .as_str()
        .context("fixture operation partition")?;
    std::fs::write(
        root.join("segments").join(partition),
        b"corrupt operation partition",
    )?;
    let params = LocalUsageSummaryParams {
        from_at_ms: Some(2_001),
        to_at_ms: Some(2_002),
        account_id: Some("account-a".into()),
        repository_id: Some("repo-a".into()),
        thread_id: Some("thread-a".into()),
        work_context_bindings: Some(serde_json::from_value(
            json!([{"schemaVersion":1,"operationId":"outside-operation",
            "capturedAtMs":1,"workContext":{"schemaVersion":1,"id":"C1","repositoryId":"different-repository",
            "nativeProjectId":"explicit-project","threadId":"different-thread"}}]),
        )?),
        ..Default::default()
    };
    let selected: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary {
            request_id,
            params: params.clone(),
        })
        .await?;
    let row = |category: &str, measured: u64, exact: Option<u64>, unknown: u64| {
        json!({
        "category":category,"repositoryBucket":"repo-a","measurementProvenance":"provider_reported",
        "measuredTokens":measured,"exactTokens":exact,"unknownObservations":unknown,"observationCount":1})
    };
    assert_eq!(
        selected.report["providerTokens"],
        json!([
            row("total_tokens", 999, Some(999), 0),
            row("input_tokens", 100, Some(100), 0),
            row("input_tokens_details.cached_tokens", 80, Some(80), 0),
            row("output_tokens", 20, Some(20), 0),
            row("output_tokens_details.reasoning_tokens", 5, Some(5), 0),
            row("cache_write_input_tokens", 0, None, 1),
        ])
    );
    assert_eq!(
        selected.report["timeRange"],
        json!({"startMs":2_001,"endMs":2_002})
    );
    assert_eq!(
        selected.report["scope"],
        json!({"type":"repository","id":"repo-a"})
    );
    assert_eq!(
        selected.report["databaseSchemaVersion"],
        codex_usage::MANIFEST_FORMAT_VERSION
    );
    assert_eq!(selected.report["coverage"]["state"], "complete");
    assert_eq!(selected.report["coverage"]["counts"], "deferred");
    assert_eq!(selected.report["counts"], Value::Null);
    assert_eq!(selected.summary["counts"], Value::Null);
    assert_eq!(selected.summary["providerTotalTokens"], 999);
    assert_eq!(selected.review, None);
    assert_eq!(
        (
            &selected.context_size,
            &selected.cost_provenance,
            &selected.lifecycle,
            &selected.timing
        ),
        (&None, &None, &None, &None)
    );
    assert!(selected.work_contexts.is_empty() && selected.operation_bindings.is_empty());
    assert!(
        selected
            .snapshot
            .source_watermark
            .starts_with("usage-v2:headline:")
    );
    if let Some(path) = std::env::var_os("CODEX_USAGE_API_FIXTURE_OUTPUT") {
        std::fs::write(
            std::path::PathBuf::from(path).with_file_name("usage-v2-api-headline-response.json"),
            serde_json::to_vec_pretty(&selected)?,
        )?;
    }
    // An actual rich request fails on the partition that the cold headline skipped.
    let id = server
        .send_request(
            "localUsage/summary",
            Some(json!({"fromAtMs":0,"toAtMs":3_000,"includeReview":true})),
        )
        .await?;
    let rich_error = timeout(
        Duration::from_secs(20),
        server.read_stream_until_error_message(RequestId::Integer(id)),
    )
    .await??;
    assert_eq!(rich_error.error.code, -32603);
    for (from, to, expected, unknown) in [(2_002, 2_003, Some(0), 0), (2_003, 2_004, None, 1)] {
        let response: LocalUsageSummaryResponse = server
            .request(|request_id| ClientRequest::LocalUsageSummary {
                request_id,
                params: LocalUsageSummaryParams {
                    from_at_ms: Some(from),
                    to_at_ms: Some(to),
                    work_context_bindings: None,
                    expected_snapshot: Some(selected.snapshot.source_watermark.clone()),
                    ..params.clone()
                },
            })
            .await?;
        assert_eq!(
            response.report["providerTokens"][0]["exactTokens"],
            json!(expected)
        );
        assert_eq!(
            response.report["providerTokens"][0]["unknownObservations"],
            unknown
        );
        assert_eq!(response.snapshot, selected.snapshot);
    }
    std::fs::write(
        root.join("work-contexts.jsonl"),
        b"corrupt exceptional bindings",
    )?;
    std::fs::write(root.join("manifest.json"), b"corrupt rich manifest")?;
    let independent: LocalUsageSummaryResponse = server
        .request(|request_id| ClientRequest::LocalUsageSummary { request_id, params })
        .await?;
    assert_eq!(independent.report, selected.report);
    assert_eq!(independent.summary, selected.summary);
    Ok(())
}

#[tokio::test]
async fn token_headlines_remain_available_during_concurrent_rich_requests() -> Result<()> {
    let home = TempDir::new()?;
    let mut store = UsageStore::open(home.path())?;
    for index in 0..256 {
        store.append_record(super::fixture_fact(
            &format!("operation-{index}"),
            "thread-a",
            "turn-a",
            "repo-a",
            1_000 + index,
            1,
        ))?;
    }
    UsagePublisher::open_at(store.root())?.publish_batch(JournalReadLimits {
        max_bytes: 1024 * 1024,
        max_entries: 512,
    })?;
    publish(store.root())?;
    drop(store);
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .build_initialized_with_timeout(Duration::from_secs(20))
        .await?;
    let mut rich = Vec::new();
    for from in 0..12 {
        rich.push(
            server
                .send_request(
                    "localUsage/summary",
                    Some(json!({"fromAtMs":from,"toAtMs":10_000,
            "includeReview":true,"operationLimit":1})),
                )
                .await?,
        );
    }
    let mut headlines = Vec::new();
    for _ in 0..4 {
        headlines.push(
            server
                .send_request(
                    "localUsage/summary",
                    Some(json!({"fromAtMs":3_000,"toAtMs":3_256,
            "repositoryId":"repo-a","includeReview":false})),
                )
                .await?,
        );
    }
    for id in headlines {
        let response: LocalUsageSummaryResponse =
            timeout(Duration::from_secs(20), server.read_response(id)).await??;
        assert_eq!(response.report["projection"], "token_headline");
        assert_eq!(response.summary["providerTotalTokens"], 256);
        assert_eq!(response.report["counts"], Value::Null);
    }
    let mut rich_successes = 0;
    for id in rich {
        match timeout(
            Duration::from_secs(20),
            server.read_stream_until_response_or_error_message(RequestId::Integer(id)),
        )
        .await??
        {
            JSONRPCMessage::Response(response) => {
                assert!(response.result["review"].is_object());
                rich_successes += 1;
            }
            JSONRPCMessage::Error(error) => {
                assert!(matches!(
                    error
                        .error
                        .data
                        .as_ref()
                        .context("bounded admission error")?["reason"]
                        .as_str(),
                    Some("reader_busy" | "snapshot_capacity_busy")
                ));
                assert_eq!(error.error.data.as_ref().unwrap()["retryable"], true);
            }
            _ => anyhow::bail!("expected terminal usage response"),
        }
    }
    assert!(
        rich_successes >= 2,
        "both admitted rich readers must complete"
    );
    Ok(())
}
