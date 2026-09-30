//! SQLite storage for the memory graph.
//!
//! One database file per engine (`sovereign.db` in the jcode home, WAL mode).
//! Each memory is a row with its searchable text mirrored into an FTS5 index,
//! so per-turn recall is an indexed full-text query instead of cloning and
//! re-tokenising every memory. Writes touch only the rows that changed; the
//! rest of the graph (tags, clusters, edges) is one small row per scope.
//! Scopes: `global`, and `project:<hash of the project dir>`.
//!
//! Layout (measured, see `tests::scaling`): `memories` is kept lean (the
//! searchable text plus scope/active) because the recall query reads a row
//! for every match before it can rank and limit; the full entry JSON and the
//! embedding (as little-endian f32 bytes, not JSON numbers) live in
//! `memory_entries` and are fetched only for the final top results. With the
//! entry inline, 10k memories with embeddings made recall 2.4x slower and
//! the file 9x larger.

use crate::memory_graph::{ClusterEntry, Edge, GraphMetadata, MemoryGraph, TagEntry};
use crate::memory_recall::{local_terms, singular};
use crate::memory_types::{MemoryEntry, TrustLevel};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const PRAGMAS: &str = "
    PRAGMA journal_mode=WAL;
    PRAGMA synchronous=NORMAL;
    PRAGMA busy_timeout=5000;
";

/// The tables, indexes and triggers; also applied by migration 6, which runs inside a transaction
/// (where the pragmas above cannot).
pub(crate) const TABLES: &str = "
    CREATE TABLE IF NOT EXISTS memories(
        rid INTEGER PRIMARY KEY,
        id TEXT NOT NULL,
        scope TEXT NOT NULL,
        active INTEGER NOT NULL,
        content TEXT NOT NULL,
        tags TEXT NOT NULL,
        UNIQUE(scope, id)
    );
    CREATE TABLE IF NOT EXISTS memory_entries(rid INTEGER PRIMARY KEY, entry TEXT NOT NULL, embedding BLOB);
    CREATE INDEX IF NOT EXISTS memories_scope_active ON memories(scope, active);
    CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
        content, tags, content='memories', content_rowid='rid', tokenize='porter unicode61'
    );
    CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
        INSERT INTO memories_fts(rowid, content, tags) VALUES (new.rid, new.content, new.tags);
    END;
    CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
        INSERT INTO memories_fts(memories_fts, rowid, content, tags) VALUES ('delete', old.rid, old.content, old.tags);
        DELETE FROM memory_entries WHERE rid = old.rid;
    END;
    CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
        INSERT INTO memories_fts(memories_fts, rowid, content, tags) VALUES ('delete', old.rid, old.content, old.tags);
        INSERT INTO memories_fts(rowid, content, tags) VALUES (new.rid, new.content, new.tags);
    END;
    CREATE TABLE IF NOT EXISTS memory_graphs(scope TEXT PRIMARY KEY, graph TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS memory_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// Everything in a `MemoryGraph` except the memories themselves.
#[derive(Serialize)]
struct GraphShape<'a> {
    graph_version: u32,
    tags: &'a HashMap<String, TagEntry>,
    clusters: &'a HashMap<String, ClusterEntry>,
    edges: &'a HashMap<String, Vec<Edge>>,
    reverse_edges: &'a HashMap<String, Vec<String>>,
    metadata: &'a GraphMetadata,
}

#[derive(Deserialize)]
struct OwnedGraphShape {
    graph_version: u32,
    #[serde(default)]
    tags: HashMap<String, TagEntry>,
    #[serde(default)]
    clusters: HashMap<String, ClusterEntry>,
    #[serde(default)]
    edges: HashMap<String, Vec<Edge>>,
    #[serde(default)]
    reverse_edges: HashMap<String, Vec<String>>,
    #[serde(default)]
    metadata: GraphMetadata,
}

static CONNECTIONS: OnceLock<Mutex<HashMap<PathBuf, Connection>>> = OnceLock::new();

/// Run `f` on this process's connection to `path`, opening it on first use.
fn with_db<R>(path: &Path, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
    let mut map = CONNECTIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("memory store lock poisoned"))?;
    if !map.contains_key(path) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::migrate::private(path);
        let mut db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        migrate(&mut db)?;
        map.insert(path.to_path_buf(), db);
    }
    f(map.get_mut(path).expect("inserted above"))
}

/// Gateway startup uses the same versioned schema migration as memory.
pub fn migrate_sovereign_db(db: &mut Connection) -> Result<()> {
    migrate(db)
}

/// Version and upgrade the file through `crate::migrate` (the one owner of `user_version`:
/// backup before migrating a file with data, refusal of a newer file), then ensure this store's schema.
fn migrate(db: &mut Connection) -> Result<()> {
    db.execute_batch("PRAGMA busy_timeout=5000")?;
    let path = db.path().filter(|p| !p.is_empty()).map(PathBuf::from);
    let had_data: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table')", [], |r| r.get(0))?;
    crate::migrate::run(db, path.as_deref().filter(|_| had_data))?;
    db.execute_batch(PRAGMAS)?;
    db.execute_batch(TABLES)?;
    Ok(())
}

/// Entry JSON without the embedding, plus the embedding as LE f32 bytes.
fn split_embedding(entry: &MemoryEntry) -> Result<(String, Option<Vec<u8>>)> {
    let mut lean = entry.clone();
    let embedding = lean.embedding.take().map(|v| v.iter().flat_map(|f| f.to_le_bytes()).collect());
    Ok((serde_json::to_string(&lean)?, embedding))
}

fn join_embedding(entry: &str, embedding: Option<Vec<u8>>) -> Result<MemoryEntry> {
    let mut entry: MemoryEntry = serde_json::from_str(entry)?;
    entry.embedding = embedding.map(|b| b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect());
    Ok(entry)
}

/// Drop the cached connection (before the file is deleted, e.g. in tests).
pub(crate) fn close(path: &Path) {
    if let Some(map) = CONNECTIONS.get()
        && let Ok(mut map) = map.lock()
    {
        map.remove(path);
    }
}

/// Changes whenever another connection commits; our own writes keep it.
pub(crate) fn data_version(path: &Path) -> Result<i64> {
    with_db(path, |db| Ok(db.query_row("PRAGMA data_version", [], |r| r.get(0))?))
}

