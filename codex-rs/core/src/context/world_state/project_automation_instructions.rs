use super::PreviousSectionState;
use super::WorldStateHash;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProjectAutomationInstructionsState(pub bool);

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum ProjectAutomationInstructionsSnapshot {
    Legacy(bool),
    Current { enabled: bool, hash: WorldStateHash },
}

impl ContextualUserFragment for ProjectAutomationInstructionsState {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("project_automation.instructions".into())
    }
    fn role(&self) -> &'static str {
        "developer"
    }
    fn requires_separate_message(&self) -> bool {
        true
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        (
            "<project_automation_instructions>",
            "</project_automation_instructions>",
        )
    }
    fn body(&self) -> String {
        "Use durable alarm_set reminders for elapsed UTC time, active work, or a terminal tool result. A reminder only nudges this task; alarm_ack acknowledges its delivery, never completes external work. Respect Stop and existing wake permissions; grant background wakes only after an explicit user request. DevCoordinator2 owns review schedules, windows, escalation and review.record receipts. On review reminders check Coordinator policy and the latest receipt, discard inactive, transferred or already-covered windows, then inspect the stated usage_stats performance_review interval, use Coordinator review.prepare, compare evidenced options, record a reasoned keep/revert/inconclusive or justified no-change decision, and complete review.record there. A token report or local alarm acknowledgement is not a completed review. Act before unrelated work; an outstanding escalation requires completion or an explicit user override that leaves the obligation open. Keep raw logs out of context. Legacy project_automation state is read-only migration evidence and creates no new clocks, workers, or admission blocks.".into()
    }
}

impl WorldStateSection for ProjectAutomationInstructionsState {
    const ID: &'static str = "project_automation_instructions";
    type Snapshot = ProjectAutomationInstructionsSnapshot;
    fn snapshot(&self) -> Self::Snapshot {
        ProjectAutomationInstructionsSnapshot::Current {
            enabled: self.0,
            hash: WorldStateHash::from_fragment(self),
        }
    }
    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer" && Self::matches_text(text)
    }
    fn has_retained_fragment_matcher() -> bool {
        true
    }
    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }
    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if !self.0
            || matches!(previous, PreviousSectionState::Known(ProjectAutomationInstructionsSnapshot::Current { enabled: true, hash }) if *hash == WorldStateHash::from_fragment(self))
        {
            None
        } else {
            Some(Box::new(*self))
        }
    }
}

#[cfg(test)]
#[path = "project_automation_instructions_tests.rs"]
mod tests;
