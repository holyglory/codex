use super::*;
use crate::context::ContextualUserFragment;
use crate::context::world_state::PreviousSectionState;
use crate::context::world_state::test_support::render_section_cases;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

#[test]
fn snapshots() {
    use PreviousSectionState::Absent;
    use PreviousSectionState::Known;
    use PreviousSectionState::Unknown;

    let unavailable = UsageStatsInstructionsState::new(/*available*/ false);
    let available = UsageStatsInstructionsState::new(/*available*/ true);

    insta::assert_snapshot!(render_section_cases(&[
        (Absent, Absent),
        (Absent, Known(&unavailable)),
        (Absent, Known(&available)),
        (Known(&unavailable), Known(&available)),
        (Known(&available), Known(&available)),
        (Known(&available), Known(&unavailable)),
        (Unknown, Known(&unavailable)),
        (Unknown, Known(&available)),
    ]));
}

#[test]
fn existing_guidance_is_not_injected_again() {
    let available = UsageStatsInstructionsState::new(/*available*/ true);
    let mut world_state = super::super::WorldState::default();
    world_state.add_section(available);
    let existing: ResponseItem = ContextualUserFragment::into(available);

    assert!(
        world_state
            .render_history_diff(/*previous*/ None, &[existing])
            .is_empty()
    );
}

#[test]
fn persisted_guidance_is_restored_only_when_missing_from_history() {
    let available = UsageStatsInstructionsState::new(/*available*/ true);
    let mut world_state = super::super::WorldState::default();
    world_state.add_section(available);
    let snapshot = world_state.snapshot();
    let retained: ResponseItem = ContextualUserFragment::into(available);

    assert_eq!(
        world_state.render_history_diff(Some(&snapshot), &[]).len(),
        1
    );
    assert!(
        world_state
            .render_history_diff(Some(&snapshot), &[retained])
            .is_empty()
    );
}
