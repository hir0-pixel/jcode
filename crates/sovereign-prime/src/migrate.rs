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
            conn.execute_batch("PRAGMA user_version = 0; DROP TABLE parked_approvals; INSERT INTO engine_settings VALUES('k','v');").unwrap();
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
