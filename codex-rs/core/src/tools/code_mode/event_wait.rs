use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::RuntimeResponse;

#[derive(Clone, Default)]
pub(super) struct EventWaitRegistry(Arc<Mutex<HashMap<String, usize>>>);

pub(crate) struct EventWaitGuard {
    registry: EventWaitRegistry,
    cell_id: String,
}

impl EventWaitRegistry {
    pub(super) fn begin(&self, cell_id: String) -> EventWaitGuard {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(cell_id.clone())
            .or_default() += 1;
        EventWaitGuard {
            registry: self.clone(),
            cell_id,
        }
    }

    pub(super) fn coalesces(&self, response: &RuntimeResponse) -> bool {
        let RuntimeResponse::Yielded {
            cell_id,
            content_items,
            ..
        } = response
        else {
            return false;
        };
        content_items.is_empty()
            && self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(cell_id.as_str())
    }
}

impl Drop for EventWaitGuard {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = registry.get_mut(&self.cell_id) {
            *count -= 1;
            if *count == 0 {
                registry.remove(&self.cell_id);
            }
        }
    }
}

#[cfg(test)]
#[path = "event_wait_tests.rs"]
mod tests;
