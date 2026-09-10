use super::PreviousSectionState;
use super::WorldStateSection;
use crate::agents_md::LoadedAgentsMd;
use crate::context::ContextualUserFragment;
use crate::context::UserInstructions;
use serde::Deserialize;
use serde::Serialize;

#[path = "agents_md_focused_policy.rs"]
mod focused_policy;
#[path = "agents_md_focused_policy_context.rs"]
mod focused_policy_context;

const REPLACEMENT_NOTICE: &str =
    "These AGENTS.md instructions replace all previously provided AGENTS.md instructions.";
const REMOVAL_NOTICE: &str = "The previously provided AGENTS.md instructions no longer apply.";

/// The AGENTS.md instructions currently visible to the model.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentsMdState {
    instructions: Option<UserInstructions>,
    focused_core_deduplicated: bool,
}

/// Persisted model-visible AGENTS.md state, without filesystem provenance.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct AgentsMdSnapshot {
    directory: Option<String>,
    text: Option<String>,
}

impl AgentsMdState {
    pub(crate) fn matches_focused_policy(text: &str) -> bool {
        focused_policy_context::FocusedPolicyFragment::matches_text(text)
    }

    pub(crate) async fn add_focused_policy(
        world_state: &mut super::WorldState,
        policy_file: &std::path::Path,
        applicability: &[&str],
        loaded: Option<&LoadedAgentsMd>,
    ) -> std::io::Result<Option<Self>> {
        let policy = focused_policy::load(policy_file, applicability).await?;
        let filtered = match (&policy, loaded) {
            (Some(policy), Some(loaded)) => loaded
                .clone_without_focused_core(&policy.source, &policy.core)
                .await
                .map(|filtered| {
                    let instructions = filtered.contextual_user_fragment();
                    Self {
                        instructions: (!instructions.text.is_empty()).then_some(instructions),
                        focused_core_deduplicated: true,
                    }
                }),
            _ => None,
        };
        let chunks = policy.map(|policy| policy.chunks).unwrap_or_default();
        macro_rules! add_chunks {
            ($($index:literal),* $(,)?) => {
                $(world_state.add_section(focused_policy_context::FocusedPolicySection::<$index> {
                    chunk: chunks.get($index).cloned(),
                });)*
            };
        }
        add_chunks!(
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        );
        Ok(filtered)
    }

    pub(crate) fn new(loaded: Option<&LoadedAgentsMd>) -> Self {
        Self {
            instructions: loaded.map(LoadedAgentsMd::contextual_user_fragment),
            focused_core_deduplicated: false,
        }
    }
}

impl WorldStateSection for AgentsMdState {
    const ID: &'static str = "agents_md";
    type Snapshot = AgentsMdSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        match &self.instructions {
            Some(instructions) => AgentsMdSnapshot {
                directory: instructions.directory.clone(),
                text: Some(instructions.text.clone()),
            },
            None => AgentsMdSnapshot::default(),
        }
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user" && UserInstructions::matches_text(text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        let current = self.snapshot();
        if matches!(previous, PreviousSectionState::Known(previous) if previous == &current) {
            return None;
        }

        let previous_may_contain_instructions = match previous {
            PreviousSectionState::Known(previous) => previous.text.is_some(),
            PreviousSectionState::Unknown => true,
            PreviousSectionState::Absent => false,
        };
        let replacement_notice = if self.focused_core_deduplicated {
            "These instructions replace earlier AGENTS.md messages only; the separately provided focused universal policy remains in effect."
        } else {
            REPLACEMENT_NOTICE
        };
        let removal_notice = if self.focused_core_deduplicated {
            "Earlier AGENTS.md messages no longer apply; the separately provided focused universal policy remains in effect."
        } else {
            REMOVAL_NOTICE
        };
        let instructions = match (&self.instructions, previous_may_contain_instructions) {
            (Some(instructions), true) => UserInstructions {
                directory: instructions.directory.clone(),
                text: format!("{replacement_notice}\n\n{}", instructions.text),
            },
            (Some(instructions), false) => instructions.clone(),
            (None, true) => UserInstructions {
                directory: None,
                text: removal_notice.to_string(),
            },
            (None, false) => return None,
        };
        Some(Box::new(instructions))
    }
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod tests;
