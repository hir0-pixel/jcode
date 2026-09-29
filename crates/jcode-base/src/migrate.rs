//! `sovereign.db` schema version (`PRAGMA user_version`) and forward-only migrations.
//!
//! Every store that opens the file calls [`run`] after its own idempotent `CREATE TABLE IF NOT
//! EXISTS` baseline. Migration N takes the schema from version N-1 to N; append new ones, never edit
//! or reorder. Before the first migration of a file that already holds data, a consistent copy is
//! written next to it (`sovereign.db.pre-vN.bak`, the version it is going TO) so a bad update or a
//! rollback of the app bundle can restore it. A file from a NEWER engine is refused, not touched.
//! This is the only place that versions the file (it lives in `jcode-base`, the lowest crate that needs it, so the forked layer never depends on sovereign-prime): jcode's memory tables (`memory_store`)
//! call [`run`] too, and their legacy layout upgrade is migration 4 below.

use anyhow::{Result, bail};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

enum Step {
    Sql(&'static str),
    Code(fn(&Connection) -> Result<()>),
}

/// `MIGRATIONS[n]` moves the schema to version `n + 1`.
const MIGRATIONS: &[Step] = &[
    // 1: baseline. The stores' own IF NOT EXISTS schemas define it; this only stamps the version.
    Step::Sql(""),
    // 2: unattended approvals parked for a late answer survive an engine restart.
    Step::Sql("CREATE TABLE IF NOT EXISTS parked_approvals(
        request_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        params TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );"),
    // 3: observability tables take EveStack's names. A file that never had the old tables gets them
    // as empty shells first so the renames always apply; the observability baseline
    // (`gateway/observability/schema.rs`) adds the newer columns and recreates views, indexes and the
    // memory-deletion trigger. Must run before that baseline creates the new names.
    Step::Sql("DROP VIEW IF EXISTS obs_fact_turn; DROP VIEW IF EXISTS obs_fact_tool_call; DROP VIEW IF EXISTS obs_alert_state;
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
    ALTER TABLE obs_alerts RENAME TO alert_state; ALTER TABLE obs_memory_deletions RENAME TO memory_deletions;"),
    // 4: memory tables (owned by jcode-base `memory_store`, which re-applies its own IF NOT EXISTS schema
    // after this): the v1 layout kept each entry's JSON inline in `memories`; it moves to `memory_entries`
    // with the embedding as little-endian f32 bytes. Formerly memory's private schema_version 1 -> 2.
    Step::Code(memory_entry_layout),
    // 5: "forget" keeps no content: the memory audit records id, scope, category and length only.
    // (The old text of already-deleted memories is dropped with the columns.)
    Step::Code(memory_audit_without_content),
];

fn memory_audit_without_content(conn: &Connection) -> Result<()> {
    if !conn.prepare("SELECT 1 FROM pragma_table_info('memory_deletions') WHERE name='content'")?.exists([])? {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_audit;
         ALTER TABLE memory_deletions ADD COLUMN category TEXT;
         ALTER TABLE memory_deletions ADD COLUMN length INTEGER NOT NULL DEFAULT 0;
         UPDATE memory_deletions SET length = length(content);
         DROP INDEX IF EXISTS memory_deletions_recent;
         ALTER TABLE memory_deletions DROP COLUMN content;
         ALTER TABLE memory_deletions DROP COLUMN tags;
         CREATE INDEX IF NOT EXISTS memory_deletions_recent ON memory_deletions(deleted_at_ms DESC);",
    )?;
    Ok(())
}

fn memory_entry_layout(conn: &Connection) -> Result<()> {
    if !conn.prepare("SELECT 1 FROM pragma_table_info('memories') WHERE name='entry'")?.exists([])? {
        return Ok(());
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS memory_entries(rid INTEGER PRIMARY KEY, entry TEXT NOT NULL, embedding BLOB);")?;
    let rows: Vec<(i64, String)> = conn
        .prepare("SELECT rid, entry FROM memories")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (rid, entry) in rows {
        let mut entry: Value = serde_json::from_str(&entry)?;
        let embedding: Option<Vec<u8>> = entry
            .as_object_mut()
            .and_then(|o| o.remove("embedding"))
            .and_then(|e| e.as_array().map(|a| a.iter().filter_map(Value::as_f64).flat_map(|f| (f as f32).to_le_bytes()).collect()));
        conn.execute("INSERT OR REPLACE INTO memory_entries(rid, entry, embedding) VALUES (?1, ?2, ?3)", params![rid, entry.to_string(), embedding])?;
    }
    conn.execute_batch("DROP TRIGGER IF EXISTS memories_ad; ALTER TABLE memories DROP COLUMN entry;")?;
    Ok(())
}

/// The schema version this engine writes.
pub const CURRENT: u32 = MIGRATIONS.len() as u32;

fn version(conn: &Connection) -> Result<u32> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Make `sovereign.db` and its `-wal`/`-shm` owner-only: it holds chats, memories and goals. A new
/// file is created 0600 (so SQLite gives its sidecars the same mode); existing ones are tightened.
pub fn private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        if !path.exists() {
            let _ = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path);
        }
        for suffix in ["", "-wal", "-shm"] {
            let mut file = path.as_os_str().to_owned();
            file.push(suffix);
            let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Open `path`, apply the store's idempotent `schema`, then migrate. A file that already held a
/// version or tables before `schema` ran is backed up first.
pub fn open(path: &Path, schema: &str) -> Result<Connection> {
    private(path);
    let conn = Connection::open(path)?;
    let had_data: bool = version(&conn)? > 0 || conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table')", [], |r| r.get(0))?;
    conn.execute_batch(schema)?;
    run(&conn, had_data.then_some(path))?;
    private(path);
    spawn_daily_backup(path);
    Ok(conn)
}

const DAILY: Duration = Duration::from_secs(24 * 3600);
static BACKUP_ERROR: Mutex<Option<String>> = Mutex::new(None);
static BACKUP_STARTED: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

/// Why the last routine backup failed (None: fine or not yet run); the gateway reports it as a status.
pub fn backup_error() -> Option<String> {
    BACKUP_ERROR.lock().ok().and_then(|e| e.clone())
}

/// Once per process and file, off the hot path: refresh `sovereign.db.daily.bak` if it is over a day old.
fn spawn_daily_backup(path: &Path) {
    let Ok(mut started) = BACKUP_STARTED.lock() else { return };
    if started.iter().any(|p| p == path) {
        return;
    }
    started.push(path.to_path_buf());
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let result = daily_backup(&path, DAILY);
        if let Ok(mut e) = BACKUP_ERROR.lock() {
            *e = result.as_ref().err().map(|e| e.to_string());
        }
        if let Err(e) = result {
            eprintln!("sovereign.db daily backup: {e}");
        }
    });
}

/// `PRAGMA quick_check`, then `VACUUM INTO sovereign.db.daily.bak` (one copy, 0600, temp then rename)
/// when that backup is missing or older than `max_age`. A db that fails the check is reported and
/// never replaces a good backup. Returns whether a new backup was written.
pub fn daily_backup(path: &Path, max_age: Duration) -> Result<bool> {
    let backup = path.with_file_name("sovereign.db.daily.bak");
    let fresh = std::fs::metadata(&backup).and_then(|m| m.modified()).ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|age| age < max_age);
    if fresh || !path.exists() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| anyhow::anyhow!("integrity check could not open sovereign.db (backup kept): {e}"))?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))
        .unwrap_or_else(|e| e.to_string());
    if check != "ok" {
        bail!("sovereign.db failed quick_check (backup kept): {check}");
    }
    let tmp = backup.with_extension("bak.tmp");
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])?;
    private(&tmp);
    std::fs::rename(&tmp, &backup)?;
    Ok(true)
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
            private(&backup);
        }
    }
    // Another opener may have migrated meanwhile: re-read the version under the write lock.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        for (i, step) in MIGRATIONS.iter().enumerate().skip(version(conn)? as usize) {
            match step {
                Step::Sql(sql) => conn.execute_batch(sql)?,
                Step::Code(f) => f(conn)?,
            }
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

    #[cfg(unix)]
    #[test]
    fn the_database_and_its_sidecars_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("migrate-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sovereign.db");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let conn = open(&path, "CREATE TABLE IF NOT EXISTS t(a);").unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO t VALUES(1);").unwrap();
        private(&path);
        for suffix in ["", "-wal", "-shm"] {
            let file = dir.join(format!("sovereign.db{suffix}"));
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "{}", file.display());
        }
        drop(conn);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn daily_backup_is_made_once_a_day_and_never_replaced_by_a_corrupt_db() {
        let dir = std::env::temp_dir().join(format!("migrate-daily-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sovereign.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t(a); INSERT INTO t VALUES(1);").unwrap();
        drop(conn);
        let bak = dir.join("sovereign.db.daily.bak");
        assert!(daily_backup(&path, DAILY).unwrap());
        assert!(!daily_backup(&path, DAILY).unwrap(), "fresh backup is not redone");
        assert_eq!(Connection::open(&bak).unwrap().query_row("SELECT a FROM t", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        std::fs::write(&path, vec![0x5a; 8192]).unwrap();
        assert!(daily_backup(&path, Duration::ZERO).is_err());
        assert_eq!(Connection::open(&bak).unwrap().query_row("SELECT a FROM t", [], |r| r.get::<_, i64>(0)).unwrap(), 1, "the good backup survives");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_memory_audit_loses_the_text_of_already_deleted_memories() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE memory_deletions(id INTEGER PRIMARY KEY AUTOINCREMENT, deleted_at_ms INTEGER NOT NULL,
                memory_id TEXT NOT NULL, scope TEXT, content TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '',
                actor TEXT, actor_via TEXT NOT NULL DEFAULT 'unidentified');
             CREATE INDEX memory_deletions_recent ON memory_deletions(deleted_at_ms DESC);
             INSERT INTO memory_deletions(deleted_at_ms,memory_id,content) VALUES(1,'m','secret text');",
        )
        .unwrap();
        memory_audit_without_content(&conn).unwrap();
        memory_audit_without_content(&conn).unwrap();
        let columns: Vec<String> = conn.prepare("SELECT name FROM pragma_table_info('memory_deletions')").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert!(!columns.iter().any(|c| c == "content" || c == "tags"), "{columns:?}");
        assert_eq!(conn.query_row("SELECT length FROM memory_deletions", [], |r| r.get::<_, i64>(0)).unwrap(), 11);
    }

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
