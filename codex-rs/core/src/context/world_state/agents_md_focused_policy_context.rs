use super::focused_policy::PolicyChunk;
use crate::context::ContextualUserFragment;
use crate::context::world_state::PreviousSectionState;
use crate::context::world_state::WorldStateSection;
use codex_protocol::models::ContentItemKind;
use serde::Deserialize;
use serde::Serialize;

pub(super) struct FocusedPolicySection<const INDEX: usize> {
    pub(super) chunk: Option<PolicyChunk>,
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct FocusedPolicySnapshot {
    chunk: Option<PolicyChunk>,
}

impl<const INDEX: usize> WorldStateSection for FocusedPolicySection<INDEX> {
    const ID: &'static str = [
        "focused_policy_0",
        "focused_policy_1",
        "focused_policy_2",
        "focused_policy_3",
        "focused_policy_4",
        "focused_policy_5",
        "focused_policy_6",
        "focused_policy_7",
        "focused_policy_8",
        "focused_policy_9",
        "focused_policy_10",
        "focused_policy_11",
        "focused_policy_12",
        "focused_policy_13",
        "focused_policy_14",
        "focused_policy_15",
        "focused_policy_16",
        "focused_policy_17",
        "focused_policy_18",
        "focused_policy_19",
        "focused_policy_20",
        "focused_policy_21",
        "focused_policy_22",
        "focused_policy_23",
        "focused_policy_24",
        "focused_policy_25",
        "focused_policy_26",
        "focused_policy_27",
        "focused_policy_28",
        "focused_policy_29",
        "focused_policy_30",
        "focused_policy_31",
    ][INDEX];
    type Snapshot = FocusedPolicySnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        FocusedPolicySnapshot {
            chunk: self.chunk.clone(),
        }
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "user"
            && FocusedPolicyFragment::matches_text(text)
            && text.contains(&format!("part={INDEX}\n"))
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
        if matches!(previous, PreviousSectionState::Known(previous) if previous == &self.snapshot())
            || matches!(previous, PreviousSectionState::Absent) && self.chunk.is_none()
        {
            return None;
        }
        Some(Box::new(FocusedPolicyFragment {
            index: INDEX,
            chunk: self.chunk.clone(),
        }))
    }
}

pub(super) struct FocusedPolicyFragment {
    index: usize,
    chunk: Option<PolicyChunk>,
}

impl ContextualUserFragment for FocusedPolicyFragment {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("agents_md.focused_policy".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            "<focused_universal_policy>\n",
            "\n</focused_universal_policy>",
        )
    }

    fn body(&self) -> String {
        let index = self.index;
        match &self.chunk {
            Some(chunk) => {
                let notice = if index == 0 {
                    "Policy v1: ordered parts replace only the prior focused policy; user/project precedence is unchanged.\n"
                } else {
                    ""
                };
                format!(
                    "revision={}\npart={index}\n{notice}\n{}",
                    chunk.revision, chunk.text
                )
            }
            None => format!(
                "part={index}\nThis focused-policy part no longer applies. Other user and project instructions are unchanged."
            ),
        }
    }
}
