//! Desktop learning REST compatibility, backed by the same learned memories
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
        ("GET", ["harness"]) => {
            let home = config.home.clone();
            match blocking(move || harness(&*store(Path::new(&home))?)).await {
                Ok(body) => respond(stream, "200 OK", &body).await,
                Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
            }
        }
        ("POST", ["harness", "rollback"]) => {
            let body: Value = match read_body(stream, req).await {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                Err(err) => return Some(Err(err)),
            };
            let Some(id) = body["id"].as_str().filter(|id| !id.is_empty()).map(str::to_string) else {
                return Some(respond(stream, "400 Bad Request", &json!({"detail": "id is required"})).await);
            };
            let home = config.home.clone();
            match blocking(move || {
                let store = store(Path::new(&home))?;
                sovereign_prime::refine::rollback(&store, "", Some(&id))
            })
            .await
            {
                Ok(message) => respond(stream, "200 OK", &json!({"ok": true, "message": message})).await,
                Err(err) => respond(stream, "409 Conflict", &json!({"ok": false, "message": err.to_string()})).await,
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

pub(crate) fn graph(home: &Path) -> Result<Value> {
    let store = store(home)?;
    let mut body = graph_from_store(&store)?;
    // Learned entries are memories too; the other memories (extraction, the memory tool) show as memory nodes.
    let referenced = store.list_all(None, None)?.into_iter().map(|e| e.id).collect();
    for m in super::memory_rest::unreferenced(&referenced)? {
        let title: String = m.content.chars().take(60).collect();
        body["memory"].as_array_mut().unwrap().push(json!({
            "source": "memory", "timestamp": m.created_at.timestamp(), "title": title, "body": m.content,
        }));
        body["nodes"].as_array_mut().unwrap().push(json!({
            "id": format!("{MEM_PREFIX}{}", m.id), "label": title, "kind": "memory",
            "timestamp": m.created_at.timestamp(), "category": m.category.to_string(),
            "useCount": m.access_count, "state": "active",
            "createdBy": m.source.unwrap_or_else(|| "model".into()), "pinned": false,
        }));
        body["stats"]["nodes"] = json!(body["stats"]["nodes"].as_u64().unwrap_or(0) + 1);
        body["stats"]["memory_nodes"] = json!(body["stats"]["memory_nodes"].as_u64().unwrap_or(0) + 1);
    }
    Ok(body)
}

/// Node id prefix for a jcode memory that no harness entry references.
const MEM_PREFIX: &str = "mem:";

/// What Prime has taught the agent: the prompt entries injected into new
/// sessions and the recent changesets (each one undoable).
fn harness(store: &sovereign_prime::entries::EntryStore) -> Result<Value> {
    let entries: Vec<Value> = store
        .list_all(None, None)?
        .into_iter()
        .filter(|e| e.kind == sovereign_prime::entries::EntryKind::Prompt)
        .map(|e| json!({
            "id": e.id, "title": e.title, "content": e.content, "scope": e.scope.as_str(),
            "path": e.path, "source": e.source, "timestamp": e.updated_at_ms / 1000,
        }))
        .collect();
    let changesets: Vec<Value> = store
        .recent_changesets(None, 30)?
        .into_iter()
        .map(|c| json!({
            "id": c.id, "summary": c.summary, "rationale": c.rationale, "edits": c.edits.len(),
            "rolledBack": c.rolled_back, "isRollback": c.rollback_of.is_some(), "timestamp": c.created_at_ms / 1000,
        }))
        .collect();
    Ok(json!({ "entries": entries, "changesets": changesets }))
}

fn graph_from_store(store: &sovereign_prime::entries::EntryStore) -> Result<Value> {
    let entries = store.list_all(None, None)?;
    let mut nodes = Vec::new();
    let memory: Vec<Value> = Vec::new();
    let mut clusters = BTreeMap::<String, usize>::new();
    for entry in entries.into_iter().filter(|entry| entry.kind == sovereign_prime::entries::EntryKind::Skill) {
        let kind = entry.kind.as_str();
        let category = if entry.path.is_empty() {
            kind.to_string()
        } else {
            entry.path.clone()
        };
        *clusters.entry(category.clone()).or_default() += 1;
        let timestamp = entry.created_at_ms / 1000;
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

pub(crate) fn node(home: &Path, id: &str) -> Result<Option<Value>> {
    if let Some(mid) = id.strip_prefix(MEM_PREFIX) {
        return Ok(super::memory_rest::find(mid)
            .map(|m| json!({"ok": true, "kind": "memory", "label": m.content.chars().take(60).collect::<String>(), "content": m.content})));
    }
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
    if entry.kind != sovereign_prime::entries::EntryKind::Skill {
        return Ok(None);
    }
    Ok(Some(
        json!({"ok": true, "kind": entry.kind.as_str(), "label": entry.title, "content": entry.content}),
    ))
}

pub(crate) fn delete(home: &Path, id: &str) -> Result<Option<Value>> {
    if let Some(mid) = id.strip_prefix(MEM_PREFIX) {
        return Ok(super::memory_rest::forget(mid)?
            .then(|| json!({"ok": true, "message": "deleted memory"})));
    }
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
    if entry.kind != sovereign_prime::entries::EntryKind::Skill {
        return Ok(None);
    }
    store.delete(id)?;
    Ok(Some(
        json!({"ok": true, "message": format!("deleted '{}'", entry.title)}),
    ))
}

fn edit(home: &Path, id: &str, content: &str) -> Result<Option<Value>> {
    if let Some(mid) = id.strip_prefix(MEM_PREFIX) {
        if content.trim().is_empty() {
            return Ok(Some(json!({"ok": false, "message": "content cannot be empty"})));
        }
        return Ok(super::memory_rest::edit(mid, content)?
            .then(|| json!({"ok": true, "message": "updated memory"})));
    }
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
    if entry.kind != sovereign_prime::entries::EntryKind::Skill {
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
    sovereign_prime::refine::edit_entry(
        store,
        id,
        sovereign_prime::entries::EntryPatch {
            content: Some(content.to_string()),
            ..Default::default()
        },
    )?;
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
    fn graph_route_shapes_skill_entries() {
        let home = TempHome::new();
        let store = test_store(&home);
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
        assert_eq!(body["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(body["nodes"][0]["kind"], "skill");
        assert_eq!(body["nodes"][0]["id"], skill.id);
        assert!(body["memory"].as_array().unwrap().is_empty());
    }

    #[test]
    fn harness_lists_prompt_entries_and_rollbackable_changesets() {
        let home = TempHome::new();
        let store = test_store(&home);
        let entry = store.create(NewEntry::new(EntryKind::Prompt, Scope::Global, "Scaffold", "Cargo first")).unwrap();
        store.create(NewEntry::new(EntryKind::Skill, Scope::Global, "Not a prompt", "x")).unwrap();
        let cs = store
            .record_changeset(None, Scope::Global, "learned scaffold", "why", "how",
                &[sovereign_prime::entries::AppliedEdit { action: sovereign_prime::entries::Action::Create, id: entry.id.clone(), before: None, after: Some(entry.clone()) }],
                None, "refine")
            .unwrap();
        let body = harness(&store).unwrap();
        assert_eq!(body["entries"].as_array().unwrap().len(), 1);
        assert_eq!(body["entries"][0]["title"], "Scaffold");
        assert_eq!(body["changesets"][0]["id"], cs);
        assert_eq!(body["changesets"][0]["rolledBack"], false);
        sovereign_prime::refine::rollback(&store, "", Some(&cs)).unwrap();
        let after = harness(&store).unwrap();
        assert!(after["entries"].as_array().unwrap().is_empty());
        assert_eq!(after["changesets"].as_array().unwrap().len(), 2);
        assert!(after["changesets"].as_array().unwrap().iter().any(|c| c["id"] == cs && c["rolledBack"] == true));
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
                EntryKind::Skill,
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
