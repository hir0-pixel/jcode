//! Prime's learned entries (`prompt`, `skill`, `subagent`) as memories.
//!
//! They live in the same `memories` table as every other memory and are written through
//! `memory_store::remember` like any other writer; what sets them apart is that they are keyed
//! by id alone (never merged by similarity) and that recall treats them specially (see
//! [`is_kept_out_of_recall`]). These functions take the database file, because Prime's store is
//! opened on an explicit home rather than on `JCODE_HOME`.

use super::{MemoryEntry, forget_graph};
use crate::memory_store;
use anyhow::Result;
use std::path::Path;

/// `project:<hash of the project dir>`, the scope of memories written from that directory.
pub fn project_scope(dir: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::path::PathBuf::from(dir).hash(&mut hasher);
    format!("project:{:016x}", hasher.finish())
}

fn invalidate(db: &Path, scope: &str) {
    forget_graph(&format!("{}#{scope}", db.display()));
}

/// Write (or replace, by id) a learned memory in `scope`.
pub fn put(db: &Path, scope: &str, entry: MemoryEntry) -> Result<()> {
    memory_store::remember(db, scope, entry)?;
    invalidate(db, scope);
    Ok(())
}

pub fn get(db: &Path, id: &str) -> Result<Option<(String, MemoryEntry)>> {
    memory_store::get_learned(db, id)
}

/// Active learned memories of these categories in `scopes` (every scope when `None`).
pub fn list(db: &Path, categories: &[&str], scopes: Option<&[String]>) -> Result<Vec<(String, MemoryEntry)>> {
    memory_store::list_learned(db, categories, scopes)
}

pub fn delete(db: &Path, id: &str) -> Result<Option<(String, MemoryEntry)>> {
    let gone = memory_store::delete_learned(db, id)?;
    if let Some((scope, _)) = &gone {
        invalidate(db, scope);
    }
    Ok(gone)
}

/// Drop every memory of `scope` (a deleted session's own rows).
pub fn drop_scope(db: &Path, scope: &str) -> Result<usize> {
    let n = memory_store::drop_scope(db, scope)?;
    invalidate(db, scope);
    Ok(n)
}

/// Close this process's cached connection to `db` (before a test deletes the file).
pub fn close(db: &Path) {
    memory_store::close(db);
}

/// Prompt notes are always in the cached system prompt, and a skill with a generated `SKILL.md`
/// is already offered by the skill list: recall shows neither again.
pub fn is_kept_out_of_recall(entry: &MemoryEntry) -> bool {
    match entry.category.to_string().as_str() {
        "prompt" => entry.category.is_learned(),
        "skill" => entry.category.is_learned() && entry.learned.as_ref().is_some_and(|l| l.listed),
        _ => false,
    }
}