/// Every scope with a stored graph (`global`, `project:<hash>`).
pub(crate) fn scopes(path: &Path) -> Result<Vec<String>> {
    with_db(path, |db| {
        let mut stmt = db.prepare("SELECT scope FROM memory_graphs ORDER BY scope")?;
        Ok(stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    })
}

/// The stored graph for `scope`, or `None` if nothing was ever saved there.
pub(crate) fn load_graph(path: &Path, scope: &str) -> Result<Option<MemoryGraph>> {
    with_db(path, |db| {
        let shape: Option<String> = db
            .query_row("SELECT graph FROM memory_graphs WHERE scope=?1", [scope], |r| r.get(0))
            .optional()?;
        let Some(shape) = shape else {
            return Ok(None);
        };
        let shape: OwnedGraphShape = serde_json::from_str(&shape)?;
        let mut graph = MemoryGraph::new();
        graph.graph_version = shape.graph_version;
        graph.tags = shape.tags;
        graph.clusters = shape.clusters;
        graph.edges = shape.edges;
        graph.reverse_edges = shape.reverse_edges;
        graph.metadata = shape.metadata;
        let mut stmt = db.prepare_cached(
            "SELECT e.entry, e.embedding FROM memories m JOIN memory_entries e ON e.rid = m.rid WHERE m.scope=?1",
        )?;
        for row in stmt.query_map([scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)))? {
            let (entry, embedding) = row?;
            let entry = join_embedding(&entry, embedding)?;
            graph.memories.insert(entry.id.clone(), entry);
        }
        Ok(Some(graph))
    })
}

