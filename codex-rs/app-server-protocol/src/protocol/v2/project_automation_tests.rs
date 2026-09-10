use super::ProjectAutomationCommand;
use super::ProjectWorkPurpose;
use pretty_assertions::assert_eq;

#[test]
fn project_automation_schema_accepts_serialized_command_field_names() {
    let schema = serde_json::to_value(schemars::schema_for!(ProjectAutomationCommand)).unwrap();
    let commands = [
        ProjectAutomationCommand::LinkWork {
            outcome_id: Some("outcome-context".into()),
            experiment_ref: Some("review-context@1".into()),
            clear_outcome: false,
            clear_experiment: false,
        },
        ProjectAutomationCommand::LinkWork {
            outcome_id: None,
            experiment_ref: None,
            clear_outcome: true,
            clear_experiment: true,
        },
        ProjectAutomationCommand::Status,
        ProjectAutomationCommand::Bind {
            purpose: ProjectWorkPurpose::Analysis,
            workstream: Some("cli".into()),
        },
        ProjectAutomationCommand::ActivateDelivery {
            target: "cli".into(),
            surface: "local executable".into(),
            acceptance: "status responds".into(),
            delivery_interval_ms: Some(1000),
            hard_stop_interval_ms: Some(2000),
        },
        ProjectAutomationCommand::Postpone {
            target: "cli".into(),
            delivery_due_at_ms: 2000,
            hard_stop_at_ms: 3000,
            authorization_ref: "owner-request".into(),
        },
        ProjectAutomationCommand::Pause {
            target: Some("cli".into()),
            authorization_ref: "owner-request".into(),
        },
        ProjectAutomationCommand::Resume {
            target: Some("cli".into()),
        },
        ProjectAutomationCommand::RecordDelivery {
            target: "cli".into(),
            delivered_at_ms: 1000,
            evidence_ref: "receipt".into(),
        },
        ProjectAutomationCommand::RequestReview {
            evidence_ref: "signal".into(),
        },
        ProjectAutomationCommand::CompleteReview {
            job_id: "00000000-0000-0000-0000-000000000001".into(),
            decision_ref: "review-context@1".into(),
        },
        ProjectAutomationCommand::Transfer {
            owner_thread_id: "00000000-0000-0000-0000-000000000001".into(),
            authorization_ref: "owner-request".into(),
        },
        ProjectAutomationCommand::Complete {
            outcome_ref: "outcome-context".into(),
        },
    ];
    for command in commands {
        let serialized = serde_json::to_value(command).unwrap();
        let variant = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"]["action"]["enum"][0] == serialized["action"])
            .unwrap();
        let properties = variant["properties"].as_object().unwrap();
        let unknown_fields = serialized
            .as_object()
            .unwrap()
            .keys()
            .filter(|name| !properties.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            unknown_fields,
            Vec::<String>::new(),
            "{}",
            serialized["action"]
        );
    }
}
