//! Hermes's `/api/sessions` REST surface, served from the engine's session
//! store. Chats never live in Hermes's Python database, so none of these may
//! be proxied there: a delete or rename would silently hit a table that has
//! never seen the session.

use super::{Config, Request, harness_request, harness_requests, query_u64, read_body, respond, session_infos};
use anyhow::Result;
use serde_json::{Value, json};
use tokio::net::TcpStream;

/// The one way a chat is deleted: the engine's session, then everything keyed to it (goals,
/// heartbeats, learning state, parked approvals, tags, traces). A session the engine no longer has
/// still gets its leftovers cleared.
pub(crate) async fn delete_everywhere(config: &Config, id: &str) -> Result<()> {
    // A deleted session's commands (background or timeout-promoted) must not outlive it.
    jcode_base::background::global().cancel_session(id).await;
    if let Err(err) = harness_request(&config.legacy_socket, json!({"req": "delete_session", "session_id": id})).await {
        // The engine's cleanup still ran; a missing file is not a failure here.
        let message = err.to_string();
        if !message.contains("not found") && !message.contains("does not exist") { return Err(err); }
    }
    forget_rows(&config.home, id)
}

/// Just the rows keyed to a session (the engine's session is already gone). Every store is still
/// tried when one fails; the failures come back as one error.
pub(crate) fn forget_rows(home: &str, id: &str) -> Result<()> {
    let mut failed = Vec::new();
    match crate::rpc::control_or_log(home) {
        Some(store) => { if let Err(e) = store.forget_session(id) { failed.push(format!("goal control: {e:#}")); } }
        None => failed.push("goal control store unavailable".into()),
    }
    match crate::rpc::entries_or_log(home) {
        Some(store) => { if let Err(e) = store.forget_session(id) { failed.push(format!("learning: {e:#}")); } }
        None => failed.push("learning store unavailable".into()),
    }
    if let Err(e) = crate::observability::forget_session(std::path::Path::new(home), id) { failed.push(format!("observability: {e:#}")); }
    if let Some(dir) = crate::rpc::attach::stage_dir(home, id) {
        if let Err(e) = std::fs::remove_dir_all(&dir) { if e.kind() != std::io::ErrorKind::NotFound { failed.push(format!("attachments: {e}")); } }
    }
    if failed.is_empty() { Ok(()) } else { anyhow::bail!("could not clear all of {id}'s data: {}", failed.join("; ")) }
}

/// Returns `None` when the path is not under `/api/sessions`.
pub(super) async fn route(stream: &mut TcpStream, req: &Request, config: &Config) -> Option<Result<()>> {
    let rest = req.path.strip_prefix("/api/sessions")?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    Some(match (req.method.as_str(), segments.as_slice()) {
        ("GET", []) => list(stream, req, config).await,
        ("GET", ["stats"]) => stats(stream, config).await,
        ("GET", ["empty", "count"]) => empty_count(stream, config).await,
        ("DELETE", ["empty"]) => delete_empty(stream, config).await,
        ("POST", ["bulk-delete"]) => bulk_delete(stream, req, config).await,
        ("POST", ["import"]) => import(stream, req, config).await,
        ("POST", ["prune"]) => prune(stream, req, config).await,
        ("GET", ["search"]) => search(stream, req, config).await,
        ("GET", [id, "export"]) => export(stream, config, id).await,
        ("GET", [id, "latest-descendant"]) => latest_descendant(stream, config, id).await,
        ("GET", [id]) => get(stream, config, id).await,
        ("GET", [id, "messages"]) => messages(stream, req, config, id).await,
        ("GET", [id, "messages", "around"]) => messages(stream, req, config, id).await,
        // No per-row index yet: an empty, complete page.
        ("GET", [_, "timeline"]) => {
            respond(stream, "200 OK", &json!({"entries": [], "pagination": {"next_cursor": null, "has_more": false}})).await
        }
        ("DELETE", [id]) => delete(stream, config, id).await,
        ("PATCH", [id]) => patch(stream, req, config, id).await,
        (method, _) => {
            super::note_unsupported("http", &format!("{method} {}", req.path));
            respond(stream, "404 Not Found", &json!({"detail": "not supported by engine", "reason": "not_supported_by_engine"})).await
        }
    })
}

