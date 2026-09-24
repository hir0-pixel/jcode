//! Local memory recall: query terms, the never-pad floor, and scored recall
//! for the memory tool. The ranking itself is an FTS5 query in the memory
//! store (`MemoryManager::recall_local`); nothing here leaves the machine.

use crate::memory::{MemoryEntry, MemoryManager, MemoryScope};
use anyhow::{Result, ensure};

/// Largest query accepted for recall (longer ones are refused, not cut).
pub const MAX_QUERY_BYTES: usize = 8 * 1024;

/// Scored recall for the memory tool: best first, score 1.0 down to near 0.
pub fn recall(manager: &MemoryManager, query: &str, limit: usize, scope: MemoryScope) -> Result<Vec<(MemoryEntry, f32)>> {
    if limit == 0 || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    // Reject rather than slicing UTF-8 or silently changing the user's query.
    ensure!(query.len() <= MAX_QUERY_BYTES, "memory query exceeds 8 KiB");
    let hits = manager.recall_local(None, query, limit, scope)?;
    let n = hits.len().max(1) as f32;
    Ok(hits.into_iter().enumerate().map(|(i, e)| (e, 1.0 - i as f32 / n)).collect())
}

pub(crate) fn local_terms(text: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "are", "but", "not", "you", "your", "with", "this", "that", "from", "have",
        "has", "was", "were", "will", "would", "can", "could", "should", "what", "when", "where", "which",
        "who", "how", "why", "into", "about", "there", "their", "they", "them", "then", "than", "also",
        "just", "like", "use", "using", "used", "please", "want", "need", "make", "does", "did", "our",
    ];
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOP.contains(w))
        .map(singular)
        .collect()
}

/// Fold simple plurals so "scripts" matches "script" ("class" stays).
fn singular(word: &str) -> String {
    match word.strip_suffix('s') {
        Some(stem) if stem.len() >= 3 && !stem.ends_with('s') => stem.to_owned(),
        _ => word.to_owned(),
    }
}

/// The "never pad" floor for local recall: how many of the query's terms a
/// memory contains (content, tags, category). A term counts when a word of
/// the memory starts with it, so "script" matches "scripting" the way the
/// index's Porter stemming does. With more than two query terms a memory must
/// match at least two, so one shared common word is not enough to inject it.
pub(crate) fn meets_term_floor(query_terms: &[String], entry: &MemoryEntry) -> bool {
    let words = local_terms(&format!("{} {} {:?}", entry.content, entry.tags.join(" "), entry.category));
    let matched = query_terms.iter().filter(|q| words.iter().any(|w| w.starts_with(q.as_str()))).count();
    matched >= if query_terms.len() <= 2 { 1 } else { 2 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryCategory;

    #[test]
    fn terms_drop_stopwords_and_fold_plurals() {
        assert_eq!(local_terms("Which scripts should you use for the reports?"), ["script", "report"]);
    }

    #[test]
    fn floor_needs_two_matching_terms_for_longer_queries() {
        let entry = MemoryEntry::new(MemoryCategory::Preference, "Prefers Nim for quick scripting");
        let terms = local_terms("language for quick scripts");
        assert!(meets_term_floor(&terms, &entry), "quick + script (prefix of scripting)");
        assert!(!meets_term_floor(&local_terms("quick weather forecast today"), &entry), "one shared word is not enough");
        assert!(meets_term_floor(&local_terms("nim"), &entry), "a one-term query needs one match");
    }

    #[test]
    fn oversized_queries_are_refused() {
        let manager = MemoryManager::new_test();
        assert!(recall(&manager, &"x".repeat(MAX_QUERY_BYTES + 1), 5, MemoryScope::All).is_err());
        assert!(recall(&manager, "", 5, MemoryScope::All).unwrap().is_empty());
    }
}
