//! Desktop memory REST over jcode's memory store: the one memory the agent
//! recalls from. Serves `GET /api/memory` + `POST /api/memory/reset` (the
//! Command Center maintenance rows) and list/add/edit/delete under
//! `/api/memory/entries`. Hermes memory providers would be a second store, so
//! `/api/memory/providers/*` is refused instead of forwarded to Python.

use super::{Request, read_body, respond};
use anyhow::{Context, Result};
use jcode_base::memory::{MemoryCategory, MemoryEntry, MemoryManager, MemoryScope};
use serde_json::{Value, json};
use tokio::net::TcpStream;

/// "user" memories are preferences; everything else is the agent's notes.
fn is_user(entry: &MemoryEntry) -> bool {
    matches!(entry.category, MemoryCategory::Preference)
}

fn all() -> Result<Vec<MemoryEntry>> {
    MemoryManager::new().list_all_scoped(MemoryScope::Global)
}

fn status() -> Result<Value> {
    let (mut memory, mut user) = (0, 0);
    for entry in all()? {
        let bytes = entry.content.len();
        if is_user(&entry) { user += bytes } else { memory += bytes }
    }
    Ok(json!({
        "active": "engine memory", "providers": [],
        "builtin_files": { "memory": memory, "user": user },
    }))
}

/// Forget every memory in `target` (`memory` | `user` | `all`); returns the labels cleared.
fn reset(target: &str) -> Result<Value> {
    let manager = MemoryManager::new();
    let mut deleted = Vec::new();
    for entry in all()? {
        let hit = match target {
            "all" => true,
            "user" => is_user(&entry),
            _ => !is_user(&entry),
        };
        if hit && manager.forget(&entry.id)? {
            deleted.push(entry.id);
        }
    }
    Ok(json!({ "ok": true, "deleted": deleted }))
}

fn row(entry: &MemoryEntry) -> Value {
    json!({
        "id": entry.id, "content": entry.content, "category": entry.category.to_string(),
        "source": entry.source, "updated_at": entry.updated_at.timestamp(),
    })
}

pub(crate) fn add(content: &str, category: &str, source: &str) -> Result<String> {
    let mut entry = MemoryEntry::new(category.parse().unwrap_or(MemoryCategory::Fact), content);
    entry.source = Some(source.to_string());
    MemoryManager::new().upsert_global_memory(entry)
}

/// Replace one memory's text; `false` when the id is unknown.
pub(crate) fn edit(id: &str, content: &str) -> Result<bool> {
    let manager = MemoryManager::new();
    let mut graph = manager.load_global_graph()?;
    let Some(memory) = graph.get_memory_mut(id) else {
        return Ok(false);
    };
    memory.content = content.to_string();
    memory.updated_at = chrono::Utc::now();
    memory.refresh_search_text();
    memory.embedding = None; // stale for the new text; recomputed by backfill
    manager.save_global_graph(&graph)?;
    Ok(true)
}

/// Global memories no harness entry points at (the model's own `memory` tool
/// writes), for the learning graph.
pub(crate) fn unreferenced(referenced: &std::collections::HashSet<String>) -> Result<Vec<MemoryEntry>> {
    Ok(all()?.into_iter().filter(|m| !referenced.contains(&m.id)).collect())
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(work).await.context("memory worker failed")?
}

pub(super) async fn route(stream: &mut TcpStream, req: &Request) -> Option<Result<()>> {
    let rest = req.path.strip_prefix("/api/memory")?;
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let (method, segments) = (req.method.as_str(), segments.as_slice());
    let body: Value = if matches!(method, "POST" | "PUT") {
        match read_body(stream, req).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            Err(err) => return Some(Err(err)),
        }
    } else {
        Value::Null
    };
    let text = body["content"].as_str().unwrap_or_default().trim().to_string();
    let result: Result<Option<Value>> = match (method, segments) {
        ("GET", []) => blocking(|| status().map(Some)).await,
        ("POST", ["reset"]) => {
            let target = body["target"].as_str().unwrap_or("all").to_string();
            blocking(move || reset(&target).map(Some)).await
        }
        ("GET", ["entries"]) => blocking(|| Ok(Some(json!({ "entries": all()?.iter().map(row).collect::<Vec<_>>() })))).await,
        ("POST", ["entries"]) if !text.is_empty() => {
            let category = body["category"].as_str().unwrap_or("fact").to_string();
            blocking(move || Ok(Some(json!({ "ok": true, "id": add(&text, &category, "desktop")? })))).await
        }
        ("PUT", ["entries", id]) if !text.is_empty() => {
            let id = id.to_string();
            blocking(move || Ok(edit(&id, &text)?.then(|| json!({ "ok": true })))).await
        }
        ("DELETE", ["entries", id]) => {
            let id = id.to_string();
            blocking(move || Ok(MemoryManager::new().forget(&id)?.then(|| json!({ "ok": true })))).await
        }
        ("POST" | "PUT", ["entries", ..]) => {
            return Some(respond(stream, "400 Bad Request", &json!({"detail": "content is required"})).await);
        }
        (_, ["providers", ..]) | ("PUT", ["provider"]) => {
            return Some(respond(stream, "404 Not Found", &json!({"detail": "not supported by engine: it has one memory"})).await);
        }
        _ => return None,
    };
    Some(match result {
        Ok(Some(value)) => respond(stream, "200 OK", &value).await,
        Ok(None) => respond(stream, "404 Not Found", &json!({"detail": "memory not found"})).await,
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_memory_is_the_single_store_behind_the_screen() {
        let _env = crate::hermes_env::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("memory-rest-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: JCODE_HOME is only touched under ENV_LOCK.
        unsafe { std::env::set_var("JCODE_HOME", &home) };
        assert_eq!(status().unwrap()["builtin_files"], json!({"memory": 0, "user": 0}));
        let note = add("repo uses cargo", "fact", "test").unwrap();
        let pref = add("likes terse", "preference", "test").unwrap();
        assert_eq!(status().unwrap()["builtin_files"], json!({"memory": 15, "user": 11}));
        assert!(edit(&note, "repo uses cargo workspaces").unwrap());
        assert!(!edit("nope", "x").unwrap());
        assert!(all().unwrap().iter().any(|m| m.content == "repo uses cargo workspaces"));
        // the learning graph lists the model's own memories and edits/deletes them by node id
        let g = crate::learning_rest::graph(&home).unwrap();
        let node = g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == json!(format!("mem:{note}"))).unwrap().clone();
        assert_eq!(node["kind"], "memory");
        assert_eq!(g["stats"]["memory_nodes"], 2);
        assert_eq!(crate::learning_rest::node(&home, node["id"].as_str().unwrap()).unwrap().unwrap()["content"], "repo uses cargo workspaces");
        let refs = std::collections::HashSet::from([pref.clone()]);
        assert_eq!(unreferenced(&refs).unwrap().len(), 1);
        assert_eq!(reset("user").unwrap()["deleted"], json!([pref]));
        assert_eq!(all().unwrap().len(), 1);
        assert_eq!(reset("all").unwrap()["deleted"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(home);
    }
}
