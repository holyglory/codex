use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol as api;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

async fn command(
    app: &mut TestAppServer,
    params: Value,
) -> Result<api::ProjectAutomationCommandResponse> {
    let request_id = app
        .send_raw_request("projectAutomation/command", Some(params))
        .await?;
    let response = app
        .read_stream_until_response_message(api::RequestId::Integer(request_id))
        .await?;
    Ok(serde_json::from_value(response.result)?)
}

async fn rejected(app: &mut TestAppServer, params: Value) -> Result<api::JSONRPCErrorError> {
    let request_id = app
        .send_raw_request("projectAutomation/command", Some(params))
        .await?;
    Ok(app
        .read_stream_until_error_message(api::RequestId::Integer(request_id))
        .await?
        .error)
}

#[tokio::test]
async fn project_automation_public_rpc_reads_legacy_state_without_enrollment() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let result = command(
        &mut app,
        json!({"projectId":"legacy-project","command":{"action":"status"}}),
    )
    .await?;
    assert!(result.project.is_none());
    Ok(())
}

#[tokio::test]
async fn project_automation_capability_requires_local_controls() -> Result<()> {
    let server = create_mock_responses_server_sequence(Vec::new()).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config("[tools.local_controls]\nenabled = false")
        .write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    assert_eq!(
        command(&mut app, json!({})).await?,
        api::ProjectAutomationCommandResponse {
            capability: None,
            project: None
        }
    );
    let unavailable = rejected(&mut app, json!({"projectId": "project"})).await?;
    assert_eq!(unavailable.code, -32600);
    Ok(())
}
