//! Lazy pinned views over the bounded sparse binding pages.
//!
//! Ordinary usage readers keep attribution failures separate from token and
//! operation facts. Typed status/CAS callers use `WorkBindingLookup` directly
//! and therefore receive corruption errors instead of an inferred clear.

use std::sync::Arc;
use std::sync::OnceLock;

use crate::WorkBindingTransition;
use crate::work_binding_store::WorkBindingLookup;

#[derive(Debug)]
enum Source {
    Lookup(WorkBindingLookup),
    Unavailable,
}

#[derive(Debug)]
pub(crate) struct WorkBindingSnapshot {
    source: Source,
    loaded: OnceLock<Option<Arc<Vec<WorkBindingTransition>>>>,
}

impl WorkBindingSnapshot {
    pub(crate) fn pin(root: &std::path::Path) -> Self {
        let source = WorkBindingLookup::open(root)
            .map(Source::Lookup)
            .unwrap_or(Source::Unavailable);
        Self {
            source,
            loaded: OnceLock::new(),
        }
    }

    pub(crate) fn transitions(&self) -> &[WorkBindingTransition] {
        self.loaded().map_or(&[], |rows| rows.as_slice())
    }

    pub(crate) fn coverage(&self) -> &'static str {
        match &self.source {
            Source::Unavailable => "unavailable",
            Source::Lookup(_) => match self.loaded() {
                Some(rows) if rows.is_empty() => "unobserved",
                Some(_) => "recorded",
                None => "unavailable",
            },
        }
    }

    fn loaded(&self) -> Option<&Arc<Vec<WorkBindingTransition>>> {
        self.loaded
            .get_or_init(|| match &self.source {
                Source::Unavailable => None,
                Source::Lookup(lookup) => lookup.read_all().ok().map(Arc::new),
            })
            .as_ref()
    }
}
