use anyhow::Context;
use anyhow::Result;
use codex_core::project_automation_id;
use codex_core::project_automation_now_ms;
use codex_event_subscriptions::ProjectAutomationCommand;
use codex_event_subscriptions::ProjectMode;
use codex_event_subscriptions::WorkPurpose;
use codex_features::Feature;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::MockServer;

enum ProjectState {
    Completed,
    Paused,
    DeliveryDue,
    HardStop,
}

async fn code_mode_turn(
    test: &TestCodex,
    server: &MockServer,
    call_id: &str,
    source: &str,
) -> Result<Value> {
    mount_sse_once(
        server,
        sse(vec![
            ev_custom_tool_call(call_id, "exec", source),
            ev_completed("tool-response"),
        ]),
    )
    .await;
    let response = mount_sse_once(
        server,
        sse(vec![
            ev_assistant_message("answer", "done"),
            ev_completed("answer-response"),
        ]),
    )
    .await;
    test.submit_turn("Check the project through Code Mode.")
        .await?;
    let output = response.single_request().custom_tool_call_output(call_id);
    let items = output["output"]
        .as_array()
        .with_context(|| format!("Code Mode must remain reachable: {output}"))?;
    assert_eq!(items.len(), 2, "{items:?}");
    let result = items[1]["text"].as_str().context("JSON result text")?;
    serde_json::from_str(result)
        .with_context(|| format!("Code Mode returned the nested result: {result}"))
}

#[test_case(ProjectState::Completed; "completed")]
#[test_case(ProjectState::Paused; "paused")]
#[test_case(ProjectState::DeliveryDue; "delivery_due")]
#[test_case(ProjectState::HardStop; "hard_stop")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_automation_code_mode_preserves_controls_and_nested_admission(
    scenario: ProjectState,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("enable Code Mode");
        })
        .build_with_auto_env(&server)
        .await?;
    let state = test.codex.state_db().context("persistent state enabled")?;
    let store = state.event_subscriptions();
    let project_id = project_automation_id(test.config.cwd.as_path());
    let thread_id = test.session_configured.thread_id;
    let now_ms = project_automation_now_ms();
    let elapsed_hours = match scenario {
        ProjectState::Completed | ProjectState::Paused => 0,
        ProjectState::DeliveryDue => 25,
        ProjectState::HardStop => 37,
    };
    let project = store
        .project_command(
            &project_id,
            thread_id,
            /*expected_revision*/ None,
            ProjectAutomationCommand::Bind {
                purpose: WorkPurpose::Implementation,
                workstream: None,
            },
            now_ms - elapsed_hours * 60 * 60 * 1000,
        )
        .await?;
    let setup = match scenario {
        ProjectState::Completed => ProjectAutomationCommand::Complete {
            outcome_ref: "test-completed-outcome".into(),
        },
        ProjectState::Paused => ProjectAutomationCommand::Pause {
            target: None,
            authorization_ref: "user-requested-pause".into(),
        },
        ProjectState::DeliveryDue | ProjectState::HardStop => {
            ProjectAutomationCommand::ActivateDelivery {
                target: "preview".into(),
                surface: "test preview".into(),
                acceptance: "preview is usable".into(),
                delivery_interval_ms: None,
                hard_stop_interval_ms: None,
            }
        }
    };
    let project = store
        .project_command(
            &project_id,
            thread_id,
            Some(project.revision),
            setup,
            now_ms,
        )
        .await?;
    let expected_mode = match scenario {
        ProjectState::Completed | ProjectState::Paused => ProjectMode::Paused,
        ProjectState::DeliveryDue => ProjectMode::DeliveryDue,
        ProjectState::HardStop => ProjectMode::RecoveryOnly,
    };
    assert_eq!(project.mode(now_ms), expected_mode);

    let observed = code_mode_turn(
        &test,
        &server,
        "before-transition",
        r#"
const status = JSON.parse(await tools.project_automation({command: {action: "status"}}));
let execution;
try {
    execution = await tools.exec_command({cmd: "echo nested-marker", login: false});
} catch (error) {
    execution = {error: String(error)};
}
text({status, execution});
"#,
    )
    .await?;
    assert_eq!(
        (
            &observed["status"]["mode"],
            &observed["status"]["completed"]
        ),
        (
            &serde_json::to_value(expected_mode)?,
            &json!(matches!(scenario, ProjectState::Completed)),
        )
    );
    match scenario {
        ProjectState::Completed | ProjectState::Paused | ProjectState::HardStop => {
            let blocked = observed["execution"].to_string();
            let reason = match scenario {
                ProjectState::Completed | ProjectState::Paused => "explicitly paused",
                ProjectState::HardStop => "delivery hard-stop deadline",
                ProjectState::DeliveryDue => unreachable!(),
            };
            assert!(blocked.contains(reason), "{blocked}");
            assert_eq!(observed["execution"]["exit_code"], Value::Null);
        }
        ProjectState::DeliveryDue => assert_eq!(
            (
                &observed["execution"]["exit_code"],
                observed["execution"]["output"].as_str().map(str::trim),
            ),
            (&json!(0), Some("nested-marker"))
        ),
    }
    let before_transition = store
        .project_status(&project_id)
        .await?
        .context("project persists after read-only controls")?;
    assert_eq!(before_transition.delivery, project.delivery);
    assert_eq!(before_transition.completed, project.completed);
    assert_eq!(before_transition.paused, project.paused);

    let transition = match scenario {
        ProjectState::Completed => json!({"action": "bind", "purpose": "implementation"}),
        ProjectState::Paused => json!({"action": "resume"}),
        ProjectState::DeliveryDue | ProjectState::HardStop => {
            json!({"action": "bind", "purpose": "recovery"})
        }
    };
    let resumed = code_mode_turn(
        &test,
        &server,
        "after-transition",
        &format!(
            r#"
const current = JSON.parse(await tools.project_automation({{command: {{action: "status"}}}}));
const status = JSON.parse(await tools.project_automation({{command: {transition}, expected_revision: current.revision}}));
const execution = await tools.exec_command({{cmd: "echo resumed-marker", login: false}});
text({{status, execution}});
"#
        ),
    )
    .await?;
    assert_eq!(
        (
            &resumed["status"]["completed"],
            &resumed["execution"]["exit_code"],
            resumed["execution"]["output"].as_str().map(str::trim),
        ),
        (&json!(false), &json!(0), Some("resumed-marker"))
    );
    let persisted = store
        .project_status(&project_id)
        .await?
        .context("project persists after authorized transition")?;
    assert_eq!(persisted.delivery, project.delivery);
    assert!(!persisted.paused);
    assert!(!persisted.completed);
    Ok(())
}
