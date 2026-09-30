//! Continual Harness entries (Prime Agent's refinement store): the learned `prompt`, `skill` and
//! `subagent` entries are rows of the one memory store (`memories`, category = the kind), written
//! through `jcode_base::memory::learned` so they share its write path, recall and injected-id record.
//! Only the rollback log (`harness_changesets`) and the learning bookkeeping live in tables of their own.
//!
//! Follows Prime Agent's `refinement.ts`. A `prompt` entry is a durable addendum rendered into the
//! cached system prompt; a `skill` entry describes a reusable SKILL.md-backed procedure; a `subagent`
//! entry is a reusable delegation spec (name, instructions, allowed tools, model hint). Every write is
//! grouped into a changeset so `/refine rollback` can restore the exact prior state, itself recorded as
//! a new (rollback) changeset.
//!
//! Reach: a learned entry is stored at `project:<hash>` when its session has a working directory (see
//! [`EntryStore::set_session_dir`]) and at `global` otherwise or when asked for. Rows migrated from the
//! old table that were local to one session keep that reach as scope `session:<id>`.

use anyhow::{Context, Result, bail};
use jcode_base::memory::learned;
use jcode_base::memory::{LearnedMeta, MemoryCategory, MemoryEntry};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_CONTENT_CHARS: usize = 4_000;
pub const MAX_PATH_CHARS: usize = 200;
/// Total budget for rendered prompt addenda (the cached static prefix).
pub const MAX_PROMPT_CHARS: usize = 6_000;