async fn stats(stream: &mut TcpStream, config: &Config) -> Result<()> {
    match session_infos(config, u64::MAX, true).await {
        Ok(sessions) => {
            let total = sessions.len();
            let archived = sessions.iter().filter(|s| s["archived"] == true).count();
            let messages = sessions.iter().map(|s| s["message_count"].as_u64().unwrap_or(0)).sum::<u64>();
            respond(stream, "200 OK", &json!({"total": total, "active_store": total - archived, "archived": archived, "messages": messages, "by_source": {"desktop": total}})).await
        }
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    }
}

async fn empty_count(stream: &mut TcpStream, config: &Config) -> Result<()> {
    let sessions = match session_infos(config, u64::MAX, false).await {
        Ok(sessions) => sessions,
        Err(err) => return respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    };
    let mut count = 0usize;
    for session in sessions {
        if session["is_active"] == true || session["archived"] == true { continue; }
        if let Some(id) = session["id"].as_str() {
            if transcript(config, id).await.is_ok_and(|m| m.is_empty()) { count += 1; }
        }
    }
    respond(stream, "200 OK", &json!({"count": count})).await
}

async fn delete_empty(stream: &mut TcpStream, config: &Config) -> Result<()> {
    let sessions = match session_infos(config, u64::MAX, false).await {
        Ok(sessions) => sessions,
        Err(err) => return respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    };
    let mut deleted = 0usize;
    for session in sessions {
        if session["is_active"] == true || session["archived"] == true { continue; }
        let Some(id) = session["id"].as_str() else { continue };
        if transcript(config, id).await.is_ok_and(|m| m.is_empty()) {
            if let Err(err) = delete_everywhere(config, id).await {
                return respond(stream, "503 Service Unavailable", &json!({"detail":err.to_string(), "deleted":deleted})).await;
            }
            deleted += 1;
        }
    }
    respond(stream, "200 OK", &json!({"ok": true, "deleted": deleted})).await
}

async fn bulk_delete(stream: &mut TcpStream, req: &Request, config: &Config) -> Result<()> {
    let body: Value = serde_json::from_slice(&read_body(stream, req).await?).unwrap_or(Value::Null);
    let Some(ids) = body["ids"].as_array() else { return respond(stream, "400 Bad Request", &json!({"detail":"ids must be an array"})).await; };
    if ids.len() > 500 || ids.iter().any(|id| id.as_str().is_none_or(|s| !valid_id(s))) {
        return respond(stream, "400 Bad Request", &json!({"detail":"ids must contain at most 500 valid session ids"})).await;
    }
    let mut deleted = 0usize;
    for id in ids.iter().filter_map(Value::as_str) {
        match find(config, id).await {
            Ok(Some(info)) if info["is_active"] != true => {
                if let Err(err) = delete_everywhere(config, id).await {
                    return respond(stream, "503 Service Unavailable", &json!({"detail":err.to_string(), "deleted":deleted})).await;
                }
                deleted += 1;
            }
            Ok(_) => {}
            Err(err) => return respond(stream, "503 Service Unavailable", &json!({"detail":err.to_string(), "deleted":deleted})).await,
        }
    }
    respond(stream, "200 OK", &json!({"ok": true, "deleted": deleted})).await
}

async fn import(stream: &mut TcpStream, req: &Request, _config: &Config) -> Result<()> {
    let body: Value = serde_json::from_slice(&read_body(stream, req).await?).unwrap_or(Value::Null);
    let Some(records) = body["sessions"].as_array() else {
        return respond(stream, "400 Bad Request", &json!({"detail":"sessions must be an array of engine session records"})).await;
    };
    if records.len() > 500 { return respond(stream, "400 Bad Request", &json!({"detail":"at most 500 sessions may be imported"})).await; }
    let parsed = match parse_import_records(records) {
        Ok(parsed) => parsed,
        Err(err) => return respond(stream, "400 Bad Request", &json!({"detail":err})).await,
    };
    let mut imported = 0usize;
    for mut session in parsed {
        if !jcode_base::session::session_exists(&session.id) {
            if let Err(err) = session.save_prepared() {
                return respond(stream, "503 Service Unavailable", &json!({"detail":err.to_string(), "imported":imported})).await;
            }
            imported += 1;
        }
    }
    respond(stream, "200 OK", &json!({"ok":true,"imported":imported,"skipped":records.len()-imported})).await
}

