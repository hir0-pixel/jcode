//! `sovereign.db` schema version (`PRAGMA user_version`) and forward-only migrations.
//!
//! Every store that opens the file calls [`run`] after its own idempotent `CREATE TABLE IF NOT
//! EXISTS` baseline. Migration N takes the schema from version N-1 to N; append new ones, never edit
//! or reorder. Before the first migration of a file that already holds data, a consistent copy is
//! written next to it (`sovereign.db.pre-vN.bak`, the version it is going TO) so a bad update or a
//! rollback of the app bundle can restore it. A file from a NEWER engine is refused, not touched.

use anyhow::{Result, bail};
use rusqlite::Connection;
use std::path::Path;

/// `MIGRATIONS[n]` moves the schema to version `n + 1`.
const MIGRATIONS: &[&str] = &[
    // 1: baseline. The stores' own IF NOT EXISTS schemas define it; this only stamps the version.
    "",
    // 2: unattended approvals parked for a late answer survive an engine restart.
    "CREATE TABLE IF NOT EXISTS parked_approvals(
        request_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        params TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );",
    // 3: observability tables take EveStack's names. A file that never had the old tables gets them
    // as empty shells first so the renames always apply; the observability baseline
    // (`gateway/observability/schema.rs`) adds the newer columns and recreates views, indexes and the
    // memory-deletion trigger. Must run before that baseline creates the new names.
    "DROP VIEW IF EXISTS obs_fact_turn; DROP VIEW IF EXISTS obs_fact_tool_call; DROP VIEW IF EXISTS obs_alert_state;
    DROP TRIGGER IF EXISTS obs_memory_audit;
    DROP INDEX IF EXISTS obs_runs_recent; DROP INDEX IF EXISTS obs_runs_session; DROP INDEX IF EXISTS obs_runs_kind_recent;
    DROP INDEX IF EXISTS obs_runs_outcome_recent; DROP INDEX IF EXISTS obs_runs_status_started; DROP INDEX IF EXISTS obs_runs_root;
    DROP INDEX IF EXISTS obs_runs_parent; DROP INDEX IF EXISTS obs_spans_run; DROP INDEX IF EXISTS obs_spans_name;
    DROP INDEX IF EXISTS obs_approvals_recent; DROP INDEX IF EXISTS obs_approvals_session; DROP INDEX IF EXISTS obs_memory_deletions_recent;
    CREATE TABLE IF NOT EXISTS obs_runs(
        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, parent_id TEXT, root_id TEXT NOT NULL,
        kind TEXT NOT NULL, title TEXT, model TEXT NOT NULL, provider TEXT NOT NULL,
        status TEXT NOT NULL, started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER,
        input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        cost_usd REAL, error TEXT, unpriced_calls INTEGER NOT NULL DEFAULT 0,
        flags INTEGER NOT NULL DEFAULT 0, replay_of TEXT
    );
    CREATE TABLE IF NOT EXISTS obs_spans(
        id TEXT PRIMARY KEY, run_id TEXT NOT NULL, parent_id TEXT NOT NULL, root_id TEXT NOT NULL,
        kind TEXT NOT NULL, name TEXT NOT NULL, status TEXT NOT NULL,
        started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER,
        input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        cost_usd REAL, error TEXT, model TEXT, provider TEXT,
        attributes TEXT NOT NULL DEFAULT '{}'
    );
    CREATE TABLE IF NOT EXISTS obs_content(id TEXT PRIMARY KEY, input TEXT, output TEXT);
    CREATE TABLE IF NOT EXISTS obs_approvals(
        id INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT, session_id TEXT NOT NULL,
        tool TEXT NOT NULL, command_preview TEXT NOT NULL, decision TEXT NOT NULL,
        actor TEXT NOT NULL, at_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS obs_alerts(
        monitor_key TEXT PRIMARY KEY, state TEXT NOT NULL, severity TEXT NOT NULL,
        message TEXT, updated_at_ms INTEGER NOT NULL, last_notified_ms INTEGER
    );
    CREATE TABLE IF NOT EXISTS obs_memory_deletions(
        id INTEGER PRIMARY KEY AUTOINCREMENT, deleted_at_ms INTEGER NOT NULL,
        memory_id TEXT NOT NULL, scope TEXT, content TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '',
        actor TEXT, actor_via TEXT NOT NULL DEFAULT 'unidentified'
    );
    ALTER TABLE obs_runs RENAME TO fact_turn; ALTER TABLE obs_spans RENAME TO spans;
    ALTER TABLE obs_content RENAME TO span_content; ALTER TABLE obs_approvals RENAME TO approvals;
    ALTER TABLE obs_alerts RENAME TO alert_state; ALTER TABLE obs_memory_deletions RENAME TO memory_deletions;",
];

