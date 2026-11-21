use super::PROBE_SERVER;
use super::run_harness_through_proxy;
use anyhow::Context;
use anyhow::Result;
use wiremock::MockServer;

#[tokio::test]
async fn harness_accepts_one_passing_case() -> Result<()> {
    crate::skip_if_no_network!(Ok(()));
    if std::env::var(PROBE_SERVER).is_ok() {
        return Ok(());
    }
    let test_name = std::thread::current()
        .name()
        .context("current libtest case name")?
        .to_owned();
    let server = MockServer::start().await;
    run_harness_through_proxy(&server, &test_name, /*realtime_target*/ None).await
}

#[tokio::test]
async fn harness_rejects_an_unknown_case() -> Result<()> {
    crate::skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;
    let error = run_harness_through_proxy(
        &server,
        "runtime_auth::tests::unknown_case",
        /*realtime_target*/ None,
    )
    .await
    .expect_err("an exact selector that matches no tests must fail");
    assert!(
        error
            .to_string()
            .contains("harness subprocess did not run exactly one passing test")
    );
    Ok(())
}