async fn prune(stream: &mut TcpStream, req: &Request, config: &Config) -> Result<()> {
    let body: Value = serde_json::from_slice(&read_body(stream, req).await?).unwrap_or(Value::Null);
    let days = body["older_than_days"].as_f64().unwrap_or(90.0);
    let cutoff = chrono::Utc::now().timestamp() as f64 - days * 86_400.0;
    let dry_run = body["dry_run"] == true;
    let include_archived = body["include_archived"] == true;
    if !days.is_finite() || days < 0.0 {
        return respond(stream, "400 Bad Request", &json!({"detail":"older_than_days must be a non-negative finite number"})).await;
    }
    let sessions = match session_infos(config, u64::MAX, include_archived).await {
        Ok(sessions) => sessions,
        Err(err) => return respond(stream, "503 Service Unavailable", &json!({"detail":err.to_string()})).await,
    };
    let mut ids = Vec::new();
    for session in sessions {
        if session["is_active"] == true || (!include_archived && session["archived"] == true) { continue; }
        if session["last_active"].as_f64().unwrap_or(f64::INFINITY) < cutoff {
            if let Some(id) = session["id"].as_str() { ids.push(id.to_string()); }
        }
    }
    let mut deleted = 0usize;
    if !dry_run {
        for id in &ids {
            if let Err(err) = delete_everywhere(config, id).await {
                return respond(stream, "503 Service Unavailable", &json!({"detail":err.to_string(),"matched":ids.len(),"deleted":deleted,"session_ids":ids})).await;
            }
            deleted += 1;
        }
    }
    respond(stream, "200 OK", &json!({"ok":true,"matched":ids.len(),"deleted":deleted,"dry_run":dry_run,"session_ids":ids})).await
}

async fn export(stream: &mut TcpStream, _config: &Config, id: &str) -> Result<()> {
    match jcode_base::session::Session::load(id) {
        Ok(session) => respond(stream, "200 OK", &json!({"session": session})).await,
        Err(_err) if !jcode_base::session::session_exists(id) => respond(stream, "404 Not Found", &json!({"detail":"session not found"})).await,
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail":err.to_string()})).await,
    }
}

