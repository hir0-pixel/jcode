//! One WebSocket client: JSON-RPC 2.0 in, harness API calls out, harness
//! events translated back into Hermes `event` notifications.

use crate::approvals::{Client, Hub};
use crate::map::{self, Out, SessionState};
use crate::observability::Observer;
use crate::{Config, MAX_FRAME_BYTES};
use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

const HARNESS_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `prompt.submit` waits for jcode to acknowledge the message. The
/// turn itself streams afterwards and may run for minutes.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IN_FLIGHT: usize = 256;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const PARSE_ERROR: i64 = -32700;
const INTERNAL: i64 = -32603;

type Ws = WebSocketStream<TcpStream>;

pub async fn close(mut ws: Ws, code: u16, reason: &str) -> Result<()> {
    let frame = CloseFrame { code: CloseCode::from(code), reason: reason.chars().take(120).collect::<String>().into() };
    let _ = ws.close(Some(frame)).await;
    Ok(())
}

struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn unsupported(method: &str) -> Self {
        Self {
            code: METHOD_NOT_FOUND,
            message: format!("{method} is not supported by this engine"),
            data: Some(json!({ "reason": "not_supported_by_engine", "method": method })),
        }
    }
    fn params(message: &str) -> Self {
        Self { code: INVALID_PARAMS, message: message.into(), data: None }
    }
    fn internal(err: anyhow::Error) -> Self {
        Self { code: INTERNAL, message: err.to_string(), data: None }
    }
}

/// A WebSocket to the Hermes feature backend for one desktop connection.
struct Upstream {
    tx: mpsc::Sender<String>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Server requests from the backend are relayed to the desktop under this
/// prefix so their ids can never collide with ours.
const UPSTREAM_REQUEST_PREFIX: &str = "up-";
const FORWARD_TIMEOUT: Duration = Duration::from_secs(120);

struct Conn {
    config: Arc<Config>,
    to_ws: mpsc::Sender<Message>,
    /// Control link: session-less requests (list, ping).
    control: Mutex<Option<mpsc::Sender<String>>>,
    /// One bridge link per session: jcode's API bridge attaches exactly one
    /// session per connection, while the desktop multiplexes many.
    links: Mutex<HashMap<String, mpsc::Sender<String>>>,
    link_tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    /// jcode `SessionInfo` for sessions this client created or attached, so a
    /// new, still-empty session (not yet persisted) appears in `session.list`.
    known: Mutex<HashMap<String, Value>>,
    next_id: AtomicU64,
    next_server_request: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    sessions: Mutex<HashMap<String, SessionState>>,
    /// Hermes server-request id → (session, jcode permission request id).
    approvals: Mutex<HashMap<String, (String, String)>>,
    in_flight: Arc<tokio::sync::Semaphore>,
    /// `prompt.submit` callers waiting for jcode's `message_accepted`.
    accept_waiters: Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>,
    /// Connection to Hermes's Python backend, opened on first forwarded call.
    upstream: Mutex<Option<Arc<Upstream>>>,
    next_forward: AtomicU64,
    hub: Arc<Hub>,
    /// This connection as seen by the approval hub.
    client: Arc<Client>,
    observer: Arc<Observer>,
}

impl Conn {
    /// Send one harness request and await its direct reply.
    async fn call(&self, request: Value) -> Result<Value> {
        let link = self.route(&request).await?;
        self.call_on(&link, request).await
    }

    async fn call_on(&self, link: &mpsc::Sender<String>, request: Value) -> Result<Value> {
        let rx = self.send_on(link, request).await?;
        let reply = tokio::time::timeout(HARNESS_CALL_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("engine did not reply in time"))?
            .map_err(|_| anyhow!("engine connection closed"))?;
        check_reply(reply)
    }

    /// The link a request belongs on: its session's link, else the control link.
    async fn route(&self, request: &Value) -> Result<mpsc::Sender<String>> {
        if let Some(sid) = request["session_id"].as_str() {
            if let Some(link) = self.links.lock().await.get(sid) {
                return Ok(link.clone());
            }
        }
        self.control.lock().await.clone().ok_or_else(|| anyhow!("engine connection closed"))
    }

