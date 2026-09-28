//! Continual Harness entries (Prime Agent's four-kind refinement store), kept
//! in `sovereign.db` instead of a flat file so the full CRUD + rollback model
//! is durable and queryable.
//!
//! Follows Prime Agent's `refinement.ts`: one flat table of entries, each one
//! of `prompt | memory | skill | subagent`, each scoped `local` (a session) or
//! `global`. A `prompt` entry is a durable addendum rendered into the cached
//! system prompt; a `memory` entry *references* a row in the memory store
//! rather than duplicating it; a `skill` entry describes a reusable
//! SKILL.md-backed procedure; a `subagent` entry is a reusable delegation
//! spec (name, instructions, allowed tools, model hint). Every write is
//! grouped into a changeset so `/refine rollback` can restore the exact prior
//! state, itself recorded as a new (rollback) changeset.

use anyhow::{Context, Result, bail};
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
    CREATE TABLE IF NOT EXISTS harness_entries(
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        title TEXT NOT NULL,
        content TEXT NOT NULL,
        path TEXT NOT NULL DEFAULT '',
        scope TEXT NOT NULL,
        session TEXT,
        reference TEXT NOT NULL DEFAULT '{}',
        arguments TEXT NOT NULL DEFAULT '{}',
        metadata TEXT NOT NULL DEFAULT '{}',
        source TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        version INTEGER NOT NULL DEFAULT 1,
        seq INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS harness_entries_render ON harness_entries(scope, session, kind, seq);
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
    Memory,
    Skill,
    Subagent,
}

impl EntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryKind::Prompt => "prompt",
            EntryKind::Memory => "memory",
            EntryKind::Skill => "skill",
            EntryKind::Subagent => "subagent",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "prompt" => Some(EntryKind::Prompt),
            "memory" => Some(EntryKind::Memory),
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
    pub scope: Scope,
    /// Owning session for `Scope::Local`; always `None` for `Scope::Global`.
    pub session: Option<String>,
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
    fn as_str(&self) -> &'static str {
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
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn entry_to_json(e: &HarnessEntry) -> Value {
    json!({
        "id": e.id, "kind": e.kind.as_str(), "title": e.title, "content": e.content, "path": e.path,
        "scope": e.scope.as_str(), "session": e.session, "reference": e.reference, "arguments": e.arguments,
        "metadata": e.metadata, "source": e.source, "created_at_ms": e.created_at_ms,
        "updated_at_ms": e.updated_at_ms, "version": e.version, "seq": e.seq,
    })
}

