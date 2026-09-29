use crate::memory_graph::MemoryGraph;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

// === Graph Cache ===
//
// Keyed by "<db path>#<scope>". An entry is valid while the database's
// `data_version` is unchanged, i.e. no other connection has committed since;
// this process's own writes refresh the entry directly.

struct GraphCacheEntry {
    graph: MemoryGraph,
    version: i64,
}

static GRAPH_CACHE: OnceLock<Mutex<HashMap<String, GraphCacheEntry>>> = OnceLock::new();

fn graph_cache() -> &'static Mutex<HashMap<String, GraphCacheEntry>> {
    GRAPH_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn cached_graph(key: &str, version: i64) -> Option<MemoryGraph> {
    with_cached(key, version, |graph| graph.cloned())
}

/// Borrow the cached graph (if still valid) without cloning it.
pub(super) fn with_cached<R>(key: &str, version: i64, f: impl FnOnce(Option<&MemoryGraph>) -> R) -> R {
    let cache = graph_cache().lock().ok();
    let entry = cache.as_ref().and_then(|cache| cache.get(key)).filter(|e| e.version == version);
    f(entry.map(|e| &e.graph))
}

pub(super) fn cache_graph(key: String, version: i64, graph: &MemoryGraph) {
    if let Ok(mut cache) = graph_cache().lock() {
        cache.insert(key, GraphCacheEntry { graph: graph.clone(), version });
    }
}

/// Forget a cached graph after a write that bypassed it (another row-level writer).
pub(super) fn forget_graph(key: &str) {
    if let Ok(mut cache) = graph_cache().lock() {
        cache.remove(key);
    }
}
