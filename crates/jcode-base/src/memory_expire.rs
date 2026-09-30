//! Mark a memory wrong or expired: the row stays, `active` goes false, so recall and the
//! extractor's known list (both `active=1` only) stop seeing it, and the reason is kept in
//! `source` so the change can be audited or undone by hand.

use crate::memory::MemoryManager;
use anyhow::Result;

impl MemoryManager {
    /// Deactivate `id` in whichever scope holds it. `Ok(false)` when the id is unknown.
    pub fn expire(&self, id: &str, reason: &str) -> Result<bool> {
        for (scope, mut graph) in self.every_scope_graph()? {
            let Some(memory) = graph.get_memory_mut(id) else {
                continue;
            };
            let previous = memory.source.take().unwrap_or_default();
            let reason = reason.trim();
            memory.source = Some(match (reason.is_empty(), previous.is_empty()) {
                (true, true) => "expired".to_string(),
                (true, false) => format!("expired (was: {previous})"),
                (false, true) => format!("expired: {reason}"),
                (false, false) => format!("expired: {reason} (was: {previous})"),
            });
            memory.active = false;
            memory.superseded_by = None;
            memory.updated_at = chrono::Utc::now();
            self.save_graph_for_scope(&scope, &graph)?;
            return Ok(true);
        }
        Ok(false)
    }
}