/// Persist `graph` for `scope` in one transaction. With `previous` (the graph
/// as last loaded or saved), only memories that changed are written.
pub(crate) fn save_graph(path: &Path, scope: &str, graph: &MemoryGraph, previous: Option<&MemoryGraph>) -> Result<()> {
    with_db(path, |db| {
        let tx = db.transaction()?;
        {
            let mut upsert = tx.prepare_cached(
                "INSERT INTO memories(id, scope, active, content, tags) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(scope, id) DO UPDATE SET active=excluded.active, content=excluded.content, tags=excluded.tags
                 RETURNING rid",
            )?;
            let mut put_entry =
                tx.prepare_cached("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)")?;
            for (id, entry) in &graph.memories {
                if previous.and_then(|p| p.memories.get(id)) == Some(entry) {
                    continue;
                }
                let rid: i64 = upsert.query_row(
                    params![id, scope, entry.active, entry.content, entry.tags.join(" ")],
                    |r| r.get(0),
                )?;
                let (json, embedding) = split_embedding(entry)?;
                put_entry.execute(params![rid, json, embedding])?;
            }
            let mut delete = tx.prepare_cached("DELETE FROM memories WHERE scope=?1 AND id=?2")?;
            match previous {
                Some(previous) => {
                    for id in previous.memories.keys().filter(|id| !graph.memories.contains_key(*id)) {
                        delete.execute(params![scope, id])?;
                    }
                }
                None => {
                    let stored: Vec<String> = tx
                        .prepare_cached("SELECT id FROM memories WHERE scope=?1")?
                        .query_map([scope], |r| r.get(0))?
                        .collect::<rusqlite::Result<_>>()?;
                    for id in stored.iter().filter(|id| !graph.memories.contains_key(*id)) {
                        delete.execute(params![scope, id])?;
                    }
                }
            }
            let shape = GraphShape {
                graph_version: graph.graph_version,
                tags: &graph.tags,
                clusters: &graph.clusters,
                edges: &graph.edges,
                reverse_edges: &graph.reverse_edges,
                metadata: &graph.metadata,
            };
            tx.execute(
                "INSERT INTO memory_graphs(scope, graph) VALUES (?1, ?2) ON CONFLICT(scope) DO UPDATE SET graph=excluded.graph",
                params![scope, serde_json::to_string(&shape)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

/// What `remember` did with an entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Remembered {
    /// A new row was written.
    Inserted(String),
    /// An identical active memory was reinforced.
    Reinforced(String),
    /// A near-duplicate (same category, same meaning) was folded into an existing memory.
    Merged { id: String, similarity: f32 },
}

impl Remembered {
    pub fn id(&self) -> &str {
        match self {
            Self::Inserted(id) | Self::Reinforced(id) | Self::Merged { id, .. } => id,
        }
    }
}

const MERGE_THRESHOLD: f32 = 0.80;
const MERGE_CANDIDATES: usize = 8;
const POLARITY: &[&str] =
    &["not", "no", "never", "without", "cannot", "don", "doesn", "isn", "won", "aren", "avoid", "instead"];
const FILLER: &[&str] = &[
    "the", "and", "for", "are", "but", "you", "your", "with", "this", "that", "from", "have", "has", "was", "were", "will",
    "would", "can", "could", "should", "what", "when", "where", "which", "who", "how", "why", "into", "about", "there",
    "their", "they", "them", "then", "than", "also", "just", "like", "please", "does", "did", "our", "its", "any", "all",
];

/// Meaning-bearing tokens of a memory's text: lowercase words, plurals folded, filler dropped.
/// Negations stay in, and so do numbers, so "prefers X" and "prefers not X", or "port 80" and
/// "port 8080", never look alike.
fn merge_tokens(text: &str) -> std::collections::BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !FILLER.contains(w) && (w.len() > 1 || w.chars().any(|c| c.is_ascii_digit())))
        .map(|w| if w.chars().any(|c| c.is_ascii_digit()) { w.to_owned() } else { singular(w) })
        .collect()
}

/// Words spelled like code (paths, snake_case, kebab-case, camelCase, dotted names), compared
/// verbatim and case-sensitively: `foo/bar` and `foo-bar` are different things.
fn code_words(text: &str) -> std::collections::BTreeSet<&str> {
    text.split_whitespace()
        .map(|w| w.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'']).trim_start_matches(['(', '"', '\'']))
        .filter(|w| w.chars().skip(1).any(|c| c.is_uppercase() || "/\\_-.:".contains(c)))
        .collect()
}

/// Similarity of two memory texts in 0..=1, or 0 when a negation, a number or a code spelling differs.
fn similarity(a: &str, b: &str) -> f32 {
    if code_words(a) != code_words(b) {
        return 0.0;
    }
    let (ta, tb) = (merge_tokens(a), merge_tokens(b));
    let has = |t: &std::collections::BTreeSet<String>, keep: &dyn Fn(&str) -> bool| -> Vec<String> {
        t.iter().filter(|w| keep(w)).cloned().collect()
    };
    let polarity = |w: &str| POLARITY.contains(&w);
    let numeric = |w: &str| w.chars().any(|c| c.is_ascii_digit());
    if has(&ta, &polarity) != has(&tb, &polarity) || has(&ta, &numeric) != has(&tb, &numeric) {
        return 0.0;
    }
    let union = ta.union(&tb).count();
    if union == 0 {
        return 0.0;
    }
    ta.intersection(&tb).count() as f32 / union as f32
}

fn trust_rank(t: &TrustLevel) -> u8 {
    match t {
        TrustLevel::High => 2,
        TrustLevel::Medium => 1,
        TrustLevel::Low => 0,
    }
}

/// Add `entry` to `scope`, folding it into an existing memory when it says the same thing:
/// an identical active one is reinforced, a near-duplicate of the same category (see
/// `similarity`) is merged into its best match. Writes only the rows involved, in one immediate
/// transaction, so a concurrent writer's rows are never rewritten or deleted. When a merge
/// replaces the survivor's wording with a longer one, the old wording is kept as an inactive row
/// superseded by the survivor, so a wrong merge can be undone.
pub(crate) fn remember(path: &Path, scope: &str, entry: MemoryEntry) -> Result<Remembered> {
    let (category, trust, source) = (entry.category.to_string(), format!("{:?}", entry.trust).to_lowercase(), entry.source.clone());
    let outcome = remember_row(path, scope, entry)?;
    let (action, similarity) = match &outcome {
        Remembered::Inserted(_) => ("inserted", None),
        Remembered::Reinforced(_) => ("reinforced", None),
        Remembered::Merged { similarity, .. } => ("merged", Some(*similarity)),
    };
    let mut span = crate::obs_sink::Span::new("memory.write")
        .attr("action", action)
        .attr("id", outcome.id())
        .attr("scope", scope)
        .attr("category", category)
        .attr("trust", trust);
    if let Some(similarity) = similarity {
        span = span.attr("similarity", similarity);
    }
    if let Some(source) = source {
        span = span.attr("source", source);
    }
    crate::obs_sink::emit(span);
    Ok(outcome)
}

fn remember_row(path: &Path, scope: &str, entry: MemoryEntry) -> Result<Remembered> {
    with_db(path, |db| {
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if entry.category.is_learned() {
            let outcome = remember_learned(&tx, scope, entry)?;
            tx.commit()?;
            return Ok(outcome);
        }
        let wanted = entry.content.trim().to_string();
        let mut dup = None;
        {
            let mut stmt = tx.prepare_cached(
                "SELECT m.rid, e.entry, e.embedding FROM memories m JOIN memory_entries e ON e.rid = m.rid
                 WHERE m.scope=?1 AND m.active=1 AND trim(m.content)=?2",
            )?;
            for row in stmt.query_map(params![scope, wanted], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<Vec<u8>>>(2)?)))? {
                let (rid, json, embedding) = row?;
                let existing = join_embedding(&json, embedding)?;
                if existing.category == entry.category {
                    dup = Some((rid, existing));
                    break;
                }
            }
        }
        let outcome = if let Some((rid, mut existing)) = dup {
            existing.reinforce(entry.source.as_deref().unwrap_or("dedup"), 0);
            let (json, embedding) = split_embedding(&existing)?;
            tx.execute("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)", params![rid, json, embedding])?;
            Remembered::Reinforced(existing.id)
        } else if let Some((rid, mut survivor, score)) = best_match(&tx, scope, &entry)? {
            survivor.reinforce(entry.source.as_deref().unwrap_or("dedup"), 0);
            if trust_rank(&entry.trust) > trust_rank(&survivor.trust) {
                survivor.trust = entry.trust.clone();
            }
            if wanted.len() > survivor.content.trim().len() {
                // Keep the old wording, inactive, so the merge can be reversed.
                let mut old = survivor.clone();
                old.id = format!("{}~was{}", survivor.id, survivor.reinforcements.len());
                old.tags.clear();
                old.reinforcements.clear();
                old.supersede(&survivor.id);
                let old_rid: i64 = tx.query_row(
                    "INSERT INTO memories(id, scope, active, content, tags) VALUES (?1, ?2, 0, ?3, '')
                     ON CONFLICT(scope, id) DO UPDATE SET content=excluded.content RETURNING rid",
                    params![old.id, scope, old.content],
                    |r| r.get(0),
                )?;
                let (json, embedding) = split_embedding(&old)?;
                tx.execute("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)", params![old_rid, json, embedding])?;
                survivor.content = wanted.clone();
                survivor.set_embedding(None, None);
                survivor.refresh_search_text();
                tx.execute("UPDATE memories SET content=?1 WHERE rid=?2", params![survivor.content, rid])?;
            }
            let (json, embedding) = split_embedding(&survivor)?;
            tx.execute("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)", params![rid, json, embedding])?;
            Remembered::Merged { id: survivor.id, similarity: score }
        } else {
            // add_memory owns the tag nodes and edges; run it on the stored shape without the memories.
            let shape: Option<String> = tx.query_row("SELECT graph FROM memory_graphs WHERE scope=?1", [scope], |r| r.get(0)).optional()?;
            let mut graph = MemoryGraph::new();
            if let Some(shape) = shape {
                let shape: OwnedGraphShape = serde_json::from_str(&shape)?;
                graph.graph_version = shape.graph_version;
                graph.tags = shape.tags;
                graph.clusters = shape.clusters;
                graph.edges = shape.edges;
                graph.reverse_edges = shape.reverse_edges;
                graph.metadata = shape.metadata;
            }
            let id = graph.add_memory(entry);
            let entry = &graph.memories[&id];
            let rid: i64 = tx.query_row(
                "INSERT INTO memories(id, scope, active, content, tags) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(scope, id) DO UPDATE SET active=excluded.active, content=excluded.content, tags=excluded.tags
                 RETURNING rid",
                params![id, scope, entry.active, entry.content, entry.tags.join(" ")],
                |r| r.get(0),
            )?;
            let (json, embedding) = split_embedding(entry)?;
            tx.execute("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)", params![rid, json, embedding])?;
            let shape = GraphShape {
                graph_version: graph.graph_version,
                tags: &graph.tags,
                clusters: &graph.clusters,
                edges: &graph.edges,
                reverse_edges: &graph.reverse_edges,
                metadata: &graph.metadata,
            };
            tx.execute(
                "INSERT INTO memory_graphs(scope, graph) VALUES (?1, ?2) ON CONFLICT(scope) DO UPDATE SET graph=excluded.graph",
                params![scope, serde_json::to_string(&shape)?],
            )?;
            Remembered::Inserted(id)
        };
        tx.commit()?;
        Ok(outcome)
    })
}