fn entry_from_json(v: &Value) -> Option<HarnessEntry> {
    if v.is_null() {
        return None;
    }
    Some(HarnessEntry {
        id: v["id"].as_str()?.to_string(),
        kind: EntryKind::parse(v["kind"].as_str()?)?,
        title: v["title"].as_str().unwrap_or_default().to_string(),
        content: v["content"].as_str().unwrap_or_default().to_string(),
        path: v["path"].as_str().unwrap_or_default().to_string(),
        scope: Scope::parse(v["scope"].as_str()?)?,
        session: v["session"].as_str().map(str::to_string),
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

impl EntryStore {
    pub fn open(home: &Path) -> Result<Self> {
        std::fs::create_dir_all(home).ok();
        let conn = Connection::open(home.join("sovereign.db")).context("opening sovereign.db")?;
        conn.execute_batch(SCHEMA)
            .context("migrating harness entry tables")?;
        Ok(Self {
            conn: Mutex::new(conn),
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

    /// In-memory store for unit tests.
    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
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

    fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<HarnessEntry> {
        let kind: String = row.get("kind")?;
        let scope: String = row.get("scope")?;
        let parse_json = |s: String| serde_json::from_str::<Value>(&s).unwrap_or(json!({}));
        Ok(HarnessEntry {
            id: row.get("id")?,
            kind: EntryKind::parse(&kind).unwrap_or(EntryKind::Prompt),
            title: row.get("title")?,
            content: row.get("content")?,
            path: row.get("path")?,
            scope: Scope::parse(&scope).unwrap_or(Scope::Local),
            session: row.get("session")?,
            reference: parse_json(row.get("reference")?),
            arguments: parse_json(row.get("arguments")?),
            metadata: parse_json(row.get("metadata")?),
            source: row.get("source")?,
            created_at_ms: row.get("created_at_ms")?,
            updated_at_ms: row.get("updated_at_ms")?,
            version: row.get("version")?,
            seq: row.get("seq")?,
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<HarnessEntry>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn
            .query_row(
                "SELECT * FROM harness_entries WHERE id = ?1",
                [id],
                Self::row_to_entry,
            )
            .optional()?)
    }

    /// Entries visible to `session`: all globals plus that session's locals,
    /// in deterministic (`seq`) order. On a `path` collision between a global
    /// and a local entry, the local one wins (Prime's local-override rule).
    pub fn list_visible(
        &self,
        session: &str,
        kind: Option<EntryKind>,
    ) -> Result<Vec<HarnessEntry>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT * FROM harness_entries WHERE (scope = 'global' OR (scope = 'local' AND session = ?1)) \
             AND (?2 IS NULL OR kind = ?2) ORDER BY seq ASC",
        )?;
        let kind_str = kind.map(|k| k.as_str());
        let rows: Vec<HarnessEntry> = stmt
            .query_map(params![session, kind_str], Self::row_to_entry)?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT * FROM harness_entries WHERE (?1 IS NULL OR scope = ?1) AND (?2 IS NULL OR session = ?2) ORDER BY seq ASC",
        )?;
        let scope_str = scope.map(|s| s.as_str());
        Ok(stmt
            .query_map(params![scope_str, session], Self::row_to_entry)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Prompt-kind entries rendered for `session`, joined with blank lines:
    /// the stable, cacheable addendum text. Capped at `MAX_PROMPT_CHARS`
    /// total: newest entries (highest `seq`) win the budget, then the kept
    /// ones are emitted oldest-first so the prefix is identical turn to turn.
    pub fn render_prompt(&self, session: &str) -> Result<String> {
        let entries = self.list_visible(session, Some(EntryKind::Prompt))?;
        let mut used = 0;
        let mut kept: Vec<&str> = entries
            .iter()
            .rev()
            .map(|e| e.content.trim())
            .filter(|c| !c.is_empty())
            .filter(|c| {
                let cost = c.chars().count() + 2;
                (used + cost <= MAX_PROMPT_CHARS).then(|| used += cost).is_some()
            })
            .collect();
        kept.reverse();
        Ok(kept.join("\n\n"))
    }

    fn insert_entry(
        conn: &Connection,
        e: &NewEntry,
        id: &str,
        seq: i64,
        at: i64,
    ) -> Result<HarnessEntry> {
        if e.scope == Scope::Local && e.session.is_none() {
            bail!("a local entry needs a session");
        }
        let title: String = e.title.chars().take(MAX_TITLE_CHARS).collect();
        let content: String = e.content.chars().take(MAX_CONTENT_CHARS).collect();
        let path: String = e.path.chars().take(MAX_PATH_CHARS).collect();
        conn.execute(
            "INSERT INTO harness_entries(id, kind, title, content, path, scope, session, reference, arguments, metadata, source, created_at_ms, updated_at_ms, version, seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12,1,?13)",
            params![
                id, e.kind.as_str(), title, content, path, e.scope.as_str(), e.session, e.reference.to_string(),
                e.arguments.to_string(), e.metadata.to_string(), e.source, at, seq
            ],
        )?;
        Ok(HarnessEntry {
            id: id.to_string(),
            kind: e.kind,
            title,
            content,
            path,
            scope: e.scope,
            session: e.session.clone(),
            reference: e.reference.clone(),
            arguments: e.arguments.clone(),
            metadata: e.metadata.clone(),
            source: e.source.clone(),
            created_at_ms: at,
            updated_at_ms: at,
            version: 1,
            seq,
        })
    }

    pub fn create(&self, e: NewEntry) -> Result<HarnessEntry> {
        let conn = self.conn.lock().unwrap_or_else(|err| err.into_inner());
        let id = uuid::Uuid::new_v4().to_string();
        let seq = Self::next_seq(&conn)?;
        Self::insert_entry(&conn, &e, &id, seq, now_ms())
    }

    /// Create with a caller-chosen id (used to restore an exact snapshot on rollback).
    fn create_with_id(&self, e: &HarnessEntry) -> Result<HarnessEntry> {
        let conn = self.conn.lock().unwrap_or_else(|err| err.into_inner());
        conn.execute(
            "INSERT INTO harness_entries(id, kind, title, content, path, scope, session, reference, arguments, metadata, source, created_at_ms, updated_at_ms, version, seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                e.id, e.kind.as_str(), e.title, e.content, e.path, e.scope.as_str(), e.session,
                e.reference.to_string(), e.arguments.to_string(), e.metadata.to_string(), e.source,
                e.created_at_ms, e.updated_at_ms, e.version, e.seq
            ],
        )?;
        Ok(e.clone())
    }

    pub fn update(&self, id: &str, patch: EntryPatch) -> Result<HarnessEntry> {
        let existing = self.get(id)?.with_context(|| format!("no entry {id}"))?;
        let conn = self.conn.lock().unwrap_or_else(|err| err.into_inner());
        let title = patch.title.unwrap_or(existing.title);
        let content = patch.content.unwrap_or(existing.content);
        let path = patch.path.unwrap_or(existing.path);
        let reference = patch.reference.unwrap_or(existing.reference);
        let arguments = patch.arguments.unwrap_or(existing.arguments);
        let metadata = patch.metadata.unwrap_or(existing.metadata);
        let at = now_ms();
        let version = existing.version + 1;
        conn.execute(
            "UPDATE harness_entries SET title=?1, content=?2, path=?3, reference=?4, arguments=?5, metadata=?6, updated_at_ms=?7, version=?8 WHERE id=?9",
            params![title, content, path, reference.to_string(), arguments.to_string(), metadata.to_string(), at, version, id],
        )?;
        Ok(HarnessEntry {
            title,
            content,
            path,
            reference,
            arguments,
            metadata,
            updated_at_ms: at,
            version,
            ..existing
        })
    }

    /// Overwrite an entry back to an exact prior snapshot (rollback of an update).
    fn restore(&self, snapshot: &HarnessEntry) -> Result<HarnessEntry> {
        let conn = self.conn.lock().unwrap_or_else(|err| err.into_inner());
        conn.execute(
            "UPDATE harness_entries SET title=?1, content=?2, path=?3, reference=?4, arguments=?5, metadata=?6, updated_at_ms=?7, version=?8 WHERE id=?9",
            params![
                snapshot.title, snapshot.content, snapshot.path, snapshot.reference.to_string(),
                snapshot.arguments.to_string(), snapshot.metadata.to_string(), snapshot.updated_at_ms, snapshot.version, snapshot.id
            ],
        )?;
        Ok(snapshot.clone())
    }

    pub fn delete(&self, id: &str) -> Result<HarnessEntry> {
        let existing = self.get(id)?.with_context(|| format!("no entry {id}"))?;
        let conn = self.conn.lock().unwrap_or_else(|err| err.into_inner());
        conn.execute("DELETE FROM harness_entries WHERE id = ?1", [id])?;
        Ok(existing)
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

    /// Copy a global entry into a session's local store (demote-by-copy).
    pub fn demote(&self, id: &str, session: &str) -> Result<HarnessEntry> {
        let existing = self.get(id)?.with_context(|| format!("no entry {id}"))?;
        if existing.scope == Scope::Local {
            bail!("entry {id} is already local");
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
            "SELECT * FROM harness_changesets WHERE (?1 IS NULL OR session = ?1) ORDER BY created_at_ms DESC LIMIT ?2",
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
                    let after = self.create_with_id(before)?;
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

    /// Whether Prime's auto-refine runs: on unless the user turned it off.
    pub fn learning_enabled(&self) -> bool {
        self.setting("learning.enabled").is_none_or(|v| v != "false")
    }

    /// Count one finished assistant turn for `session` and say whether the
    /// auto-refine gate is due (`interval` turns and `cooldown_ms` since the
    /// last gate call). Due resets the counter and stamps `now`. Persisted, so
    /// a reconnect or restart keeps counting (Prime's per-session counters).
    pub fn learn_checkpoint(&self, session: &str, interval: usize, cooldown_ms: i64, now: i64) -> Result<Option<usize>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO harness_learn_state(session, turns, last_review_ms) VALUES (?1, 1, 0) \
             ON CONFLICT(session) DO UPDATE SET turns = turns + 1",
            [session],
        )?;
        let (turns, last): (i64, i64) = conn.query_row(
            "SELECT turns, last_review_ms FROM harness_learn_state WHERE session = ?1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if (turns as usize) < interval || now - last < cooldown_ms {
            return Ok(None);
        }
        conn.execute(
            "UPDATE harness_learn_state SET turns = 0, last_review_ms = ?2 WHERE session = ?1",
            params![session, now],
        )?;
        Ok(Some(turns as usize))
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
        let store = EntryStore::memory().unwrap();
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
        let store = EntryStore::memory().unwrap();
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
        let store = EntryStore::memory().unwrap();
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
        let store = EntryStore::memory().unwrap();
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
    fn learn_counters_persist_across_reopen() {
        let dir = std::env::temp_dir().join(format!("learn-state-{}", uuid::Uuid::new_v4()));
        let due = |store: &EntryStore, now| store.learn_checkpoint("s1", 3, 1_000, now).unwrap();
        let store = EntryStore::open(&dir).unwrap();
        assert_eq!((due(&store, 5_000), due(&store, 5_001)), (None, None));
        drop(store);
        // A restart keeps the count: the third turn trips the gate, then cooldown holds.
        let store = EntryStore::open(&dir).unwrap();
        assert_eq!(due(&store, 5_002), Some(3));
        assert_eq!((due(&store, 5_003), due(&store, 5_004), due(&store, 5_005)), (None, None, None));
        assert_eq!(due(&store, 9_000), Some(4));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn learning_is_on_by_default_and_the_setting_persists() {
        let store = EntryStore::memory().unwrap();
        assert!(store.learning_enabled());
        store.set_setting("learning.enabled", "false").unwrap();
        assert!(!store.learning_enabled());
        store.set_setting("learning.enabled", "true").unwrap();
        assert!(store.learning_enabled());
    }

    #[test]
    fn promote_copies_local_entry_to_global_without_moving_it() {
        let store = EntryStore::memory().unwrap();
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
        let store = EntryStore::memory().unwrap();
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
        let store = EntryStore::memory().unwrap();
        let created = store
            .create(entry(EntryKind::Memory, Scope::Global, None))
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
        let store = EntryStore::memory().unwrap();
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
        let store = EntryStore::memory().unwrap();
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
        let store = EntryStore::memory().unwrap();
        store
            .create(entry(EntryKind::Subagent, Scope::Local, Some("s1")))
            .unwrap();
        assert_eq!(store.list_visible("s1", None).unwrap().len(), 1);
        assert_eq!(store.list_visible("s2", None).unwrap().len(), 0);
    }
}