    async fn send_on(&self, link: &mpsc::Sender<String>, request: Value) -> Result<oneshot::Receiver<Value>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut frame = request;
        frame["v"] = json!(1);
        frame["id"] = json!(id);
        link.send(frame.to_string()).await.map_err(|_| anyhow!("engine connection closed"))?;
        Ok(rx)
    }

    /// Open a new in-process bridge link and start pumping its frames.
    async fn open_link(self: &Arc<Self>) -> Result<mpsc::Sender<String>> {
        let (ours, theirs) = tokio::io::duplex(MAX_FRAME_BYTES);
        let (their_read, their_write) = tokio::io::split(theirs);
        let bridge = tokio::spawn(jcode_harness_api_server::run_bridge_stream(
            their_read,
            their_write,
            self.config.legacy_socket.clone(),
        ));
        let (our_read, mut our_write) = tokio::io::split(ours);
        let hello = json!({"v": 1, "id": 0, "req": "hello", "min_version": 1, "max_version": 1, "client": "sovereign-gateway"});
        our_write.write_all(format!("{hello}\n").as_bytes()).await?;
        let mut lines = BufReader::new(our_read).lines();
        let hello_ok: Value = serde_json::from_str(&lines.next_line().await?.ok_or_else(|| anyhow!("engine closed"))?)?;
        if hello_ok["ev"] != "hello_ok" {
            bridge.abort();
            return Err(anyhow!("engine unavailable"));
        }
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let writer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if our_write.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let weak = Arc::downgrade(self);
        let reader = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(conn) = weak.upgrade() else { break };
                if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                    conn.on_harness_frame(frame).await;
                }
            }
        });
        self.link_tasks.lock().await.extend([bridge.abort_handle(), writer.abort_handle(), reader.abort_handle()]);
        Ok(tx)
    }

    /// Attach this connection to `session_id` once.
    async fn ensure_attached(self: &Arc<Self>, session_id: &str) -> Result<Value> {
        if self.links.lock().await.contains_key(session_id) {
            return Ok(Value::Null);
        }
        let link = self.open_link().await?;
        match self.call_on(&link, json!({ "req": "attach_session", "session_id": session_id })).await {
            Ok(reply) => {
                self.links.lock().await.insert(session_id.to_string(), link);
                self.client.sessions.lock().await.insert(session_id.to_string());
                if reply["session"].is_object() {
                    self.known.lock().await.insert(session_id.to_string(), reply["session"].clone());
                }
                Ok(reply)
            }
            Err(err) => Err(err),
        }
    }

    /// Send a message and wait until jcode acknowledges it (or rejects it).
    async fn submit(self: &Arc<Self>, session_id: &str, text: &str) -> Result<()> {
        self.ensure_attached(session_id).await?;
        let (accepted_tx, accepted_rx) = oneshot::channel();
        self.accept_waiters.lock().await.entry(session_id.to_string()).or_default().push(accepted_tx);
        let link = self.route(&json!({ "session_id": session_id })).await?;
        let reply = self.send_on(&link, json!({ "req": "send_message", "session_id": session_id, "content": text })).await?;
        tokio::select! {
            _ = accepted_rx => Ok(()),
            reply = reply => match reply {
                Ok(reply) => check_reply(reply).map(|_| ()),
                Err(_) => Err(anyhow!("engine connection closed")),
            },
            _ = tokio::time::sleep(ACCEPT_TIMEOUT) => Err(anyhow!("engine did not accept the message in time")),
        }
    }

    async fn send_json(&self, value: Value) {
        let _ = self.to_ws.send(Message::Text(value.to_string())).await;
    }

    async fn emit(&self, ty: &str, session_id: Option<&str>, payload: Value) {
        let mut params = json!({ "type": ty, "payload": payload });
        if let Some(sid) = session_id {
            let seq = self.sessions.lock().await.entry(sid.to_string()).or_default().next_seq();
            params["session_id"] = json!(sid);
            params["seq"] = json!(seq);
        }
        self.send_json(json!({ "jsonrpc": "2.0", "method": "event", "params": params })).await;
    }

    async fn on_harness_frame(&self, frame: Value) {
        if std::env::var_os("SOVEREIGN_GATEWAY_TRACE").is_some() {
            eprintln!("sovereign-gateway: harness {}", frame.to_string().chars().take(300).collect::<String>());
        }
        if frame["ev"] == "message_accepted" {
            if let Some(sid) = frame["session_id"].as_str() {
                for waiter in self.accept_waiters.lock().await.remove(sid).unwrap_or_default() {
                    let _ = waiter.send(());
                }
            }
        }
        if let Some(id) = frame["reply_to"].as_u64() {
            if let Some(tx) = self.pending.lock().await.remove(&id) {
                let _ = tx.send(frame);
                return;
            }
        }
        self.observer.harness_event(&frame);
        let outs = map::map_event(&frame, &mut *self.sessions.lock().await);
        for out in outs {
            match out {
                Out::Event { ty, session_id, payload } => {
                    self.observer.event(&session_id, ty, &payload);
                    self.emit(ty, Some(&session_id), payload).await;
                }
                Out::Approval { session_id, request_id, tool_name, description } => {
                    let id = format!("srv-{}", self.next_server_request.fetch_add(1, Ordering::Relaxed));
                    self.approvals.lock().await.insert(id.clone(), (session_id.clone(), request_id.clone()));
                    self.send_json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": "approval",
                        "params": {
                            "session_id": session_id,
                            "request_id": request_id,
                            "command": description,
                            "description": description,
                            "tool_name": tool_name,
                            "choices": ["once", "session", "always", "deny"],
                            "allow_permanent": true,
                            "allow_session": true,
                        }
                    }))
                    .await;
                }
            }
        }
    }

    async fn resolve_approval(&self, session_id: &str, request_id: &str, choice: &str) -> Result<()> {
        self.call(json!({
            "req": "permission_response",
            "session_id": session_id,
            "request_id": request_id,
            "decision": map::approval_decision(choice),
        }))
        .await?;
        Ok(())
    }

    async fn dispatch(self: &Arc<Self>, method: &str, p: &Value) -> Result<Value, RpcError> {
        let sid = || p["session_id"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| RpcError::params("session_id is required"));
        let call = |req: Value| async move { self.call(req).await.map_err(RpcError::internal) };
        match method {
            "ping" | "gateway.ping" => Ok(json!({})),
            "setup.status" => Ok(json!({
                "provider_configured": true,
                "ready": true,
                "ok": true,
                "free_tier": false,
                "other_providers": false,
                "inference_provider": self.config.provider,
            })),
            "setup.runtime_check" => Ok(json!({
                "ok": true,
                "provider": self.config.provider,
                "model": self.config.model,
                "source": "engine",
            })),
            "free_tier.status" => Ok(json!({
                "has_guest": false, "enabled": false, "available": false,
                "notice_pending": false, "model": "", "label": "",
            })),
            "model.options" => Ok(json!({
                "providers": [{
                    "slug": self.config.provider,
                    "name": self.config.provider,
                    "models": [self.config.model],
                    "total_models": 1,
                    "is_current": true,
                    "authenticated": true,
                }],
                "model": self.config.model,
                "provider": self.config.provider,
            })),
            "wake.status" => Ok(json!({
                "listening": false, "owned_by_caller": false, "phrase": "", "provider": "",
                "configured_surface": "", "input_device": {}, "available": false,
                "hint": "Wake word is not available in this engine.", "enabled": false,
                "audio_silent": false, "capture": "off", "local_input_available": false,
                "sample_rate": 16000, "frame_length": 512,
            })),
            "session.active_list" => {
                let sessions = self.sessions.lock().await;
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                let items: Vec<Value> = sessions
                    .iter()
                    .filter(|(_, s)| s.turn_active())
                    .map(|(id, s)| json!({
                        "current": false, "id": id, "session_key": id, "last_active": now, "started_at": now,
                        "message_count": 0, "model": s.model.clone().unwrap_or_default(), "preview": "",
                        "status": "streaming", "title": "",
                    }))
                    .collect();
                Ok(json!({ "sessions": items }))
            }
            "commands.catalog" => {
                let pairs = json!([
                    ["/refine", "Learn one durable improvement from this session (Continual Harness)"],
                    ["/refine rollback", "Undo the last /refine"],
                    ["/harness", "Show the learned instructions"],
                ]);
                Ok(json!({
                    "pairs": pairs, "sub": {}, "canon": {}, "commands": {},
                    "categories": [{ "name": "Harness", "pairs": pairs }],
                    "skills": {}, "skill_count": 0, "warning": "",
                }))
            }
            "profiles.list" => Ok(json!({
                "profiles": [{
                    "name": "default", "path": self.config.home, "is_default": true,
                    "model": self.config.model, "provider": self.config.provider,
                    "display_name": "Default", "description": "", "skill_count": 0,
                }],
                "bot_mode_protocol": false,
            })),
            "pet.info" => Ok(json!({ "enabled": false })),
            "subagent.list" => Ok(json!({ "subagents": [], "delegations": [] })),
            "process.list" => Ok(json!({ "processes": [] })),
            "session.control.read" => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                Ok(json!({ "control": { "goal": null, "loop": null, "heartbeat": null, "revision": "0", "updated_at": now } }))
            }
            "complete.path" => {
                let word = p["word"].as_str().unwrap_or_default().to_string();
                let cwd = p["cwd"].as_str().unwrap_or(&self.config.default_cwd).to_string();
                let items = tokio::task::spawn_blocking(move || map::complete_path(&word, &cwd)).await.unwrap_or_default();
                Ok(json!({ "items": items }))
            }
            "slash.exec" => {
                let command = p["command"].as_str().unwrap_or_default().chars().take(80).collect::<String>();
                let words: Vec<&str> = command.trim_start_matches('/').split_whitespace().collect();
                if let Some(message) = self.harness_command(&words, p["session_id"].as_str()).await {
                    return Ok(json!({ "status": "ok", "type": "exec", "output": message, "message": message }));
                }
                crate::note_unsupported("slash", &command);
                let message = format!("/{} is not available in this engine yet.", command.trim_start_matches('/'));
                Ok(json!({ "status": "error", "message": message, "output": message }))
            }
            "projects.tree" => Ok(json!({ "projects": [], "active_id": null, "scoped_session_ids": [] })),
            "gateway.capabilities" => Ok(json!({ "per_session_exclusive_submit": false })),
            "client.capabilities" => Ok(json!({ "server_requests": ["approval"] })),
            "session.create" => {
                let cwd = p["cwd"].as_str().unwrap_or(&self.config.default_cwd).to_string();
                let link = self.open_link().await.map_err(RpcError::internal)?;
                let reply = self
                    .call_on(&link, json!({ "req": "create_session", "working_dir": cwd }))
                    .await
                    .map_err(RpcError::internal)?;
                let id = reply["session"]["session_id"].as_str().unwrap_or_default().to_string();
                self.links.lock().await.insert(id.clone(), link);
                self.client.sessions.lock().await.insert(id.clone());
                self.known.lock().await.insert(id.clone(), reply["session"].clone());
                if let Some(title) = p["title"].as_str().filter(|t| !t.is_empty()) {
                    let _ = self.call(json!({ "req": "rename_session", "session_id": id, "title": title })).await;
                }
                let sessions = self.sessions.lock().await;
                Ok(json!({
                    "session_id": id,
                    "stored_session_id": id,
                    "message_count": 0,
                    "messages": [],
                    "info": map::live_info(&id, sessions.get(&id), &cwd, &self.config.version, &self.config.model, &self.config.provider),
                }))
            }
            "session.resume" | "session.activate" => {
                let id = sid()?.to_string();
                let attached = self.ensure_attached(&id).await.map_err(RpcError::internal)?;
                let cwd = attached["session"]["working_dir"].as_str().unwrap_or(&self.config.default_cwd).to_string();
                let messages = if p["omit_messages"].as_bool() == Some(true) {
                    Vec::new()
                } else {
                    let history = call(json!({ "req": "get_history", "session_id": id })).await?;
                    map::transcript(&history["messages"])
                };
                let sessions = self.sessions.lock().await;
                let running = sessions.get(&id).is_some_and(SessionState::turn_active);
                Ok(json!({
                    "session_id": id,
                    "stored_session_id": id,
                    "message_count": messages.len(),
                    "messages": messages,
                    "running": running,
                    "info": map::live_info(&id, sessions.get(&id), &cwd, &self.config.version, &self.config.model, &self.config.provider),
                }))
            }
            "session.history" => {
                let id = sid()?;
                self.ensure_attached(id).await.map_err(RpcError::internal)?;
                let history = call(json!({ "req": "get_history", "session_id": id })).await?;
                let messages = map::transcript(&history["messages"]);
                Ok(json!({ "count": messages.len(), "messages": messages }))
            }
            "session.list" => {
                let mut req = json!({ "req": "list_sessions" });
                if let Some(limit) = p["limit"].as_u64() {
                    req["limit"] = json!(limit.min(1000));
                }
                let reply = call(req).await?;
                let mut rows: Vec<Value> = reply["sessions"]
                    .as_array()
                    .map(|list| list.iter().filter(|s| s["parent_session_id"].is_null()).map(map::session_row).collect())
                    .unwrap_or_default();
                for (id, info) in self.known.lock().await.iter() {
                    if info["parent_session_id"].is_null() && !rows.iter().any(|r| r["id"] == id.as_str()) {
                        rows.insert(0, map::session_row(info));
                    }
                }
                Ok(json!({ "sessions": rows }))
            }
            "prompt.submit" => {
                let id = sid()?.to_string();
                let text = map::prompt_text(&p["text"]);
                if text.trim().is_empty() {
                    return Err(RpcError::params("text is required"));
                }
                let busy = self.sessions.lock().await.get(&id).is_some_and(SessionState::turn_active);
                self.ensure_attached(&id).await.map_err(RpcError::internal)?;
                // jcode does not auto-title sessions; Hermes titles from the first
                // prompt. Rename before sending: jcode refuses renames mid-turn.
                let untitled = {
                    let known = self.known.lock().await;
                    known.get(&id).is_none_or(|info| info["title"].as_str().is_none_or(str::is_empty))
                };
                if untitled && !busy && let Some(title) = map::derive_title(p["title_preview"].as_str(), &text) {
                    if self.call(json!({ "req": "rename_session", "session_id": id, "title": title })).await.is_ok() {
                        if let Some(info) = self.known.lock().await.get_mut(&id) {
                            info["title"] = json!(title);
                        }
                    }
                }
                let run = self.observer.start_turn(&id, &text);
                if let Err(err) = self.submit(&id, &text).await {
                    self.observer.failed_submit(&id, &run, &err.to_string());
                    return Err(RpcError::internal(err));
                }
                Ok(json!({ "status": if busy { "queued" } else { "streaming" } }))
            }
            "session.steer" => {
                let id = sid()?;
                let text = map::prompt_text(&p["text"]);
                self.ensure_attached(id).await.map_err(RpcError::internal)?;
                call(json!({ "req": "soft_interrupt", "session_id": id, "content": text })).await?;
                Ok(json!({ "status": "steered" }))
            }
            "session.interrupt" => {
                let id = sid()?;
                let busy = self.sessions.lock().await.get(id).is_some_and(SessionState::turn_active)
                    || self.observer.has_active_run(id);
                self.ensure_attached(id).await.map_err(RpcError::internal)?;
                call(json!({ "req": "cancel", "session_id": id })).await?;
                Ok(json!({
                    "status": if busy { "interrupted" } else { "not_interrupted" },
                    "interrupted": busy,
                }))
            }
            "session.title" => {
                let id = sid()?;
                let title = p["title"].as_str().unwrap_or_default();
                self.ensure_attached(id).await.map_err(RpcError::internal)?;
                call(json!({ "req": "rename_session", "session_id": id, "title": title })).await?;
                Ok(json!({ "session_id": id, "title": title }))
            }
            "session.compress" => {
                let id = sid()?;
                self.ensure_attached(id).await.map_err(RpcError::internal)?;
                call(json!({ "req": "compact", "session_id": id })).await?;
                Ok(json!({ "status": "compressed" }))
            }
            "session.usage" => {
                let id = sid()?;
                let sessions = self.sessions.lock().await;
                let usage = sessions.get(id).map(SessionState::usage_json).unwrap_or_else(|| json!({}));
                Ok(usage)
            }
            "config.get" => {
                let key = p["key"].as_str().unwrap_or_default();
                match key {
                    "project" => {
                        let cwd = p["cwd"].as_str().filter(|c| !c.is_empty()).unwrap_or(&self.config.default_cwd).to_string();
                        let lookup = cwd.clone();
                        let branch = tokio::task::spawn_blocking(move || map::git_branch(&lookup)).await.ok().flatten();
                        Ok(json!({ "cwd": cwd, "branch": branch }))
                    }
                    "model" | "provider" => Ok(json!({
                        "value": if key == "model" { &self.config.model } else { &self.config.provider },
                        "model": self.config.model,
                        "provider": self.config.provider,
                    })),
                    _ => Ok(json!({ "value": null })),
                }
            }
            "approval.received" => {
                sid()?;
                Ok(json!({ "acknowledged": true }))
            }
            "approval.pending" => {
                let id = sid()?;
                Ok(json!({ "approvals": self.hub.pending_for(id).await }))
            }
            "approval.respond" => {
                let id = sid()?.to_string();
                let choice = p["choice"].as_str().unwrap_or("deny").to_string();
                let wanted = p["request_id"].as_str().map(str::to_string);
                let matching: Vec<(String, String)> = {
                    let mut approvals = self.approvals.lock().await;
                    let keys: Vec<String> = approvals
                        .iter()
                        .filter(|(_, (s, r))| *s == id && wanted.as_ref().is_none_or(|w| w == r))
                        .map(|(k, _)| k.clone())
                        .collect();
                    keys.into_iter().filter_map(|k| approvals.remove(&k)).collect()
                };
                let mut resolved = matching.len();
                resolved += self.hub.answer_session(&id, wanted.as_deref(), &choice).await;
                for (session, request) in matching {
                    self.resolve_approval(&session, &request, &choice).await.map_err(RpcError::internal)?;
                }
                Ok(json!({ "resolved": resolved }))
            }
            _ => self.forward(method, p).await,
        }
    }

    /// `/refine`, `/refine rollback`, `/harness`. `None` for other commands.
    async fn harness_command(self: &Arc<Self>, words: &[&str], session_id: Option<&str>) -> Option<String> {
        use sovereign_prime::harness::{Harness, Outcome, Turn};
        let harness = Harness::new(std::path::Path::new(&self.config.home));
        let result: anyhow::Result<String> = match words {
            ["harness", ..] => Ok(match harness.current() {
                text if text.trim().is_empty() => "No learned instructions yet. Run /refine after a session worth learning from.".into(),
                text => format!("Learned instructions (applied to new sessions):\n\n{}", text.trim()),
            }),
            ["refine", "rollback", ..] => harness.rollback().map(|previous| {
                if previous.trim().is_empty() {
                    "Rolled back: learned instructions are empty again.".to_string()
                } else {
                    format!("Rolled back to the previous learned instructions:\n\n{}", previous.trim())
                }
            }),
            ["refine", ..] => async {
                let sid = session_id.filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("/refine needs an open session"))?;
                let complete = self.config.complete.clone().ok_or_else(|| anyhow!("no model is available for /refine"))?;
                self.ensure_attached(sid).await?;
                let history = self.call(json!({ "req": "get_history", "session_id": sid })).await?;
                let turns: Vec<Turn> = history["messages"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|m| Turn {
                                role: m["role"].as_str().unwrap_or_default().to_string(),
                                text: m["content"].as_str().unwrap_or_default().to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if turns.is_empty() {
                    return Ok("Nothing to learn from yet: this session has no messages.".to_string());
                }
                let (system, user) = harness.refine_request(&turns);
                let reply = complete(system, user).await?;
                Ok(match harness.apply(&reply, &turns)? {
                    Outcome::Updated { changes, added, removed } => {
                        let mut text = format!("Learned: {changes}\n");
                        for line in &added {
                            text.push_str(&format!("+ {line}\n"));
                        }
                        for line in &removed {
                            text.push_str(&format!("- {line}\n"));
                        }
                        text.push_str("Applies to new sessions. Undo with /refine rollback.");
                        text
                    }
                    Outcome::NoChange { reason } => format!("No change: {reason}"),
                })
            }
            .await,
            _ => return None,
        };
        Some(match result {
            Ok(message) => message,
            Err(err) => format!("{err:#}"),
        })
    }

    /// Forward a method the Rust harness does not own to Hermes's backend.
    async fn forward(self: &Arc<Self>, method: &str, params: &Value) -> Result<Value, RpcError> {
        if std::env::var_os("SOVEREIGN_TRACE_FORWARD").is_some() {
            eprintln!("sovereign: forward RPC {method}");
        }
        // Only methods Hermes actually defines may wake the Python backend;
        // anything else is answered here without starting it.
        if !crate::contract_methods().contains(method) {
            return Err(RpcError { code: METHOD_NOT_FOUND, message: format!("unknown method {method}"), data: None });
        }
        let Some(features) = self.config.features.clone() else {
            crate::note_unsupported("rpc", method);
            return Err(RpcError::unsupported(method));
        };
        let upstream = self.upstream(&features).await.map_err(RpcError::internal)?;
        let id = format!("fwd-{}", self.next_forward.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        upstream.pending.lock().await.insert(id.clone(), tx);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        upstream.tx.send(frame.to_string()).await.map_err(|_| RpcError::internal(anyhow!("feature backend connection closed")))?;
        let reply = tokio::time::timeout(FORWARD_TIMEOUT, rx)
            .await
            .map_err(|_| RpcError::internal(anyhow!("{method} timed out in the feature backend")))?
            .map_err(|_| RpcError::internal(anyhow!("the feature backend restarted; try again")))?;
        features.touch();
        match reply.get("error") {
            Some(err) => Err(RpcError {
                code: err["code"].as_i64().unwrap_or(INTERNAL),
                message: err["message"].as_str().unwrap_or("feature backend error").to_string(),
                data: err.get("data").cloned(),
            }),
            None => Ok(reply["result"].clone()),
        }
    }

    /// The live upstream connection, (re)opened as needed.
    async fn upstream(self: &Arc<Self>, features: &crate::features::Features) -> Result<Arc<Upstream>> {
        let mut slot = self.upstream.lock().await;
        if let Some(up) = slot.as_ref() {
            if !up.tx.is_closed() {
                return Ok(up.clone());
            }
        }
        let port = features.port().await?;
        let url = format!("ws://127.0.0.1:{port}/api/ws?token={}", features.token);
        let (ws, _) = tokio_tungstenite::connect_async(url).await.context("connecting to the feature backend")?;
        let (mut ws_tx, mut ws_rx) = ws.split();
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let writer = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if ws_tx.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        });
        let up = Arc::new_cyclic(|weak: &std::sync::Weak<Upstream>| {
            let weak_up = weak.clone();
            let conn = Arc::downgrade(self);
            let reader = tokio::spawn(async move {
                while let Some(Ok(msg)) = ws_rx.next().await {
                    let Message::Text(text) = msg else { continue };
                    let Ok(mut frame) = serde_json::from_str::<Value>(&text) else { continue };
                    let (Some(conn), Some(up)) = (conn.upgrade(), weak_up.upgrade()) else { break };
                    if frame.get("method").is_none() {
                        if let Some(id) = frame["id"].as_str() {
                            if let Some(tx) = up.pending.lock().await.remove(id) {
                                let _ = tx.send(frame);
                            }
                        }
                    } else if frame["method"] == "event" {
                        // The desktop already has our own gateway.ready.
                        if frame["params"]["type"] != "gateway.ready" {
                            conn.send_json(frame).await;
                        }
                    } else if frame.get("id").is_some() {
                        let raw = match &frame["id"] {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        frame["id"] = json!(format!("{UPSTREAM_REQUEST_PREFIX}{raw}"));
                        conn.send_json(frame).await;
                    }
                }
            });
            Upstream { tx, pending: Mutex::new(HashMap::new()), tasks: vec![writer.abort_handle(), reader.abort_handle()] }
        });
        *slot = Some(up.clone());
        Ok(up)
    }

    /// A reply to one of our server requests (approvals, or relayed ones).
    async fn on_client_reply(&self, frame: &Value) {
        let Some(id) = frame["id"].as_str() else { return };
        if let Some(raw) = id.strip_prefix(UPSTREAM_REQUEST_PREFIX) {
            if let Some(up) = self.upstream.lock().await.clone() {
                let mut reply = frame.clone();
                reply["id"] = raw.parse::<u64>().map(|n| json!(n)).unwrap_or_else(|_| json!(raw));
                let _ = up.tx.send(reply.to_string()).await;
            }
            return;
        }
        let choice = frame["result"]["choice"].as_str().unwrap_or("deny");
        if self.hub.answer(id, choice).await {
            return;
        }
        let Some((session, request)) = self.approvals.lock().await.remove(id) else { return };
        let _ = self.resolve_approval(&session, &request, choice).await;
    }
}