/// The schema version this engine writes.
pub const CURRENT: u32 = MIGRATIONS.len() as u32;

fn version(conn: &Connection) -> Result<u32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Open `path`, apply the store's idempotent `schema`, then migrate. A file that already held a
/// version or tables before `schema` ran is backed up first.
pub fn open(path: &Path, schema: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    let had_data: bool = version(&conn)? > 0 || conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table')", [], |r| r.get(0))?;
    conn.execute_batch(schema)?;
    run(&conn, had_data.then_some(path))?;
    Ok(conn)
}

/// Bring `conn` to [`CURRENT`]; `backup_of` is the file to copy first (None: nothing worth keeping).
pub fn run(conn: &Connection, backup_of: Option<&Path>) -> Result<()> {
    let have = version(conn)?;
    if have > CURRENT {
        bail!("sovereign.db is schema v{have}, newer than this engine (v{CURRENT}); restore the sovereign.db.pre-v*.bak taken before that update or install the newer engine");
    }
    if have == CURRENT {
        return Ok(());
    }
    if let Some(path) = backup_of {
        let backup = path.with_file_name(format!("sovereign.db.pre-v{CURRENT}.bak"));
        // Once per target version: the first backup is the true pre-migration state.
        if !backup.exists() {
            let tmp = backup.with_extension("bak.tmp");
            let _ = std::fs::remove_file(&tmp);
            conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])?;
            std::fs::rename(&tmp, &backup)?;
        }
    }
    // Another opener may have migrated meanwhile: re-read the version under the write lock.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version(conn)? as usize) {
            conn.execute_batch(sql)?;
            conn.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        }
        Ok(())
    })();
    conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_forward_once_backs_up_existing_data_and_refuses_a_newer_file() {
        let dir = std::env::temp_dir().join(format!("migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sovereign.db");
        let schema = "CREATE TABLE IF NOT EXISTS engine_settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);";
        let backup = dir.join(format!("sovereign.db.pre-v{CURRENT}.bak"));
        // A brand-new file needs no backup.
        drop(open(&path, schema).unwrap());
        assert!(!backup.exists());
        // A pre-versioning file (user_version 0) with user data.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 0; DROP TABLE parked_approvals; DROP TABLE fact_turn; DROP TABLE spans; DROP TABLE span_content; DROP TABLE approvals; DROP TABLE alert_state; DROP TABLE memory_deletions; INSERT INTO engine_settings VALUES('k','v');").unwrap();
        }
        let conn = open(&path, schema).unwrap();
        assert_eq!(version(&conn).unwrap(), CURRENT);
        let old = Connection::open(&backup).unwrap();
        assert_eq!(old.query_row("SELECT value FROM engine_settings", [], |r| r.get::<_, String>(0)).unwrap(), "v");
        assert!(old.prepare("SELECT 1 FROM parked_approvals").is_err(), "the backup is the pre-migration state");
        conn.execute("INSERT INTO parked_approvals VALUES('a','s','{}',1)", []).unwrap();
        // Idempotent: reopening changes nothing.
        drop(open(&path, schema).unwrap());
        assert_eq!(conn.query_row("SELECT count(*) FROM parked_approvals", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        // A newer engine's file is refused untouched.
        conn.execute_batch(&format!("PRAGMA user_version = {}", CURRENT + 1)).unwrap();
        assert!(open(&path, schema).unwrap_err().to_string().contains("newer than this engine"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
