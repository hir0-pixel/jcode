//! One WebSocket client: JSON-RPC 2.0 in, harness API calls out, harness
//! events translated back into Hermes `event` notifications.

use crate::map::{self, Out, SessionState};
use crate::{Config, MAX_FRAME_BYTES};
use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
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

struct Conn {
    config: Arc<Config>,
    to_ws: mpsc::Sender<Message>,
    to_harness: mpsc::Sender<String>,
    next_id: AtomicU64,
    next_server_request: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    sessions: Mutex<HashMap<String, SessionState>>,
    /// Hermes server-request id → (session, jcode permission request id).
    approvals: Mutex<HashMap<String, (String, String)>>,
    in_flight: Arc<tokio::sync::Semaphore>,
    /// Sessions this connection is attached to; jcode only accepts
    /// session-scoped requests on an attached connection.
    attached: Mutex<HashSet<String>>,
    /// `prompt.submit` callers waiting for jcode's `message_accepted`.
    accept_waiters: Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>,
}

impl Conn {
    /// Send one harness request and await its direct reply.
    async fn call(&self, request: Value) -> Result<Value> {
        let rx = self.send_request(request).await?;
        let reply = tokio::time::timeout(HARNESS_CALL_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("engine did not reply in time"))?
            .map_err(|_| anyhow!("engine connection closed"))?;
        check_reply(reply)
    }