fn check_reply(reply: Value) -> Result<Value> {
    if reply["ev"] == "error" {
        return Err(anyhow!("{}", reply["message"].as_str().unwrap_or("engine error")));
    }
    Ok(reply)
}

fn rpc_error(id: Value, err: RpcError) -> Value {
    let mut error = json!({ "code": err.code, "message": err.message });
    if let Some(data) = err.data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

pub async fn run(ws: Ws, config: Arc<Config>, hub: Arc<Hub>, observer: Arc<Observer>) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (to_ws, mut ws_out) = mpsc::channel::<Message>(1024);
    let client = Arc::new(Client { id: hub.next_client_id(), to_ws: to_ws.clone(), sessions: Mutex::new(Default::default()) });
    hub.add(client.clone()).await;
    let conn = Arc::new(Conn {
        config,
        to_ws,
        control: Mutex::new(None),
        links: Mutex::new(HashMap::new()),
        link_tasks: Mutex::new(Vec::new()),
        known: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        next_server_request: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        approvals: Mutex::new(HashMap::new()),
        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
        accept_waiters: Mutex::new(HashMap::new()),
        upstream: Mutex::new(None),
        next_forward: AtomicU64::new(1),
        hub: hub.clone(),
        client: client.clone(),
        observer,
    });

    // Control link first: if the engine is unreachable, refuse the client.
    match conn.open_link().await {
        Ok(control) => *conn.control.lock().await = Some(control),
        Err(_) => {
            let _ = ws_tx.send(Message::Close(Some(CloseFrame { code: CloseCode::from(1011), reason: "engine unavailable".into() }))).await;
            return Ok(());
        }
    }

    let writer = tokio::spawn(async move {
        while let Some(msg) = ws_out.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let epoch = crate::auth::generate_token()[..16].to_string();
    conn.emit("gateway.ready", None, json!({ "skin": {}, "change_events": false, "replay_epoch": epoch })).await;

    while let Some(msg) = ws_rx.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        if std::env::var_os("SOVEREIGN_GATEWAY_TRACE").is_some() {
            eprintln!("sovereign-gateway: client {}", text.chars().take(300).collect::<String>());
        }
        let frame: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                let err = RpcError { code: PARSE_ERROR, message: "parse error".into(), data: None };
                conn.send_json(rpc_error(Value::Null, err)).await;
                continue;
            }
        };
        if frame.get("method").is_none() && (frame.get("result").is_some() || frame.get("error").is_some()) {
            conn.on_client_reply(&frame).await;
            continue;
        }
        let Some(method) = frame["method"].as_str().map(str::to_string) else {
            let err = RpcError { code: -32600, message: "invalid request".into(), data: None };
            conn.send_json(rpc_error(frame["id"].clone(), err)).await;
            continue;
        };
        let id = frame["id"].clone();
        let Ok(permit) = conn.in_flight.clone().try_acquire_owned() else {
            let err = RpcError { code: INTERNAL, message: "too many requests in flight".into(), data: None };
            conn.send_json(rpc_error(id, err)).await;
            continue;
        };
        let conn = conn.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
            let result = conn.dispatch(&method, &params).await;
            if id.is_null() {
                return; // notification: no reply
            }
            let reply = match result {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(err) => rpc_error(id, err),
            };
            conn.send_json(reply).await;
        });
    }

    writer.abort();
    hub.remove(client.id).await;
    for task in conn.link_tasks.lock().await.drain(..) {
        task.abort();
    }
    Ok(())
}
