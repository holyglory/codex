//! Permission acknowledgements must describe the requested effective policy.

use super::*;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;

pub(super) async fn next_read_only_profile_settings(
    server: &mut AppServerSession,
    thread_id: ThreadId,
    expected: &PermissionProfileSelection,
) -> ThreadSettingsUpdatedNotification {
    let approval = expected.approval_policy.expect("expected approval policy");
    let reviewer: codex_app_server_protocol::ApprovalsReviewer = expected
        .approvals_reviewer
        .expect("expected reviewer")
        .into();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(/*secs*/ 2);
    for _ in 0..20 {
        let event = tokio::time::timeout_at(deadline, server.next_event())
            .await
            .expect("app-server should confirm the requested read-only profile")
            .expect("app-server event stream should remain open");
        if let codex_app_server_client::AppServerEvent::ServerNotification(notification) =
            event.event
            && let ServerNotification::ThreadSettingsUpdated(notification) = *notification
            && notification.thread_id == thread_id.to_string()
            && notification
                .thread_settings
                .active_permission_profile
                .as_ref()
                .is_some_and(|profile| profile.id == expected.profile_id)
            && notification.thread_settings.approval_policy == approval
            && notification.thread_settings.approvals_reviewer == reviewer
            && notification.thread_settings.sandbox_policy
                == (codex_app_server_protocol::SandboxPolicy::ReadOnly {
                    network_access: false,
                })
        {
            return notification;
        }
    }
    panic!("expected requested read-only settings for thread {thread_id}");
}

#[tokio::test]
async fn read_only_profile_wait_rejects_stale_same_thread_settings() -> Result<()> {
    let app = make_test_app().await;
    let thread_id = ThreadId::new();
    let selection = PermissionProfileSelection {
        profile_id: "server-only".into(),
        approval_policy: Some(AskForApproval::OnRequest),
        approvals_reviewer: Some(ApprovalsReviewer::User),
        display_label: "server-only".into(),
    };
    let expected = ThreadSettingsUpdatedNotification {
        thread_id: thread_id.to_string(),
        thread_settings: codex_app_server_protocol::ThreadSettings {
            disabled_plugin_ids: Vec::new(),
            cwd: app.config.cwd.clone(),
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: codex_app_server_protocol::ApprovalsReviewer::User,
            sandbox_policy: codex_app_server_protocol::SandboxPolicy::ReadOnly {
                network_access: false,
            },
            active_permission_profile: Some(codex_app_server_protocol::ActivePermissionProfile {
                id: "server-only".into(),
                extends: Some(":read-only".into()),
            }),
            model: "profile-fixture".into(),
            model_provider: "openai".into(),
            service_tier: None,
            effort: None,
            summary: None,
            collaboration_mode: CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: "profile-fixture".into(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            },
            multi_agent_mode: Default::default(),
            personality: None,
        },
    };
    let mut stale = expected.clone();
    stale.thread_settings.active_permission_profile =
        Some(codex_app_server_protocol::ActivePermissionProfile::read_only());
    let mut wrong_approval = expected.clone();
    wrong_approval.thread_settings.approval_policy = AskForApproval::Never;
    let mut wrong_reviewer = expected.clone();
    wrong_reviewer.thread_settings.approvals_reviewer =
        codex_app_server_protocol::ApprovalsReviewer::AutoReview;
    let mut wrong_sandbox = expected.clone();
    wrong_sandbox.thread_settings.sandbox_policy =
        codex_app_server_protocol::SandboxPolicy::DangerFullAccess;
    let notifications = [
        stale,
        wrong_approval,
        wrong_reviewer,
        wrong_sandbox,
        expected.clone(),
    ];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let codex_home = app.config.codex_home.display().to_string();
    let (close, closed) = tokio::sync::oneshot::channel();
    let fake = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        while let Some(frame) = socket.next().await {
            let tokio_tungstenite::tungstenite::Message::Text(text) = frame? else {
                continue;
            };
            let request: serde_json::Value = serde_json::from_str(&text)?;
            if request["method"] == "initialize" {
                let response = serde_json::json!({
                    "id": request["id"],
                    "result": {"userAgent": "permission-fixture", "codexHome": codex_home},
                });
                socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        response.to_string().into(),
                    ))
                    .await?;
            } else if request["method"] == "initialized" {
                // Queue the same-thread stale and mismatched payloads before the
                // accepted snapshot. The order is deterministic and needs no sleep.
                for notification in notifications {
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            serde_json::to_string(&ServerNotification::ThreadSettingsUpdated(
                                notification,
                            ))?
                            .into(),
                        ))
                        .await?;
                }
                break;
            }
        }
        let _ = closed.await;
        socket.close(/*msg*/ None).await?;
        Result::<()>::Ok(())
    });
    let client = crate::connect_remote_app_server(crate::RemoteAppServerEndpoint::WebSocket {
        websocket_url,
        auth_token: None,
    })
    .await?;
    let mut server = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        client,
        crate::app_server_session::ThreadParamsMode::Remote,
    );
    let observed = next_read_only_profile_settings(&mut server, thread_id, &selection).await;
    let _ = close.send(());
    fake.await??;
    server.shutdown().await?;
    assert_eq!(observed, expected);
    Ok(())
}
