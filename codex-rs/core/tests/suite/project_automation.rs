use anyhow::Context;
use anyhow::Result;
use codex_core::config::Constrained;
use codex_core::project_automation_id;
use codex_core::project_automation_now_ms;
use codex_event_subscriptions::ProjectAutomation;
use codex_event_subscriptions::ProjectAutomationCommand;
use codex_event_subscriptions::ProjectMode;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WorkPurpose;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::MockServer;

const HOUR_MS: i64 = 60 * 60 * 1000;

async fn wait_for_review_turn(
    test: &TestCodex,
    worker_thread_id: codex_protocol::ThreadId,
) -> Result<std::sync::Arc<codex_core::CodexThread>> {
    let worker = test.thread_manager.get_thread(worker_thread_id).await?;
    wait_for_event(&worker, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(worker)
}

async fn request_review(test: &TestCodex) -> Result<ProjectAutomation> {
    let state = test.codex.state_db().context("persistent state enabled")?;
    let store = state.event_subscriptions();
    let project_id = project_automation_id(test.config.cwd.as_path());
    let project = store
        .project_command(
            &project_id,
            test.session_configured.thread_id,
            /*expected_revision*/ None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Analysis,
                workstream: None,
            },
            project_automation_now_ms(),
        )
        .await?;
    Ok(store
        .project_command(
            &project_id,
            test.session_configured.thread_id,
            Some(project.revision),
            ProjectAutomationCommand::RequestReview {
                evidence_ref: "test-review-signal".into(),
            },
            project_automation_now_ms(),
        )
        .await?)
}

async fn tool_turn(
    test: &TestCodex,
    server: &MockServer,
    call_id: &str,
    tool_name: &str,
    arguments: Value,
) -> Result<[ResponsesRequest; 2]> {
    let mocks =
        mount_function_call_agent_response(server, call_id, &arguments.to_string(), tool_name)
            .await;
    test.submit_turn(&format!("Run the {call_id} check."))
        .await?;
    Ok([
        mocks.function_call.single_request(),
        mocks.completion.single_request(),
    ])
}

async fn echo_turn(
    test: &TestCodex,
    server: &MockServer,
    marker: &str,
) -> Result<[ResponsesRequest; 2]> {
    tool_turn(
        test,
        server,
        marker,
        "exec_command",
        json!({
            "cmd": format!("echo {marker}"),
            "login": false,
            "yield_time_ms": 10_000,
            "max_output_tokens": 1024,
        }),
    )
    .await
}

fn assert_echo_output(request: &ResponsesRequest, marker: &str) {
    let output = request
        .function_call_output_text(marker)
        .expect("the next model request must contain the command result");
    assert!(output.contains("Process exited with code 0"), "{output}");
    assert!(output.lines().any(|line| line == marker), "{output}");
}

fn project_output(request: &ResponsesRequest, call_id: &str) -> Result<Value> {
    let output = request
        .function_call_output_text(call_id)
        .context("the next model request must contain the project result")?;
    serde_json::from_str(&output).with_context(|| format!("project tool returned: {output}"))
}

