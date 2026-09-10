use std::io::Cursor;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::StartThreadOptions;
use codex_core::project_automation_id;
use codex_core::project_automation_now_ms;
use codex_event_subscriptions::ProjectAutomationCommand;
use codex_event_subscriptions::ProjectMode;
use codex_event_subscriptions::WakeDisposition;
use codex_event_subscriptions::WorkPurpose;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
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
use wiremock::Request;

const HOUR_MS: i64 = 3_600_000;

fn request_body(request: &Request) -> Option<Value> {
    let compressed = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    let body = if compressed {
        zstd::stream::decode_all(Cursor::new(&request.body)).ok()?
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body).ok()
}

fn request_thread_id(request: &Request) -> Option<ThreadId> {
    let body = request_body(request)?;
    ThreadId::from_string(body.pointer("/client_metadata/thread_id")?.as_str()?).ok()
}

fn has_call_output(request: &Request, call_id: &str) -> bool {
    request_body(request).is_some_and(|body| {
        body["input"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        })
    })
}

async fn text_turn(test: &TestCodex, server: &MockServer, prompt: &str) -> Result<()> {
    let response = mount_sse_once(
        server,
        sse(vec![
            ev_assistant_message("text-message", "Text-only response."),
            ev_completed("text-response"),
        ]),
    )
    .await;
    test.submit_text_turn(prompt).await?;
    assert!(response.single_request().body_contains_text(prompt));
    Ok(())
}