/// A learned memory (`prompt`, `skill`, `subagent`) is written by exact id only: no content
/// match, no similarity merge, so two alike notes stay two rows and a rollback restores exactly
/// what was there. A new id is a new row (with its tag nodes); a known id replaces the row.
fn remember_learned(tx: &rusqlite::Transaction, scope: &str, mut entry: MemoryEntry) -> Result<Remembered> {
    let known: Option<i64> =
        tx.query_row("SELECT rid FROM memories WHERE scope=?1 AND id=?2", params![scope, entry.id], |r| r.get(0)).optional()?;
    if let Some(rid) = known {
        entry.refresh_search_text();
        tx.execute(
            "UPDATE memories SET active=?1, content=?2, tags=?3 WHERE rid=?4",
            params![entry.active, entry.content, entry.tags.join(" "), rid],
        )?;
        let (json, embedding) = split_embedding(&entry)?;
        tx.execute("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)", params![rid, json, embedding])?;
        return Ok(Remembered::Reinforced(entry.id));
    }
    let mut graph = stored_shape(tx, scope)?;
    let id = graph.add_memory(entry);
    let entry = &graph.memories[&id];
    let rid: i64 = tx.query_row(
        "INSERT INTO memories(id, scope, active, content, tags) VALUES (?1, ?2, ?3, ?4, ?5) RETURNING rid",
        params![id, scope, entry.active, entry.content, entry.tags.join(" ")],
        |r| r.get(0),
    )?;
    let (json, embedding) = split_embedding(entry)?;
    tx.execute("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)", params![rid, json, embedding])?;
    save_shape(tx, scope, &graph)?;
    Ok(Remembered::Inserted(id))
}

/// The stored graph of `scope` without its memories (an empty graph if none was saved).
fn stored_shape(tx: &Connection, scope: &str) -> Result<MemoryGraph> {
    let shape: Option<String> = tx.query_row("SELECT graph FROM memory_graphs WHERE scope=?1", [scope], |r| r.get(0)).optional()?;
    let mut graph = MemoryGraph::new();
    if let Some(shape) = shape {
        let shape: OwnedGraphShape = serde_json::from_str(&shape)?;
        graph.graph_version = shape.graph_version;
        graph.tags = shape.tags;
        graph.clusters = shape.clusters;
        graph.edges = shape.edges;
        graph.reverse_edges = shape.reverse_edges;
        graph.metadata = shape.metadata;
    }
    Ok(graph)
}

fn save_shape(tx: &Connection, scope: &str, graph: &MemoryGraph) -> Result<()> {
    let shape = GraphShape {
        graph_version: graph.graph_version,
        tags: &graph.tags,
        clusters: &graph.clusters,
        edges: &graph.edges,
        reverse_edges: &graph.reverse_edges,
        metadata: &graph.metadata,
    };
    tx.execute(
        "INSERT INTO memory_graphs(scope, graph) VALUES (?1, ?2) ON CONFLICT(scope) DO UPDATE SET graph=excluded.graph",
        params![scope, serde_json::to_string(&shape)?],
    )?;
    Ok(())
}

/// Make sure `scope` has a graph row (an empty one), so `every_scope_graph` lists rows written
/// straight into `memories` by a migration.
pub(crate) fn ensure_scope(conn: &Connection, scope: &str) -> Result<()> {
    let empty = MemoryGraph::new();
    let shape = GraphShape {
        graph_version: empty.graph_version,
        tags: &empty.tags,
        clusters: &empty.clusters,
        edges: &empty.edges,
        reverse_edges: &empty.reverse_edges,
        metadata: &empty.metadata,
    };
    conn.execute(
        "INSERT OR IGNORE INTO memory_graphs(scope, graph) VALUES (?1, ?2)",
        params![scope, serde_json::to_string(&shape)?],
    )?;
    Ok(())
}

/// Active learned memories in `categories`, with their scope: all scopes when `scopes` is `None`.
pub(crate) fn list_learned(path: &Path, categories: &[&str], scopes: Option<&[String]>) -> Result<Vec<(String, MemoryEntry)>> {
    with_db(path, |db| {
        let mut stmt = db.prepare_cached(
            "SELECT m.scope, e.entry, e.embedding FROM memories m JOIN memory_entries e ON e.rid = m.rid
             WHERE m.active = 1 AND json_extract(e.entry, '$.category.custom') IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<Vec<u8>>>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (scope, json, embedding) = row?;
            if scopes.is_some_and(|s| !s.contains(&scope)) {
                continue;
            }
            let entry = join_embedding(&json, embedding)?;
            if entry.category.is_learned() && categories.contains(&entry.category.to_string().as_str()) {
                out.push((scope, entry));
            }
        }
        Ok(out)
    })
}

/// The learned memory with this id, wherever it lives.
pub(crate) fn get_learned(path: &Path, id: &str) -> Result<Option<(String, MemoryEntry)>> {
    with_db(path, |db| {
        let mut stmt = db.prepare_cached(
            "SELECT m.scope, e.entry, e.embedding FROM memories m JOIN memory_entries e ON e.rid = m.rid WHERE m.id = ?1 AND m.active = 1",
        )?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<Vec<u8>>>(2)?)))?;
        for row in rows {
            let (scope, json, embedding) = row?;
            let entry = join_embedding(&json, embedding)?;
            if entry.category.is_learned() {
                return Ok(Some((scope, entry)));
            }
        }
        Ok(None)
    })
}

