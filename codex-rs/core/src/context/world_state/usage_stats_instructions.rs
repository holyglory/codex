use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

const INSTRUCTIONS: &str = "Local usage analytics are available through `usage_stats`. For usage-based analysis or reflection about this agent, call it before answering and ground the answer in its result: use `task_tree_summary` (`root_thread_id=\"current\"`, `include_descendants=true`) for the task tree, or `summary` for broader scopes. Map model-routed `/usage all|chat|repo` to `summary` scopes `all|current_chat|current_repository`. Do not search OpenAI documentation for local usage unless the user asks about official product behavior.";

/// Whether built-in local usage guidance should be visible to the model.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UsageStatsInstructionsState {
    available: bool,
}

impl UsageStatsInstructionsState {
    pub(crate) fn new(available: bool) -> Self {
        Self { available }
    }
}

impl ContextualUserFragment for UsageStatsInstructionsState {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("usage_stats.instructions".to_string())
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
        ("<usage_stats_instructions>", "</usage_stats_instructions>")
    }

    fn body(&self) -> String {
        format!("\n{INSTRUCTIONS}\n")
    }
}

impl WorldStateSection for UsageStatsInstructionsState {
    const ID: &'static str = "usage_stats_instructions";
    type Snapshot = bool;

    fn snapshot(&self) -> Self::Snapshot {
        self.available
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
        if !self.available
            || matches!(previous, PreviousSectionState::Known(previous) if *previous)
            || matches!(previous, PreviousSectionState::Unknown)
        {
            return None;
        }

        Some(Box::new(*self))
    }
}

#[cfg(test)]
#[path = "usage_stats_instructions_tests.rs"]
mod tests;
