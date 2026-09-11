use super::*;

#[test]
fn project_automation_instruction_updates_append_once_after_legacy_snapshots() {
    let current = ProjectAutomationInstructionsState(true);
    let legacy: ProjectAutomationInstructionsSnapshot = serde_json::from_str("true").unwrap();
    assert!(
        current
            .render_diff(PreviousSectionState::Known(&legacy))
            .is_some()
    );
    assert!(current.render_diff(PreviousSectionState::Unknown).is_some());
    let saved = current.snapshot();
    assert!(
        current
            .render_diff(PreviousSectionState::Known(&saved))
            .is_none()
    );
    let previous = ProjectAutomationInstructionsSnapshot::Current {
        enabled: true,
        hash: WorldStateHash("earlier-guidance".into()),
    };
    assert!(
        current
            .render_diff(PreviousSectionState::Known(&previous))
            .is_some()
    );
}
