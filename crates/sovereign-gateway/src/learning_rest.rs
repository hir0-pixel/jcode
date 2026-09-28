//! Desktop learning REST compatibility, backed by the same M10a harness rows
//! as the `learning.*` RPC methods.

use super::{Config, Request, read_body, respond};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use tokio::net::TcpStream;

pub(super) async fn route(
    stream: &mut TcpStream,
    req: &Request,
    config: &Config,
) -> Option<Result<()>> {
    let rest = req.path.strip_prefix("/api/learning")?;
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    Some(match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["graph"]) => {
            let home = config.home.clone();
            match blocking(move || graph(Path::new(&home))).await {
                Ok(body) => respond(stream, "200 OK", &body).await,
                Err(err) => {
                    respond(
                        stream,
                        "503 Service Unavailable",
                        &json!({"detail": err.to_string()}),
                    )
                    .await
                }
            }
        }
        ("GET", ["node"]) => {
            let Some(id) = super::auth::query_param(req.query.as_deref().unwrap_or_default(), "id")
            else {
                return Some(
                    respond(
                        stream,
                        "400 Bad Request",
                        &json!({"detail": "id is required"}),
                    )
                    .await,
                );
            };
            let home = config.home.clone();
            match blocking(move || node(Path::new(&home), &id)).await {
                Ok(Some(body)) => respond(stream, "200 OK", &body).await,
                Ok(None) => {
                    respond(
                        stream,
                        "404 Not Found",
                        &json!({"ok": false, "message": "learning node not found"}),
                    )
                    .await
                }
                Err(err) => {
                    respond(
                        stream,
                        "503 Service Unavailable",
                        &json!({"detail": err.to_string()}),
                    )
                    .await
                }
            }
        }
        ("DELETE", ["node"]) => {
            let body: Value = match read_body(stream, req).await {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                Err(err) => return Some(Err(err)),
            };
            let Some(id) = body["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_string)
            else {
                return Some(
                    respond(
                        stream,
                        "400 Bad Request",
                        &json!({"detail": "id is required"}),
                    )
                    .await,
                );
            };
            let home = config.home.clone();
            match blocking(move || delete(Path::new(&home), &id)).await {
                Ok(Some(body)) => respond(stream, "200 OK", &body).await,
                Ok(None) => {
                    respond(
                        stream,
                        "404 Not Found",
                        &json!({"ok": false, "message": "learning node not found"}),
                    )
                    .await
                }
                Err(err) => {
                    respond(
                        stream,
                        "503 Service Unavailable",
                        &json!({"detail": err.to_string()}),
                    )
                    .await
                }
            }
        }
        ("PUT", ["node"]) => {
            let body: Value = match read_body(stream, req).await {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                Err(err) => return Some(Err(err)),
            };
            let (Some(id), Some(content)) = (
                body["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string),
                body["content"].as_str().map(str::to_string),
            ) else {
                return Some(
                    respond(
                        stream,
                        "400 Bad Request",
                        &json!({"detail": "id and content are required"}),
                    )
                    .await,
                );
            };
            let home = config.home.clone();
            match blocking(move || edit(Path::new(&home), &id, &content)).await {
                Ok(Some(body)) => respond(stream, "200 OK", &body).await,
                Ok(None) => {
                    respond(
                        stream,
                        "404 Not Found",
                        &json!({"ok": false, "message": "learning node not found"}),
                    )
                    .await
                }
                Err(err) => {
                    respond(
                        stream,
                        "503 Service Unavailable",
                        &json!({"detail": err.to_string()}),
                    )
                    .await
                }
            }
        }
        _ => {
            respond(
                stream,
                "404 Not Found",
                &json!({"detail": "not supported by engine"}),
            )
            .await
        }
    })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    Ok(tokio::task::spawn_blocking(work)
        .await
        .context("learning REST worker failed")??)
}

fn store(home: &Path) -> Result<std::sync::Arc<sovereign_prime::entries::EntryStore>> {
    sovereign_prime::entries::EntryStore::open_cached(home).context("opening learning store")
}

/// A learned memory's text lives once, in jcode's memory store; its harness
/// entry only holds `reference.memory_id` (see `refine::MemorySink`). Entries
/// without a pointer (learning off) carry their own text.
fn memory_body(entry: &sovereign_prime::entries::HarnessEntry) -> String {
    entry.reference["memory_id"]
        .as_str()
        .and_then(|id| {
            let graph = jcode_base::memory::MemoryManager::new().load_global_graph().ok()?;
            graph.get_memory(id).map(|m| m.content.clone())
        })
        .unwrap_or_else(|| entry.content.clone())
}