    async fn send_request(&self, request: Value) -> Result<oneshot::Receiver<Value>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut frame = request;
        frame["v"] = json!(1);
        frame["id"] = json!(id);
        self.to_harness.send(frame.to_string()).await.map_err(|_| anyhow!("engine connection closed"))?;
        Ok(rx)
    }

    /// Attach this connection to `session_id` once.
    async fn ensure_attached(&self, session_id: &str) -> Result<Value> {
        if self.attached.lock().await.contains(session_id) {
            return Ok(Value::Null);
        }
        let reply = self.call(json!({ "req": "attach_session", "session_id": session_id })).await?;
        self.attached.lock().await.insert(session_id.to_string());
        Ok(reply)
    }

    /// Send a message and wait until jcode acknowledges it (or rejects it).
    async fn submit(&self, session_id: &str, text: &str) -> Result<()> {
        self.ensure_attached(session_id).await?;
        let (accepted_tx, accepted_rx) = oneshot::channel();
        self.accept_waiters.lock().await.entry(session_id.to_string()).or_default().push(accepted_tx);
        let reply = self.send_request(json!({ "req": "send_message", "session_id": session_id, "content": text })).await?;
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
        let outs = map::map_event(&frame, &mut *self.sessions.lock().await);
        for out in outs {
            match out {
                Out::Event { ty, session_id, payload } => self.emit(ty, Some(&session_id), payload).await,
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

    async fn dispatch(&self, method: &str, p: &Value) -> Result<Value, RpcError> {
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
            "commands.catalog" => Ok(json!({
                "pairs": [], "sub": {}, "canon": {}, "commands": {}, "categories": [],
                "skills": {}, "skill_count": 0, "warning": "",
            })),
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
                crate::note_unsupported("slash", &command);
                let message = format!("/{} is not available in this engine yet.", command.trim_start_matches('/'));
                Ok(json!({ "status": "error", "message": message, "output": message }))
            }
            "projects.tree" => Ok(json!({ "projects": [], "active_id": null, "scoped_session_ids": [] })),
            "gateway.capabilities" => Ok(json!({ "per_session_exclusive_submit": false })),
            "client.capabilities" => Ok(json!({ "server_requests": ["approval"] })),
            "session.create" => {
                let cwd = p["cwd"].as_str().unwrap_or(&self.config.default_cwd).to_string();
                let reply = call(json!({ "req": "create_session", "working_dir": cwd })).await?;
                let id = reply["session"]["session_id"].as_str().unwrap_or_default().to_string();
                self.attached.lock().await.insert(id.clone());
                if let Some(title) = p["title"].as_str().filter(|t| !t.is_empty()) {
                    let _ = self.call(json!({ "req": "rename_session", "session_id": id, "title": title })).await;
                }
                let sessions = self.sessions.lock().await;
                Ok(json!({
                    "session_id": id,
                    "stored_session_id": id,
                    "message_count": 0,
                    "messages": [],
                    "info": map::live_info(&id, sessions.get(&id), &cwd, &self.config.version),
                }))
            }
            "session.resume" => {
                let id = sid()?.to_string();
                let attached = call(json!({ "req": "attach_session", "session_id": id })).await?;
                self.attached.lock().await.insert(id.clone());
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
                    "info": map::live_info(&id, sessions.get(&id), &cwd, &self.config.version),
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
                let rows: Vec<Value> = reply["sessions"]
                    .as_array()
                    .map(|list| list.iter().filter(|s| s["parent_session_id"].is_null()).map(map::session_row).collect())
                    .unwrap_or_default();
                Ok(json!({ "sessions": rows }))
            }
            "prompt.submit" => {
                let id = sid()?.to_string();
                let text = map::prompt_text(&p["text"]);
                if text.trim().is_empty() {
                    return Err(RpcError::params("text is required"));
                }
                let busy = self.sessions.lock().await.get(&id).is_some_and(SessionState::turn_active);
                self.submit(&id, &text).await.map_err(RpcError::internal)?;
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
                let busy = self.sessions.lock().await.get(id).is_some_and(SessionState::turn_active);
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
                let resolved = matching.len();
                for (session, request) in matching {
                    self.resolve_approval(&session, &request, &choice).await.map_err(RpcError::internal)?;
                }
                Ok(json!({ "resolved": resolved }))
            }
            _ => {
                crate::note_unsupported("rpc", method);
                Err(RpcError::unsupported(method))
            }
        }
    }

    /// A reply to one of our server requests (currently only `approval`).
    async fn on_client_reply(&self, frame: &Value) {
        let Some(id) = frame["id"].as_str() else { return };
        let Some((session, request)) = self.approvals.lock().await.remove(id) else { return };
        let choice = frame["result"]["choice"].as_str().unwrap_or("deny");
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

pub async fn run(ws: Ws, config: Arc<Config>) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (to_ws, mut ws_out) = mpsc::channel::<Message>(1024);
    let (to_harness, mut harness_out) = mpsc::channel::<String>(256);

    // In-process harness bridge over an in-memory duplex.
    let (ours, theirs) = tokio::io::duplex(MAX_FRAME_BYTES);
    let (their_read, their_write) = tokio::io::split(theirs);
    let bridge = tokio::spawn(jcode_harness_api_server::run_bridge_stream(
        their_read,
        their_write,
        config.legacy_socket.clone(),
    ));
    let (our_read, mut our_write) = tokio::io::split(ours);

    let conn = Arc::new(Conn {
        config,
        to_ws,
        to_harness,
        next_id: AtomicU64::new(1),
        next_server_request: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        approvals: Mutex::new(HashMap::new()),
        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
        attached: Mutex::new(HashSet::new()),
        accept_waiters: Mutex::new(HashMap::new()),
    });

    // Handshake with the bridge before accepting client traffic.
    let hello = json!({"v": 1, "id": 0, "req": "hello", "min_version": 1, "max_version": 1, "client": "sovereign-gateway"});
    our_write.write_all(format!("{hello}\n").as_bytes()).await?;
    let mut lines = BufReader::new(our_read).lines();
    let hello_ok: Value = serde_json::from_str(&lines.next_line().await?.ok_or_else(|| anyhow!("engine closed"))?)?;
    if hello_ok["ev"] != "hello_ok" {
        let _ = ws_tx.send(Message::Close(Some(CloseFrame { code: CloseCode::from(1011), reason: "engine unavailable".into() }))).await;
        bridge.abort();
        return Ok(());
    }

    let writer = tokio::spawn(async move {
        while let Some(msg) = ws_out.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    let harness_writer = tokio::spawn(async move {
        while let Some(line) = harness_out.recv().await {
            if our_write.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let reader_conn = conn.clone();
    let harness_reader = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                reader_conn.on_harness_frame(frame).await;
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
    harness_writer.abort();
    harness_reader.abort();
    bridge.abort();
    Ok(())
}
