use super::*;
use base64::Engine as _;
use codex_app_server_protocol::ImageReference as V2ImageReference;
#[cfg(not(windows))]
use codex_features::Feature;
use core_test_support::is_remote_test_environment;
use pretty_assertions::assert_eq;
use test_case::test_case;

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[derive(Clone, Copy)]
enum Invocation {
    Direct,
    #[cfg(not(windows))]
    CodeMode,
}

#[derive(Clone, Copy)]
enum ImageOperation {
    Generate,
    Edit,
    EditRecent,
}

#[test_case(Invocation::Direct, ImageOperation::Generate; "direct_generation")]
#[test_case(Invocation::Direct, ImageOperation::Edit; "direct_edit")]
#[test_case(Invocation::Direct, ImageOperation::EditRecent; "direct_recent_image_edit")]
#[cfg_attr(not(windows), test_case(Invocation::CodeMode, ImageOperation::Generate; "code_mode_generation"))]
#[cfg_attr(not(windows), test_case(Invocation::CodeMode, ImageOperation::Edit; "code_mode_edit"))]
#[cfg_attr(not(windows), test_case(Invocation::CodeMode, ImageOperation::EditRecent; "code_mode_recent_image_edit"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imagegen_uses_selected_profile_without_legacy_auth(
    invocation: Invocation,
    operation: ImageOperation,
) -> Result<()> {
    let home = TempDir::new()?;
    let backend = MockServer::start().await;
    let config = MockResponsesConfig::new(&backend.uri())
        .with_model("mock-model")
        .with_provider_name("OpenAI")
        .with_provider_base_url(&format!("{}/api/codex", backend.uri()))
        .with_root_config(&format!(
            "chatgpt_base_url = \"{}\"\ncli_auth_credentials_store = \"file\"",
            backend.uri()
        ))
        .with_provider_config("requires_openai_auth = true\nsupports_websockets = false");
    #[cfg(not(windows))]
    let config = match invocation {
        Invocation::Direct => config,
        Invocation::CodeMode => config.enable_feature(Feature::CodeModeOnly),
    };
    config.write(home.path())?;
    let alpha = persist_managed_chatgpt_profile(home.path(), "alpha", /*priority*/ 10)?;
    let beta = persist_managed_chatgpt_profile(home.path(), "beta", /*priority*/ 1)?;
    let mut registry = AccountRegistry {
        default_account_id: Some(alpha.metadata.id.clone()),
        accounts: vec![alpha.metadata.clone(), beta.metadata.clone()],
        ..AccountRegistry::default()
    };
    registry.auto_selection.enabled = true;
    RegistryStore::new(home.path()).create(&registry)?;
    assert!(!home.path().join("auth.json").exists());
    mount_observed_probe(&backend, &alpha, /*used_percent*/ 100, 1..).await;
    mount_observed_probe(&backend, &beta, /*used_percent*/ 10, 1..).await;

    let png_bytes = base64::engine::general_purpose::STANDARD.decode(PNG)?;
    let mut arguments = json!({"prompt": "paint a blue whale"});
    let mut input = vec![V2UserInput::Text {
        text: "generate the image".to_string(),
        text_elements: Vec::new(),
    }];
    let image_path = match operation {
        ImageOperation::Generate => "/api/codex/images/generations",
        ImageOperation::Edit => {
            let reference = home.path().join("reference.png");
            std::fs::write(&reference, &png_bytes)?;
            arguments["referenced_image_paths"] = json!([reference]);
            "/api/codex/images/edits"
        }
        ImageOperation::EditRecent => {
            arguments["num_last_images_to_include"] = json!(1);
            input.push(V2UserInput::Image {
                image: V2ImageReference::Inline {
                    url: format!("data:image/png;base64,{PNG}"),
                },
                detail: None,
            });
            "/api/codex/images/edits"
        }
    };
    // Image path arguments are host paths; remote threads must use attached images instead.
    let remote_path_edit =
        matches!(operation, ImageOperation::Edit) && is_remote_test_environment();
    let expected_image_requests = if remote_path_edit { 0 } else { 1 };
    Mock::given(method("POST"))
        .and(path(image_path))
        .and(header(
            "authorization",
            format!("Bearer {}", beta.access_token),
        ))
        .and(header("chatgpt-account-id", beta.workspace_id.as_str()))
        .respond_with(ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
            "created": 1,
            "background": "opaque",
            "data": [{"b64_json": PNG}]
        })))
        .expect(expected_image_requests)
        .mount(&backend)
        .await;
    let tool_call = match invocation {
        Invocation::Direct => responses::ev_function_call_with_namespace(
            "image-call",
            "image_gen",
            "imagegen",
            &arguments.to_string(),
        ),
        #[cfg(not(windows))]
        Invocation::CodeMode => responses::ev_custom_tool_call(
            "image-call",
            "exec",
            &format!(
                "const result = await tools.image_gen__imagegen({arguments}); generatedImage(result);"
            ),
        ),
    };
    let mocked = responses::mount_sse_sequence(
        &backend,
        vec![
            responses::sse(vec![
                responses::ev_response_created("image-response"),
                tool_call,
                responses::ev_completed("image-response"),
            ]),
            create_final_assistant_message_sse_response(if remote_path_edit {
                "attach the image instead of using a host path"
            } else {
                "image saved"
            })?,
        ],
    )
    .await;
    let mut app = fresh_desktop_server(home.path()).await?;
    let thread = app
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?
        .thread;
    let completed = timeout(
        EVENT_TIMEOUT,
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id,
            input,
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let requests = mocked.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {}", beta.access_token))
        );
        assert_eq!(
            request.header("chatgpt-account-id"),
            Some(beta.workspace_id.clone())
        );
    }
    let output_dir = home
        .path()
        .join("generated_images")
        .join(&completed.thread_id);
    if remote_path_edit {
        let output = match invocation {
            Invocation::Direct => requests[1].function_call_output("image-call"),
            #[cfg(not(windows))]
            Invocation::CodeMode => requests[1].custom_tool_call_output("image-call"),
        };
        assert!(
            output
                .to_string()
                .contains("referenced image paths are unavailable in this session"),
            "the model should receive the remote path error: {output}"
        );
        assert!(!output_dir.exists());
        assert!(
            backend
                .received_requests()
                .await
                .context("failed to fetch received requests")?
                .iter()
                .all(|request| !request.url.path().starts_with("/api/codex/images/"))
        );
    } else {
        let saved = std::fs::read_dir(output_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(saved.len(), 1);
        assert_eq!(std::fs::read(saved[0].path())?, png_bytes);
    }
    assert!(!home.path().join("auth.json").exists());
    backend.verify().await;
    Ok(())
}
