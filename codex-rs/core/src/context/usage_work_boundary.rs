//! One-shot runtime guidance for an unbound usage-work boundary.

use super::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;

/// A bounded reminder emitted once when a turn starts without an inherited or explicit binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageWorkBoundaryPrompt {}

impl UsageWorkBoundaryPrompt {
    pub(crate) fn new(_boundary_id: String) -> Self {
        Self {}
    }
}

impl ContextualUserFragment for UsageWorkBoundaryPrompt {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("usage_work.boundary_prompt".to_string())
    }

    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<usage_work_boundary>", "</usage_work_boundary>")
    }

    fn body(&self) -> ContentItem {
        input_text(format!(
            "\nA usage-work attribution boundary is missing. Resolve this boundary once before continuing. If authoritative project, workstream, or outcome IDs are already available, declare them with `usage_work`; do not ask the user. If the attribution decision or IDs are genuinely unresolved, ask the user only for that decision, then call `usage_work_boundary` with the answer. Keep attribution sparse: declare only when the job or outcome changes, never per operation, retry, or context step.\nOptions: continue, select, new, clear, unknown.\n",
        ))
    }
}