async fn child_echo(
    child: &Arc<CodexThread>,
    server: &MockServer,
    call_id: &str,
) -> Result<String> {
    let responses = mount_function_call_agent_response(
        server,
        call_id,
        &json!({"cmd": "echo inherited-implementation", "login": false, "yield_time_ms": 10_000})
            .to_string(),
        "exec_command",
    )
    .await;
    child
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Continue the existing implementation task.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    responses.function_call.single_request();
    responses
        .completion
        .single_request()
        .function_call_output_text(call_id)
        .context("model-visible implementation result")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_partial_fixture_retains_migrated_state_across_text_turns() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let home = Arc::new(tempfile::tempdir()?);
    let home_lifetime = Arc::downgrade(&home);
    let TestCodex {
        codex,
        config,
        session_configured,
        ..
    } = test_codex()
        .with_home(home)
        .build_with_auto_env(&server)
        .await?;
    assert!(home_lifetime.upgrade().is_some());
    assert!(config.sqlite.queue_db_path().is_file());
    let state = codex.state_db().context("initialized persistent state")?;
    let project_id = project_automation_id(config.cwd.as_path());
    let mut projects = Vec::new();
    for prompt in [
        "Analyze the project.",
        "Continue the analysis without tools.",
    ] {
        let response = mount_sse_once(
            &server,
            sse(vec![
                ev_assistant_message("analysis-message", "Analysis complete."),
                ev_completed("analysis-response"),
            ]),
        )
        .await;
        codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        wait_for_event(&codex, |event| {
            assert!(!matches!(event, EventMsg::Error(_)), "{event:?}");
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert!(response.single_request().body_contains_text(prompt));
        let project = state
            .event_subscriptions()
            .project_status(&project_id)
            .await?
            .context("text-only project enrollment")?;
        assert_eq!(
            project
                .threads
                .get(&session_configured.thread_id.to_string()),
            Some(&WorkPurpose::Analysis)
        );
        assert!(project.delivery.is_empty());
        projects.push(project);
    }
    assert_eq!(
        (
            projects[1].started_at_ms,
            projects[1].next_review_at_ms,
            &projects[1].threads,
        ),
        (
            projects[0].started_at_ms,
            projects[0].next_review_at_ms,
            &projects[0].threads,
        )
    );
    assert!(projects[1].last_activity_at_ms >= projects[0].last_activity_at_ms);
    codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_guardian_turns_do_not_enroll_or_change_project_work() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let state = test.codex.state_db().context("persistent state")?;
    let project_id = project_automation_id(test.config.cwd.as_path());
    assert_eq!(
        state
            .event_subscriptions()
            .project_status(&project_id)
            .await?,
        None
    );
    for source in [
        SessionSource::Internal(InternalSessionSource::Guardian),
        SessionSource::SubAgent(SubAgentSource::Other("guardian".into())),
    ] {
        let before = state
            .event_subscriptions()
            .project_status(&project_id)
            .await?;
        let reviewer = test
            .thread_manager
            .start_thread(StartThreadOptions {
                session_source: Some(source),
                environments: Some(vec![test.executor_environment().selection().clone()]),
                ..StartThreadOptions::new(test.config.clone())
            })
            .await?;
        let response = mount_sse_once(
            &server,
            sse(vec![
                ev_assistant_message("review-message", "Review complete."),
                ev_completed("review-response"),
            ]),
        )
        .await;
        reviewer
            .thread
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Review the proposed action, without doing project work.".into(),
                text_elements: Vec::new(),
            }]))
            .await?;
        wait_for_event(&reviewer.thread, |event| {
            assert!(!matches!(event, EventMsg::Error(_)), "{event:?}");
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let request = response.single_request();
        let developer_text = request.message_input_texts("developer").join("\n");
        assert!(!developer_text.contains("<project_automation_instructions>"));
        assert!(!developer_text.contains("<usage_stats_instructions>"));
        assert_eq!(
            state
                .event_subscriptions()
                .project_status(&project_id)
                .await?,
            before
        );
        reviewer.thread.shutdown_and_wait().await?;
        if before.is_none() {
            text_turn(&test, &server, "Analyze the project as ordinary user work.").await?;
            assert!(
                state
                    .event_subscriptions()
                    .project_status(&project_id)
                    .await?
                    .is_some()
            );
        }
    }
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_text_only_enrolls_analysis_and_tracks_existing_specification_and_discussion()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let automatic = test_codex().build_with_auto_env(&server).await?;
    text_turn(
        &automatic,
        &server,
        "Analyze the project without calling tools.",
    )
    .await?;
    let state = automatic.codex.state_db().context("persistent state")?;
    let project = state
        .event_subscriptions()
        .project_status(&project_automation_id(automatic.config.cwd.as_path()))
        .await?
        .context("text-only work is enrolled")?;
    assert_eq!(
        (
            project
                .threads
                .get(&automatic.session_configured.thread_id.to_string()),
            project.mode(project_automation_now_ms()),
            project.delivery.is_empty()
        ),
        (
            Some(&WorkPurpose::Analysis),
            ProjectMode::PerformanceOnly,
            true
        )
    );
    assert!(project.last_activity_at_ms >= project.started_at_ms);
    assert_eq!(project.review_window_start_ms, project.started_at_ms);
    text_turn(
        &automatic,
        &server,
        "Continue the analysis without calling tools.",
    )
    .await?;
    let continued = state
        .event_subscriptions()
        .project_status(&project.project_id)
        .await?
        .context("continued text-only work")?;
    assert_eq!(
        (
            continued.started_at_ms,
            continued.review_window_start_ms,
            continued.next_review_at_ms,
            continued.review_interval_ms,
        ),
        (
            project.started_at_ms,
            project.review_window_start_ms,
            project.next_review_at_ms,
            project.review_interval_ms,
        )
    );
    assert!(continued.last_activity_at_ms >= project.last_activity_at_ms);
    automatic.codex.shutdown_and_wait().await?;

    for purpose in [WorkPurpose::Specification, WorkPurpose::Discussion] {
        let test = test_codex().build_with_auto_env(&server).await?;
        let state = test.codex.state_db().context("persistent state")?;
        let store = state.event_subscriptions();
        let project_id = project_automation_id(test.config.cwd.as_path());
        let before = store
            .project_command(
                &project_id,
                test.session_configured.thread_id,
                /*expected_revision*/ None,
                ProjectAutomationCommand::Bind {
                    purpose,
                    workstream: None,
                },
                project_automation_now_ms() - HOUR_MS,
            )
            .await?;
        text_turn(
            &test,
            &server,
            "Continue the existing non-implementation discussion without tools.",
        )
        .await?;
        let after = store
            .project_status(&project_id)
            .await?
            .context("existing text-only project")?;
        assert_eq!(after.threads, before.threads);
        assert_eq!(after.delivery, before.delivery);
        assert!(after.last_activity_at_ms > before.last_activity_at_ms);
        assert_eq!(after.review_window_start_ms, before.review_window_start_ms);
        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_real_child_inherits_parent_purpose_workstream_and_deadline()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable normal agent tools");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("enable normal v2 spawning");
        })
        .build_with_auto_env(&server)
        .await?;
    let owner = test.session_configured.thread_id;
    let state = test.codex.state_db().context("persistent state")?;
    let store = state.event_subscriptions();
    let project_id = project_automation_id(test.config.cwd.as_path());
    let bound = store
        .project_command(
            &project_id,
            owner,
            /*expected_revision*/ None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: Some("api".into()),
            },
            project_automation_now_ms() - 25 * HOUR_MS,
        )
        .await?;
    let linked = store
        .project_command(
            &project_id,
            owner,
            Some(bound.revision),
            ProjectAutomationCommand::LinkWork {
                outcome_id: Some("p-enrollment-fixture".into()),
                experiment_ref: Some("experiment-enrollment-fixture@1".into()),
                clear_outcome: false,
                clear_experiment: false,
            },
            project_automation_now_ms(),
        )
        .await?;
    store
        .project_command(
            &project_id,
            owner,
            Some(linked.revision),
            ProjectAutomationCommand::ActivateDelivery {
                target: "preview".into(),
                surface: "local preview".into(),
                acceptance: "usable preview".into(),
                delivery_interval_ms: None,
                hard_stop_interval_ms: None,
            },
            project_automation_now_ms(),
        )
        .await?;

    let spawn = mount_sse_once_match(
        &server,
        move |request: &Request| {
            request_thread_id(request) == Some(owner) && !has_call_output(request, "spawn-child")
        },
        sse(vec![
        ev_function_call_with_namespace("spawn-child", "collaboration", "spawn_agent", &json!({
            "message": "Continue the API implementation; first acknowledge the task without tools.",
            "task_name": "ordinary_worker", "fork_turns": "none",
        }).to_string()),
        ev_completed("spawn-response"),
    ]),
    )
    .await;
    let child_response = mount_sse_once_match(
        &server,
        move |request: &Request| request_thread_id(request).is_some_and(|thread| thread != owner),
        sse(vec![
            ev_assistant_message(
                "child-first-message",
                "Ready for the existing implementation.",
            ),
            ev_completed("child-first-response"),
        ]),
    )
    .await;
    let root_response = mount_sse_once_match(
        &server,
        move |request: &Request| {
            request_thread_id(request) == Some(owner) && has_call_output(request, "spawn-child")
        },
        sse(vec![
            ev_assistant_message("root-message", "Ordinary child spawned."),
            ev_completed("root-response"),
        ]),
    )
    .await;
    let mut created = test.thread_manager.subscribe_thread_created();
    test.submit_turn("Delegate the existing API implementation to one ordinary worker.")
        .await?;
    spawn.single_request();
    let root_requests = root_response
        .requests()
        .into_iter()
        .filter(|request| {
            request.body_json()["client_metadata"]["thread_id"] == json!(owner)
                && request.function_call_output_text("spawn-child").is_some()
        })
        .collect::<Vec<_>>();
    assert_eq!(root_requests.len(), 1);
    let output: Value = serde_json::from_str(
        &root_requests[0]
            .function_call_output_text("spawn-child")
            .context("spawn result")?,
    )?;
    assert!(output.get("task_name").is_some(), "{output}");
    let child_id = created.recv().await?;
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let child_requests = child_response
        .requests()
        .into_iter()
        .filter(|request| request.body_json()["client_metadata"]["thread_id"] == json!(child_id))
        .collect::<Vec<_>>();
    assert_eq!(child_requests.len(), 1);
    assert_eq!(child.config_snapshot().await.parent_thread_id, Some(owner));
    let inherited = store
        .project_status(&project_id)
        .await?
        .context("inherited child binding")?;
    assert_eq!(
        (
            inherited.threads.get(&child_id.to_string()),
            inherited
                .thread_workstreams
                .get(&child_id.to_string())
                .map(String::as_str),
            inherited.mode_for_thread(child_id, project_automation_now_ms())
        ),
        (
            Some(&WorkPurpose::Implementation),
            Some("api"),
            ProjectMode::DeliveryDue
        )
    );
    assert_eq!(
        (
            inherited.thread_outcomes.get(&child_id.to_string()),
            inherited.thread_experiments.get(&child_id.to_string())
        ),
        (
            linked.thread_outcomes.get(&owner.to_string()),
            linked.thread_experiments.get(&owner.to_string())
        )
    );
    let allowed = child_echo(&child, &server, "child-before-hard-stop").await?;
    assert!(allowed.contains("Process exited with code 0"), "{allowed}");
    assert!(
        allowed
            .lines()
            .any(|line| line == "inherited-implementation"),
        "{allowed}"
    );

    let current = store
        .project_status(&project_id)
        .await?
        .context("current child binding")?;
    let stopped = store
        .project_command(
            &project_id,
            owner,
            Some(current.revision),
            ProjectAutomationCommand::ActivateDelivery {
                target: "urgent-preview".into(),
                surface: "local urgent preview".into(),
                acceptance: "usable urgent preview".into(),
                delivery_interval_ms: Some(HOUR_MS),
                hard_stop_interval_ms: Some(2 * HOUR_MS),
            },
            project_automation_now_ms(),
        )
        .await?;
    assert_eq!(
        stopped.mode_for_thread(child_id, project_automation_now_ms()),
        ProjectMode::RecoveryOnly
    );
    let blocked = child_echo(&child, &server, "child-after-hard-stop").await?;
    assert!(blocked.contains("delivery hard-stop deadline"), "{blocked}");
    assert!(!blocked.contains("Process exited with code 0"), "{blocked}");
    child.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_claimed_review_worker_is_exempt_from_parent_enrollment() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let owner = test.session_configured.thread_id;
    let state = test.codex.state_db().context("persistent state")?;
    let store = state.event_subscriptions();
    let project_id = project_automation_id(test.config.cwd.as_path());
    let bound = store
        .project_command(
            &project_id,
            owner,
            /*expected_revision*/ None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: Some("api".into()),
            },
            project_automation_now_ms() - 37 * HOUR_MS,
        )
        .await?;
    let delivery = store
        .project_command(
            &project_id,
            owner,
            Some(bound.revision),
            ProjectAutomationCommand::ActivateDelivery {
                target: "preview".into(),
                surface: "local preview".into(),
                acceptance: "usable preview".into(),
                delivery_interval_ms: None,
                hard_stop_interval_ms: None,
            },
            project_automation_now_ms(),
        )
        .await?;
    let pending = store
        .project_command(
            &project_id,
            owner,
            Some(delivery.revision),
            ProjectAutomationCommand::RequestReview {
                evidence_ref: "enrollment-review-fixture".into(),
            },
            project_automation_now_ms(),
        )
        .await?;
    assert_eq!(
        pending.mode_for_thread(owner, project_automation_now_ms()),
        ProjectMode::RecoveryOnly
    );
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message(
                "review-message",
                "Verified review evidence is still required.",
            ),
            ev_completed("review-response"),
        ]),
    )
    .await;
    let mut created = test.thread_manager.subscribe_thread_created();
    assert_eq!(
        test.thread_manager
            .run_project_review_worker(owner, &project_id)
            .await?,
        WakeDisposition::Started
    );
    let worker_id = created.recv().await?;
    let worker = test.thread_manager.get_thread(worker_id).await?;
    wait_for_event(&worker, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    response.single_request();
    let after = store
        .project_status(&project_id)
        .await?
        .context("pending review project")?;
    assert!(!after.threads.contains_key(&worker_id.to_string()));
    assert_eq!(after, pending);
    worker.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
