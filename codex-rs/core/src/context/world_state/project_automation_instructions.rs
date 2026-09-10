use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProjectAutomationInstructionsState(pub bool);

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
        "For substantive project work use project_automation to bind the current purpose once, not before every tool. Specification, discussion and analysis do not imply delivery. Activate a delivery target only when a meaningful preliminary result is expected and authorized. Defaults: delivery at24h runs concurrently with development; only36h restricts ordinary affected implementation. User postponements update deadlines, not delivery evidence. On codex.project wakes read project status, discard superseded/paused work and handle due jobs. For performance reviews use usage_stats performance_review plus Coordinator review prepare: identify avoidable delay, compare alternatives, implement only authorized repository-local workflow improvements, verify before/after, record retain/revert/inconclusive or justified no-change, then complete the review using its evidence ref. A token report alone is not a review. Keep raw logs out of context. Continue independent authorized work while approvals or builds are pending; preserve operation IDs and use durable event subscriptions instead of duplicate attempts or repeated model status checks.".into()
    }
}

impl WorldStateSection for ProjectAutomationInstructionsState {
    const ID: &'static str = "project_automation_instructions";
    type Snapshot = bool;
    fn snapshot(&self) -> bool {
        self.0
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
        previous: PreviousSectionState<'_, bool>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if !self.0
            || matches!(
                previous,
                PreviousSectionState::Known(true) | PreviousSectionState::Unknown
            )
        {
            None
        } else {
            Some(Box::new(*self))
        }
    }
}