fn graph(home: &Path) -> Result<Value> {
    let store = store(home)?;
    graph_from_store(&store)
}

fn graph_from_store(store: &sovereign_prime::entries::EntryStore) -> Result<Value> {
    let entries = store.list_all(None, None)?;
    let mut nodes = Vec::new();
    let mut memory = Vec::new();
    let mut clusters = BTreeMap::<String, usize>::new();
    for entry in entries.into_iter().filter(|entry| {
        matches!(
            entry.kind,
            sovereign_prime::entries::EntryKind::Memory
                | sovereign_prime::entries::EntryKind::Skill
        )
    }) {
        let kind = entry.kind.as_str();
        let category = if entry.path.is_empty() {
            kind.to_string()
        } else {
            entry.path.clone()
        };
        *clusters.entry(category.clone()).or_default() += 1;
        let timestamp = entry.created_at_ms / 1000;
        if kind == "memory" {
            memory.push(json!({
                "source": "memory", "timestamp": timestamp, "title": entry.title, "body": memory_body(&entry),
            }));
        }
        nodes.push(json!({
            "id": entry.id,
            "label": entry.title,
            "kind": kind,
            "timestamp": timestamp,
            "category": category,
            "useCount": entry.metadata["use_count"].as_u64().or_else(|| entry.metadata["useCount"].as_u64()).unwrap_or(0),
            "state": entry.metadata["state"].as_str().unwrap_or("active"),
            "createdBy": entry.source,
            "pinned": entry.metadata["pinned"].as_bool().unwrap_or(false),
        }));
    }
    let cluster_rows: Vec<Value> = clusters
        .into_iter()
        .map(|(category, count)| json!({"category": category, "count": count}))
        .collect();
    let memory_nodes = nodes.iter().filter(|node| node["kind"] == "memory").count();
    let learned_skills = nodes.iter().filter(|node| node["kind"] == "skill").count();
    let node_count = nodes.len();
    let category_count = cluster_rows.len();
    Ok(json!({
        "nodes": nodes,
        "edges": [],
        "clusters": cluster_rows,
        "memory": memory,
        "stats": {
            "nodes": node_count, "related_edges": 0, "edges_per_node": 0,
            "linked_nodes": 0, "isolated_pct": if node_count == 0 { 0 } else { 100 },
            "categories": category_count, "agent_created": 0, "used": 0,
            "memory_nodes": memory_nodes, "memory_skill_edges": 0, "learned_skills": learned_skills,
        },
    }))
}

fn node(home: &Path, id: &str) -> Result<Option<Value>> {
    let store = store(home)?;
    node_from_store(&store, id)
}

fn node_from_store(
    store: &sovereign_prime::entries::EntryStore,
    id: &str,
) -> Result<Option<Value>> {
    let Some(entry) = store.get(id)? else {
        return Ok(None);
    };
    if !matches!(
        entry.kind,
        sovereign_prime::entries::EntryKind::Memory | sovereign_prime::entries::EntryKind::Skill
    ) {
        return Ok(None);
    }
    Ok(Some(
        json!({"ok": true, "kind": entry.kind.as_str(), "label": entry.title, "content": memory_body(&entry)}),
    ))
}

fn delete(home: &Path, id: &str) -> Result<Option<Value>> {
    let store = store(home)?;
    delete_from_store(&store, id)
}

fn delete_from_store(
    store: &sovereign_prime::entries::EntryStore,
    id: &str,
) -> Result<Option<Value>> {
    let Some(entry) = store.get(id)? else {
        return Ok(None);
    };
    if !matches!(
        entry.kind,
        sovereign_prime::entries::EntryKind::Memory | sovereign_prime::entries::EntryKind::Skill
    ) {
        return Ok(None);
    }
    store.delete(id)?;
    if let Some(memory_id) = entry.reference["memory_id"].as_str() {
        let _ = jcode_base::memory::MemoryManager::new().forget(memory_id);
    }
    Ok(Some(
        json!({"ok": true, "message": format!("deleted '{}'", entry.title)}),
    ))
}

fn edit(home: &Path, id: &str, content: &str) -> Result<Option<Value>> {
    let store = store(home)?;
    edit_in_store(&store, id, content)
}

