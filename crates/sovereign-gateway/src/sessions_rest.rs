//! Hermes's `/api/sessions` REST surface, served from the engine's session
//! store. Chats never live in Hermes's Python database, so none of these may
//! be proxied there: a delete or rename would silently hit a table that has
//! never seen the session.

use super::{Config, Request, harness_request, harness_requests, query_u64, read_body, respond, session_infos};
use anyhow::Result;
use serde_json::{Value, json};
use tokio::net::TcpStream;

/// Returns `None` when the path is not under `/api/sessions`.
pub(super) async fn route(stream: &mut TcpStream, req: &Request, config: &Config) -> Option<Result<()>> {
    let rest = req.path.strip_prefix("/api/sessions")?;
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    Some(match (req.method.as_str(), segments.as_slice()) {
        ("GET", []) => list(stream, req, config).await,
        ("GET", ["search"]) => search(stream, req, config).await,
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
    Ok(session_infos(config, 1000, true).await?.into_iter().find(|s| s["id"] == id))
}

async fn get(stream: &mut TcpStream, config: &Config, id: &str) -> Result<()> {
    match find(config, id).await {
        Ok(Some(info)) => respond(stream, "200 OK", &info).await,
        Ok(None) => respond(stream, "404 Not Found", &json!({"detail": "session not found"})).await,
        Err(err) => respond(stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
    }
}

/// The whole conversation (snapshot plus journal), oldest first.
async fn transcript(config: &Config, id: &str) -> Result<Vec<Value>> {
    let reply = harness_request(&config.legacy_socket, json!({"req": "peek_session", "session_id": id, "limit": 100_000})).await?;
    Ok(reply["messages"]
        .as_array()
        .map(|list| list.iter().map(|m| json!({"role": m["role"], "content": m["content"], "text": m["content"]})).collect())
        .unwrap_or_default())
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
    let sessions = match session_infos(config, 500, false).await {
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
        Ok(Some(_)) => match harness_request(&config.legacy_socket, json!({"req": "delete_session", "session_id": id})).await {
            Ok(_) => respond(stream, "200 OK", &json!({"ok": true})).await,
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