/// Remove the learned memory with this id (its row, FTS entry and entry JSON); returns what it was.
pub(crate) fn delete_learned(path: &Path, id: &str) -> Result<Option<(String, MemoryEntry)>> {
    let Some((scope, entry)) = get_learned(path, id)? else { return Ok(None) };
    with_db(path, |db| {
        db.execute("DELETE FROM memories WHERE scope=?1 AND id=?2", params![scope, id])?;
        Ok(())
    })?;
    Ok(Some((scope, entry)))
}

/// Remove every memory in `scope` (a deleted session's own rows) and its graph row.
pub(crate) fn drop_scope(path: &Path, scope: &str) -> Result<usize> {
    with_db(path, |db| {
        let n = db.execute("DELETE FROM memories WHERE scope=?1", [scope])?;
        db.execute("DELETE FROM memory_graphs WHERE scope=?1", [scope])?;
        Ok(n)
    })
}

/// The active memory in `scope` that `entry` most closely repeats (same category, similarity at or
/// above `MERGE_THRESHOLD`), found through the FTS index so only a handful of rows are compared.
fn best_match(tx: &rusqlite::Transaction, scope: &str, entry: &MemoryEntry) -> Result<Option<(i64, MemoryEntry, f32)>> {
    let mut terms = local_terms(&entry.content);
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return Ok(None);
    }
    let query = terms.iter().map(|t| format!("\"{}\"", t.replace('"', ""))).collect::<Vec<_>>().join(" OR ");
    let mut stmt = tx.prepare_cached(
        "SELECT top.rid, e.entry, e.embedding, m.content FROM (
             SELECT m.rid, bm25(memories_fts) AS score FROM memories_fts JOIN memories m ON m.rid = memories_fts.rowid
             WHERE memories_fts MATCH ?1 AND m.active = 1 AND m.scope = ?2 ORDER BY score LIMIT ?3
         ) top JOIN memories m ON m.rid = top.rid JOIN memory_entries e ON e.rid = top.rid ORDER BY top.score",
    )?;
    let mut best: Option<(i64, MemoryEntry, f32)> = None;
    let rows = stmt.query_map(params![query, scope, MERGE_CANDIDATES as i64], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<Vec<u8>>>(2)?, r.get::<_, String>(3)?))
    })?;
    for row in rows {
        let (rid, json, embedding, content) = row?;
        let score = similarity(&entry.content, &content);
        if score >= MERGE_THRESHOLD && best.as_ref().is_none_or(|(_, _, b)| score > *b) {
            let existing = join_embedding(&json, embedding)?;
            if existing.category == entry.category {
                best = Some((rid, existing, score));
            }
        }
    }
    Ok(best)
}

/// Active memories in `scopes` matching any of `terms` (already lowercased
/// word tokens), best BM25 first. Terms are quoted so no query syntax leaks in.
pub(crate) fn search(path: &Path, scopes: &[String], terms: &[String], limit: usize) -> Result<Vec<MemoryEntry>> {
    if scopes.is_empty() || terms.is_empty() {
        return Ok(Vec::new());
    }
    let query = terms.iter().map(|t| format!("\"{}\"", t.replace('"', ""))).collect::<Vec<_>>().join(" OR ");
    let scope_marks = (0..scopes.len()).map(|i| format!("?{}", i + 3)).collect::<Vec<_>>().join(",");
    // Rank and limit on the lean table, then fetch full entries for the winners.
    let sql = format!(
        "SELECT e.entry, e.embedding FROM (
             SELECT m.rid, bm25(memories_fts) AS score FROM memories_fts JOIN memories m ON m.rid = memories_fts.rowid
             WHERE memories_fts MATCH ?1 AND m.active = 1 AND m.scope IN ({scope_marks})
             ORDER BY score LIMIT ?2
         ) top JOIN memory_entries e ON e.rid = top.rid ORDER BY top.score"
    );
    with_db(path, |db| {
        let mut stmt = db.prepare_cached(&sql)?;
        let mut args: Vec<&dyn rusqlite::ToSql> = vec![&query, &limit];
        for scope in scopes {
            args.push(scope);
        }
        let rows = stmt.query_map(args.as_slice(), |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)))?;
        rows.map(|row| {
            let (entry, embedding) = row?;
            join_embedding(&entry, embedding)
        })
        .collect()
    })
}

/// Run `f` once per database, remembered under `key` in `memory_meta`.
pub(crate) fn once(path: &Path, key: &str, f: impl FnOnce() -> Result<usize>) -> Result<usize> {
    let done = with_db(path, |db| {
        Ok(db.query_row("SELECT 1 FROM memory_meta WHERE key=?1", [key], |_| Ok(())).optional()?.is_some())
    })?;
    if done {
        return Ok(0);
    }
    let n = f()?;
    with_db(path, |db| {
        db.execute("INSERT OR REPLACE INTO memory_meta(key, value) VALUES (?1, ?2)", params![key, n.to_string()])?;
        Ok(())
    })?;
    Ok(n)
}

pub(crate) fn meta_get(path: &Path, key: &str) -> Result<Option<String>> {
    with_db(path, |db| Ok(db.query_row("SELECT value FROM memory_meta WHERE key=?1", [key], |r| r.get(0)).optional()?))
}

