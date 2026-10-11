use super::*;
use crate::context::world_state::test_support::FragmentSectionTestExt;

#[test]
fn project_automation_instruction_updates_append_once_after_legacy_snapshots() {
    let current = ProjectAutomationInstructionsState(true);
    let legacy: ProjectAutomationInstructionsSnapshot = serde_json::from_str("true").unwrap();
    assert!(
        current
            .render_fragment_diff(PreviousSectionState::Known(&legacy))
            .0
            .is_some()
    );
    assert!(
        current
            .render_fragment_diff(PreviousSectionState::Unknown)
            .0
            .is_some()
    );
    let saved = ProjectAutomationInstructionsSnapshot::Current {
        enabled: true,
        hash: WorldStateHash::from_fragment(&current),
    };
    assert!(
        current
            .render_fragment_diff(PreviousSectionState::Known(&saved))
            .1
            .is_none()
    );
    let previous = ProjectAutomationInstructionsSnapshot::Current {
        enabled: true,
        hash: WorldStateHash("earlier-guidance".into()),
    };
    assert!(
        current
            .render_fragment_diff(PreviousSectionState::Known(&previous))
            .0
            .is_some()
    );
}