async fn activate_delivery(
    test: &TestCodex,
    server: &MockServer,
    started_at_ms: i64,
) -> Result<ProjectAutomation> {
    let project_id = project_automation_id(test.config.cwd.as_path());
    let state = test.codex.state_db().context("persistent state enabled")?;
    let store = state.event_subscriptions();
    store
        .project_command(
            &project_id,
            test.session_configured.thread_id,
            /*expected_revision*/ None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: None,
            },
            started_at_ms,
        )
        .await?;
    let [_, request] = tool_turn(
        test,
        server,
        "bind-implementation",
        "project_automation",
        json!({"command": {"action": "bind", "purpose": "implementation"}}),
    )
    .await?;
    let bound = project_output(&request, "bind-implementation")?;
    let revision = bound["revision"].as_u64().context("bound revision")?;
    let [_, activation_request] = tool_turn(
        test,
        server,
        "activate-preview",
        "project_automation",
        json!({
            "command": {
                "action": "activate_delivery",
                "target": "preview",
                "surface": "test preview",
                "acceptance": "the requested preview is usable",
            },
            "expected_revision": revision,
        }),
    )
    .await?;
    let activated = project_output(&activation_request, "activate-preview")?;
    assert_eq!(activated["delivery"][0]["target"], "preview");
    store
        .project_status(&project_id)
        .await?
        .context("activated delivery")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_defaults_to_analysis_and_binds_specification_across_turns() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let state = test.codex.state_db().context("persistent state enabled")?;
    let store = state.event_subscriptions();
    let project_id = project_automation_id(test.config.cwd.as_path());
    assert_eq!(store.project_status(&project_id).await?, None);

    let analysis_requests = echo_turn(&test, &server, "analysis-default").await?;
    assert_echo_output(&analysis_requests[1], "analysis-default");
    assert!(
        analysis_requests[0].body_json()["tools"]
            .as_array()
            .context("advertised tools")?
            .iter()
            .any(|tool| tool["name"] == "project_automation")
    );
    let analysis = store
        .project_status(&project_id)
        .await?
        .context("automatic analysis enrollment")?;
    assert_eq!(
        (
            analysis
                .threads
                .get(&test.session_configured.thread_id.to_string()),
            analysis.mode(project_automation_now_ms()),
            analysis.delivery.is_empty(),
        ),
        (
            Some(&WorkPurpose::Analysis),
            ProjectMode::PerformanceOnly,
            true
        )
    );
    assert!(analysis.next_review_at_ms > analysis.started_at_ms);

    let bind_requests = tool_turn(
        &test,
        &server,
        "bind-specification",
        "project_automation",
        json!({
            "command": {"action": "bind", "purpose": "specification"},
            "expected_revision": analysis.revision,
        }),
    )
    .await?;
    let bound = project_output(&bind_requests[1], "bind-specification")?;
    assert_eq!(
        (&bound["purpose"], &bound["mode"], &bound["delivery"]),
        (
            &json!("specification"),
            &json!("performance_only"),
            &json!([])
        )
    );

    let status_requests = tool_turn(
        &test,
        &server,
        "later-status",
        "project_automation",
        json!({"command": {"action": "status"}}),
    )
    .await?;
    assert_eq!(project_output(&status_requests[1], "later-status")?, bound);
    assert_eq!(
        project_output(&status_requests[0], "bind-specification")?,
        bound
    );
    let mut expected = analysis;
    expected.threads.insert(
        test.session_configured.thread_id.to_string(),
        WorkPurpose::Specification,
    );
    let persisted = store
        .project_status(&project_id)
        .await?
        .context("bound project")?;
    assert_eq!(persisted.threads, expected.threads);
    assert_eq!(persisted.delivery, expected.delivery);

    let mut retained_fragment = None;
    for request in analysis_requests
        .into_iter()
        .chain(bind_requests)
        .chain(status_requests)
    {
        let text = request.message_input_texts("developer").join("\n");
        let opening = "<project_automation_instructions>";
        let closing = "</project_automation_instructions>";
        assert_eq!(text.matches(opening).count(), 1);
        assert_eq!(text.matches(closing).count(), 1);
        let fragment = text
            .split_once(opening)
            .context("project instructions opening")?
            .1
            .split_once(closing)
            .context("project instructions closing")?
            .0;
        assert!(fragment.len() <= 4096);
        if let Some(expected) = &retained_fragment {
            assert_eq!(fragment, expected);
        } else {
            retained_fragment = Some(fragment.to_owned());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_hard_stop_blocks_only_affected_implementation() -> Result<()> {
    skip_if_no_network!(Ok(()));

    for (elapsed_hours, expected_mode) in [
        (25, ProjectMode::DeliveryDue),
        (37, ProjectMode::RecoveryOnly),
    ] {
        let server = start_mock_server().await;
        let test = test_codex().build_with_auto_env(&server).await?;
        let now_ms = project_automation_now_ms();
        let project = activate_delivery(&test, &server, now_ms - elapsed_hours * HOUR_MS).await?;
        assert_eq!(project.mode(now_ms), expected_mode);

        let [_, implementation_request] =
            echo_turn(&test, &server, "implementation-at-deadline").await?;
        if expected_mode == ProjectMode::DeliveryDue {
            assert_echo_output(&implementation_request, "implementation-at-deadline");
            continue;
        }
        let blocked = implementation_request
            .function_call_output_text("implementation-at-deadline")
            .context("hard-stop result visible to model")?;
        assert!(blocked.contains("delivery hard-stop deadline"), "{blocked}");
        assert!(!blocked.contains("Process exited with code 0"), "{blocked}");

        for purpose in [
            WorkPurpose::Analysis,
            WorkPurpose::Specification,
            WorkPurpose::Recovery,
        ] {
            let purpose_name = serde_json::to_value(purpose)?;
            let bind_call = format!("bind-{}", purpose_name.as_str().context("purpose name")?);
            let [_, bound_request] = tool_turn(
                &test,
                &server,
                &bind_call,
                "project_automation",
                json!({"command": {"action": "bind", "purpose": purpose}}),
            )
            .await?;
            assert_eq!(
                project_output(&bound_request, &bind_call)?["purpose"],
                purpose_name
            );
            let marker = format!("allowed-{bind_call}");
            let [_, allowed_request] = echo_turn(&test, &server, &marker).await?;
            assert_echo_output(&allowed_request, &marker);
        }

        for (workstream, expected_mode) in [
            ("independent", ProjectMode::PerformanceOnly),
            ("default", ProjectMode::RecoveryOnly),
        ] {
            let bind_call = format!("bind-implementation-{workstream}");
            let [_, bound_request] = tool_turn(
                &test,
                &server,
                &bind_call,
                "project_automation",
                json!({"command": {
                    "action": "bind",
                    "purpose": "implementation",
                    "workstream": workstream,
                }}),
            )
            .await?;
            assert_eq!(
                project_output(&bound_request, &bind_call)?["purpose"],
                "implementation"
            );
            let marker = format!("implementation-{workstream}");
            let [_, request] = echo_turn(&test, &server, &marker).await?;
            if expected_mode == ProjectMode::PerformanceOnly {
                assert_echo_output(&request, &marker);
            } else {
                let blocked = request
                    .function_call_output_text(&marker)
                    .context("hard-stop result")?;
                assert!(blocked.contains("delivery hard-stop deadline"), "{blocked}");
            }
        }
        let state = test.codex.state_db().context("persistent state enabled")?;
        let persisted = state
            .event_subscriptions()
            .project_status(&project.project_id)
            .await?
            .context("project remains enrolled")?;
        assert_eq!(persisted.delivery, project.delivery);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_user_postponement_unblocks_later_turn_without_recording_delivery()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let now_ms = project_automation_now_ms();
    let project = activate_delivery(&test, &server, now_ms - 37 * HOUR_MS).await?;
    let [_, blocked_request] = echo_turn(&test, &server, "before-postponement").await?;
    let blocked = blocked_request
        .function_call_output_text("before-postponement")
        .context("hard-stop result")?;
    assert!(blocked.contains("delivery hard-stop deadline"), "{blocked}");

    let state = test.codex.state_db().context("persistent state enabled")?;
    let store = state.event_subscriptions();
    let delivery_due_at_ms = now_ms + 24 * HOUR_MS;
    let hard_stop_at_ms = now_ms + 36 * HOUR_MS;
    let postponed = store
        .project_command(
            &project.project_id,
            test.session_configured.thread_id,
            Some(project.revision),
            ProjectAutomationCommand::Postpone {
                target: "preview".into(),
                delivery_due_at_ms,
                hard_stop_at_ms,
                authorization_ref: "user-approved-postponement".into(),
            },
            now_ms,
        )
        .await?;
    let mut expected_delivery = project.delivery;
    let target = expected_delivery
        .get_mut("preview")
        .context("preview obligation")?;
    target.delivery_due_at_ms = delivery_due_at_ms;
    target.hard_stop_at_ms = hard_stop_at_ms;
    target.revision += 1;
    target.job = None;
    assert_eq!(postponed.delivery, expected_delivery);

    let [_, status_request] = tool_turn(
        &test,
        &server,
        "postponed-status",
        "project_automation",
        json!({"command": {"action": "status"}}),
    )
    .await?;
    let status = project_output(&status_request, "postponed-status")?;
    assert_eq!(status["mode"], "normal");
    assert_eq!(status["revision"], postponed.revision);
    assert_eq!(
        (
            &status["delivery"][0]["deliveryDueAtMs"],
            &status["delivery"][0]["hardStopAtMs"],
            &status["delivery"][0]["deliveredAtMs"],
        ),
        (
            &json!(delivery_due_at_ms),
            &json!(hard_stop_at_ms),
            &Value::Null
        )
    );
    let [_, resumed_request] = echo_turn(&test, &server, "after-postponement").await?;
    assert_echo_output(&resumed_request, "after-postponement");
    let persisted = store
        .project_status(&project.project_id)
        .await?
        .context("postponed project")?;
    assert_eq!(persisted.delivery, expected_delivery);
    assert_eq!(
        (
            persisted.delivery["preview"].delivered_at_ms,
            persisted.delivery["preview"].evidence_ref.as_deref(),
        ),
        (None, None)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_exec_carries_saved_work_links_and_actual_operation_identity()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let bound = tool_turn(
        &test,
        &server,
        "bind-work-context",
        "project_automation",
        json!({"command":{"action":"bind","purpose":"analysis","workstream":"workflow"}}),
    )
    .await?;
    let bound = project_output(&bound[1], "bind-work-context")?;
    let linked=tool_turn(&test,&server,"link-work-context","project_automation",json!({"expected_revision":bound["revision"],"command":{"action":"link_work","outcome_id":"outcome-context","experiment_ref":"review-context@1"}})).await?;
    let linked = project_output(&linked[1], "link-work-context")?;
    let command = if test
        .executor_environment()
        .selection()
        .cwd
        .infer_path_convention()
        == Some(codex_utils_path_uri::PathConvention::Windows)
    {
        "echo $env:DEVCOORDINATOR_WORK_CONTEXT"
    } else {
        "printf '%s\\n' \"$DEVCOORDINATOR_WORK_CONTEXT\""
    };
    let requests = tool_turn(
        &test,
        &server,
        "observe-work-context",
        "exec_command",
        json!({"cmd":command,"login":false,"yield_time_ms":10000}),
    )
    .await?;
    let output = requests[1]
        .function_call_output_text("observe-work-context")
        .context("work context command result")?;
    assert!(output.contains("Process exited with code 0"), "{output}");
    let captured: Value = serde_json::from_str(
        output
            .lines()
            .find(|line| line.starts_with('{'))
            .context("bounded work context envelope")?,
    )?;
    assert_eq!(captured["native_project_id"], linked["projectId"]);
    assert_eq!(
        captured["thread_id"],
        json!(test.session_configured.thread_id)
    );
    assert_eq!(captured["workstream_id"], json!("workflow"));
    assert_eq!(captured["outcome_id"], json!("outcome-context"));
    assert_eq!(captured["experiment_ref"], json!("review-context@1"));
    assert!(
        captured["operation_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(
        captured["turn_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_review_worker_uses_fresh_bounded_context_and_keeps_unfinished_job_pending()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
            config.workspace_roots = vec![config.cwd.clone()];
            config
                .permissions
                .set_workspace_roots(config.workspace_roots.clone());
            config
                    .permissions
                    .set_permission_profile(
                        codex_protocol::models::PermissionProfile::workspace_write(),
                    )
                    .expect("test repository writes are authorized");
        })
        .build_with_auto_env(&server)
        .await?;
    let parent_response = mount_function_call_agent_response(
        &server,
        "parent-repository-write",
        &json!({
            "cmd": "echo parent-authorized > parent-review-authority.txt",
            "login": false,
            "yield_time_ms": 10_000,
        })
        .to_string(),
        "exec_command",
    )
    .await;
    test.submit_text_turn("parent-history-must-not-enter-review-worker")
        .await?;
    assert!(
        parent_response
            .function_call
            .single_request()
            .body_contains_text("parent-history-must-not-enter-review-worker")
    );
    let parent_write = parent_response
        .completion
        .single_request()
        .function_call_output_text("parent-repository-write")
        .context("parent repository-write preflight")?;
    assert!(
        parent_write.contains("Process exited with code 0"),
        "{parent_write}"
    );
    assert_eq!(
        std::fs::read_to_string(test.config.cwd.join("parent-review-authority.txt"))?.trim(),
        "parent-authorized"
    );
    let parent_effective = test.codex.config_snapshot().await;
    assert_eq!(
        parent_effective.workspace_roots,
        vec![test.config.cwd.clone()]
    );
    let project = request_review(&test).await?;
    let job = project.review.as_ref().context("pending review")?;
    let write_response = mount_sse_once(
        &server,
        sse(vec![
            ev_function_call(
                "review-workflow-write",
                "exec_command",
                &json!({"cmd": "echo optimized > project-review-workflow.txt", "login": false, "yield_time_ms": 10_000}).to_string(),
            ),
            ev_completed("write-attempt-response"),
        ]),
    ).await;
    let outside = tempfile::tempdir()?;
    let outside_file = outside.path().join("review-must-not-write.txt");
    let outside_command = format!(
        "echo forbidden > {}",
        shlex::try_quote(outside_file.to_str().context("outside fixture path")?)?
    );
    let outside_response = mount_sse_once(
        &server,
        sse(vec![
            ev_function_call(
                "review-outside-write",
                "exec_command",
                &json!({"cmd":outside_command,"login":false,"yield_time_ms":10_000}).to_string(),
            ),
            ev_completed("outside-attempt-response"),
        ]),
    )
    .await;
    let worker_response = mount_function_call_agent_response(
        &server,
        "review-read-only-echo",
        &json!({"cmd": "echo review-read-only-echo", "login": false, "yield_time_ms": 10_000})
            .to_string(),
        "exec_command",
    )
    .await;
    let mut created = test.thread_manager.subscribe_thread_created();
    assert_eq!(
        test.thread_manager
            .run_project_review_worker(test.session_configured.thread_id, &project.project_id)
            .await?,
        WakeDisposition::Started
    );
    let worker_thread_id = created.recv().await?;
    let worker = wait_for_review_turn(&test, worker_thread_id).await?;
    let snapshot = worker.config_snapshot().await;
    assert!(
        matches!(snapshot.session_source, SessionSource::SubAgent(SubAgentSource::ThreadSpawn { parent_thread_id, .. }) if parent_thread_id == test.session_configured.thread_id)
    );
    assert_eq!(snapshot.forked_from_thread_id, None);
    let parent_profile = test.codex.config_snapshot().await;
    let roots = if parent_profile.profile_workspace_roots.is_empty() {
        &parent_profile.workspace_roots
    } else {
        &parent_profile.profile_workspace_roots
    };
    let authority = parent_profile
        .permission_profile
        .clone()
        .materialize_project_roots_with_workspace_roots(roots);
    let requested = codex_protocol::models::PermissionProfile::workspace_write_with(
        &[],
        parent_profile.permission_profile.network_sandbox_policy(),
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    )
    .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&test.config.cwd));
    assert_eq!(
        snapshot.permission_profile,
        codex_protocol::intersect_effective_permission_profiles(
            &authority,
            &requested,
            test.config.cwd.as_path()
        )?
    );
    assert_echo_output(
        &worker_response.completion.single_request(),
        "review-read-only-echo",
    );
    let write_result = worker_response
        .function_call
        .single_request()
        .function_call_output_text("review-workflow-write")
        .context("authorized repository edit result")?;
    assert!(
        write_result.contains("Process exited with code 0"),
        "{write_result}"
    );
    assert_eq!(
        std::fs::read_to_string(test.config.cwd.join("project-review-workflow.txt"))?.trim(),
        "optimized"
    );
    let outside_result = worker_response
        .function_call
        .single_request()
        .function_call_output_text("review-outside-write")
        .context("out-of-scope write rejection")?;
    assert!(
        !outside_result.contains("Process exited with code 0"),
        "{outside_result}"
    );
    assert!(!outside_file.exists());
    outside_response.single_request();
    let request = write_response.single_request();
    assert!(!request.body_contains_text("parent-history-must-not-enter-review-worker"));
    assert!(
        !request
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("<project_performance_review>"))
    );
    let prompt = request
        .message_input_texts("developer")
        .into_iter()
        .find(|text| text.starts_with("<project_performance_review>"))
        .context("compact worker task")?;
    assert!(prompt.len() <= 2048);
    let body = prompt
        .strip_suffix("</project_performance_review>")
        .context("review closing marker")?
        .trim_end();
    let references: Value =
        serde_json::from_str(body.rsplit_once('\n').context("bounded references")?.1)?;
    assert_eq!(
        references["usage_stats"],
        json!({
            "action": "performance_review",
            "repository": "current",
            "from_at_ms": project.review_window_start_ms,
            "to_at_ms": job.due_at_ms,
        })
    );
    assert_eq!(
        (&references["job_id"], &references["window_end_ms"]),
        (&json!(job.id), &json!(job.due_at_ms))
    );
    assert_ne!(job.due_at_ms, project.next_review_at_ms);
    let state = test.codex.state_db().context("persistent state enabled")?;
    let store = state.event_subscriptions();
    assert_eq!(
        store
            .claim_project_review_worker(
                &project.project_id,
                job.id,
                test.thread_manager.reserve_thread_id(),
                project_automation_now_ms()
            )
            .await?,
        Some(worker_thread_id)
    );
    assert_eq!(
        test.thread_manager
            .run_project_review_worker(test.session_configured.thread_id, &project.project_id)
            .await?,
        WakeDisposition::Started
    );
    assert_eq!(worker_response.function_call.requests().len(), 1);
    assert_eq!(worker_response.completion.requests().len(), 1);
    let pending = store
        .project_status(&project.project_id)
        .await?
        .context("review remains enrolled")?;
    assert_eq!(pending.review.as_ref().map(|job| job.id), Some(job.id));
    assert_eq!(pending.last_review_ref, None);
    assert_eq!(pending.last_activity_at_ms, project.last_activity_at_ms);
    let refreshed = store
        .project_command(
            &project.project_id,
            test.session_configured.thread_id,
            Some(pending.revision),
            ProjectAutomationCommand::RequestReview {
                evidence_ref: "changed-relevant-evidence".into(),
            },
            project_automation_now_ms(),
        )
        .await?;
    assert_eq!(refreshed.review.as_ref().map(|job| job.id), Some(job.id));
    let fresh_response = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message(
                "fresh-review-message",
                "Awaiting verified decision evidence.",
            ),
            ev_completed("fresh-review-response"),
        ]),
    )
    .await;
    assert_eq!(
        test.thread_manager
            .run_project_review_worker(test.session_configured.thread_id, &project.project_id)
            .await?,
        WakeDisposition::Started
    );
    wait_for_event(&worker, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert!(
        fresh_response
            .single_request()
            .body_contains_text("changed-relevant-evidence")
    );
    assert!(created.try_recv().is_err());
    assert_eq!(
        test.thread_manager
            .run_project_review_worker(test.session_configured.thread_id, &project.project_id)
            .await?,
        WakeDisposition::Started
    );
    assert_eq!(fresh_response.requests().len(), 1);
    worker.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_review_worker_reuses_persisted_identity_after_cold_resume_and_requires_complete_review()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
        })
        .build_with_auto_env(&server)
        .await?;
    let project = request_review(&test).await?;
    let job = project.review.as_ref().context("pending review")?;
    let first_response = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("first-review-message", "worker-history-survives-restart"),
            ev_completed("first-review-response"),
        ]),
    )
    .await;
    let mut created = test.thread_manager.subscribe_thread_created();
    assert_eq!(
        test.thread_manager
            .run_project_review_worker(test.session_configured.thread_id, &project.project_id)
            .await?,
        WakeDisposition::Started
    );
    let worker_thread_id = created.recv().await?;
    let worker = wait_for_review_turn(&test, worker_thread_id).await?;
    let state = test.codex.state_db().context("persistent state enabled")?;
    let store = state.event_subscriptions();
    let pending = store
        .project_status(&project.project_id)
        .await?
        .context("pending review")?;
    store
        .project_command(
            &project.project_id,
            test.session_configured.thread_id,
            Some(pending.revision),
            ProjectAutomationCommand::RequestReview {
                evidence_ref: "new-evidence-before-restart".into(),
            },
            project_automation_now_ms(),
        )
        .await?;
    worker.ensure_rollout_materialized().await;
    worker.flush_rollout().await?;
    worker.shutdown_and_wait().await?;
    test.codex.ensure_rollout_materialized().await;
    test.codex.flush_rollout().await?;
    let owner_rollout = test
        .codex
        .rollout_path()
        .context("persistent owner history")?;
    test.codex.shutdown_and_wait().await?;
    test.thread_manager.remove_thread(&worker_thread_id).await;
    test.thread_manager
        .remove_thread(&test.session_configured.thread_id)
        .await;
    let resumed_owner = test
        .thread_manager
        .resume_thread_from_rollout(
            test.config.clone(),
            owner_rollout,
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?;
    assert_eq!(resumed_owner.thread_id, test.session_configured.thread_id);
    let resumed_response = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message(
                "resumed-review-message",
                "Decision evidence is still pending.",
            ),
            ev_completed("resumed-review-response"),
        ]),
    )
    .await;
    assert_eq!(
        test.thread_manager
            .run_project_review_worker(resumed_owner.thread_id, &project.project_id)
            .await?,
        WakeDisposition::Started
    );
    assert_eq!(created.recv().await?, worker_thread_id);
    let restored_worker = wait_for_review_turn(&test, worker_thread_id).await?;
    assert!(
        resumed_response
            .single_request()
            .body_contains_text("worker-history-survives-restart")
    );
    assert_eq!(first_response.requests().len(), 1);
    let state = resumed_owner
        .thread
        .state_db()
        .context("persistent state enabled")?;
    let store = state.event_subscriptions();
    let pending = store
        .project_status(&project.project_id)
        .await?
        .context("pending review")?;
    let completed = store
        .project_command(
            &project.project_id,
            worker_thread_id,
            Some(pending.revision),
            ProjectAutomationCommand::CompleteReview {
                job_id: job.id,
                decision_ref: "review-record@1".into(),
            },
            project_automation_now_ms(),
        )
        .await?;
    assert_eq!(completed.review_window_start_ms, job.due_at_ms);
    assert_eq!(
        test.thread_manager
            .run_project_review_worker(resumed_owner.thread_id, &project.project_id)
            .await?,
        WakeDisposition::Started
    );
    assert_eq!(resumed_response.requests().len(), 1);
    assert_eq!(
        codex_core::CodexThread::project_review_worker_idle(
            restored_worker.thread_extension_data()
        )
        .await?,
        Some(resumed_owner.thread_id)
    );
    restored_worker.wait_until_terminated().await;
    resumed_owner.thread.shutdown_and_wait().await?;
    Ok(())
}