const SCHEMA: &str = "
    PRAGMA journal_mode=WAL;
    PRAGMA busy_timeout=5000;
    CREATE TABLE IF NOT EXISTS harness_changesets(
        id TEXT PRIMARY KEY,
        session TEXT,
        scope TEXT NOT NULL,
        summary TEXT NOT NULL,
        rationale TEXT NOT NULL,
        expected_outcome TEXT NOT NULL,
        ops TEXT NOT NULL,
        rollback_of TEXT,
        rolled_back INTEGER NOT NULL DEFAULT 0,
        source TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS harness_changesets_recent ON harness_changesets(session, created_at_ms DESC);
    CREATE TABLE IF NOT EXISTS harness_seq(name TEXT PRIMARY KEY, value INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS harness_watermark(session TEXT PRIMARY KEY, seen INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS engine_settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS harness_learn_state(
        session TEXT PRIMARY KEY,
        turns INTEGER NOT NULL,
        last_review_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS harness_pending_refine(
        session TEXT PRIMARY KEY,
        instructions TEXT,
        global INTEGER NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Prompt,
    Skill,
    Subagent,
}

impl EntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryKind::Prompt => "prompt",
            EntryKind::Skill => "skill",
            EntryKind::Subagent => "subagent",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "prompt" => Some(EntryKind::Prompt),
            "skill" => Some(EntryKind::Skill),
            "subagent" => Some(EntryKind::Subagent),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Local,
    Global,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Local => "local",
            Scope::Global => "global",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Scope::Local),
            "global" => Some(Scope::Global),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessEntry {
    pub id: String,
    pub kind: EntryKind,
    pub title: String,
    pub content: String,
    pub path: String,
    /// `Global` for the `global` memory scope, `Local` for a project or session scope.
    pub scope: Scope,
    /// The owning session of a `session:<id>` scope (migrated rows); `None` otherwise.
    pub session: Option<String>,
    /// The memory scope the row lives in: `global`, `project:<hash>` or `session:<id>`.
    pub memory_scope: String,
    pub reference: Value,
    pub arguments: Value,
    pub metadata: Value,
    pub source: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub version: i64,
    pub seq: i64,
}

#[derive(Debug, Clone)]
pub struct NewEntry {
    pub kind: EntryKind,
    pub title: String,
    pub content: String,
    pub path: String,
    pub scope: Scope,
    /// The session a `Local` entry is learned in; its working directory picks the project scope.
    pub session: Option<String>,
    pub reference: Value,
    pub arguments: Value,
    pub metadata: Value,
    pub source: String,
}

impl NewEntry {
    pub fn new(
        kind: EntryKind,
        scope: Scope,
        title: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            content: content.into(),
            path: String::new(),
            scope,
            session: None,
            reference: json!({}),
            arguments: json!({}),
            metadata: json!({}),
            source: "user".into(),
        }
    }
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct EntryPatch {
    pub title: Option<String>,
    pub content: Option<String>,
    pub path: Option<String>,
    pub reference: Option<Value>,
    pub arguments: Option<Value>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Create,
    Update,
    Delete,
}

impl Action {
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Update => "update",
            Action::Delete => "delete",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "create" => Some(Action::Create),
            "update" => Some(Action::Update),
            "delete" => Some(Action::Delete),
            _ => None,
        }
    }
}

/// One edit as recorded in a changeset: the action taken plus the full
/// before/after snapshots needed to invert it exactly.
#[derive(Debug, Clone)]
pub struct AppliedEdit {
    pub action: Action,
    pub id: String,
    pub before: Option<HarnessEntry>,
    pub after: Option<HarnessEntry>,
}

#[derive(Debug, Clone)]
pub struct Changeset {
    pub id: String,
    pub session: Option<String>,
    pub scope: Scope,
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub edits: Vec<AppliedEdit>,
    pub rollback_of: Option<String>,
    pub rolled_back: bool,
    pub source: String,
    pub created_at_ms: i64,
}

pub struct EntryStore {
    conn: Mutex<Connection>,
    /// `sovereign.db`, where the learned entries live as memories.
    pub(crate) db: PathBuf,
    /// A unit test's private home, removed with the store.
    #[cfg(test)]
    scratch: Option<PathBuf>,
}

#[cfg(test)]
impl Drop for EntryStore {
    fn drop(&mut self) {
        if let Some(home) = &self.scratch {
            learned::close(&self.db);
            let _ = std::fs::remove_dir_all(home);
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `(Scope, session)` of a memory scope string.
fn split_scope(memory_scope: &str) -> (Scope, Option<String>) {
    match memory_scope.strip_prefix("session:") {
        Some(session) => (Scope::Local, Some(session.to_string())),
        None if memory_scope == "global" => (Scope::Global, None),
        None => (Scope::Local, None),
    }
}

fn entry_to_json(e: &HarnessEntry) -> Value {
    json!({
        "id": e.id, "kind": e.kind.as_str(), "title": e.title, "content": e.content, "path": e.path,
        "scope": e.memory_scope, "reference": e.reference, "arguments": e.arguments,
        "metadata": e.metadata, "source": e.source, "created_at_ms": e.created_at_ms,
        "updated_at_ms": e.updated_at_ms, "version": e.version, "seq": e.seq,
    })
}

/// A snapshot from a changeset. Older ones say `"scope": "local"` plus a `session`; that reads as
/// the `session:<id>` scope the migration gave those rows. Snapshots of the retired `memory` kind
/// have nothing to restore and read as `None`.
fn entry_from_json(v: &Value) -> Option<HarnessEntry> {
    if v.is_null() {
        return None;
    }
    let memory_scope = match (v["scope"].as_str()?, v["session"].as_str()) {
        ("local", Some(session)) => format!("session:{session}"),
        ("local", None) => "global".to_string(),
        (scope, _) => scope.to_string(),
    };
    let (scope, session) = split_scope(&memory_scope);
    Some(HarnessEntry {
        id: v["id"].as_str()?.to_string(),
        kind: EntryKind::parse(v["kind"].as_str()?)?,
        title: v["title"].as_str().unwrap_or_default().to_string(),
        content: v["content"].as_str().unwrap_or_default().to_string(),
        path: v["path"].as_str().unwrap_or_default().to_string(),
        scope,
        session,
        memory_scope,
        reference: v["reference"].clone(),
        arguments: v["arguments"].clone(),
        metadata: v["metadata"].clone(),
        source: v["source"].as_str().unwrap_or_default().to_string(),
        created_at_ms: v["created_at_ms"].as_i64().unwrap_or(0),
        updated_at_ms: v["updated_at_ms"].as_i64().unwrap_or(0),
        version: v["version"].as_i64().unwrap_or(1),
        seq: v["seq"].as_i64().unwrap_or(0),
    })
}

fn applied_edit_to_json(e: &AppliedEdit) -> Value {
    json!({
        "action": e.action.as_str(), "id": e.id,
        "before": e.before.as_ref().map(entry_to_json).unwrap_or(Value::Null),
        "after": e.after.as_ref().map(entry_to_json).unwrap_or(Value::Null),
    })
}

fn applied_edit_from_json(v: &Value) -> Option<AppliedEdit> {
    Some(AppliedEdit {
        action: Action::parse(v["action"].as_str()?)?,
        id: v["id"].as_str()?.to_string(),
        before: entry_from_json(&v["before"]),
        after: entry_from_json(&v["after"]),
    })
}

/// The memory row of an entry: the body is the memory's content, the title is a tag (so recall
/// matches it), everything else rides in `learned`.
fn to_memory(e: &HarnessEntry) -> MemoryEntry {
    let mut m = MemoryEntry::new(MemoryCategory::Custom(e.kind.as_str().to_string()), e.content.clone());
    m.id = e.id.clone();
    m.tags = if e.title.trim().is_empty() { Vec::new() } else { vec![e.title.clone()] };
    m.source = Some(e.source.clone());
    m.created_at = chrono::DateTime::from_timestamp_millis(e.created_at_ms).unwrap_or_default();
    m.updated_at = chrono::DateTime::from_timestamp_millis(e.updated_at_ms).unwrap_or_default();
    m.learned = Some(LearnedMeta {
        title: e.title.clone(),
        path: e.path.clone(),
        reference: e.reference.clone(),
        arguments: e.arguments.clone(),
        metadata: e.metadata.clone(),
        version: e.version,
        seq: e.seq,
        // A skill with a generated SKILL.md is offered by the skill list already.
        listed: e.kind == EntryKind::Skill && crate::skill_files::skills_dir().is_some(),
    });
    m.refresh_search_text();
    m
}

/// The entry a memory row holds; `None` for any memory that is not a learned one.
fn from_memory(memory_scope: &str, m: &MemoryEntry) -> Option<HarnessEntry> {
    let kind = EntryKind::parse(&m.category.to_string()).filter(|_| m.category.is_learned())?;
    let meta = m.learned.clone().unwrap_or_default();
    let (scope, session) = split_scope(memory_scope);
    Some(HarnessEntry {
        id: m.id.clone(),
        kind,
        title: meta.title,
        content: m.content.clone(),
        path: meta.path,
        scope,
        session,
        memory_scope: memory_scope.to_string(),
        reference: meta.reference,
        arguments: meta.arguments,
        metadata: meta.metadata,
        source: m.source.clone().unwrap_or_default(),
        created_at_ms: m.created_at.timestamp_millis(),
        updated_at_ms: m.updated_at.timestamp_millis(),
        version: meta.version.max(1),
        seq: meta.seq,
    })
}

impl EntryStore {
    pub fn open(home: &Path) -> Result<Self> {
        std::fs::create_dir_all(home).ok();
        let db = home.join("sovereign.db");
        let conn = crate::migrate::open(&db, SCHEMA).context("opening sovereign.db")?;
        Ok(Self {
            conn: Mutex::new(conn),
            db,
            #[cfg(test)]
            scratch: None,
        })
    }

    /// One cached store per `home` per process, so callers (the gateway's
    /// per-RPC harness commands, the automatic learning pass) do not reopen
    /// `sovereign.db` on every call.
    pub fn open_cached(home: &Path) -> Result<Arc<Self>> {
        static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<EntryStore>>>> = OnceLock::new();
        let mut map = STORES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(store) = map.get(home) {
            return Ok(store.clone());
        }
        let store = Arc::new(Self::open(home)?);
        map.insert(home.to_path_buf(), store.clone());
        Ok(store)
    }

    /// A store on a fresh temporary home, removed when dropped (unit tests).
    #[cfg(test)]
    pub fn temp() -> Result<Self> {
        let home = std::env::temp_dir().join(format!("entries-{}", uuid::Uuid::new_v4()));
        let mut store = Self::open(&home)?;
        store.scratch = Some(home);
        Ok(store)
    }

    fn next_seq(conn: &Connection) -> Result<i64> {
        conn.execute(
            "INSERT INTO harness_seq(name, value) VALUES ('seq', 1) ON CONFLICT(name) DO UPDATE SET value = value + 1",
            [],
        )?;
        Ok(conn.query_row(
            "SELECT value FROM harness_seq WHERE name = 'seq'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Tell the store where `session` works, so what it learns lands in that project's scope.
    /// Written only when it changes.
    pub fn set_session_dir(&self, session: &str, dir: &str) -> Result<()> {
        let key = format!("session_dir:{session}");
        if self.setting(&key).as_deref() != Some(dir) {
            self.set_setting(&key, dir)?;
        }
        Ok(())
    }

    fn project_scope_of(&self, session: &str) -> Option<String> {
        self.setting(&format!("session_dir:{session}")).filter(|d| !d.trim().is_empty()).map(|d| learned::project_scope(&d))
    }

    /// Where a new entry goes: `global` when asked for, else the session's project, else `global`.
    fn scope_for_new(&self, e: &NewEntry) -> Result<String> {
        match e.scope {
            Scope::Global => Ok("global".to_string()),
            Scope::Local => {
                let session = e.session.as_deref().context("a local entry needs a session")?;
                Ok(self.project_scope_of(session).unwrap_or_else(|| "global".to_string()))
            }
        }
    }

    /// The memory scopes `session` reads learned entries from.
    fn visible_scopes(&self, session: &str) -> Vec<String> {
        let mut scopes = vec!["global".to_string(), format!("session:{session}")];
        scopes.extend(self.project_scope_of(session));
        scopes
    }

    fn entries_in(&self, kind: Option<EntryKind>, scopes: Option<&[String]>) -> Result<Vec<HarnessEntry>> {
        let all = ["prompt", "skill", "subagent"];
        let kinds: Vec<&str> = kind.map_or(all.to_vec(), |k| vec![k.as_str()]);
        let mut out: Vec<HarnessEntry> =
            learned::list(&self.db, &kinds, scopes)?.iter().filter_map(|(scope, m)| from_memory(scope, m)).collect();
        out.sort_by_key(|e| e.seq);
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<HarnessEntry>> {
        Ok(learned::get(&self.db, id)?.and_then(|(scope, m)| from_memory(&scope, &m)))
    }

    /// Entries visible to `session`: all globals plus that session's project and session-local
    /// entries, in deterministic (`seq`) order. On a `path` collision between a global
    /// and a local entry, the local one wins (Prime's local-override rule).
    pub fn list_visible(
        &self,
        session: &str,
        kind: Option<EntryKind>,
    ) -> Result<Vec<HarnessEntry>> {
        let rows = self.entries_in(kind, Some(&self.visible_scopes(session)))?;
        let mut by_path: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut out: Vec<HarnessEntry> = Vec::new();
        for entry in rows {
            let key = if entry.path.is_empty() {
                format!("__id:{}", entry.id)
            } else {
                entry.path.clone()
            };
            if let Some(&idx) = by_path.get(&key) {
                if entry.scope == Scope::Local {
                    out[idx] = entry;
                }
                continue;
            }
            by_path.insert(key, out.len());
            out.push(entry);
        }
        out.sort_by_key(|e| e.seq);
        Ok(out)
    }

    pub fn list_all(
        &self,
        scope: Option<Scope>,
        session: Option<&str>,
    ) -> Result<Vec<HarnessEntry>> {
        Ok(self
            .entries_in(None, None)?
            .into_iter()
            .filter(|e| scope.is_none_or(|s| e.scope == s) && session.is_none_or(|s| e.session.as_deref() == Some(s)))
            .collect())
    }

    /// The prompt notes rendered for `session` and their ids: the stable, cacheable addendum text.
    /// Capped at `MAX_PROMPT_CHARS` total: newest entries (highest `seq`) win the budget, then the
    /// kept ones are emitted oldest-first so the prefix is identical turn to turn.
    pub fn render_prompt_with_ids(&self, session: &str) -> Result<(String, Vec<String>)> {
        let entries = self.list_visible(session, Some(EntryKind::Prompt))?;
        let mut used = 0;
        let mut kept: Vec<&HarnessEntry> = entries
            .iter()
            .rev()
            .filter(|e| !e.content.trim().is_empty())
            .filter(|e| {
                let cost = e.content.trim().chars().count() + 2;
                (used + cost <= MAX_PROMPT_CHARS).then(|| used += cost).is_some()
            })
            .collect();
        kept.reverse();
        Ok((kept.iter().map(|e| e.content.trim()).collect::<Vec<_>>().join("\n\n"), kept.iter().map(|e| e.id.clone()).collect()))
    }

    pub fn render_prompt(&self, session: &str) -> Result<String> {
        Ok(self.render_prompt_with_ids(session)?.0)
    }

    /// Write `e` exactly as it is (a new id, or an exact snapshot restored on rollback).
    pub(crate) fn restore(&self, e: &HarnessEntry) -> Result<HarnessEntry> {
        learned::put(&self.db, &e.memory_scope, to_memory(e))?;
        Ok(e.clone())
    }

    pub fn create(&self, e: NewEntry) -> Result<HarnessEntry> {
        let memory_scope = self.scope_for_new(&e)?;
        let seq = Self::next_seq(&self.conn.lock().unwrap_or_else(|err| err.into_inner()))?;
        let at = now_ms();
        let (scope, session) = split_scope(&memory_scope);
        self.restore(&HarnessEntry {
            id: uuid::Uuid::new_v4().to_string(),
            kind: e.kind,
            title: e.title.chars().take(MAX_TITLE_CHARS).collect(),
            content: e.content.chars().take(MAX_CONTENT_CHARS).collect(),
            path: e.path.chars().take(MAX_PATH_CHARS).collect(),
            scope,
            session,
            memory_scope,
            reference: e.reference,
            arguments: e.arguments,
            metadata: e.metadata,
            source: e.source,
            created_at_ms: at,
            updated_at_ms: at,
            version: 1,
            seq,
        })
    }

    pub fn update(&self, id: &str, patch: EntryPatch) -> Result<HarnessEntry> {
        let existing = self.get(id)?.with_context(|| format!("no entry {id}"))?;
        self.restore(&HarnessEntry {
            title: patch.title.unwrap_or(existing.title),
            content: patch.content.unwrap_or(existing.content),
            path: patch.path.unwrap_or(existing.path),
            reference: patch.reference.unwrap_or(existing.reference),
            arguments: patch.arguments.unwrap_or(existing.arguments),
            metadata: patch.metadata.unwrap_or(existing.metadata),
            updated_at_ms: now_ms(),
            version: existing.version + 1,
            ..existing
        })
    }

    pub fn delete(&self, id: &str) -> Result<HarnessEntry> {
        learned::delete(&self.db, id)?
            .and_then(|(scope, m)| from_memory(&scope, &m))
            .with_context(|| format!("no entry {id}"))
    }

    /// Copy a local entry into the global store (Prime's promote-by-copy).
    pub fn promote(&self, id: &str) -> Result<HarnessEntry> {
        let existing = self.get(id)?.with_context(|| format!("no entry {id}"))?;
        if existing.scope == Scope::Global {
            bail!("entry {id} is already global");
        }
        self.create(NewEntry {
            kind: existing.kind,
            title: existing.title,
            content: existing.content,
            path: existing.path,
            scope: Scope::Global,
            session: None,
            reference: existing.reference,
            arguments: existing.arguments,
            metadata: existing.metadata,
            source: existing.source,
        })
    }

    /// Copy a global entry into a session's project scope (demote-by-copy).
    pub fn demote(&self, id: &str, session: &str) -> Result<HarnessEntry> {
        let existing = self.get(id)?.with_context(|| format!("no entry {id}"))?;
        if existing.scope == Scope::Local {
            bail!("entry {id} is already local");
        }
        if self.project_scope_of(session).is_none() {
            bail!("session {session} has no working directory to keep a local copy in");
        }
        self.create(NewEntry {
            kind: existing.kind,
            title: existing.title,
            content: existing.content,
            path: existing.path,
            scope: Scope::Local,
            session: Some(session.to_string()),
            reference: existing.reference,
            arguments: existing.arguments,
            metadata: existing.metadata,
            source: existing.source,
        })
    }

    /// Record a changeset (a group of already-applied edits) so it can be
    /// rolled back exactly. Returns the changeset id.
    pub fn record_changeset(
        &self,
        session: Option<&str>,
        scope: Scope,
        summary: &str,
        rationale: &str,
        expected_outcome: &str,
        edits: &[AppliedEdit],
        rollback_of: Option<&str>,
        source: &str,
    ) -> Result<String> {
        let conn = self.conn.lock().unwrap_or_else(|err| err.into_inner());
        let id = uuid::Uuid::new_v4().to_string();
        let ops = Value::Array(edits.iter().map(applied_edit_to_json).collect()).to_string();
        conn.execute(
            "INSERT INTO harness_changesets(id, session, scope, summary, rationale, expected_outcome, ops, rollback_of, rolled_back, source, created_at_ms) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10)",
            params![id, session, scope.as_str(), summary, rationale, expected_outcome, ops, rollback_of, source, now_ms()],
        )?;
        Ok(id)
    }

    fn row_to_changeset(row: &rusqlite::Row) -> rusqlite::Result<Changeset> {
        let ops: String = row.get("ops")?;
        let scope: String = row.get("scope")?;
        let edits: Vec<AppliedEdit> = serde_json::from_str::<Value>(&ops)
            .ok()
            .and_then(|v| {
                v.as_array()
                    .map(|a| a.iter().filter_map(applied_edit_from_json).collect())
            })
            .unwrap_or_default();
        Ok(Changeset {
            id: row.get("id")?,
            session: row.get("session")?,
            scope: Scope::parse(&scope).unwrap_or(Scope::Local),
            summary: row.get("summary")?,
            rationale: row.get("rationale")?,
            expected_outcome: row.get("expected_outcome")?,
            edits,
            rollback_of: row.get("rollback_of")?,
            rolled_back: row.get::<_, i64>("rolled_back")? != 0,
            source: row.get("source")?,
            created_at_ms: row.get("created_at_ms")?,
        })
    }

    pub fn changeset(&self, id: &str) -> Result<Option<Changeset>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn
            .query_row(
                "SELECT * FROM harness_changesets WHERE id = ?1",
                [id],
                Self::row_to_changeset,
            )
            .optional()?)
    }

    pub fn recent_changesets(&self, session: Option<&str>, limit: usize) -> Result<Vec<Changeset>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT * FROM harness_changesets WHERE (?1 IS NULL OR session = ?1) ORDER BY created_at_ms DESC, rowid DESC LIMIT ?2",
        )?;
        Ok(stmt
            .query_map(params![session, limit as i64], Self::row_to_changeset)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Roll back `id` (or, if `None`, the most recent non-rolled-back
    /// changeset for `session`) by restoring every touched entry's exact
    /// `before` state, recorded as a new changeset whose `rollback_of` points
    /// at the target. Rolling back a `create` deletes the entry; rolling
    /// back a `delete` recreates it verbatim (same id); rolling back an
    /// `update` restores the prior snapshot.
    pub fn rollback(&self, id: Option<&str>, session: Option<&str>) -> Result<Changeset> {
        let target = match id {
            Some(id) => self
                .changeset(id)?
                .with_context(|| format!("no changeset {id}"))?,
            None => self
                .recent_changesets(session, 20)?
                .into_iter()
                .find(|c| !c.rolled_back)
                .context("nothing to roll back")?,
        };
        if target.rolled_back {
            bail!("changeset {} was already rolled back", target.id);
        }
        let mut inverse = Vec::new();
        for edit in target.edits.iter().rev() {
            match edit.action {
                Action::Create => {
                    let after = edit
                        .after
                        .as_ref()
                        .context("create edit missing its snapshot")?;
                    let before = self.delete(&after.id)?;
                    inverse.push(AppliedEdit {
                        action: Action::Delete,
                        id: after.id.clone(),
                        before: Some(before),
                        after: None,
                    });
                }
                Action::Delete => {
                    let before = edit
                        .before
                        .as_ref()
                        .context("delete edit missing its snapshot")?;
                    let after = self.restore(before)?;
                    inverse.push(AppliedEdit {
                        action: Action::Create,
                        id: before.id.clone(),
                        before: None,
                        after: Some(after),
                    });
                }
                Action::Update => {
                    let before = edit
                        .before
                        .as_ref()
                        .context("update edit missing its snapshot")?;
                    let after = self.restore(before)?;
                    inverse.push(AppliedEdit {
                        action: Action::Update,
                        id: before.id.clone(),
                        before: edit.after.clone(),
                        after: Some(after),
                    });
                }
            }
        }
        {
            let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "UPDATE harness_changesets SET rolled_back = 1 WHERE id = ?1",
                [&target.id],
            )?;
        }
        let rollback_id = self.record_changeset(
            target.session.as_deref(),
            target.scope,
            &format!("Rolled back: {}", target.summary),
            "user-requested rollback",
            "restore the prior state",
            &inverse,
            Some(&target.id),
            "refine",
        )?;
        self.changeset(&rollback_id)?
            .context("just-recorded changeset vanished")
    }

    /// Resolve a stored `subagent` entry by name (case-insensitive title
    /// match, local overriding global as usual) for `session`. M10b's swarm
    /// wiring can call this to turn a spawn request naming `spec: <name>`
    /// into that entry's `content` (instructions), `reference` (allowed
    /// tools) and `arguments` (a model hint) — not wired to the swarm here.
    pub fn resolve_subagent_spec(&self, session: &str, name: &str) -> Result<Option<HarnessEntry>> {
        let entries = self.list_visible(session, Some(EntryKind::Subagent))?;
        Ok(entries
            .into_iter()
            .find(|e| e.title.eq_ignore_ascii_case(name)))
    }

    /// `refine.run(instructions)` (the model-callable tool and the REPL host
    /// function): schedule a refinement for `session`, applied at turn end,
    /// never mid-turn. A later call before turn end just updates the pending
    /// instructions (Prime's "single pending request per turn").
    pub fn schedule_refine(
        &self,
        session: &str,
        instructions: Option<&str>,
        global: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO harness_pending_refine(session, instructions, global, created_at_ms) VALUES (?1,?2,?3,?4) \
             ON CONFLICT(session) DO UPDATE SET instructions = ?2, global = ?3, created_at_ms = ?4",
            params![session, instructions, global as i64, now_ms()],
        )?;
        Ok(())
    }

    /// How many messages of `session` an auto-refine checkpoint has already
    /// seen, so a restart never re-examines them.
    pub fn watermark(&self, session: &str) -> usize {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT seen FROM harness_watermark WHERE session = ?1", [session], |r| r.get::<_, i64>(0))
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0) as usize
    }

    pub fn set_watermark(&self, session: &str, seen: usize) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO harness_watermark(session, seen) VALUES (?1,?2) ON CONFLICT(session) DO UPDATE SET seen = ?2",
            params![session, seen as i64],
        )?;
        Ok(())
    }

    /// The engine's persisted settings (`config.get`/`config.set`); `None` when unset.
    pub fn setting(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT value FROM engine_settings WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO engine_settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DELETE FROM engine_settings WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Every setting whose key starts with `prefix`, as (key without the prefix, value).
    pub fn settings_with_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(mut stmt) = conn.prepare("SELECT key, value FROM engine_settings WHERE substr(key, 1, length(?1)) = ?1") else {
            return Vec::new();
        };
        let rows = stmt.query_map([prefix], |r| Ok((r.get::<_, String>(0)?[prefix.len()..].to_string(), r.get::<_, String>(1)?)));
        rows.map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    /// Unattended approvals parked for a late answer, so a restart keeps them.
    pub fn park_save(&self, request_id: &str, session_id: &str, params: &str, created_at_ms: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT OR REPLACE INTO parked_approvals(request_id, session_id, params, created_at_ms) VALUES (?1,?2,?3,?4)",
            params![request_id, session_id, params, created_at_ms],
        )?;
        Ok(())
    }

    pub fn park_delete(&self, request_id: &str) {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute("DELETE FROM parked_approvals WHERE request_id = ?1", [request_id]);
    }

    /// Rows newer than `min_created_ms` as `(request_id, session_id, params, created_at_ms)`; older ones are dropped.
    pub fn park_load(&self, min_created_ms: i64) -> Vec<(String, String, String, i64)> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute("DELETE FROM parked_approvals WHERE created_at_ms < ?1", [min_created_ms]);
        let Ok(mut stmt) = conn.prepare("SELECT request_id, session_id, params, created_at_ms FROM parked_approvals ORDER BY created_at_ms") else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Drop the learning state, parked approvals, surface/bot tags and session-scoped learned
    /// entries of a deleted session.
    pub fn forget_session(&self, session: &str) -> Result<()> {
        learned::drop_scope(&self.db, &format!("session:{session}"))?;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        for table in ["harness_watermark", "harness_learn_state", "harness_pending_refine"] {
            conn.execute(&format!("DELETE FROM {table} WHERE session = ?1"), [session])?;
        }
        conn.execute("DELETE FROM parked_approvals WHERE session_id = ?1", [session])?;
        conn.execute(
            "DELETE FROM engine_settings WHERE key = ?1 OR (key LIKE 'bot\\_session:%' ESCAPE '\\' AND value = ?2)",
            params![format!("session_surface:{session}"), session],
        )?;
        conn.execute("DELETE FROM engine_settings WHERE key = ?1", [format!("session_dir:{session}")])?;
        Ok(())
    }

    /// Whether Prime's auto-refine runs: on unless the user turned it off.
    pub fn learning_enabled(&self) -> bool {
        self.setting("learning.enabled").is_none_or(|v| v != "false")
    }

    /// Record how many assistant messages `session` has produced since its last review and say
    /// whether a review is due (Prime's `_assistantTurnsSinceAutoRefine`, which counts assistant
    /// messages that are not error/abort). Due needs `interval` messages, or `force` (a compaction,
    /// which Prime reviews regardless of the count), and `cooldown_ms` since the last review
    /// (`last > 0`, as Prime). Due stays due until `learn_reviewed` resets the count and stamps the
    /// time. Persisted, so a reconnect or restart keeps the cooldown.
    pub fn learn_checkpoint(&self, session: &str, assistants: usize, interval: usize, cooldown_ms: i64, now: i64, force: bool) -> Result<Option<usize>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO harness_learn_state(session, turns, last_review_ms) VALUES (?1, ?2, 0) \
             ON CONFLICT(session) DO UPDATE SET turns = ?2",
            params![session, assistants as i64],
        )?;
        let last: i64 = conn.query_row("SELECT last_review_ms FROM harness_learn_state WHERE session = ?1", [session], |r| r.get(0))?;
        if (last > 0 && now - last < cooldown_ms) || (assistants < interval && !force) {
            return Ok(None);
        }
        Ok(Some(assistants))
    }

    /// The gate actually ran for `session`: restart the turn count and the cooldown.
    pub fn learn_reviewed(&self, session: &str, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE harness_learn_state SET turns = 0, last_review_ms = ?2 WHERE session = ?1",
            params![session, now],
        )?;
        Ok(())
    }

    /// `refine.status()`: whether a refinement is scheduled for `session`.
    pub fn refine_pending(&self, session: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn
            .query_row(
                "SELECT 1 FROM harness_pending_refine WHERE session = ?1",
                [session],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Take (and clear) `session`'s pending refine request, if any, so the
    /// turn-end scheduler runs it exactly once.
    pub fn take_pending_refine(&self, session: &str) -> Result<Option<(Option<String>, bool)>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row = conn
            .query_row(
                "SELECT instructions, global FROM harness_pending_refine WHERE session = ?1",
                [session],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        if row.is_some() {
            conn.execute(
                "DELETE FROM harness_pending_refine WHERE session = ?1",
                [session],
            )?;
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: EntryKind, scope: Scope, session: Option<&str>) -> NewEntry {
        let mut e = NewEntry::new(kind, scope, "Title", "Content").with_path("topic/a");
        if let Some(s) = session {
            e = e.with_session(s);
        }
        e
    }

    #[test]
    fn crud_roundtrip() {
        let store = EntryStore::temp().unwrap();
        let created = store
            .create(entry(EntryKind::Prompt, Scope::Local, Some("s1")))
            .unwrap();
        assert_eq!(store.get(&created.id).unwrap().unwrap().content, "Content");
        let updated = store
            .update(
                &created.id,
                EntryPatch {
                    content: Some("New".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.version, 2);
        assert_eq!(store.get(&created.id).unwrap().unwrap().content, "New");
        let deleted = store.delete(&created.id).unwrap();
        assert_eq!(deleted.content, "New");
        assert!(store.get(&created.id).unwrap().is_none());
    }

    #[test]
    fn scope_resolution_local_overrides_global_by_path() {
        let store = EntryStore::temp().unwrap();
        store.set_session_dir("s1", "/work/a").unwrap();
        let g = store
            .create(entry(EntryKind::Prompt, Scope::Global, None))
            .unwrap();
        store
            .update(
                &g.id,
                EntryPatch {
                    content: Some("global rule".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let visible_before = store.list_visible("s1", Some(EntryKind::Prompt)).unwrap();
        assert_eq!(visible_before.len(), 1);
        assert_eq!(visible_before[0].content, "global rule");

        let mut local = entry(EntryKind::Prompt, Scope::Local, Some("s1"));
        local.content = "local override".into();
        store.create(local).unwrap();
        let visible_after = store.list_visible("s1", Some(EntryKind::Prompt)).unwrap();
        assert_eq!(
            visible_after.len(),
            1,
            "same path collapses to one, local wins"
        );
        assert_eq!(visible_after[0].content, "local override");

        // A different session never sees another session's local entry.
        let visible_other = store.list_visible("s2", Some(EntryKind::Prompt)).unwrap();
        assert_eq!(visible_other.len(), 1);
        assert_eq!(visible_other[0].content, "global rule");
    }

    #[test]
    fn render_prompt_is_deterministic_by_append_order() {
        let store = EntryStore::temp().unwrap();
        for (i, text) in ["first", "second", "third"].iter().enumerate() {
            let mut e = entry(EntryKind::Prompt, Scope::Local, Some("s1"));
            e.path = format!("topic/{i}");
            e.content = text.to_string();
            store.create(e).unwrap();
        }
        assert_eq!(
            store.render_prompt("s1").unwrap(),
            "first\n\nsecond\n\nthird"
        );
    }

    #[test]
    fn render_prompt_is_capped_newest_first_and_stable() {
        let store = EntryStore::temp().unwrap();
        for i in 0..5 {
            let mut e = entry(EntryKind::Prompt, Scope::Local, Some("s1"));
            e.path = format!("topic/{i}");
            e.content = format!("{i}{}", "x".repeat(2_499));
            store.create(e).unwrap();
        }
        let out = store.render_prompt("s1").unwrap();
        assert!(out.chars().count() <= MAX_PROMPT_CHARS);
        // Budget fits two 2,500-char entries: the newest two, oldest-first.
        assert!(out.starts_with('3') && out.contains("\n\n4") && !out.contains("2x"));
        assert_eq!(out, store.render_prompt("s1").unwrap());
    }

    #[test]
    fn learn_state_persists_across_reopen_and_compaction_ignores_the_count_but_not_the_cooldown() {
        let dir = std::env::temp_dir().join(format!("learn-state-{}", uuid::Uuid::new_v4()));
        let due = |store: &EntryStore, n, now, force| store.learn_checkpoint("s1", n, 3, 1_000, now, force).unwrap();
        let store = EntryStore::open(&dir).unwrap();
        assert_eq!((due(&store, 1, 5_000, false), due(&store, 2, 5_001, false)), (None, None));
        drop(store);
        let store = EntryStore::open(&dir).unwrap();
        assert_eq!(due(&store, 3, 5_002, false), Some(3));
        // Not reviewed yet (say the turn was busy): still due, the window is not lost.
        assert_eq!(due(&store, 4, 5_003, false), Some(4));
        // A compaction is due at any count.
        assert_eq!(due(&store, 1, 5_003, true), Some(1));
        store.learn_reviewed("s1", 5_003).unwrap();
        assert_eq!((due(&store, 9, 5_004, false), due(&store, 9, 5_500, true)), (None, None), "cooling defers both");
        assert_eq!((due(&store, 9, 6_003, false), due(&store, 1, 6_003, true)), (Some(9), Some(1)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn learning_is_on_by_default_and_the_setting_persists() {
        let store = EntryStore::temp().unwrap();
        assert!(store.learning_enabled());
        store.set_setting("learning.enabled", "false").unwrap();
        assert!(!store.learning_enabled());
        store.set_setting("learning.enabled", "true").unwrap();
        assert!(store.learning_enabled());
    }

    #[test]
    fn promote_copies_local_entry_to_global_without_moving_it() {
        let store = EntryStore::temp().unwrap();
        store.set_session_dir("s1", "/work/a").unwrap();
        let local = store
            .create(entry(EntryKind::Prompt, Scope::Local, Some("s1")))
            .unwrap();
        let global = store.promote(&local.id).unwrap();
        assert_ne!(global.id, local.id, "promotion copies, it does not move");
        assert_eq!(global.scope, Scope::Global);
        assert!(
            store.get(&local.id).unwrap().is_some(),
            "the local copy still exists"
        );
    }

    #[test]
    fn rollback_restores_create_update_delete_exactly() {
        let store = EntryStore::temp().unwrap();
        let created = store
            .create(entry(EntryKind::Prompt, Scope::Local, Some("s1")))
            .unwrap();
        let cs1 = store
            .record_changeset(
                Some("s1"),
                Scope::Local,
                "created",
                "r",
                "e",
                &[AppliedEdit {
                    action: Action::Create,
                    id: created.id.clone(),
                    before: None,
                    after: Some(created.clone()),
                }],
                None,
                "refine",
            )
            .unwrap();
        let updated = store
            .update(
                &created.id,
                EntryPatch {
                    content: Some("v2".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let cs2 = store
            .record_changeset(
                Some("s1"),
                Scope::Local,
                "updated",
                "r",
                "e",
                &[AppliedEdit {
                    action: Action::Update,
                    id: created.id.clone(),
                    before: Some(created.clone()),
                    after: Some(updated),
                }],
                None,
                "refine",
            )
            .unwrap();

        // Roll back the update: content goes back to the original.
        store.rollback(Some(&cs2), None).unwrap();
        assert_eq!(store.get(&created.id).unwrap().unwrap().content, "Content");
        assert!(store.changeset(&cs2).unwrap().unwrap().rolled_back);

        // Roll back the create: the entry is gone.
        store.rollback(Some(&cs1), None).unwrap();
        assert!(store.get(&created.id).unwrap().is_none());

        // Rolling back the same changeset twice is refused.
        assert!(store.rollback(Some(&cs1), None).is_err());
    }

    #[test]
    fn rollback_of_a_delete_recreates_the_entry_with_the_same_id() {
        let store = EntryStore::temp().unwrap();
        let created = store
            .create(entry(EntryKind::Subagent, Scope::Global, None))
            .unwrap();
        let deleted = store.delete(&created.id).unwrap();
        let cs = store
            .record_changeset(
                None,
                Scope::Global,
                "deleted",
                "r",
                "e",
                &[AppliedEdit {
                    action: Action::Delete,
                    id: created.id.clone(),
                    before: Some(deleted),
                    after: None,
                }],
                None,
                "refine",
            )
            .unwrap();
        store.rollback(Some(&cs), None).unwrap();
        let restored = store.get(&created.id).unwrap().unwrap();
        assert_eq!(restored.id, created.id);
        assert_eq!(restored.content, created.content);
    }

    #[test]
    fn schedule_refine_is_pending_until_taken_once() {
        let store = EntryStore::temp().unwrap();
        assert!(!store.refine_pending("s1").unwrap());
        store
            .schedule_refine("s1", Some("be terser"), false)
            .unwrap();
        assert!(store.refine_pending("s1").unwrap());
        // A second schedule before turn-end just updates instructions.
        store
            .schedule_refine("s1", Some("be terser and use Nim"), true)
            .unwrap();
        let (instructions, global) = store.take_pending_refine("s1").unwrap().unwrap();
        assert_eq!(instructions.as_deref(), Some("be terser and use Nim"));
        assert!(global);
        assert!(
            store.take_pending_refine("s1").unwrap().is_none(),
            "taken exactly once"
        );
        assert!(!store.refine_pending("s1").unwrap());
    }

    #[test]
    fn resolves_a_subagent_spec_by_name_case_insensitively() {
        let store = EntryStore::temp().unwrap();
        let mut e = entry(EntryKind::Subagent, Scope::Global, None);
        e.title = "Code Reviewer".into();
        e.content = "Review diffs for correctness bugs.".into();
        store.create(e).unwrap();
        let resolved = store.resolve_subagent_spec("s1", "code reviewer").unwrap();
        assert_eq!(
            resolved.unwrap().content,
            "Review diffs for correctness bugs."
        );
        assert!(store.resolve_subagent_spec("s1", "nope").unwrap().is_none());
    }

    #[test]
    fn no_local_entry_leaks_into_another_session() {
        let store = EntryStore::temp().unwrap();
        store.set_session_dir("s1", "/work/a").unwrap();
        store.set_session_dir("s2", "/work/b").unwrap();
        store
            .create(entry(EntryKind::Subagent, Scope::Local, Some("s1")))
            .unwrap();
        assert_eq!(store.list_visible("s1", None).unwrap().len(), 1);
        assert_eq!(store.list_visible("s2", None).unwrap().len(), 0);
    }

    fn raw(store: &EntryStore) -> Connection {
        Connection::open(&store.db).unwrap()
    }

    #[test]
    fn entries_are_memories_and_nothing_else_holds_their_text() {
        let store = EntryStore::temp().unwrap();
        store.set_session_dir("s1", "/work/a").unwrap();
        let e = store.create(entry(EntryKind::Skill, Scope::Local, Some("s1"))).unwrap();
        assert_eq!(e.memory_scope, learned::project_scope("/work/a"), "a session with a directory learns into its project");
        let db = raw(&store);
        let (category, scope, content): (String, String, String) = db
            .query_row(
                "SELECT json_extract(e.entry, '$.category.custom'), m.scope, m.content FROM memories m JOIN memory_entries e ON e.rid = m.rid WHERE m.id = ?1",
                [&e.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((category.as_str(), scope, content.as_str()), ("skill", e.memory_scope.clone(), "Content"));
        assert!(db.prepare("SELECT 1 FROM harness_entries").is_err(), "the old table does not exist");
        // No directory: global.
        let g = store.create(entry(EntryKind::Prompt, Scope::Local, Some("s9"))).unwrap();
        assert_eq!((g.memory_scope.as_str(), g.scope), ("global", Scope::Global));
        // Delete removes the memory row.
        store.delete(&e.id).unwrap();
        assert_eq!(db.query_row("SELECT count(*) FROM memories WHERE id = ?1", [&e.id], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn similar_prompt_notes_stay_separate_rows() {
        let store = EntryStore::temp().unwrap();
        let mut ids = Vec::new();
        for (i, text) in ["Always run the linter before committing code.", "Always run the linter before committing code!", "Always run the linter before committing code."].iter().enumerate() {
            let mut e = entry(EntryKind::Prompt, Scope::Global, None);
            e.path = format!("p/{i}");
            e.content = text.to_string();
            ids.push(store.create(e).unwrap().id);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);
        assert_eq!(store.list_visible("s1", Some(EntryKind::Prompt)).unwrap().len(), 3, "no merge, no reinforcement");
    }

    #[test]
    fn rollback_restores_every_field_of_the_entry() {
        let store = EntryStore::temp().unwrap();
        store.set_session_dir("s1", "/work/a").unwrap();
        let before = store.create(entry(EntryKind::Subagent, Scope::Local, Some("s1"))).unwrap();
        let after = store
            .update(&before.id, EntryPatch { title: Some("Other".into()), content: Some("Changed".into()), path: Some("x/y".into()), reference: Some(json!({"a": 1})), ..Default::default() })
            .unwrap();
        let edit = |action, b, a| AppliedEdit { action, id: before.id.clone(), before: b, after: a };
        let cs = store.record_changeset(Some("s1"), Scope::Local, "u", "r", "e", &[edit(Action::Update, Some(before.clone()), Some(after.clone()))], None, "refine").unwrap();
        store.rollback(Some(&cs), None).unwrap();
        assert_eq!(store.get(&before.id).unwrap().unwrap(), before, "every field, version and order included");
        // A delete comes back the same, in the same scope, under the same id.
        let gone = store.delete(&before.id).unwrap();
        let cs = store.record_changeset(Some("s1"), Scope::Local, "d", "r", "e", &[edit(Action::Delete, Some(gone), None)], None, "refine").unwrap();
        store.rollback(Some(&cs), None).unwrap();
        assert_eq!(store.get(&before.id).unwrap().unwrap(), before);
    }

    #[test]
    fn a_session_local_entry_dies_with_its_session() {
        let store = EntryStore::temp().unwrap();
        store.create(NewEntry::new(EntryKind::Prompt, Scope::Global, "G", "for everyone")).unwrap();
        // What migration 6 makes of an old session-local row.
        let mut legacy = store.create(NewEntry::new(EntryKind::Prompt, Scope::Global, "T", "kept only here")).unwrap();
        store.delete(&legacy.id).unwrap();
        legacy.memory_scope = "session:s1".into();
        store.restore(&legacy).unwrap();
        assert_eq!(store.list_visible("s1", None).unwrap().len(), 2);
        assert_eq!(store.list_visible("s2", None).unwrap().len(), 1);
        store.forget_session("s1").unwrap();
        assert_eq!(store.list_visible("s1", None).unwrap().len(), 1, "only the global one is left");
    }

    #[test]
    fn prompt_notes_come_with_their_ids_and_recall_leaves_them_out() {
        let store = EntryStore::temp().unwrap();
        let mut e = entry(EntryKind::Prompt, Scope::Global, None);
        e.content = "Always run the linter before committing".into();
        let note = store.create(e).unwrap();
        let (text, ids) = store.render_prompt_with_ids("s1").unwrap();
        assert_eq!((text.as_str(), ids), ("Always run the linter before committing", vec![note.id.clone()]));
        let (_, m) = learned::get(&store.db, &note.id).unwrap().unwrap();
        assert!(learned::is_kept_out_of_recall(&m), "a prompt note is in the cached prefix, so recall never shows it");
        let mut s = entry(EntryKind::Skill, Scope::Global, None);
        s.content = "Run the release checklist".into();
        let skill = store.create(s).unwrap();
        let (_, m) = learned::get(&store.db, &skill.id).unwrap().unwrap();
        assert_eq!(learned::is_kept_out_of_recall(&m), crate::skill_files::skills_dir().is_some(), "a skill is left to the skill list only when its SKILL.md exists");
    }
}
