//! `spawn_tree.list/load/save`, derived from the engine's own child sessions
//! (a subagent is a session whose `parent_session_id` is the parent), so the
//! desktop's composer status stack shows subagent history without a second
//! snapshot store. A "path" is the virtual `spawn-tree:<parent session id>`.

use super::*;

const PREFIX: &str = "spawn-tree:";

fn children<'a>(reply: &'a Value, parent: &str) -> Vec<&'a Value> {
    let rows = reply["sessions"].as_array().map(Vec::as_slice).unwrap_or_default();
    rows.iter().filter(|s| s["parent_session_id"].as_str() == Some(parent)).collect()
}

fn active_ms(row: &Value) -> i64 {
    row["last_active_at_ms"].as_i64().or(row["updated_at_ms"].as_i64()).unwrap_or(0)
}

/// One index row per parent session that has children, newest first.
fn entries(reply: &Value, only: Option<&str>, limit: usize) -> Vec<Value> {
    let rows = reply["sessions"].as_array().map(Vec::as_slice).unwrap_or_default();
    let mut parents: Vec<&str> = rows.iter().filter_map(|s| s["parent_session_id"].as_str()).filter(|p| !p.is_empty()).collect();
    parents.sort_unstable();
    parents.dedup();
    let mut out: Vec<(i64, Value)> = parents
        .into_iter()
        .filter(|p| only.is_none_or(|o| o == *p))
        .map(|parent| {
            let kids = children(reply, parent);
            let started = kids.iter().map(|k| active_ms(k)).min().unwrap_or(0);
            let finished = kids.iter().map(|k| active_ms(k)).max().unwrap_or(0);
            let label = rows.iter().find(|s| s["session_id"].as_str() == Some(parent)).and_then(|s| s["title"].as_str()).unwrap_or_default();
            (finished, json!({
                "path": format!("{PREFIX}{parent}"), "session_id": parent, "label": label, "count": kids.len(),
                "started_at": started as f64 / 1000.0, "finished_at": finished as f64 / 1000.0,
            }))
        })
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().take(limit).map(|(_, v)| v).collect()
}

impl Conn {
    pub(super) async fn spawn_tree(self: &Arc<Self>, method: &str, p: &Value) -> Result<Value, RpcError> {
        let session = p["session_id"].as_str().filter(|s| !s.is_empty());
        if method == "spawn_tree.save" {
            // Nothing to persist: the tree is derived from live engine data.
            let session = session.unwrap_or_default();
            return Ok(json!({ "path": format!("{PREFIX}{session}"), "session_id": session }));
        }
        let reply = self.call(json!({ "req": "list_sessions" })).await.map_err(RpcError::internal)?;
        if method == "spawn_tree.list" {
            let limit = p["limit"].as_u64().unwrap_or(50) as usize;
            return Ok(json!({ "entries": entries(&reply, session, limit) }));
        }
        let parent = p["path"].as_str().and_then(|path| path.strip_prefix(PREFIX)).ok_or_else(|| RpcError::params("unknown spawn-tree path"))?;
        let live = self.sessions.lock().await;
        let subagents: Vec<Value> = children(&reply, parent)
            .into_iter()
            .map(|row| {
                let mut snapshot = Self::subagent_snapshot(row, parent, &live);
                // An idle child is history here, not a running one.
                let running = live.get(row["session_id"].as_str().unwrap_or_default()).is_some_and(SessionState::turn_active);
                if !running && snapshot["status"] == "running" {
                    snapshot["status"] = json!("completed");
                }
                snapshot
            })
            .collect();
        let entry = entries(&reply, Some(parent), 1).into_iter().next().unwrap_or(json!({}));
        Ok(json!({
            "session_id": parent, "label": entry["label"], "started_at": entry["started_at"],
            "finished_at": entry["finished_at"], "subagents": subagents,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Value {
        json!({ "sessions": [
            { "session_id": "p1", "title": "Refactor", "last_active_at_ms": 9000 },
            { "session_id": "c1", "parent_session_id": "p1", "agent_label": "explorer", "last_active_at_ms": 2000 },
            { "session_id": "c2", "parent_session_id": "p1", "agent_label": "tester", "last_active_at_ms": 5000 },
            { "session_id": "c3", "parent_session_id": "p2", "last_active_at_ms": 7000 },
        ]})
    }

    #[test]
    fn spawn_tree_entries_come_from_child_sessions_newest_first() {
        let all = entries(&reply(), None, 10);
        assert_eq!(all.iter().map(|e| e["session_id"].clone()).collect::<Vec<_>>(), [json!("p2"), json!("p1")]);
        let p1 = &entries(&reply(), Some("p1"), 10)[0];
        assert_eq!((p1["path"].as_str(), p1["count"].as_u64(), p1["label"].as_str()), (Some("spawn-tree:p1"), Some(2), Some("Refactor")));
        assert_eq!((p1["started_at"].as_f64(), p1["finished_at"].as_f64()), (Some(2.0), Some(5.0)));
        assert_eq!(entries(&reply(), None, 1).len(), 1);
        assert_eq!(children(&reply(), "p1").len(), 2);
    }
}