pub(crate) fn meta_set(path: &Path, key: &str, value: &str) -> Result<()> {
    with_db(path, |db| {
        db.execute("INSERT OR REPLACE INTO memory_meta(key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    })
}

/// One-time import of the old JSON graphs (`memory/global.json`,
/// `memory/projects/<hash>.json`); each file is renamed `*.json.imported`.
pub(crate) fn import_json_once(db_path: &Path, memory_dir: &Path, load: impl Fn(&Path) -> Result<MemoryGraph>) -> Result<usize> {
    once(db_path, "json_imported", || {
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        let global = memory_dir.join("global.json");
        if global.is_file() {
            files.push((global, "global".into()));
        }
        if let Ok(dir) = std::fs::read_dir(memory_dir.join("projects")) {
            for file in dir.flatten() {
                let path = file.path();
                if path.extension().is_some_and(|e| e == "json")
                    && let Some(hash) = path.file_stem().and_then(|s| s.to_str())
                {
                    files.push((path.clone(), format!("project:{hash}")));
                }
            }
        }
        let mut imported = 0;
        for (file, scope) in files {
            let graph = load(&file).with_context(|| format!("reading {}", file.display()))?;
            save_graph(db_path, &scope, &graph, None)?;
            imported += graph.memories.len();
            let _ = std::fs::rename(&file, file.with_extension("json.imported"));
        }
        Ok(imported)
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_recall::{local_terms, meets_term_floor};
    use crate::memory_types::MemoryCategory;

    fn db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        (dir, path)
    }

    fn graph(texts: &[&str]) -> MemoryGraph {
        let mut g = MemoryGraph::new();
        for t in texts {
            g.add_memory(MemoryEntry::new(MemoryCategory::Preference, *t));
        }
        g
    }

    /// Recall exactly as `MemoryManager::recall_local` does it.
    fn recall(path: &Path, scopes: &[&str], query: &str, limit: usize) -> Vec<MemoryEntry> {
        let mut terms = local_terms(query);
        terms.sort();
        terms.dedup();
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        search(path, &scopes, &terms, limit * 4)
            .unwrap()
            .into_iter()
            .filter(|e| meets_term_floor(&terms, e))
            .take(limit)
            .collect()
    }

    #[test]
    fn remember_writes_one_row_and_keeps_a_concurrent_writers_rows() {
        let (_d, path) = db();
        save_graph(&path, "global", &graph(&["first fact"]), None).unwrap();
        // Another handle (the memory agent, a second process) adds a row after we loaded.
        let stale = load_graph(&path, "global").unwrap().unwrap();
        let mut theirs = stale.clone();
        theirs.add_memory(MemoryEntry::new(MemoryCategory::Fact, "theirs"));
        save_graph(&path, "global", &theirs, None).unwrap();
        // Our write, made from the stale view, must not delete theirs.
        let id = remember(&path, "global", MemoryEntry::new(MemoryCategory::Fact, "ours")).unwrap().id().to_string();
        let again = remember(&path, "global", MemoryEntry::new(MemoryCategory::Fact, "ours")).unwrap().id().to_string();
        assert_eq!(id, again);
        let all = load_graph(&path, "global").unwrap().unwrap();
        let mut texts: Vec<_> = all.memories.values().map(|m| m.content.as_str()).collect();
        texts.sort();
        assert_eq!(texts, ["first fact", "ours", "theirs"]);
        assert_eq!(all.memories[&id].strength, 2);
    }

    #[test]
    fn round_trips_and_recalls_the_relevant_memory_only() {
        let (_d, path) = db();
        let g = graph(&[
            "The user prefers pnpm over npm for package installs",
            "Deploys go through the staging cluster first",
            "The user's preferred language for quick scripts is Nim",
        ]);
        save_graph(&path, "global", &g, None).unwrap();
        let back = load_graph(&path, "global").unwrap().unwrap();
        assert_eq!(back.memories, g.memories);

        let hits = recall(&path, &["global"], "Which programming language should you use when you write a quick script for me?", 5);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("Nim"));
        let hits = recall(&path, &["global"], "install the package dependencies with pnpm", 5);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("pnpm"));
        // Never pad: nothing clearly relevant, nothing returned.
        assert!(recall(&path, &["global"], "what is the weather in Lahore today", 5).is_empty());
        assert!(recall(&path, &["global"], "", 5).is_empty());
    }

    #[test]
    fn writes_only_changes_and_keeps_the_index_in_sync() {
        let (_d, path) = db();
        let mut g = graph(&["Staging deploys need a green build", "Reports are due on Friday"]);
        save_graph(&path, "global", &g, None).unwrap();
        let before = g.clone();
        let id = g.memories.values().find(|m| m.content.contains("Staging")).unwrap().id.clone();
        g.memories.get_mut(&id).unwrap().content = "Production deploys need two approvals".into();
        let gone = g.memories.values().find(|m| m.content.contains("Friday")).unwrap().id.clone();
        g.memories.remove(&gone);
        save_graph(&path, "global", &g, Some(&before)).unwrap();

        assert!(recall(&path, &["global"], "staging green build", 5).is_empty(), "old text left the index");
        assert!(recall(&path, &["global"], "reports due friday", 5).is_empty(), "deleted memory left the index");
        let hits = recall(&path, &["global"], "production deploys approvals", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(load_graph(&path, "global").unwrap().unwrap().memories.len(), 1);
    }

    #[test]
    fn scopes_are_isolated_and_inactive_memories_are_not_recalled() {
        let (_d, path) = db();
        save_graph(&path, "project:a", &graph(&["Project A builds with cargo make"]), None).unwrap();
        let mut b = graph(&["Project B builds with cargo make too"]);
        b.memories.values_mut().for_each(|m| m.active = false);
        save_graph(&path, "project:b", &b, None).unwrap();
        assert_eq!(recall(&path, &["global", "project:a"], "how does it build with cargo make", 5).len(), 1);
        assert!(recall(&path, &["project:b"], "how does it build with cargo make", 5).is_empty());
        assert!(load_graph(&path, "project:c").unwrap().is_none());
    }

    #[test]
    fn imports_json_graphs_once_and_keeps_a_backup() {
        let (dir, path) = db();
        let memory_dir = dir.path().join("memory");
        std::fs::create_dir_all(memory_dir.join("projects")).unwrap();
        let g = graph(&["Imported preference: tabs over spaces"]);
        std::fs::write(memory_dir.join("global.json"), serde_json::to_vec(&g).unwrap()).unwrap();
        std::fs::write(memory_dir.join("projects/abc.json"), serde_json::to_vec(&graph(&["Project note"])).unwrap()).unwrap();
        let load = |p: &Path| Ok(serde_json::from_slice::<MemoryGraph>(&std::fs::read(p)?)?);
        assert_eq!(import_json_once(&path, &memory_dir, load).unwrap(), 2);
        assert_eq!(import_json_once(&path, &memory_dir, load).unwrap(), 0, "runs once");
        assert!(memory_dir.join("global.json.imported").is_file());
        assert_eq!(load_graph(&path, "global").unwrap().unwrap().memories, g.memories);
        assert_eq!(load_graph(&path, "project:abc").unwrap().unwrap().memories.len(), 1);
    }

    #[test]
    fn migrates_a_v1_database_with_inline_entries() {
        let (_d, path) = db();
        let mut entry = MemoryEntry::new(MemoryCategory::Preference, "The user prefers tabs over spaces");
        entry.embedding = Some(vec![0.25; 4]);
        {
            // The v1 layout: entry JSON (embedding included) inline in `memories`.
            let v1 = Connection::open(&path).unwrap();
            v1.execute_batch(
                "CREATE TABLE memories(rid INTEGER PRIMARY KEY, id TEXT NOT NULL, scope TEXT NOT NULL, active INTEGER NOT NULL,
                     content TEXT NOT NULL, tags TEXT NOT NULL, entry TEXT NOT NULL, UNIQUE(scope, id));
                 CREATE VIRTUAL TABLE memories_fts USING fts5(content, tags, content='memories', content_rowid='rid', tokenize='porter unicode61');
                 CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
                     INSERT INTO memories_fts(rowid, content, tags) VALUES (new.rid, new.content, new.tags); END;
                 CREATE TABLE memory_graphs(scope TEXT PRIMARY KEY, graph TEXT NOT NULL);",
            )
            .unwrap();
            v1.execute(
                "INSERT INTO memories(id, scope, active, content, tags, entry) VALUES (?1, 'global', 1, ?2, '', ?3)",
                (&entry.id, &entry.content, serde_json::to_string(&entry).unwrap()),
            )
            .unwrap();
            v1.execute("INSERT INTO memory_graphs(scope, graph) VALUES ('global', '{\"graph_version\":1}')", []).unwrap();
        }
        let graph = load_graph(&path, "global").unwrap().unwrap();
        assert_eq!(graph.memories.get(&entry.id), Some(&entry), "entry and embedding survive the move");
        assert_eq!(recall(&path, &["global"], "tabs or spaces preference", 5).len(), 1);
        let columns: Vec<String> = Connection::open(&path)
            .unwrap()
            .prepare("SELECT name FROM pragma_table_info('memories')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(!columns.contains(&"entry".to_string()));
    }

    #[test]
    fn a_v4_memory_database_is_versioned_backed_up_and_a_newer_file_is_refused() {
        let (_d, path) = db();
        let entry = MemoryEntry::new(MemoryCategory::Fact, "Deploys go out on Fridays");
        let mut g = MemoryGraph::new();
        g.add_memory(entry.clone());
        save_graph(&path, "global", &g, None).unwrap();
        close(&path);
        {
            // What the previous release left behind: memory's own schema_version 4 in a file at user_version 3.
            let old = Connection::open(&path).unwrap();
            old.execute_batch("PRAGMA user_version = 3; INSERT OR REPLACE INTO memory_meta VALUES('schema_version','4');").unwrap();
        }
        assert_eq!(load_graph(&path, "global").unwrap().unwrap().memories.get(&entry.id), Some(&entry));
        assert_eq!(recall(&path, &["global"], "deploys friday", 5).len(), 1);
        let version: u32 = Connection::open(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, crate::migrate::CURRENT);
        let backup = path.with_file_name(format!("sovereign.db.pre-v{}.bak", crate::migrate::CURRENT));
        assert!(Connection::open(backup).unwrap().query_row("SELECT count(*) FROM memories", [], |r| r.get::<_, i64>(0)).unwrap() == 1);
        // A file written by a newer engine is refused, not opened.
        close(&path);
        Connection::open(&path).unwrap().execute_batch(&format!("PRAGMA user_version = {}", crate::migrate::CURRENT + 1)).unwrap();
        assert!(load_graph(&path, "global").unwrap_err().to_string().contains("newer than this engine"));
    }

    /// `cargo test -p jcode-base --release --lib memory_store::tests::scaling -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scaling() {
        const WORDS: &[&str] = &["deploy", "staging", "cluster", "pnpm", "rust", "python", "invoice", "billing",
            "report", "deadline", "review", "security", "login", "refactor", "database", "index", "cache", "script",
            "language", "prefers", "always", "never", "project", "team", "meeting", "friday", "export", "api"];
        let query = "Which language should you use when you write a quick script for the billing report?";
        for n in [100usize, 1_000, 10_000] {
            let (_d, path) = db();
            let mut seed = 42u64;
            let mut g = MemoryGraph::new();
            for i in 0..n {
                let text: Vec<&str> = (0..18)
                    .map(|_| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        WORDS[(seed >> 33) as usize % WORDS.len()]
                    })
                    .collect();
                let mut e = MemoryEntry::new(MemoryCategory::Fact, format!("memory {i}: {}", text.join(" ")));
                e.embedding = Some(vec![0.1; 384]);
                g.add_memory(e);
            }
            save_graph(&path, "global", &g, None).unwrap();
            let t = std::time::Instant::now();
            let hits = recall(&path, &["global"], query, 5);
            let recall_ms = t.elapsed().as_secs_f64() * 1e3;
            let before = g.clone();
            g.add_memory(MemoryEntry::new(MemoryCategory::Fact, "one new memory"));
            let t = std::time::Instant::now();
            save_graph(&path, "global", &g, Some(&before)).unwrap();
            let save_ms = t.elapsed().as_secs_f64() * 1e3;
            eprintln!("n={n:>6}: recall {recall_ms:6.2} ms/turn ({} hits), save one new memory {save_ms:6.2} ms", hits.len());
        }
    }

    fn pref(text: &str) -> MemoryEntry {
        MemoryEntry::new(MemoryCategory::Preference, text)
    }

    fn active_count(path: &Path, scope: &str) -> usize {
        load_graph(path, scope).unwrap().map(|g| g.active_memories().count()).unwrap_or(0)
    }

    fn learned(kind: &str, id: &str, text: &str) -> MemoryEntry {
        let mut e = MemoryEntry::new(MemoryCategory::Custom(kind.to_string()), text);
        e.id = id.to_string();
        e
    }

    #[test]
    fn learned_memories_are_keyed_by_id_and_never_merged() {
        let (_d, path) = db();
        // Identical and near-identical bodies stay separate rows, unlike any other category.
        for (i, text) in ["Always run the linter before committing code", "Always run the linter before committing code", "Always run the linter before committing code."].iter().enumerate() {
            let out = remember(&path, "global", learned("prompt", &format!("p{i}"), text)).unwrap();
            assert_eq!(out, Remembered::Inserted(format!("p{i}")));
        }
        assert_eq!(list_learned(&path, &["prompt"], None).unwrap().len(), 3);
        // The same id replaces the row (an update), and only that row.
        let out = remember(&path, "global", learned("prompt", "p1", "Run tests before the linter")).unwrap();
        assert_eq!(out, Remembered::Reinforced("p1".into()));
        let rows = list_learned(&path, &["prompt"], None).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|(_, e)| e.id == "p1" && e.content == "Run tests before the linter"));
        // The full-text index followed the update.
        let hits = search(&path, &["global".into()], &["tests".into()], 5).unwrap();
        assert_eq!(hits.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["p1"]);
        // Other kinds are not returned for a prompt query, a plain memory never is.
        remember(&path, "global", pref("The user prefers tabs")).unwrap();
        remember(&path, "project:x", learned("skill", "k1", "Ship it")).unwrap();
        assert_eq!(list_learned(&path, &["skill"], None).unwrap().len(), 1);
        assert_eq!(list_learned(&path, &["prompt", "skill", "subagent"], Some(&["global".to_string()])).unwrap().len(), 3);
        assert_eq!(get_learned(&path, "k1").unwrap().unwrap().0, "project:x");
        assert!(get_learned(&path, "nope").unwrap().is_none());
        // Delete removes the row and its index entry.
        assert!(delete_learned(&path, "p0").unwrap().is_some());
        assert_eq!(list_learned(&path, &["prompt"], None).unwrap().len(), 2);
        assert_eq!(drop_scope(&path, "project:x").unwrap(), 1);
        assert!(get_learned(&path, "k1").unwrap().is_none());
    }

    #[test]
    fn recall_leaves_prompt_notes_out_and_offers_the_other_learned_kinds() {
        let (_d, path) = db();
        remember(&path, "global", learned("prompt", "p", "Always run the release checklist before shipping")).unwrap();
        remember(&path, "global", learned("subagent", "a", "Release checker: run the release checklist before shipping")).unwrap();
        let hits = search(&path, &["global".into()], &["release".into(), "checklist".into()], 5).unwrap();
        let shown: Vec<&str> = hits.iter().filter(|e| !crate::memory::learned::is_kept_out_of_recall(e)).map(|e| e.id.as_str()).collect();
        assert_eq!(shown, ["a"], "the prompt note is in the cached prompt already; the subagent spec is recalled");
        let listed = {
            let mut e = learned("skill", "s", "Release checklist steps");
            e.learned = Some(crate::memory_types::LearnedMeta { listed: true, ..Default::default() });
            e
        };
        assert!(crate::memory::learned::is_kept_out_of_recall(&listed), "a skill the skill list offers is not recalled too");
    }

    #[test]
    fn a_paraphrase_merges_into_the_existing_memory() {
        let (_d, path) = db();
        let first = remember(&path, "global", pref("The user prefers tabs over spaces in Rust code")).unwrap();
        let second = remember(&path, "global", pref("User prefers tabs over spaces in Rust code.")).unwrap();
        assert!(matches!(first, Remembered::Inserted(_)));
        assert!(matches!(second, Remembered::Merged { .. }), "{second:?}");
        assert_eq!(second.id(), first.id());
        assert_eq!(active_count(&path, "global"), 1);
    }

    #[test]
    fn opposite_or_different_numbers_do_not_merge() {
        let (_d, path) = db();
        remember(&path, "global", pref("prefers to use semicolons in JavaScript files")).unwrap();
        let negated = remember(&path, "global", pref("prefers not to use semicolons in JavaScript files")).unwrap();
        assert!(matches!(negated, Remembered::Inserted(_)), "{negated:?}");
        remember(&path, "global", pref("dev server runs on port 8080 for this project")).unwrap();
        let other = remember(&path, "global", pref("dev server runs on port 3000 for this project")).unwrap();
        assert!(matches!(other, Remembered::Inserted(_)), "{other:?}");
        assert_eq!(active_count(&path, "global"), 4);
    }

    #[test]
    fn different_categories_and_scopes_stay_separate() {
        let (_d, path) = db();
        remember(&path, "global", pref("always run the linter before committing changes")).unwrap();
        let fact = remember(&path, "global", MemoryEntry::new(MemoryCategory::Fact, "always run the linter before committing changes")).unwrap();
        assert!(matches!(fact, Remembered::Inserted(_)));
        let other = remember(&path, "project:x", pref("always run the linter before committing changes")).unwrap();
        assert!(matches!(other, Remembered::Inserted(_)));
    }

    #[test]
    fn a_longer_wording_replaces_the_survivor_and_the_old_text_is_kept_inactive() {
        let (_d, path) = db();
        let first = remember(&path, "global", pref("prefers tabs over spaces in Rust code")).unwrap();
        let merged = remember(&path, "global", pref("prefers tabs over spaces in Rust code always")).unwrap();
        assert!(matches!(merged, Remembered::Merged { .. }), "{merged:?}");
        let graph = load_graph(&path, "global").unwrap().unwrap();
        let survivor = &graph.memories[first.id()];
        assert_eq!(survivor.content, "prefers tabs over spaces in Rust code always");
        assert_eq!(survivor.strength, 2);
        let old = graph.memories.values().find(|m| !m.active).expect("old wording kept");
        assert_eq!(old.content, "prefers tabs over spaces in Rust code");
        assert_eq!(old.superseded_by.as_deref(), Some(first.id()));
        assert_eq!(graph.active_memories().count(), 1);
    }

    #[test]
    fn a_merge_keeps_the_higher_trust() {
        let (_d, path) = db();
        let mut low = pref("prefers dark mode in every editor");
        low.trust = TrustLevel::Low;
        let first = remember(&path, "global", low).unwrap();
        let mut high = pref("prefers dark mode in every editor.");
        high.trust = TrustLevel::High;
        remember(&path, "global", high).unwrap();
        let graph = load_graph(&path, "global").unwrap().unwrap();
        assert_eq!(graph.memories[first.id()].trust, TrustLevel::High);
    }
}