fn edit_in_store(
    store: &sovereign_prime::entries::EntryStore,
    id: &str,
    content: &str,
) -> Result<Option<Value>> {
    let Some(entry) = store.get(id)? else {
        return Ok(None);
    };
    if !matches!(
        entry.kind,
        sovereign_prime::entries::EntryKind::Memory | sovereign_prime::entries::EntryKind::Skill
    ) {
        return Ok(None);
    }
    if content.trim().is_empty() {
        return Ok(Some(
            json!({"ok": false, "message": "content cannot be empty"}),
        ));
    }
    if content.chars().count() > sovereign_prime::entries::MAX_CONTENT_CHARS {
        return Ok(Some(json!({
            "ok": false,
            "message": format!("content exceeds {} characters", sovereign_prime::entries::MAX_CONTENT_CHARS),
        })));
    }
    if let Some(memory_id) = entry.reference["memory_id"].as_str() {
        let manager = jcode_base::memory::MemoryManager::new();
        let mut graph = manager.load_global_graph()?;
        if let Some(memory) = graph.get_memory_mut(memory_id) {
            memory.content = content.to_string();
            memory.updated_at = chrono::Utc::now();
            manager.save_global_graph(&graph)?;
        }
    } else {
        store.update(
            id,
            sovereign_prime::entries::EntryPatch {
                content: Some(content.to_string()),
                ..Default::default()
            },
        )?;
    }
    Ok(Some(
        json!({"ok": true, "message": format!("updated '{}'", entry.title)}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sovereign_prime::entries::{EntryKind, EntryStore, NewEntry, Scope};
    use std::path::PathBuf;

    struct TempHome(PathBuf);

    impl TempHome {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sovereign-learning-rest-{}-{}",
                std::process::id(),
                super::super::auth::generate_token()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_store(home: &TempHome) -> EntryStore {
        EntryStore::open(&home.0).unwrap()
    }

    #[test]
    fn graph_route_shapes_memory_and_skill_entries() {
        let home = TempHome::new();
        let store = test_store(&home);
        let memory = store
            .create(NewEntry::new(
                EntryKind::Memory,
                Scope::Global,
                "Preference",
                "Use Rust",
            ))
            .unwrap();
        let skill = store
            .create(NewEntry::new(
                EntryKind::Skill,
                Scope::Global,
                "Review",
                "Check tests",
            ))
            .unwrap();
        store
            .create(NewEntry::new(
                EntryKind::Prompt,
                Scope::Global,
                "Hidden kind",
                "not a graph node",
            ))
            .unwrap();

        let body = graph_from_store(&store).unwrap();
        assert_eq!(body["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(body["nodes"][0]["kind"], "memory");
        assert_eq!(body["nodes"][0]["id"], memory.id);
        assert_eq!(body["nodes"][1]["kind"], "skill");
        assert_eq!(body["nodes"][1]["id"], skill.id);
        assert_eq!(body["memory"][0]["body"], "Use Rust");
    }

    #[test]
    fn detail_route_returns_the_desktop_node_shape() {
        let home = TempHome::new();
        let store = test_store(&home);
        let entry = store
            .create(NewEntry::new(
                EntryKind::Skill,
                Scope::Global,
                "Review",
                "Check tests",
            ))
            .unwrap();
        let body = node_from_store(&store, &entry.id).unwrap().unwrap();
        assert_eq!(
            body,
            json!({"ok": true, "kind": "skill", "label": "Review", "content": "Check tests"})
        );
    }

    #[test]
    fn delete_route_removes_the_same_harness_entry() {
        let home = TempHome::new();
        let store = test_store(&home);
        let entry = store
            .create(NewEntry::new(
                EntryKind::Memory,
                Scope::Global,
                "Preference",
                "Use Rust",
            ))
            .unwrap();
        let body = delete_from_store(&store, &entry.id).unwrap().unwrap();
        assert_eq!(body["ok"], true);
        assert!(store.get(&entry.id).unwrap().is_none());
    }

    #[test]
    fn edit_route_updates_the_same_harness_entry() {
        let home = TempHome::new();
        let store = test_store(&home);
        let entry = store
            .create(NewEntry::new(
                EntryKind::Skill,
                Scope::Global,
                "Review",
                "old",
            ))
            .unwrap();
        let body = edit_in_store(&store, &entry.id, "new").unwrap().unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(store.get(&entry.id).unwrap().unwrap().content, "new");
    }
}