async fn latest_descendant(stream: &mut TcpStream, config: &Config, id: &str) -> Result<()> {
    match session_infos(config, u64::MAX, true).await {
        Ok(sessions) => {
            if !sessions.iter().any(|s| s["id"] == id) {
                return respond(stream, "404 Not Found", &json!({"detail":"session not found"})).await;
            }
            let mut parent = id.to_string();
            let mut path = vec![id.to_string()];
            let mut seen = std::collections::HashSet::from([parent.clone()]);
            loop {
                let next = sessions.iter().filter(|s| {
                    jcode_base::session::Session::load(s["id"].as_str().unwrap_or_default())
                        .ok().is_some_and(|record| record.parent_id.as_deref() == Some(&parent))
                })
                    .max_by(|a, b| a["last_active"].as_f64().unwrap_or_default().total_cmp(&b["last_active"].as_f64().unwrap_or_default()));
                let Some(next) = next else { break };
                let Some(next_id) = next["id"].as_str() else { break };
                if !seen.insert(next_id.to_string()) { break; }
                parent = next_id.to_string(); path.push(parent.clone());
            }
            respond(stream, "200 OK", &json!({"requested_session_id":id,"session_id":parent,"path":path,"changed":parent != id})).await
        }
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail":err.to_string()})).await,
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn parse_import_records(records: &[Value]) -> std::result::Result<Vec<jcode_base::session::Session>, String> {
    records.iter().map(|value| {
        let record = value.get("record").or_else(|| value.get("session")).unwrap_or(value);
        let session: jcode_base::session::Session = serde_json::from_value(record.clone())
            .map_err(|_| "each session must contain a valid engine session record".to_string())?;
        if !valid_id(&session.id) { return Err("each session must contain a valid engine session record".into()); }
        Ok(session)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::{parse_import_records, route, valid_id};
    use crate::{Config, Request};
    use std::{net::SocketAddr, time::{SystemTime, UNIX_EPOCH}};
    use tokio::{io::AsyncReadExt, net::{TcpListener, TcpStream}};

    #[test]
    fn forgetting_a_session_leaves_nothing_and_the_driver_no_work() {
        use sovereign_prime::agent_loop::{ControlStore, Heartbeat, SessionGoal};
        let home = std::env::temp_dir().join(format!("forget-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        let observer = crate::observability::Observer::open(&home, "p", "m", None).unwrap();
        let (control, entries) = (ControlStore::open(&home).unwrap(), sovereign_prime::entries::EntryStore::open(&home).unwrap());
        for sid in ["gone", "kept"] {
            control.set_goal(sid, Some(&SessionGoal::new("ship"))).unwrap();
            control.upsert_heartbeat(&Heartbeat::new(sid, "check", 60)).unwrap();
            entries.park_save(&format!("req-{sid}"), sid, "{}", 5).unwrap();
            entries.learn_checkpoint(sid, 1, 5, 0, 1, false).unwrap();
            entries.set_watermark(sid, 3).unwrap();
            entries.set_setting(&format!("session_surface:{sid}"), "cron@1").unwrap();
            entries.set_setting(&format!("bot_session:chat-{sid}"), sid).unwrap();
        }
        let db = rusqlite::Connection::open(home.join("sovereign.db")).unwrap();
        for sid in ["gone", "kept"] {
            db.execute("INSERT INTO fact_turn(id,session_id,root_id,kind,model,provider,status,started_at_ms) VALUES(?1,?2,?1,'chat','m','p','complete',1)", [format!("run-{sid}"), sid.into()]).unwrap();
            db.execute("INSERT INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms) VALUES(?1,?2,?2,?2,'execute_tool','bash','complete',1)", [format!("span-{sid}"), format!("run-{sid}")]).unwrap();
            db.execute("INSERT INTO span_content(id,input) VALUES(?1,'x'),(?2,'y')", [format!("span-{sid}"), format!("run-{sid}")]).unwrap();
            db.execute("INSERT INTO approvals(session_id,tool,command_preview,decision,actor,at_ms) VALUES(?1,'bash','rm','allow','u',1)", [sid]).unwrap();
        }
        super::forget_rows(home.to_str().unwrap(), "gone").unwrap();
        assert_eq!(control.active_sessions().unwrap(), ["kept"]);
        let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        // Only "kept" is left: one row each, two for the tables holding two rows per session.
        for (table, rows) in [("fact_turn", 1), ("spans", 1), ("span_content", 2), ("approvals", 1), ("parked_approvals", 1), ("harness_watermark", 1), ("harness_learn_state", 1), ("engine_settings", 2), ("session_heartbeats", 1), ("session_goals", 1)] {
            assert_eq!(count(&format!("SELECT COUNT(*) FROM {table}")), rows, "{table}");
        }
        drop((observer, db));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn forgetting_a_blank_id_spares_every_attachment_and_store_errors_are_reported() {
        let home = std::env::temp_dir().join(format!("forget-blank-{}", std::process::id()));
        let kept = home.join("attachments/keep");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(kept.join("x.png"), b"x").unwrap();
        for id in ["", "///", "a/b"] {
            super::forget_rows(home.to_str().unwrap(), id).unwrap();
        }
        assert!(kept.join("x.png").is_file(), "no id can reach the attachments root");
        let file = std::env::temp_dir().join(format!("forget-not-a-dir-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let err = super::forget_rows(file.to_str().unwrap(), "s").unwrap_err().to_string();
        assert!(err.contains("could not clear all of s"), "{err}");
        std::fs::remove_dir_all(home).ok();
        std::fs::remove_file(file).ok();
    }

    #[test]
    fn session_ids_cannot_escape_the_session_store() {
        assert!(valid_id("s_123-a"));
        for id in ["", "../secret", "a/b", "a\\b", &"x".repeat(129)] {
            assert!(!valid_id(id), "accepted unsafe id: {id}");
        }
    }

    #[test]
    fn exported_session_envelope_is_importable_and_batches_validate_before_writes() {
        let session = jcode_base::session::Session::create_with_id("s_roundtrip".into(), None, Some("test".into()));
        let exported = serde_json::json!({"session": session});
        assert_eq!(parse_import_records(std::slice::from_ref(&exported)).unwrap()[0].id, "s_roundtrip");
        let invalid = [exported, serde_json::json!({"session": {"id":"../unsafe"}})];
        assert!(parse_import_records(&invalid).is_err());
    }

    #[tokio::test]
    async fn every_repaired_session_endpoint_reaches_its_engine_handler() {
        let config = Config {
            bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            token: "test".into(),
            version: "test".into(),
            legacy_socket: std::env::temp_dir().join(format!(
                "sovereign-session-rest-no-daemon-{}-{}.sock",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            )),
            default_cwd: "/tmp".into(),
            allow_non_loopback: false,
            provider: "local".into(),
            model: "test".into(),
            reasoning_efforts: Vec::new(),
            profile_model_applies: true,
            home: "/tmp".into(),
            complete: None,
            approval_secret: "test".into(),
            features: None,
            learning: None,
        };
        let cases = [
            ("GET", "/api/sessions/stats", "", "503"),
            ("GET", "/api/sessions/empty/count", "", "503"),
            ("DELETE", "/api/sessions/empty", "", "503"),
            ("POST", "/api/sessions/bulk-delete", r#"{"ids":[]}"#, "200"),
            ("POST", "/api/sessions/import", r#"{"sessions":[]}"#, "200"),
            ("POST", "/api/sessions/prune", r#"{"dry_run":true}"#, "503"),
            ("GET", "/api/sessions/missing/latest-descendant", "", "503"),
            ("GET", "/api/sessions/missing/export", "", "404"),
        ];

        for (method, path, body, expected_status) in cases {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
            let mut client = TcpStream::connect(address).await.unwrap();
            let mut server = accept.await.unwrap();
            let req = Request {
                method: method.into(),
                path: path.into(),
                query: None,
                headers: vec![("content-length".into(), body.len().to_string())],
                body_prefix: body.as_bytes().to_vec(),
            };

            assert!(route(&mut server, &req, &config).await.is_some(), "unmatched route: {method} {path}");
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {expected_status}")), "{method} {path}: {response}");
        }
    }
}

async fn list(stream: &mut TcpStream, req: &Request, config: &Config) -> Result<()> {
    let limit = query_u64(req, "limit").unwrap_or(50).clamp(1, 1000);
    match session_infos(config, limit, false).await {
        Ok(sessions) => {
            let total = sessions.len();
            respond(stream, "200 OK", &json!({"sessions": sessions, "total": total, "limit": limit, "offset": 0})).await
        }
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    }
}

/// Stored record for one session, archived ones included.
async fn find(config: &Config, id: &str) -> Result<Option<Value>> {
    if !valid_id(id) || !jcode_base::session::session_exists(id) { return Ok(None); }
    let session = jcode_base::session::Session::load(id)?;
    let runtime = harness_request(&config.legacy_socket, json!({"req":"list_sessions"})).await?;
    let active = runtime["sessions"].as_array().into_iter().flatten()
        .find(|info| info["session_id"] == id)
        .is_some_and(|info| matches!(info["status"].as_str(), Some("running" | "processing")));
    Ok(Some(json!({
        "id": session.id, "title": session.display_title_or_name(),
        "preview": session.messages.iter().rev().find_map(|m| m.content.iter().find_map(|b| match b {
            jcode_base::message::ContentBlock::Text { text, .. } => Some(text.clone()), _ => None,
        })).unwrap_or_default(),
        "source":"desktop", "started_at":session.created_at.timestamp() as f64,
        "last_active":session.updated_at.timestamp() as f64, "ended_at":session.updated_at.timestamp() as f64,
        "is_active":active,
        "message_count":session.messages.len(), "archived":false, "cwd":session.working_dir,
        "model":session.model, "parent_session_id":session.parent_id,
    })))
}

async fn get(stream: &mut TcpStream, config: &Config, id: &str) -> Result<()> {
    match find(config, id).await {
        Ok(Some(info)) => respond(stream, "200 OK", &info).await,
        Ok(None) => respond(stream, "404 Not Found", &json!({"detail": "session not found"})).await,
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    }
}

/// The whole conversation (snapshot plus journal), oldest first.
async fn transcript(_config: &Config, id: &str) -> Result<Vec<Value>> {
    let session = jcode_base::session::Session::load(id)?;
    Ok(session.messages.into_iter().map(|m| {
        let content = m.content.into_iter().filter_map(|b| match b {
            jcode_base::message::ContentBlock::Text { text, .. } => Some(text), _ => None,
        }).collect::<Vec<_>>().join("\n");
        json!({"role":serde_json::to_value(m.role).unwrap_or(Value::Null), "content":content, "text":content})
    }).collect())
}

async fn messages(stream: &mut TcpStream, req: &Request, config: &Config, id: &str) -> Result<()> {
    let all = match transcript(config, id).await {
        Ok(all) => all,
        Err(err) => return respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    };
    let len = all.len();
    let limit = query_u64(req, "limit").map_or(len, |l| l as usize);
    let offset = query_u64(req, "offset").unwrap_or(0) as usize;
    let oldest = super::auth::query_param(req.query.as_deref().unwrap_or_default(), "order").as_deref() == Some("oldest");
    // `latest` pages count back from the end but are returned oldest first.
    let (start, end) = if oldest {
        (offset.min(len), (offset + limit).min(len))
    } else {
        let end = len.saturating_sub(offset);
        (end.saturating_sub(limit), end)
    };
    let page = all[start..end].to_vec();
    let returned = page.len();
    let body = json!({
        "session_id": id,
        "messages": page,
        "pagination": {"limit": limit, "offset": offset, "order": if oldest { "oldest" } else { "latest" }, "returned": returned},
    });
    respond(stream, "200 OK", &body).await
}

async fn search(stream: &mut TcpStream, req: &Request, config: &Config) -> Result<()> {
    let q = super::auth::query_param(req.query.as_deref().unwrap_or_default(), "q").unwrap_or_default().to_lowercase();
    if q.trim().is_empty() {
        return respond(stream, "200 OK", &json!({"results": []})).await;
    }
    let sessions = match session_infos(config, u64::MAX, false).await {
        Ok(sessions) => sessions,
        Err(err) => return respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    };
    let mut results = Vec::new();
    for info in sessions {
        let id = info["id"].as_str().unwrap_or_default().to_string();
        let title_hit = info["title"].as_str().is_some_and(|t| t.to_lowercase().contains(&q));
        let hit = if title_hit {
            Some((info["title"].as_str().unwrap_or_default().to_string(), None))
        } else {
            transcript(config, &id).await.unwrap_or_default().into_iter().find_map(|m| {
                let text = m["content"].as_str()?;
                text.to_lowercase().contains(&q).then(|| (snippet(text, &q), m["role"].as_str().map(str::to_string)))
            })
        };
        if let Some((snippet, role)) = hit {
            results.push(json!({
                "session_id": id,
                "lineage_root": id,
                "snippet": snippet,
                "role": role,
                "model": null,
                "source": "sovereign",
                "session_started": info["started_at"],
                "last_active": info["last_active"],
            }));
        }
        if results.len() >= 50 {
            break;
        }
    }
    respond(stream, "200 OK", &json!({"results": results})).await
}

/// About 120 characters around the first match, on character boundaries.
fn snippet(text: &str, needle: &str) -> String {
    let lower = text.to_lowercase();
    let at = lower.find(needle).unwrap_or(0);
    let chars: Vec<char> = text.chars().collect();
    let pos = lower[..at].chars().count();
    let start = pos.saturating_sub(40);
    let end = (pos + 80).min(chars.len());
    chars[start..end].iter().collect::<String>().trim().to_string()
}

async fn delete(stream: &mut TcpStream, config: &Config, id: &str) -> Result<()> {
    match find(config, id).await {
        Ok(Some(info)) if info["is_active"] == true => {
            respond(stream, "409 Conflict", &json!({"detail": "session is running; stop it before deleting"})).await
        }
        Ok(Some(_)) => match delete_everywhere(config, id).await {
            Ok(_) => respond(stream, "200 OK", &json!({"ok": true})).await,
            Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
        },
        // No file, but an index row (or the daemon's memory) can still list
        // it: run the idempotent cleanup so a deleted chat leaves the list.
        Ok(None) if valid_id(id) => match delete_everywhere(config, id).await {
            Ok(_) => respond(stream, "200 OK", &json!({"ok": true, "already_absent": true})).await,
            Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
        },
        Ok(None) => respond(stream, "200 OK", &json!({"ok": true, "already_absent": true})).await,
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    }
}

async fn patch(stream: &mut TcpStream, req: &Request, config: &Config, id: &str) -> Result<()> {
    let body: Value = serde_json::from_slice(&read_body(stream, req).await?).unwrap_or(Value::Null);
    let socket = &config.legacy_socket;
    let result = async {
        if let Some(title) = body["title"].as_str() {
            // jcode renames only through an attachment.
            harness_requests(socket, &[
                json!({"req": "attach_session", "session_id": id}),
                json!({"req": "rename_session", "session_id": id, "title": title}),
            ])
            .await?;
        }
        if let Some(archived) = body["archived"].as_bool() {
            let req = if archived { "archive_session" } else { "restore_session" };
            harness_request(socket, json!({"req": req, "session_id": id})).await?;
        }
        // `pinned` and `unread` are desktop-side display state; the engine has
        // no auto-archive sweep for them to protect, so they are accepted as is.
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(()) => respond(stream, "200 OK", &json!({"ok": true, "title": body["title"]})).await,
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    }
}
