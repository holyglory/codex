use super::*;
use crate::context::world_state::test_support::FragmentSectionTestExt;

#[test]
fn project_automation_instruction_updates_append_once_after_legacy_snapshots() {
    let current = ProjectAutomationInstructionsState(true);
    let legacy: ProjectAutomationInstructionsSnapshot = serde_json::from_str("true").unwrap();
    assert!(
        current
            .render_fragment_diff(PreviousSectionState::Known(&legacy))
            .1
            .is_some()
    );
    let (saved, fragment) = current.render_fragment_diff(PreviousSectionState::Unknown);
    assert!(fragment.is_some());
    let saved = saved.expect("persist current instruction state");
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
            .1
            .is_some()
    );
}
