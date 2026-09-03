use anyhow::Result;
use codex_usage::UsageStore;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

#[tokio::test]
async fn temporary_usage_store_failure_recovers_before_model_request() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let home = Arc::new(TempDir::new()?);
    let test = test_codex()
        .with_home(Arc::clone(&home))
        .build_with_auto_env(&server)
        .await?;
    let usage_path = home.path().join("usage");
    assert!(
        !usage_path.exists(),
        "usage storage should remain lazy until the first model request"
    );
    std::fs::write(&usage_path, b"temporary filesystem conflict")?;
    let restore_path = usage_path.clone();
    let restore = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::remove_file(&restore_path).expect("remove temporary conflict");
        std::fs::create_dir(&restore_path).expect("restore usage directory");
    });

    test.submit_turn("recover accounting before sending this request")
        .await?;
    restore.await.expect("restore task");

    response_mock.single_request();
    assert_eq!(
        UsageStore::open(home.path())
            .await?
            .doctor()
            .await?
            .incomplete_operations,
        0
    );
    Ok(())
}
