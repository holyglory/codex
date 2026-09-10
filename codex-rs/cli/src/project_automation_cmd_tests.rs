use super::*;
use pretty_assertions::assert_eq;

#[test]
fn project_automation_bind_accepts_optional_scope_and_workstream() {
    let command = ProjectAutomationCommand::try_parse_from([
        "project",
        "bind",
        "implementation",
        "--thread",
        "task-id",
        "--workstream",
        "adapter",
    ])
    .unwrap();
    assert!(
        matches!(command.action, ProjectAction::Bind { purpose: Purpose::Implementation, workstream: Some(ref workstream) } if workstream == "adapter")
    );
    assert_eq!(command.thread.as_deref(), Some("task-id"));
    assert_eq!(command.project, None);
    assert!(ProjectAutomationCommand::try_parse_from(["project", "bind", "unknown"]).is_err());
}

#[test]
fn project_automation_cli_requires_receipt_references_not_verification_flags() {
    assert!(
        ProjectAutomationCommand::try_parse_from([
            "project",
            "record-delivery",
            "--target",
            "cli",
            "--delivered-at-ms",
            "100",
            "--verified",
        ])
        .is_err()
    );
    assert!(
        ProjectAutomationCommand::try_parse_from(
            ["project", "complete-review", "--job-id", "job",]
        )
        .is_err()
    );
}

#[test]
fn project_automation_link_work_requires_explicit_nonconflicting_changes() {
    for args in [
        vec!["project", "link-work"],
        vec![
            "project",
            "link-work",
            "--outcome-id",
            "outcome-context",
            "--clear-outcome",
        ],
        vec![
            "project",
            "link-work",
            "--experiment-ref",
            "review-context@1",
            "--clear-experiment",
        ],
    ] {
        assert!(ProjectAutomationCommand::try_parse_from(args).is_err());
    }
    let command = ProjectAutomationCommand::try_parse_from([
        "project",
        "link-work",
        "--outcome-id",
        "outcome-context",
        "--clear-experiment",
    ])
    .unwrap();
    let ProjectAction::LinkWork {
        outcome_id,
        experiment_ref,
        clear_outcome,
        clear_experiment,
    } = command.action
    else {
        panic!("expected link-work command");
    };
    assert_eq!(
        (outcome_id, experiment_ref, clear_outcome, clear_experiment),
        (Some("outcome-context".to_string()), None, false, true)
    );
    for flag in [
        "--clear-outcome",
        "--clear-experiment",
        "--outcome-id",
        "--experiment-ref",
    ] {
        let mut args = vec!["project", "link-work", flag];
        if flag.ends_with("id") {
            args.push("outcome-context");
        }
        if flag.ends_with("ref") {
            args.push("review-context@1");
        }
        assert!(ProjectAutomationCommand::try_parse_from(args).is_ok());
    }
}

#[test]
fn project_automation_capability_output_snapshot() {
    let response = api::ProjectAutomationCommandResponse {
        capability: Some(api::ProjectAutomationCapability { version: 1 }),
        project: None,
    };
    insta::assert_snapshot!(render(&response, /*json*/ false, /*thread_id*/ None).unwrap(), @"Project automation v1 available; no enrolled project returned.");
    assert_eq!(
        serde_json::from_str::<api::ProjectAutomationCommandResponse>(
            &render(&response, /*json*/ true, /*thread_id*/ None).unwrap()
        )
        .unwrap(),
        response
    );
}

#[test]
fn project_automation_output_compacts_large_state_without_claiming_it_is_complete() {
    let mut response: api::ProjectAutomationCommandResponse = serde_json::from_value(serde_json::json!({
        "capability": {"version": 1},
        "project": {"projectId": "project-test", "ownerThreadId": "00000000-0000-0000-0000-000000000001",
            "revision": 3, "mode": "performanceOnly", "startedAtMs": 100, "lastActivityAtMs": 100,
            "reviewWindowStartMs": 100, "nextReviewAtMs": 200, "reviewIntervalMs": 100,
            "paused": false, "completed": false, "threads": {}, "threadWorkstreams": {"task": "x".repeat(MAX_OUTPUT_BYTES)},
            "threadOutcomes": {"task": "outcome-context"}, "threadExperiments": {"task": "review-context@1"},
            "delivery": {}, "review": null, "lastReviewRef": null}
    })).unwrap();
    let output = render(&response, /*json*/ true, Some("task")).unwrap();
    assert!(output.len() < MAX_OUTPUT_BYTES);
    let summary: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(summary["detailsOmitted"], true);
    assert_eq!(summary["revision"], 3);
    assert_eq!(summary["outcomeId"], "outcome-context");
    assert_eq!(summary["experimentRef"], "review-context@1");
    insta::assert_snapshot!(render(&response, /*json*/ false, Some("task")).unwrap(), @r#"
    Project "project-test" | revision 3 | "performanceOnly"
    Task "task" | outcome: "outcome-context" | experiment: "review-context@1"
    Owner: 00000000-0000-0000-0000-000000000001 | next review: 200 ms UTC
    "#);
    assert!(
        render(&response, /*json*/ false, Some("task"))
            .unwrap()
            .len()
            < MAX_OUTPUT_BYTES
    );
    response.project.as_mut().unwrap().completed = true;
    insta::assert_snapshot!(render(&response, /*json*/ false, /*thread_id*/ None).unwrap(), @r#"Project "project-test" | revision 3 | completed"#);
    let completed: serde_json::Value =
        serde_json::from_str(&render(&response, /*json*/ true, /*thread_id*/ None).unwrap())
            .unwrap();
    assert_eq!(completed["completed"], true);
}
