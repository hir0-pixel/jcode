//! Per-turn memory recall (sovereign).
//!
//! Recall runs inline for the user turn being answered: an indexed full-text
//! query over the stored memories (`MemoryManager::recall_local`), published
//! as pending memory that the caller takes right away, so inject-once and
//! never-pad bookkeeping stay in one place. Recall calls no model. Writing
//! memories from a chat is `memory_extract` (facts) and the Prime loop.

use crate::memory::{self, MemoryManager};
use crate::obs_sink::{self, Span};

fn manager_for_working_dir(working_dir: Option<&str>) -> MemoryManager {
    match working_dir {
        Some(dir) if !dir.trim().is_empty() => MemoryManager::new().with_project_dir(dir),
        _ => MemoryManager::new(),
    }
}

/// Recall for the user turn being answered; matches are published as pending
/// memory, which the caller takes right away.
pub fn recall_local_now(
    session_id: &str,
    messages: &[crate::message::Message],
    working_dir: Option<&str>,
) {
    let query = memory::format_focused_query_for_relevance(messages);
    let query = crate::util::truncate_str(&query, crate::memory_recall::MAX_QUERY_BYTES);
    if query.trim().is_empty() {
        return;
    }
    let manager = manager_for_working_dir(working_dir);
    let Ok(recall) = manager.recall_local_detailed(Some(session_id), query, 5, memory::MemoryScope::All) else {
        return;
    };
    let ids = |entries: &[memory::MemoryEntry]| entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    obs_sink::emit(
        Span::new("memory.recall")
            .session(session_id)
            .attr("terms", recall.terms.iter().take(12).cloned().collect::<Vec<_>>())
            .attr("candidates", recall.candidates)
            .attr("returned", ids(&recall.entries))
            .attr("suppressed", recall.suppressed),
    );
    let relevant = recall.entries;
    let Some(prompt) = memory::format_relevant_prompt(&relevant, 5) else {
        return;
    };
    obs_sink::emit(
        Span::new("memory.inject")
            .session(session_id)
            .attr("ids", ids(&relevant))
            .attr("chars", prompt.chars().count()),
    );
    let display = memory::format_relevant_display_prompt(&relevant, 5);
    memory::set_pending_memory_for_project_with_selection(
        session_id,
        prompt,
        relevant.len(),
        &relevant,
        display,
        working_dir,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_without_working_dir_does_not_infer_process_project() {
        let manager = manager_for_working_dir(None);
        assert!(manager.load_project_graph().unwrap().memories.is_empty());
    }
}
