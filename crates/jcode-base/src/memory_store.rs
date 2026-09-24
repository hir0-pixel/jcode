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
use crate::memory_types::MemoryEntry;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SCHEMA: &str = "
    PRAGMA journal_mode=WAL;
    PRAGMA synchronous=NORMAL;
    PRAGMA busy_timeout=5000;
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
    CREATE TABLE IF NOT EXISTS obs_runs(
        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, parent_id TEXT, root_id TEXT NOT NULL,
        kind TEXT NOT NULL, title TEXT, model TEXT NOT NULL, provider TEXT NOT NULL,
        status TEXT NOT NULL, started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER,
        input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        cost_usd REAL, error TEXT, unpriced_calls INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS obs_runs_recent ON obs_runs(started_at_ms DESC);
    CREATE INDEX IF NOT EXISTS obs_runs_session ON obs_runs(session_id, started_at_ms DESC);
    CREATE TABLE IF NOT EXISTS obs_spans(
        id TEXT PRIMARY KEY, run_id TEXT NOT NULL, parent_id TEXT NOT NULL, root_id TEXT NOT NULL,
        kind TEXT NOT NULL, name TEXT NOT NULL, status TEXT NOT NULL,
        started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER,
        input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        cost_usd REAL, error TEXT, model TEXT, provider TEXT,
        attributes TEXT NOT NULL DEFAULT '{}'
    );
    CREATE INDEX IF NOT EXISTS obs_spans_run ON obs_spans(run_id, started_at_ms);
    CREATE TABLE IF NOT EXISTS obs_content(id TEXT PRIMARY KEY, input TEXT, output TEXT);
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
        let mut db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        migrate(&mut db)?;
        map.insert(path.to_path_buf(), db);
    }
    f(map.get_mut(path).expect("inserted above"))
}

const SCHEMA_VERSION: i64 = 3;

/// Gateway startup uses the same versioned schema migration as memory.
pub fn migrate_sovereign_db(db: &mut Connection) -> Result<()> {
    migrate(db)
}

/// Bring an existing database to `SCHEMA_VERSION`, then ensure the schema.
/// v1 kept the entry JSON (with the embedding as JSON numbers) inline in
/// `memories`; v2 moves it to `memory_entries`.
fn migrate(db: &mut Connection) -> Result<()> {
    let has_inline_entry = db
        .prepare("SELECT 1 FROM pragma_table_info('memories') WHERE name='entry'")?
        .exists([])?;
    if has_inline_entry {
        let tx = db.transaction()?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS memory_entries(rid INTEGER PRIMARY KEY, entry TEXT NOT NULL, embedding BLOB);")?;
        let rows: Vec<(i64, String)> = tx
            .prepare("SELECT rid, entry FROM memories")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (rid, entry) in rows {
            let entry: MemoryEntry = serde_json::from_str(&entry)?;
            let (entry, embedding) = split_embedding(&entry)?;
            tx.execute("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)", params![rid, entry, embedding])?;
        }
        tx.execute_batch("DROP TRIGGER IF EXISTS memories_ad; ALTER TABLE memories DROP COLUMN entry;")?;
        tx.commit()?;
    }
    db.execute_batch(SCHEMA)?;
    db.execute("INSERT OR REPLACE INTO memory_meta(key, value) VALUES ('schema_version', ?1)", [SCHEMA_VERSION.to_string()])?;
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
}
