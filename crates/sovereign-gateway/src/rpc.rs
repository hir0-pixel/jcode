//! One WebSocket client: JSON-RPC 2.0 in, harness API calls out, harness
//! events translated back into Hermes `event` notifications.

use crate::approvals::{Client, Hub};
use crate::map::{self, Out, SessionState};
use crate::observability::Observer;
use crate::{Config, MAX_FRAME_BYTES};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
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

fn foreign_handle(source: &str, path: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{source}:{path}").as_bytes())
    )
}

struct ForeignCandidate {
    source: &'static str,
    path: std::path::PathBuf,
    external_id: String,
    title: String,
    cwd: Option<String>,
    mtime: f64,
    turn_count: usize,
    excerpt: String,
}

fn foreign_candidates(source: Option<&str>) -> Result<Vec<ForeignCandidate>> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME")
        .map(|h| Path::new(&h).to_path_buf())
        .unwrap_or_else(|| Path::new("/nonexistent").to_path_buf());
    let max_log_bytes = 32 * 1024 * 1024;
    if source.is_none_or(|s| s == "claude") {
        let root = home.join(".claude/projects").canonicalize().ok();
        for s in jcode_base::import::list_claude_code_sessions()? {
            let Ok(path) = Path::new(&s.full_path).canonicalize() else {
                continue;
            };
            let Ok(meta) = path.metadata() else { continue };
            if meta.len() > max_log_bytes
                || root.as_ref().is_none_or(|root| !path.starts_with(root))
            {
                continue;
            }
            out.push(ForeignCandidate {
                source: "claude",
                path,
                external_id: s.session_id,
                title: s.summary.unwrap_or_else(|| s.first_prompt.clone()),
                cwd: s.project_path,
                mtime: s
                    .modified
                    .or(s.created)
                    .map(|t| t.timestamp() as f64)
                    .unwrap_or_default(),
                turn_count: s.message_count as usize,
                excerpt: s.first_prompt,
            });
        }
    }
    if source.is_none_or(|s| s == "codex") {
        let root = home.join(".codex/sessions").canonicalize().ok();
        let mut pending = root.iter().cloned().collect::<Vec<_>>();
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !kind.is_file() || path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let Ok(path) = path.canonicalize() else {
                    continue;
                };
                let Ok(meta) = path.metadata() else { continue };
                if meta.len() > max_log_bytes
                    || root.as_ref().is_none_or(|root| !path.starts_with(root))
                {
                    continue;
                }
                let Ok(Some(record)) = jcode_base::import::load_codex_external_session(&path)
                else {
                    continue;
                };
                let first = record
                    .messages
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| m.text.as_str())
                    .unwrap_or_default();
                out.push(ForeignCandidate {
                    source: "codex",
                    path,
                    external_id: record.session_id,
                    title: record.title.unwrap_or_else(|| {
                        first
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .chars()
                            .take(180)
                            .collect()
                    }),
                    cwd: record.working_dir,
                    mtime: record.updated_at.timestamp() as f64,
                    turn_count: record.messages.len(),
                    excerpt: first.chars().take(200).collect(),
                });
            }
        }
    }
    out.sort_by(|a, b| {
        b.mtime
            .total_cmp(&a.mtime)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(out)
}

fn foreign_turns(candidate: &ForeignCandidate) -> Result<Vec<Value>> {
    if candidate.source == "claude" {
        let session = jcode_base::import::preview_claude_code_session_from_file(
            &candidate.path,
            &candidate.external_id,
        )?;
        return Ok(session.messages.into_iter().map(|m| json!({"role":serde_json::to_value(m.role).unwrap_or(Value::Null),"content":m.content.into_iter().filter_map(|b| match b { jcode_base::message::ContentBlock::Text{text,..} => Some(text), _=>None }).collect::<Vec<_>>().join("\n")})).collect());
    }
    let record = jcode_base::import::load_codex_external_session(&candidate.path)?
        .ok_or_else(|| anyhow!("unreadable Codex session"))?;
    Ok(record
        .messages
        .into_iter()
        .map(|m| json!({"role":m.role,"content":m.text}))
        .collect())
}

pub async fn close(mut ws: Ws, code: u16, reason: &str) -> Result<()> {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.chars().take(120).collect::<String>().into(),
    };
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
        Self {
            code: INVALID_PARAMS,
            message: message.into(),
            data: None,
        }
    }
    fn internal(err: anyhow::Error) -> Self {
        Self {
            code: INTERNAL,
            message: err.to_string(),
            data: None,
        }
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

pub(crate) struct Conn {
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
    /// Sessions created here that have not had a turn yet, so jcode has not
    /// persisted them; only these are merged into `session.list` from `known`.
    fresh: Mutex<std::collections::HashSet<String>>,
    /// Per chat: (turn generation, turns since the last learning pass). A
    /// learning timer only fires if its generation is still current.
    learn_state: Mutex<HashMap<String, (u64, usize)>>,
    learning_now: Mutex<std::collections::HashSet<String>>,
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
    pub(crate) observer: Arc<Observer>,
    run_kind: &'static str,
    run_title: Option<String>,
    replay_of: Option<String>,
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
        self.control
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("engine connection closed"))
    }

    async fn send_on(
        &self,
        link: &mpsc::Sender<String>,
        request: Value,
    ) -> Result<oneshot::Receiver<Value>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut frame = request;
        frame["v"] = json!(1);
        frame["id"] = json!(id);
        link.send(frame.to_string())
            .await
            .map_err(|_| anyhow!("engine connection closed"))?;
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
        let hello_ok: Value = serde_json::from_str(
            &lines
                .next_line()
                .await?
                .ok_or_else(|| anyhow!("engine closed"))?,
        )?;
        if hello_ok["ev"] != "hello_ok" {
            bridge.abort();
            return Err(anyhow!("engine unavailable"));
        }
        let (tx, mut rx) = mpsc::channel::<String>(256);
        let writer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if our_write
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
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
        self.link_tasks.lock().await.extend([
            bridge.abort_handle(),
            writer.abort_handle(),
            reader.abort_handle(),
        ]);
        Ok(tx)
    }

    /// Attach this connection to `session_id` once.
    async fn ensure_attached(self: &Arc<Self>, session_id: &str) -> Result<Value> {
        if self.links.lock().await.contains_key(session_id) {
            return Ok(Value::Null);
        }
        let link = self.open_link().await?;
        match self
            .call_on(
                &link,
                json!({ "req": "attach_session", "session_id": session_id }),
            )
            .await
        {
            Ok(reply) => {
                self.links.lock().await.insert(session_id.to_string(), link);
                self.client
                    .sessions
                    .lock()
                    .await
                    .insert(session_id.to_string());
                if reply["session"].is_object() {
                    self.known
                        .lock()
                        .await
                        .insert(session_id.to_string(), reply["session"].clone());
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
        self.accept_waiters
            .lock()
            .await
            .entry(session_id.to_string())
            .or_default()
            .push(accepted_tx);
        let link = self.route(&json!({ "session_id": session_id })).await?;
        let reply = self
            .send_on(
                &link,
                json!({ "req": "send_message", "session_id": session_id, "content": text }),
            )
            .await?;
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
            params["session_id"] = json!(sid);
            params = self.observer.replay_event(params);
        }
        self.send_json(json!({ "jsonrpc": "2.0", "method": "event", "params": params }))
            .await;
    }

    async fn on_harness_frame(self: &Arc<Self>, frame: Value) {
        if std::env::var_os("SOVEREIGN_GATEWAY_TRACE").is_some() {
            eprintln!(
                "sovereign-gateway: harness {}",
                frame.to_string().chars().take(300).collect::<String>()
            );
        }
        if frame["ev"] == "message_accepted" {
            if let Some(sid) = frame["session_id"].as_str() {
                for waiter in self
                    .accept_waiters
                    .lock()
                    .await
                    .remove(sid)
                    .unwrap_or_default()
                {
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
                Out::Event {
                    ty,
                    session_id,
                    payload,
                } => {
                    self.observer.event(&session_id, ty, &payload);
                    let completed = ty == "message.complete";
                    let payload_for_loop = completed.then(|| payload.clone());
                    self.emit(ty, Some(&session_id), payload).await;
                    if let Some(loop_payload) = payload_for_loop {
                        self.schedule_learning(session_id.clone());
                        self.schedule_agent_loop(session_id, loop_payload);
                    }
                }
                Out::Approval {
                    session_id,
                    request_id,
                    tool_name,
                    description,
                } => {
                    if self.hub.is_headless(&session_id).await {
                        // Headless (`/api/agent/run`): deny outright, no desktop prompt.
                        let conn = self.clone();
                        tokio::spawn(async move {
                            let _ = conn
                                .resolve_approval(&session_id, &request_id, "deny")
                                .await;
                        });
                        continue;
                    }
                    let id = format!(
                        "srv-{}",
                        self.next_server_request.fetch_add(1, Ordering::Relaxed)
                    );
                    self.approvals
                        .lock()
                        .await
                        .insert(id.clone(), (session_id.clone(), request_id.clone()));
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

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    /// Full conversation of `session` (attaching this connection if needed).
    pub(crate) async fn history(self: &Arc<Self>, session: &str) -> Result<Value> {
        self.ensure_attached(session).await?;
        self.call(json!({ "req": "get_history", "session_id": session }))
            .await
    }

    pub(crate) async fn session_cwd(&self, session: &str) -> Option<String> {
        self.known
            .lock()
            .await
            .get(session)
            .and_then(|info| info["working_dir"].as_str().map(str::to_string))
    }

    async fn set_session_cwd(
        self: &Arc<Self>,
        session: &str,
        raw: &str,
    ) -> Result<Value, RpcError> {
        let cwd = std::fs::canonicalize(raw).map_err(|e| RpcError::internal(anyhow!(e)))?;
        if !cwd.is_dir() {
            return Err(RpcError::params("cwd must be an existing directory"));
        }
        if self
            .sessions
            .lock()
            .await
            .get(session)
            .is_some_and(SessionState::turn_active)
        {
            return Err(RpcError {
                code: 409,
                message: "session busy".into(),
                data: None,
            });
        }
        self.ensure_attached(session)
            .await
            .map_err(RpcError::internal)?;
        self.call(json!({ "req": "set_working_dir", "session_id": session, "working_dir": cwd }))
            .await
            .map_err(RpcError::internal)?;
        let cwd = cwd.to_string_lossy().into_owned();
        let mut known = self.known.lock().await;
        let info = known
            .entry(session.to_string())
            .or_insert_with(|| json!({"session_id": session}));
        info["working_dir"] = json!(cwd);
        let result = json!({
            "model": self.config.model, "provider": self.config.provider, "cwd": cwd,
            "running": false, "stored_session_id": session, "desktop_contract": 8,
        });
        drop(known);
        self.emit("session.info", Some(session), result.clone())
            .await;
        Ok(result)
    }

    /// Start the learning timer for `session` after a completed turn.
    fn schedule_learning(self: &Arc<Self>, session: String) {
        let Some(learning) = self.config.learning.clone() else {
            return;
        };
        let conn = self.clone();
        tokio::spawn(async move {
            let (generation, delay) = {
                let mut state = conn.learn_state.lock().await;
                let entry = state.entry(session.clone()).or_default();
                entry.0 += 1;
                entry.1 += 1;
                let delay = if entry.1 >= crate::learn::CADENCE_TURNS {
                    crate::learn::CADENCE_IDLE.min(learning.idle)
                } else {
                    learning.idle
                };
                (entry.0, delay)
            };
            tokio::time::sleep(delay).await;
            // A newer turn started or finished: its own timer takes over.
            if conn.learn_state.lock().await.get(&session).map(|e| e.0) != Some(generation) {
                return;
            }
            if conn
                .sessions
                .lock()
                .await
                .get(&session)
                .is_some_and(SessionState::turn_active)
            {
                return;
            }
            if !conn.learning_now.lock().await.insert(session.clone()) {
                return;
            }
            let result = crate::learn::pass(&conn, &session, &learning).await;
            conn.learning_now.lock().await.remove(&session);
            if let Some(entry) = conn.learn_state.lock().await.get_mut(&session) {
                entry.1 = 0;
            }
            match result {
                Ok(Some(text)) => {
                    conn.emit(
                        "status.update",
                        Some(&session),
                        json!({ "kind": "learning", "text": text }),
                    )
                    .await
                }
                Ok(None) => {}
                Err(err) => eprintln!("sovereign: learning pass for {session} failed: {err:#}"),
            }
        });
    }

    /// After a completed turn, maybe inject goal/autonomous/heartbeat continuation.
    fn schedule_agent_loop(self: &Arc<Self>, session_id: String, payload: Value) {
        let conn = self.clone();
        tokio::spawn(async move {
            let home = Path::new(&conn.config.home);
            let store = match sovereign_prime::agent_loop::ControlStore::open_cached(home) {
                Ok(store) => store,
                Err(err) => {
                    eprintln!("sovereign: agent loop store for {session_id}: {err:#}");
                    return;
                }
            };
            let usage = payload.get("usage").cloned().unwrap_or(json!({}));
            let tokens = usage["total"]
                .as_u64()
                .or_else(|| {
                    let input = usage["input"].as_u64().unwrap_or(0);
                    let output = usage["output"].as_u64().unwrap_or(0);
                    (input + output > 0).then_some(input + output)
                })
                .unwrap_or(0) as i64;
            let interrupted = payload["status"].as_str() == Some("interrupted");
            let subagents_running = conn.child_sessions_running(&session_id).await;
            let continuation = match sovereign_prime::agent_loop::after_turn(
                &store,
                &session_id,
                tokens,
                subagents_running,
                interrupted,
            ) {
                Ok(c) => c,
                Err(err) => {
                    eprintln!("sovereign: after_turn for {session_id}: {err:#}");
                    return;
                }
            };
            let continuation = match continuation {
                Some(c) => Some(c),
                None if !interrupted
                    && !conn
                        .sessions
                        .lock()
                        .await
                        .get(&session_id)
                        .is_some_and(SessionState::turn_active) =>
                {
                    sovereign_prime::agent_loop::due_heartbeat(&store, &session_id)
                        .ok()
                        .flatten()
                }
                None => None,
            };
            let Some(cont) = continuation else { return };
            tokio::time::sleep(Duration::from_millis(400)).await;
            if conn
                .sessions
                .lock()
                .await
                .get(&session_id)
                .is_some_and(SessionState::turn_active)
            {
                return;
            }
            let prompt = match cont {
                sovereign_prime::agent_loop::Continuation::Goal(p)
                | sovereign_prime::agent_loop::Continuation::Autonomous(p) => p,
                sovereign_prime::agent_loop::Continuation::Heartbeat { prompt, .. } => prompt,
            };
            if let Err(err) = conn.submit(&session_id, &prompt).await {
                eprintln!("sovereign: agent loop submit for {session_id}: {err:#}");
            }
        });
    }

    async fn child_sessions_running(self: &Arc<Self>, parent_id: &str) -> bool {
        let Ok(reply) = self.call(json!({ "req": "list_sessions" })).await else {
            return false;
        };
        let sessions = self.sessions.lock().await;
        reply["sessions"].as_array().is_some_and(|list| {
            list.iter().any(|s| {
                s["parent_session_id"].as_str() == Some(parent_id)
                    && s["session_id"]
                        .as_str()
                        .is_some_and(|id| sessions.get(id).is_some_and(SessionState::turn_active))
            })
        })
    }

    async fn list_owned_children(self: &Arc<Self>, parent_id: &str) -> Result<Vec<Value>> {
        let reply = self.call(json!({ "req": "list_sessions" })).await?;
        let sessions = self.sessions.lock().await;
        Ok(reply["sessions"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter(|s| s["parent_session_id"].as_str() == Some(parent_id))
                    .map(|s| Self::subagent_snapshot(s, parent_id, &sessions))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn subagent_snapshot(
        info: &Value,
        parent_id: &str,
        sessions: &HashMap<String, SessionState>,
    ) -> Value {
        let child_id = info["session_id"].as_str().unwrap_or_default();
        let subagent_id = info["agent_label"].as_str().unwrap_or(child_id);
        let started_ms = info["last_active_at_ms"]
            .as_i64()
            .or(info["updated_at_ms"].as_i64())
            .unwrap_or(0);
        json!({
            "subagent_id": subagent_id,
            "parent_id": parent_id,
            "goal": info["title"].as_str().or(info["agent_label"].as_str()),
            "child_session_id": child_id,
            "status": Self::map_subagent_status(info, sessions),
            "model": info["model"],
            "started_at": started_ms as f64 / 1000.0,
            "task_index": 0,
            "task_count": 1,
            "accepting_steer": true,
        })
    }

    fn map_subagent_status(info: &Value, sessions: &HashMap<String, SessionState>) -> Value {
        let child_id = info["session_id"].as_str().unwrap_or_default();
        if sessions
            .get(child_id)
            .is_some_and(SessionState::turn_active)
        {
            return json!("running");
        }
        let status = info["swarm_status"]
            .as_str()
            .or(info["status"].as_str())
            .unwrap_or("ready");
        // Desktop SubagentStatus: completed|failed|interrupted|queued|running
        let mapped = match status {
            "running" | "processing" | "ready" | "idle" => "running",
            "queued" => "queued",
            "completed" => "completed",
            "failed" | "error" | "timeout" => "failed",
            "stopped" | "interrupted" | "cancelled" | "canceled" => "interrupted",
            other => other,
        };
        json!(mapped)
    }

    async fn resolve_child_session(
        self: &Arc<Self>,
        parent_id: &str,
        subagent_id: &str,
    ) -> Option<String> {
        let reply = self.call(json!({ "req": "list_sessions" })).await.ok()?;
        reply["sessions"].as_array()?.iter().find_map(|s| {
            if s["parent_session_id"].as_str() != Some(parent_id) {
                return None;
            }
            let sid = s["session_id"].as_str()?;
            if sid == subagent_id
                || s["agent_label"].as_str() == Some(subagent_id)
                || s["title"].as_str() == Some(subagent_id)
                || s["friendly_name"].as_str() == Some(subagent_id)
            {
                Some(sid.to_string())
            } else {
                None
            }
        })
    }

    async fn resolve_approval(
        &self,
        session_id: &str,
        request_id: &str,
        choice: &str,
    ) -> Result<()> {
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
        let sid = || {
            p["session_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| RpcError::params("session_id is required"))
        };
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
            "session.active_list" => {
                let stored = super::session_infos(&self.config, 1000, false)
                    .await
                    .map_err(RpcError::internal)?;
                let current = p["current_session_id"].as_str().unwrap_or_default();
                let mut active: HashMap<String, (bool, Option<String>)> = stored
                    .iter()
                    .filter(|s| s["is_active"] == true)
                    .filter_map(|s| {
                        s["id"].as_str().map(|id| {
                            (
                                id.to_string(),
                                (false, s["model"].as_str().map(str::to_owned)),
                            )
                        })
                    })
                    .collect();
                for (id, state) in self
                    .sessions
                    .lock()
                    .await
                    .iter()
                    .filter(|(_, state)| state.turn_active())
                {
                    active.insert(id.clone(), (true, state.model.clone()));
                }
                if !current.is_empty() && self.observer.has_active_run(current) {
                    active.insert(current.to_string(), (true, None));
                }
                let rows: HashMap<&str, &Value> = stored
                    .iter()
                    .filter_map(|s| s["id"].as_str().map(|id| (id, s)))
                    .collect();
                let known = self.known.lock().await;
                let items: Vec<Value> = active.into_iter().map(|(id, (streaming, model))| {
                    let row = rows.get(id.as_str()).copied();
                    let title = row.and_then(|s| s["title"].as_str()).or_else(|| known.get(&id).and_then(|s| s["title"].as_str())).unwrap_or("Untitled");
                    let started = row.map(|s| s["started_at"].clone()).unwrap_or(Value::Null);
                    let last = row.map(|s| s["last_active"].clone()).unwrap_or(Value::Null);
                    json!({"current":id == current,"id":id,"session_key":id,"last_active":last,"started_at":started,
                        "message_count":row.map(|s|s["message_count"].clone()).unwrap_or(json!(0)),
                        "model":model.or_else(||row.and_then(|s|s["model"].as_str().map(str::to_owned))).unwrap_or_else(||self.config.model.clone()),
                        "preview":row.map(|s|s["preview"].clone()).unwrap_or_else(||json!("")),
                        "status":if streaming {"streaming"} else {"running"},"title":title})
                }).collect();
                Ok(json!({ "sessions": items }))
            }
            "commands.catalog" => {
                let pairs = json!([
                    [
                        "/refine",
                        "Propose evidence-backed Continual Harness edits from this session"
                    ],
                    [
                        "/refine --global",
                        "Same, scoped to every session instead of just this one"
                    ],
                    [
                        "/refine rollback",
                        "Undo the last /refine (or a specific changeset id)"
                    ],
                    [
                        "/refine status",
                        "Show current harness entries and recent refinements"
                    ],
                    ["/harness", "Show the learned instructions"],
                    ["/goal", "Set or manage the unattended session goal"],
                    [
                        "/autonomous",
                        "Run self-paced work with optional quality gates"
                    ],
                    ["/loop", "Alias for /autonomous"],
                    ["/heartbeat", "Schedule idle heartbeats for this session"],
                ]);
                Ok(json!({
                    "pairs": pairs, "sub": {}, "canon": {}, "commands": {},
                    "categories": [{ "name": "Harness", "pairs": pairs }],
                    "skills": {}, "skill_count": jcode_base::skill::SkillRegistry::shared_snapshot().list().len(), "warning": "",
                }))
            }
            // Hermes desktop's Learning UI, served natively over Continual
            // Harness entries instead of forwarded to the Python backend.
            "learning.frames" => {
                let cols = p["cols"].as_u64().unwrap_or(80).max(1) as usize;
                let rows = p["rows"].as_u64().unwrap_or(24).max(1) as usize;
                let store = sovereign_prime::entries::EntryStore::open_cached(
                    std::path::Path::new(&self.config.home),
                )
                .map_err(RpcError::internal)?;
                let entries = store.list_all(None, None).map_err(RpcError::internal)?;
                let categories: Vec<Value> =
                    ["prompt", "memory", "skill", "subagent"].iter().map(|k| json!({ "name": k, "count": entries.iter().filter(|e| e.kind.as_str() == *k).count() })).collect();
                let summary: Vec<String> = entries
                    .iter()
                    .rev()
                    .take(rows.saturating_sub(2).max(1))
                    .map(|e| format!("[{}/{}] {}", e.kind.as_str(), e.scope.as_str(), e.title))
                    .collect();
                let frame = summary.iter().cloned().collect::<Vec<_>>().join("\n");
                Ok(json!({
                    "frames": [{ "text": frame, "cols": cols, "rows": rows }],
                    "legend": { "prompt": "P", "memory": "M", "skill": "S", "subagent": "A" },
                    "categories": categories,
                    "buckets": categories,
                    "summary": summary,
                    "axis": { "start": entries.first().map(|e| e.created_at_ms).unwrap_or(0), "end": entries.last().map(|e| e.updated_at_ms).unwrap_or(0) },
                    "count": entries.len(),
                    "cols": cols,
                    "rows": rows,
                }))
            }
            "learning.detail" => {
                let id = p["id"].as_str().unwrap_or_default();
                let store = sovereign_prime::entries::EntryStore::open_cached(
                    std::path::Path::new(&self.config.home),
                )
                .map_err(RpcError::internal)?;
                Ok(match store.get(id).map_err(RpcError::internal)? {
                    Some(e) => {
                        json!({ "ok": true, "kind": e.kind.as_str(), "id": e.id, "label": e.title, "content": e.content })
                    }
                    None => json!({ "ok": false, "message": format!("no harness entry {id}") }),
                })
            }
            "learning.delete" => {
                let id = p["id"].as_str().unwrap_or_default();
                let store = sovereign_prime::entries::EntryStore::open_cached(
                    std::path::Path::new(&self.config.home),
                )
                .map_err(RpcError::internal)?;
                Ok(match store.delete(id) {
                    Ok(_) => json!({ "ok": true }),
                    Err(err) => json!({ "ok": false, "message": err.to_string() }),
                })
            }
            "learning.edit" => {
                let id = p["id"].as_str().unwrap_or_default();
                let content = p["content"].as_str().map(str::to_string);
                let store = sovereign_prime::entries::EntryStore::open_cached(
                    std::path::Path::new(&self.config.home),
                )
                .map_err(RpcError::internal)?;
                Ok(
                    match store.update(
                        id,
                        sovereign_prime::entries::EntryPatch {
                            content,
                            ..Default::default()
                        },
                    ) {
                        Ok(_) => json!({ "ok": true }),
                        Err(err) => json!({ "ok": false, "message": err.to_string() }),
                    },
                )
            }
            "subagent.list" => {
                let id = sid()?;
                let subagents = self
                    .list_owned_children(id)
                    .await
                    .map_err(RpcError::internal)?;
                Ok(json!({ "subagents": subagents, "delegations": [] }))
            }
            "subagent.interrupt" => {
                let parent = sid()?;
                let subagent_id = p["subagent_id"]
                    .as_str()
                    .ok_or_else(|| RpcError::params("subagent_id is required"))?;
                let Some(child) = self.resolve_child_session(parent, subagent_id).await else {
                    return Ok(json!({ "found": false, "subagent_id": subagent_id }));
                };
                // Best-effort: a finished child may reject cancel.
                let cancelled = call(json!({ "req": "cancel", "session_id": child }))
                    .await
                    .is_ok();
                Ok(
                    json!({ "found": true, "subagent_id": subagent_id, "child_session_id": child, "cancelled": cancelled }),
                )
            }
            "subagent.steer" => {
                let parent = sid()?;
                let subagent_id = p["subagent_id"]
                    .as_str()
                    .ok_or_else(|| RpcError::params("subagent_id is required"))?;
                let content = p["content"]
                    .as_str()
                    .or(p["text"].as_str())
                    .unwrap_or_default();
                if content.trim().is_empty() {
                    return Err(RpcError::params("text is required"));
                }
                let Some(child) = self.resolve_child_session(parent, subagent_id).await else {
                    return Ok(
                        json!({ "status": "rejected", "subagent_id": subagent_id, "text": content }),
                    );
                };
                match call(
                    json!({ "req": "soft_interrupt", "session_id": child, "content": content }),
                )
                .await
                {
                    Ok(_) => Ok(
                        json!({ "status": "queued", "subagent_id": subagent_id, "text": content }),
                    ),
                    Err(_) => Ok(
                        json!({ "status": "rejected", "subagent_id": subagent_id, "text": content }),
                    ),
                }
            }
            "subagent.tail" => {
                // Desktop SubagentTranscript expects { available, text, truncated } (≤16 KiB).
                const TAIL_BYTES: usize = 16_384;
                let parent = sid()?;
                let subagent_id = p["subagent_id"]
                    .as_str()
                    .ok_or_else(|| RpcError::params("subagent_id is required"))?;
                let Some(child) = self.resolve_child_session(parent, subagent_id).await else {
                    return Ok(
                        json!({ "subagent_id": subagent_id, "available": false, "text": "", "truncated": false }),
                    );
                };
                self.ensure_attached(&child)
                    .await
                    .map_err(RpcError::internal)?;
                let history = call(json!({ "req": "get_history", "session_id": child })).await?;
                let mut text = String::new();
                if let Some(list) = history["messages"].as_array() {
                    for m in list {
                        let role = m["role"].as_str().unwrap_or("assistant");
                        let body = m["content"].as_str().unwrap_or("").trim();
                        if body.is_empty() {
                            continue;
                        }
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(role);
                        text.push_str(": ");
                        text.push_str(body);
                    }
                }
                let truncated = text.len() > TAIL_BYTES;
                if truncated {
                    text = text
                        .chars()
                        .rev()
                        .take(TAIL_BYTES)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
                }
                Ok(json!({
                    "subagent_id": subagent_id,
                    "available": !text.is_empty(),
                    "text": text,
                    "truncated": truncated,
                }))
            }
            "spawn_tree.list" => Ok(json!({ "entries": [] })),
            "spawn_tree.save" => {
                Ok(json!({ "ok": true, "path": p["path"].as_str().unwrap_or("") }))
            }
            "spawn_tree.load" => Ok(json!({ "session_id": null, "entries": [] })),
            "delegation.pause" => Ok(json!({ "paused": p["paused"].as_bool().unwrap_or(true) })),
            "delegation.status" => {
                let id = sid()?;
                let children = self
                    .list_owned_children(id)
                    .await
                    .map_err(RpcError::internal)?;
                let active: Vec<Value> = children
                    .into_iter()
                    .filter(|c| matches!(c["status"].as_str(), Some("running" | "queued")))
                    .collect();
                Ok(json!({
                    "active": active,
                    "paused": false,
                    "max_spawn_depth": 4,
                    "max_concurrent_children": 8,
                }))
            }
            "groups.list" => Ok(json!({ "rooms": [], "next_offset": null })),
            "groups.capabilities" => Ok(json!({
                "protocol_version": 1,
                "driver": false,
                "persistent_process": false,
                "authority_gateway_id": "",
                "room_link": { "linked": false, "room_id": null, "gateway_id": null },
                "features": [],
                "methods": [],
                "max_log_limit": 0,
            })),
            "session.control.read" => {
                let id = sid()?;
                let store = sovereign_prime::agent_loop::ControlStore::open_cached(Path::new(
                    &self.config.home,
                ))
                .map_err(RpcError::internal)?;
                Ok(json!({ "control": store.control_snapshot(id).map_err(RpcError::internal)? }))
            }
            "session.control" => {
                let id = sid()?;
                let action = p["action"]
                    .as_str()
                    .ok_or_else(|| RpcError::params("action is required"))?;
                let args = p.get("args").cloned().unwrap_or(json!({}));
                let store = sovereign_prime::agent_loop::ControlStore::open_cached(Path::new(
                    &self.config.home,
                ))
                .map_err(RpcError::internal)?;
                let (control, dispatch) =
                    sovereign_prime::agent_loop::control_action(&store, id, action, &args)
                        .map_err(RpcError::internal)?;
                Ok(json!({ "control": control, "dispatch": dispatch }))
            }
            "complete.path" => {
                let word = p["word"].as_str().unwrap_or_default().to_string();
                let cwd = p["cwd"]
                    .as_str()
                    .unwrap_or(&self.config.default_cwd)
                    .to_string();
                let items = tokio::task::spawn_blocking(move || map::complete_path(&word, &cwd))
                    .await
                    .unwrap_or_default();
                Ok(json!({ "items": items }))
            }
            "slash.exec" => {
                let command = p["command"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect::<String>();
                let words: Vec<&str> = command.trim_start_matches('/').split_whitespace().collect();
                if let Some(message) = self.harness_command(&words, p["session_id"].as_str()).await
                {
                    return Ok(
                        json!({ "status": "ok", "type": "exec", "output": message, "message": message }),
                    );
                }
                crate::note_unsupported("slash", &command);
                let message = format!(
                    "/{} is not available in this engine yet.",
                    command.trim_start_matches('/')
                );
                Ok(json!({ "status": "error", "message": message, "output": message }))
            }
            "gateway.capabilities" => Ok(json!({ "per_session_exclusive_submit": false })),
            "client.capabilities" => Ok(json!({ "server_requests": ["approval"] })),
            "session.create" => {
                let profile = crate::profile::current();
                let cwd = p["cwd"]
                    .as_str()
                    .unwrap_or(&self.config.default_cwd)
                    .to_string();
                let link = self.open_link().await.map_err(RpcError::internal)?;
                let mut create = json!({ "req": "create_session", "working_dir": cwd });
                if let Some(prompt) = p["system_prompt"].as_str() {
                    create["system_prompt"] = json!(prompt);
                } else if let Some(prompt) = profile.system_prompt.as_deref() {
                    create["system_prompt"] = json!(prompt);
                }
                let reply = self
                    .call_on(&link, create)
                    .await
                    .map_err(RpcError::internal)?;
                let id = reply["session"]["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                self.links.lock().await.insert(id.clone(), link);
                self.client.sessions.lock().await.insert(id.clone());
                self.known
                    .lock()
                    .await
                    .insert(id.clone(), reply["session"].clone());
                self.fresh.lock().await.insert(id.clone());
                let selected_model = p["model"]
                    .as_str()
                    .filter(|model| !model.trim().is_empty())
                    .or(profile.model.as_deref());
                let selected_provider = p["provider"]
                    .as_str()
                    .filter(|provider| !provider.trim().is_empty())
                    .or(profile.provider.as_deref());
                let selected_effort = p["reasoning_effort"]
                    .as_str()
                    .filter(|effort| !effort.trim().is_empty())
                    .or(profile.reasoning_effort.as_deref());
                if let Some(model) = selected_model {
                    let route_model =
                        jcode_base::provider::MultiProvider::model_switch_request_for_session_route(
                            model,
                            selected_provider,
                            p["route_api_method"].as_str(),
                        );
                    let session_link =
                        self.links.lock().await.get(&id).cloned().ok_or_else(|| {
                            RpcError::internal(anyhow!("new session link was lost"))
                        })?;
                    self.call_on(
                        &session_link,
                        json!({
                            "req": "set_model", "session_id": id.clone(), "model": route_model,
                        }),
                    )
                    .await
                    .map_err(RpcError::internal)?;
                }
                if let Some(effort) = selected_effort {
                    let session_link =
                        self.links.lock().await.get(&id).cloned().ok_or_else(|| {
                            RpcError::internal(anyhow!("new session link was lost"))
                        })?;
                    self.call_on(&session_link, json!({
                        "req": "set_reasoning_effort", "session_id": id.clone(), "effort": effort,
                    })).await.map_err(RpcError::internal)?;
                }
                if let Some(title) = p["title"].as_str().filter(|t| !t.is_empty()) {
                    let _ = self
                        .call(json!({ "req": "rename_session", "session_id": id, "title": title }))
                        .await;
                }
                let sessions = self.sessions.lock().await;
                let mut info = map::live_info(
                    &id,
                    sessions.get(&id),
                    &cwd,
                    &self.config.version,
                    &self.config.model,
                    &self.config.provider,
                );
                if let Some(model) = selected_model {
                    info["model"] = json!(model);
                }
                if let Some(provider) = selected_provider {
                    info["provider"] = json!(provider);
                }
                info["memory_enabled"] = json!(jcode_base::config::memory_enabled());
                if let Some(effort) = selected_effort {
                    info["reasoning_effort"] = json!(effort);
                }
                Ok(json!({
                    "session_id": id,
                    "stored_session_id": id,
                    "message_count": 0,
                    "messages": [],
                    "info": info,
                }))
            }
            "session.resume" | "session.activate" => {
                let id = sid()?.to_string();
                let attached = self
                    .ensure_attached(&id)
                    .await
                    .map_err(RpcError::internal)?;
                let cwd = attached["session"]["working_dir"]
                    .as_str()
                    .unwrap_or(&self.config.default_cwd)
                    .to_string();
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
                    .map(|list| {
                        list.iter()
                            .filter(|s| s["parent_session_id"].is_null())
                            .map(map::session_row)
                            .collect()
                    })
                    .unwrap_or_default();
                let fresh = self.fresh.lock().await;
                for (id, info) in self.known.lock().await.iter() {
                    if fresh.contains(id)
                        && info["parent_session_id"].is_null()
                        && !rows.iter().any(|r| r["id"] == id.as_str())
                    {
                        rows.insert(0, map::session_row(info));
                    }
                }
                Ok(json!({ "sessions": rows }))
            }
            // Chats live only in the engine's store, so every chat-bound method
            // is answered here; forwarding one to Hermes's Python backend would
            // act on a database that has never seen these sessions.
            "session.close" => {
                let id = sid()?;
                let closed = self.links.lock().await.remove(id).is_some();
                self.client.sessions.lock().await.remove(id);
                Ok(json!({ "closed": closed }))
            }
            "session.delete" => {
                let id = sid()?.to_string();
                if self
                    .sessions
                    .lock()
                    .await
                    .get(&id)
                    .is_some_and(SessionState::turn_active)
                    || self.observer.has_active_run(&id)
                {
                    return Err(RpcError::params(
                        "session is running; stop it before deleting",
                    ));
                }
                self.links.lock().await.remove(&id);
                self.client.sessions.lock().await.remove(&id);
                call(json!({ "req": "delete_session", "session_id": id })).await?;
                jcode_app_core::tool::stop_repl_session(&id).await;
                self.known.lock().await.remove(&id);
                self.sessions.lock().await.remove(&id);
                Ok(json!({ "deleted": true }))
            }
            "session.set_hidden" => {
                let id = sid()?;
                let hidden = p["hidden"].as_bool().unwrap_or(true);
                let req = if hidden {
                    "archive_session"
                } else {
                    "restore_session"
                };
                call(json!({ "req": req, "session_id": id })).await?;
                Ok(json!({ "hidden": hidden, "session_key": id }))
            }
            "session.most_recent" => {
                let reply = call(json!({ "req": "list_sessions", "limit": 1 }))
                    .await
                    .ok();
                let top = reply
                    .as_ref()
                    .and_then(|r| r["sessions"].as_array())
                    .and_then(|list| list.first())
                    .cloned();
                Ok(match top {
                    Some(info) => {
                        let row = map::session_row(&info);
                        json!({ "session_id": row["id"], "title": row["title"], "started_at": row["started_at"], "source": "sovereign" })
                    }
                    None => json!({ "session_id": null }),
                })
            }
            "session.branch" => {
                let id = sid()?.to_string();
                self.ensure_attached(&id)
                    .await
                    .map_err(RpcError::internal)?;
                let forked = call(json!({ "req": "fork_session", "session_id": id })).await?;
                let child = forked["session"]["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                if child.is_empty() {
                    return Err(RpcError::internal(anyhow!(
                        "engine did not return the branched session"
                    )));
                }
                let attached = self
                    .ensure_attached(&child)
                    .await
                    .map_err(RpcError::internal)?;
                let title = p["name"]
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);
                if let Some(title) = &title {
                    let _ = self
                        .call(
                            json!({ "req": "rename_session", "session_id": child, "title": title }),
                        )
                        .await;
                }
                let history = call(json!({ "req": "get_history", "session_id": child })).await?;
                let messages = map::transcript(&history["messages"]);
                let cwd = attached["session"]["working_dir"]
                    .as_str()
                    .unwrap_or(&self.config.default_cwd)
                    .to_string();
                let sessions = self.sessions.lock().await;
                Ok(json!({
                    "session_id": child,
                    "stored_session_id": child,
                    "title": title.unwrap_or_else(|| "Branch".into()),
                    "parent": id,
                    "message_count": messages.len(),
                    "messages": messages,
                    "info": map::live_info(&child, sessions.get(&child), &cwd, &self.config.version, &self.config.model, &self.config.provider),
                }))
            }
            "session.undo" => {
                let id = sid()?;
                if self
                    .sessions
                    .lock()
                    .await
                    .get(id)
                    .is_some_and(SessionState::turn_active)
                {
                    return Err(RpcError::params(
                        "session is running; undo works on an idle session",
                    ));
                }
                self.ensure_attached(id).await.map_err(RpcError::internal)?;
                let history = call(json!({ "req": "get_history", "session_id": id })).await?;
                // jcode's rewind indexes only user and assistant messages, so
                // positions must be counted among those (tool rows excluded).
                let messages: Vec<Value> = history["messages"]
                    .as_array()
                    .map(|all| {
                        all.iter()
                            .filter(|m| m["role"] == "user" || m["role"] == "assistant")
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                let Some(last_user) = messages.iter().rposition(|m| m["role"] == "user") else {
                    return Ok(json!({ "removed": 0 }));
                };
                let removed = messages.len() - last_user;
                if last_user == 0 {
                    call(json!({ "req": "clear", "session_id": id })).await?;
                } else {
                    call(json!({ "req": "rewind", "session_id": id, "message_index": last_user }))
                        .await?;
                }
                Ok(json!({ "removed": removed }))
            }
            "session.status" => {
                let id = sid()?;
                let title = self
                    .known
                    .lock()
                    .await
                    .get(id)
                    .and_then(|info| info["title"].as_str().map(str::to_string))
                    .unwrap_or_else(|| "Untitled".into());
                let sessions = self.sessions.lock().await;
                let state = sessions.get(id);
                let usage = state
                    .map(SessionState::usage_json)
                    .unwrap_or_else(|| json!({}));
                let output = format!(
                    "Session: {title}\nID: {id}\nModel: {} ({})\nState: {}\nTokens: {} in, {} out",
                    self.config.model,
                    self.config.provider,
                    if state.is_some_and(SessionState::turn_active) {
                        "running"
                    } else {
                        "idle"
                    },
                    usage["input"].as_u64().unwrap_or(0),
                    usage["output"].as_u64().unwrap_or(0),
                );
                Ok(json!({ "output": output }))
            }
            "session.save" => {
                let id = sid()?.to_string();
                self.ensure_attached(&id)
                    .await
                    .map_err(RpcError::internal)?;
                let history = call(json!({ "req": "get_history", "session_id": id })).await?;
                let dir = std::path::Path::new(&self.config.home)
                    .join("sessions")
                    .join("saved");
                let file = dir.join(format!(
                    "{id}-{}.json",
                    chrono::Utc::now().format("%Y%m%d-%H%M%S")
                ));
                let body = serde_json::to_vec_pretty(
                    &json!({ "session_id": id, "messages": map::transcript(&history["messages"]) }),
                )
                .map_err(|e| RpcError::internal(anyhow!(e)))?;
                std::fs::create_dir_all(&dir)
                    .and_then(|()| std::fs::write(&file, body))
                    .map_err(|e| RpcError::internal(anyhow!(e)))?;
                Ok(json!({ "file": file.to_string_lossy() }))
            }
            "session.redirect" => {
                let id = sid()?;
                let text = map::prompt_text(&p["text"]);
                self.ensure_attached(id).await.map_err(RpcError::internal)?;
                call(json!({ "req": "soft_interrupt", "session_id": id, "content": text })).await?;
                Ok(json!({ "status": "queued", "text": text }))
            }
            "session.events.since" => {
                let id = sid()?;
                let last_seen = p["last_seen"]
                    .as_u64()
                    .ok_or_else(|| RpcError::params("last_seen must be an integer"))?;
                let (events, latest_seq, truncated, epoch) =
                    self.observer.replay_since(id, last_seen);
                let count = events.len();
                let approvals = self.approvals.lock().await;
                let open_requests: Vec<Value> = approvals.iter().filter(|(_, (session, _))| session == id)
                    .map(|(id, (session_id, request_id))| json!({
                        "id": id, "method": "approval", "params": {"session_id": session_id, "request_id": request_id}
                    })).collect();
                Ok(
                    json!({ "events": events, "latest_seq": latest_seq, "truncated": truncated,
                    "count": count, "epoch": epoch, "open_requests": open_requests }),
                )
            }
            "session.events.stats" => Ok(self.observer.replay_stats()),
            "session.context_breakdown" => {
                let id = sid()?;
                let history = self.history(id).await.map_err(RpcError::internal)?;
                let text: String = history["messages"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|message| {
                        message["content"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let tokens = text.chars().count().div_ceil(4);
                let model = self
                    .sessions
                    .lock()
                    .await
                    .get(id)
                    .and_then(|s| s.model.clone())
                    .unwrap_or_else(|| self.config.model.clone());
                let context_max = jcode_base::provider::context_limit_for_model_with_provider(
                    &model,
                    Some(&self.config.provider),
                )
                .unwrap_or(0);
                Ok(json!({
                    "categories": [{"id":"conversation","label":"Conversation","color":"#8a8a8a","tokens":tokens}],
                    "context_max": context_max, "context_percent": if context_max > 0 { tokens * 100 / context_max } else { 0 },
                    "context_used": tokens, "estimated_total": tokens, "context_estimated": true,
                    "context_source": "engine_transcript_estimate", "model": model, "context_files": [],
                }))
            }
            "session.cwd.set" => {
                let id = sid()?;
                let cwd = p["cwd"]
                    .as_str()
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| RpcError::params("cwd is required"))?;
                self.set_session_cwd(id, cwd).await
            }
            "session.workspace.move" => {
                let id = p["session_key"]
                    .as_str()
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| RpcError::params("session_key is required"))?;
                let cwd = p["cwd"]
                    .as_str()
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| RpcError::params("cwd is required"))?;
                let result = self.set_session_cwd(id, cwd).await?;
                let cwd = result["cwd"].as_str().unwrap_or_default();
                Ok(
                    json!({ "cwd": cwd, "branch": map::git_branch(cwd), "git_repo_root": map::git_repo_root(cwd) }),
                )
            }
            "insights.get" => {
                let days = p["days"].as_u64().unwrap_or(30).clamp(1, 3650);
                let observer = self.observer.clone();
                tokio::task::spawn_blocking(move || observer.insights(days))
                    .await
                    .map_err(|e| RpcError::internal(anyhow!(e)))?
                    .map_err(|e| RpcError::internal(anyhow!(e)))
            }
            "usage.bars" => {
                let (spent, priced_calls) = self
                    .observer
                    .metered_usage()
                    .map_err(|e| RpcError::internal(anyhow!(e)))?;
                Ok(
                    json!({"ok":true,"available":false,"status":"no_subscription_entitlement_source",
                    "metered_spend_usd":format!("{spent:.4}"),"priced_calls":priced_calls}),
                )
            }
            "session.foreign.list" => {
                let source = p["source"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                if source
                    .as_deref()
                    .is_some_and(|s| !matches!(s, "claude" | "codex"))
                {
                    return Err(RpcError::params("source must be claude or codex"));
                }
                let offset = p["offset"].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
                let limit = p["limit"].as_u64().unwrap_or(25).clamp(1, 50) as usize;
                let rows = tokio::task::spawn_blocking(move || foreign_candidates(source.as_deref()).map(|all| {
                    let total = all.len();
                    let rows = all.into_iter().skip(offset).take(limit).map(|s| {
                        json!({"id":foreign_handle(s.source, &s.path.to_string_lossy()),"source":s.source,
                            "label":if s.source=="claude" {"Claude Code"} else {"Codex"},"title":s.title,
                            "cwd":s.cwd,"mtime":s.mtime,"turn_count":s.turn_count,"excerpt":s.excerpt})
                    }).collect::<Vec<_>>();
                    (rows, (offset + limit < total).then_some(offset + limit))
                })).await.map_err(|e| RpcError::internal(anyhow!(e)))?.map_err(RpcError::internal)?;
                Ok(
                    json!({"sessions":rows.0,"next_offset":rows.1,"host":std::env::var("HOSTNAME").unwrap_or_else(|_|"local".into()),"unreadable":0}),
                )
            }
            "session.foreign.preview" | "session.foreign.import" => {
                let handle = p["id"]
                    .as_str()
                    .filter(|s| s.len() == 64)
                    .ok_or_else(|| RpcError::params("id must be a foreign-session handle"))?
                    .to_string();
                let importer = if method == "session.foreign.import" {
                    "import"
                } else {
                    "preview"
                };
                tokio::task::spawn_blocking(move || {
                    let source = foreign_candidates(None)?.into_iter().find(|s| foreign_handle(s.source, &s.path.to_string_lossy()) == handle)
                        .ok_or_else(|| anyhow!("session no longer available; refresh the list"))?;
                    if importer == "import" {
                        let imported_id = if source.source=="claude" { jcode_base::import::imported_claude_code_session_id(&source.external_id) } else { jcode_base::import::imported_codex_session_id(&source.external_id) };
                        let already = jcode_base::session::Session::load(&imported_id).is_ok();
                        let session = if source.source=="claude" { jcode_base::import::import_session_from_file(&source.path, &source.external_id)? }
                            else { jcode_base::import::import_codex_session_from_path(&source.path, Some(&source.external_id))? };
                        return Ok(json!({"session_id":session.id,"already_imported":already}));
                    }
                    let turns = foreign_turns(&source)?;
                    let total = turns.len();
                    let messages = turns.into_iter().rev().take(40).collect::<Vec<_>>().into_iter().rev().map(|mut m| {
                        if let Some(text)=m["content"].as_str() { m["content"] = json!(text.chars().take(8000).collect::<String>()); }
                        m
                    }).collect::<Vec<_>>();
                    let imported_id = if source.source=="claude" { jcode_base::import::imported_claude_code_session_id(&source.external_id) } else { jcode_base::import::imported_codex_session_id(&source.external_id) };
                    Ok(json!({"messages":messages,"total":total,"truncated":total>40,"already_imported":if jcode_base::session::Session::load(&imported_id).is_ok(){json!(imported_id)}else{Value::Null},"cwd":source.cwd}))
                }).await.map_err(|e| RpcError::internal(anyhow!(e)))?.map_err(RpcError::internal)
            }
            "prompt.submit" => {
                let id = sid()?.to_string();
                let text = map::prompt_text(&p["text"]);
                if text.trim().is_empty() {
                    return Err(RpcError::params("text is required"));
                }
                let busy = self
                    .sessions
                    .lock()
                    .await
                    .get(&id)
                    .is_some_and(SessionState::turn_active);
                self.ensure_attached(&id)
                    .await
                    .map_err(RpcError::internal)?;
                // jcode does not auto-title sessions; Hermes titles from the first
                // prompt. Rename before sending: jcode refuses renames mid-turn.
                let untitled = {
                    let known = self.known.lock().await;
                    known
                        .get(&id)
                        .is_none_or(|info| info["title"].as_str().is_none_or(str::is_empty))
                };
                if untitled
                    && !busy
                    && let Some(title) = map::derive_title(p["title_preview"].as_str(), &text)
                {
                    if self
                        .call(json!({ "req": "rename_session", "session_id": id, "title": title }))
                        .await
                        .is_ok()
                    {
                        if let Some(info) = self.known.lock().await.get_mut(&id) {
                            info["title"] = json!(title);
                        }
                    }
                }
                self.fresh.lock().await.remove(&id);
                self.learn_state
                    .lock()
                    .await
                    .entry(id.clone())
                    .or_default()
                    .0 += 1;
                let run =
                    self.observer
                        .start_turn(&id, &text, self.run_kind, self.run_title.as_deref());
                if let Some(original) = &self.replay_of {
                    self.observer.link_replay(&run, original);
                }
                if let Err(err) = self.submit(&id, &text).await {
                    self.observer.failed_submit(&id, &run, &err.to_string());
                    return Err(RpcError::internal(err));
                }
                let mut response = json!({ "status": if busy { "queued" } else { "streaming" } });
                if self.replay_of.is_some() {
                    response["run_id"] = json!(run);
                }
                Ok(response)
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
                let busy = self
                    .sessions
                    .lock()
                    .await
                    .get(id)
                    .is_some_and(SessionState::turn_active)
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
                let usage = sessions
                    .get(id)
                    .map(SessionState::usage_json)
                    .unwrap_or_else(|| json!({}));
                Ok(usage)
            }
            "config.get" => {
                let key = p["key"].as_str().unwrap_or_default();
                match key {
                    "project" => {
                        let cwd = p["cwd"]
                            .as_str()
                            .filter(|c| !c.is_empty())
                            .unwrap_or(&self.config.default_cwd)
                            .to_string();
                        let lookup = cwd.clone();
                        let branch = tokio::task::spawn_blocking(move || map::git_branch(&lookup))
                            .await
                            .ok()
                            .flatten();
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
            "config.set"
                if p["session_id"].as_str().is_some_and(|id| !id.is_empty())
                    && (p["key"] == "model"
                        || (p["key"] == "reasoning"
                            && p["scope"] != "global"
                            && p["value"].as_str().is_some_and(|value| {
                                jcode_provider_core::canonical_reasoning_effort(value).is_some()
                                    || jcode_base::prompt::is_swarm_effort(value)
                            }))) =>
            {
                let id = sid()?;
                self.ensure_attached(id).await.map_err(RpcError::internal)?;
                let value = p["value"].as_str().unwrap_or_default();
                let request = match p["key"].as_str().unwrap_or_default() {
                    "reasoning" => json!({
                        "req": "set_reasoning_effort", "session_id": id, "effort": value,
                    }),
                    "model" => {
                        let mut words = value.split_whitespace();
                        let model = words
                            .next()
                            .filter(|model| !model.starts_with('-'))
                            .ok_or_else(|| {
                                RpcError::params("model value must start with a model name")
                            })?;
                        let mut provider = None;
                        while let Some(option) = words.next() {
                            if option == "--provider" {
                                provider = words.next();
                            }
                        }
                        let model = jcode_base::provider::MultiProvider::model_switch_request_for_session_route(
                            model,
                            provider,
                            p["route_api_method"].as_str(),
                        );
                        json!({ "req": "set_model", "session_id": id, "model": model })
                    }
                    _ => unreachable!(),
                };
                call(request).await?;
                Ok(json!({ "value": value }))
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
                    keys.into_iter()
                        .filter_map(|k| approvals.remove(&k))
                        .collect()
                };
                let mut resolved = matching.len();
                resolved += self
                    .hub
                    .answer_session(&id, wanted.as_deref(), &choice)
                    .await;
                for (session, request) in matching {
                    self.resolve_approval(&session, &request, &choice)
                        .await
                        .map_err(RpcError::internal)?;
                }
                Ok(json!({ "resolved": resolved }))
            }
            _ => self.forward(method, p).await,
        }
    }

    /// `/refine [instructions] [--global]`, `/refine rollback [id] [--global]`,
    /// `/refine status`, `/harness`. `None` for other commands.
    async fn harness_command(
        self: &Arc<Self>,
        words: &[&str],
        session_id: Option<&str>,
    ) -> Option<String> {
        use sovereign_prime::entries::EntryStore;
        use sovereign_prime::harness::Harness;
        let legacy = Harness::new(std::path::Path::new(&self.config.home));
        let result: anyhow::Result<String> = match words {
            ["harness", ..] => {
                let mut text = match legacy.current() {
                    t if t.trim().is_empty() => "No learned instructions yet. Run /refine after a session worth learning from.".to_string(),
                    t => format!("Learned instructions (applied to new sessions):\n\n{}", t.trim()),
                };
                if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
                    if let Ok(store) =
                        EntryStore::open_cached(std::path::Path::new(&self.config.home))
                    {
                        let addenda = store.render_prompt(sid).unwrap_or_default();
                        if !addenda.trim().is_empty() {
                            text.push_str(
                                "\n\nContinual Harness prompt entries for this session:\n\n",
                            );
                            text.push_str(addenda.trim());
                        }
                    }
                }
                Ok(text)
            }
            ["refine", "status", ..] => (|| -> anyhow::Result<String> {
                let sid = session_id
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("/refine needs an open session"))?;
                let store = EntryStore::open_cached(std::path::Path::new(&self.config.home))?;
                Ok(sovereign_prime::refine::status(&store, sid))
            })(),
            ["refine", "rollback", rest @ ..] => (|| -> anyhow::Result<String> {
                let sid = session_id
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("/refine needs an open session"))?;
                let id = rest.iter().find(|w| **w != "--global").copied();
                let store = EntryStore::open_cached(std::path::Path::new(&self.config.home))?;
                sovereign_prime::refine::rollback(&store, sid, id)
            })(),
            ["goal", rest @ ..] => (|| -> anyhow::Result<String> {
                let sid = session_id
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("/goal needs an open session"))?;
                let store = sovereign_prime::agent_loop::ControlStore::open_cached(
                    std::path::Path::new(&self.config.home),
                )?;
                sovereign_prime::agent_loop::handle_goal_command(&store, sid, &rest.join(" "))
            })(),
            ["autonomous" | "loop", rest @ ..] => (|| -> anyhow::Result<String> {
                let sid = session_id
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("/autonomous needs an open session"))?;
                let store = sovereign_prime::agent_loop::ControlStore::open_cached(
                    std::path::Path::new(&self.config.home),
                )?;
                sovereign_prime::agent_loop::handle_autonomous_command(&store, sid, &rest.join(" "))
            })(),
            ["heartbeat", rest @ ..] => (|| -> anyhow::Result<String> {
                let sid = session_id
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("/heartbeat needs an open session"))?;
                let store = sovereign_prime::agent_loop::ControlStore::open_cached(
                    std::path::Path::new(&self.config.home),
                )?;
                sovereign_prime::agent_loop::handle_heartbeat_command(&store, sid, &rest.join(" "))
            })(),
            ["refine", rest @ ..] => {
                async {
                    let sid = session_id
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| anyhow!("/refine needs an open session"))?;
                    let complete = self
                        .config
                        .complete
                        .clone()
                        .ok_or_else(|| anyhow!("no model is available for /refine"))?;
                    let global = rest.contains(&"--global");
                    let instructions: Vec<&str> =
                        rest.iter().filter(|w| **w != "--global").copied().collect();
                    let instructions = (!instructions.is_empty()).then(|| instructions.join(" "));
                    self.ensure_attached(sid).await?;
                    let history = self
                        .call(json!({ "req": "get_history", "session_id": sid }))
                        .await?;
                    let turns: Vec<sovereign_prime::harness::Turn> = history["messages"]
                        .as_array()
                        .map(|list| {
                            list.iter()
                                .map(|m| sovereign_prime::harness::Turn {
                                    role: m["role"].as_str().unwrap_or_default().to_string(),
                                    text: m["content"].as_str().unwrap_or_default().to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if turns.is_empty() {
                        return Ok(
                            "Nothing to learn from yet: this session has no messages.".to_string()
                        );
                    }
                    let store = EntryStore::open_cached(std::path::Path::new(&self.config.home))?;
                    let (system, user) = sovereign_prime::refine::build_request(
                        &store,
                        sid,
                        &turns,
                        instructions.as_deref(),
                        global,
                    );
                    let started = crate::observability::now();
                    let reply = complete(system, user).await;
                    self.observer.record_aux(
                        sid,
                        "other",
                        Some("Refine harness entries"),
                        None,
                        None,
                        started,
                        reply.as_ref().ok().and_then(|done| done.usage),
                        reply.as_ref().err().map(|err| err.to_string()).as_deref(),
                    );
                    let reply = reply?.text;
                    match sovereign_prime::refine::apply(
                        &store, sid, &reply, &turns, global, "refine",
                    ) {
                        Ok(outcome) => {
                            let mut text = format!(
                                "Refined ({}): {}\n",
                                if global { "global" } else { "this session" },
                                outcome.summary
                            );
                            if !outcome.created.is_empty() {
                                text.push_str(&format!("+ created {}\n", outcome.created.len()));
                            }
                            if !outcome.updated.is_empty() {
                                text.push_str(&format!("~ updated {}\n", outcome.updated.len()));
                            }
                            if !outcome.deleted.is_empty() {
                                text.push_str(&format!("- deleted {}\n", outcome.deleted.len()));
                            }
                            text.push_str(&format!(
                                "Undo with /refine rollback {}",
                                outcome.changeset_id
                            ));
                            Ok(text)
                        }
                        Err(err) => Ok(format!("No change: {err:#}")),
                    }
                }
                .await
            }
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
            return Err(RpcError {
                code: METHOD_NOT_FOUND,
                message: format!("unknown method {method}"),
                data: None,
            });
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
        upstream
            .tx
            .send(frame.to_string())
            .await
            .map_err(|_| RpcError::internal(anyhow!("feature backend connection closed")))?;
        let reply = tokio::time::timeout(FORWARD_TIMEOUT, rx)
            .await
            .map_err(|_| RpcError::internal(anyhow!("{method} timed out in the feature backend")))?
            .map_err(|_| RpcError::internal(anyhow!("the feature backend restarted; try again")))?;
        features.touch(method, "forwarded-rpc");
        match reply.get("error") {
            Some(err) => Err(RpcError {
                code: err["code"].as_i64().unwrap_or(INTERNAL),
                message: err["message"]
                    .as_str()
                    .unwrap_or("feature backend error")
                    .to_string(),
                data: err.get("data").cloned(),
            }),
            None => Ok(reply["result"].clone()),
        }
    }

    /// The live upstream connection, (re)opened as needed.
    async fn upstream(
        self: &Arc<Self>,
        features: &crate::features::Features,
    ) -> Result<Arc<Upstream>> {
        let mut slot = self.upstream.lock().await;
        if let Some(up) = slot.as_ref() {
            if !up.tx.is_closed() {
                return Ok(up.clone());
            }
        }
        let port = features.port().await?;
        let url = format!("ws://127.0.0.1:{port}/api/ws?token={}", features.token);
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .context("connecting to the feature backend")?;
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
                    let Ok(mut frame) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    let (Some(conn), Some(up)) = (conn.upgrade(), weak_up.upgrade()) else {
                        break;
                    };
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
            Upstream {
                tx,
                pending: Mutex::new(HashMap::new()),
                tasks: vec![writer.abort_handle(), reader.abort_handle()],
            }
        });
        *slot = Some(up.clone());
        Ok(up)
    }

    /// A reply to one of our server requests (approvals, or relayed ones).
    async fn on_client_reply(&self, frame: &Value) {
        let Some(id) = frame["id"].as_str() else {
            return;
        };
        if let Some(raw) = id.strip_prefix(UPSTREAM_REQUEST_PREFIX) {
            if let Some(up) = self.upstream.lock().await.clone() {
                let mut reply = frame.clone();
                reply["id"] = raw
                    .parse::<u64>()
                    .map(|n| json!(n))
                    .unwrap_or_else(|_| json!(raw));
                let _ = up.tx.send(reply.to_string()).await;
            }
            return;
        }
        let choice = frame["result"]["choice"].as_str().unwrap_or("deny");
        if self.hub.answer(id, choice).await {
            return;
        }
        let Some((session, request)) = self.approvals.lock().await.remove(id) else {
            return;
        };
        let _ = self.resolve_approval(&session, &request, choice).await;
    }
}

fn check_reply(reply: Value) -> Result<Value> {
    if reply["ev"] == "error" {
        return Err(anyhow!(
            "{}",
            reply["message"].as_str().unwrap_or("engine error")
        ));
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

/// `/api/agent/run`: one headless prompt on its own hidden session, run to
/// completion (or `timeout`), with every tool approval denied outright and no
/// desktop involved at all. Mirrors [`run`]'s `Conn` setup, minus the
/// WebSocket: `to_ws` just feeds a channel this function drains itself.
/// Replay a completed run: branch the session, rewind to that turn, resubmit the prompt.
pub async fn replay_run(
    config: Arc<Config>,
    hub: Arc<Hub>,
    observer: Arc<Observer>,
    run_id: &str,
) -> Result<Value> {
    let run_id = run_id.to_string();
    let (session, turn_index, prompt) = tokio::task::spawn_blocking({
        let observer = observer.clone();
        let run_id = run_id.clone();
        move || observer.replay_turn_index(&run_id)
    })
    .await??;
    let prompt =
        prompt.ok_or_else(|| anyhow!("run has no captured prompt; enable content capture"))?;

    let (to_ws, mut ws_out) = mpsc::channel::<Message>(1024);
    let client = Arc::new(Client {
        id: hub.next_client_id(),
        to_ws: to_ws.clone(),
        sessions: Mutex::new(Default::default()),
    });
    let conn = Arc::new(Conn {
        config: config.clone(),
        to_ws,
        control: Mutex::new(None),
        links: Mutex::new(HashMap::new()),
        link_tasks: Mutex::new(Vec::new()),
        known: Mutex::new(HashMap::new()),
        fresh: Mutex::new(std::collections::HashSet::new()),
        learn_state: Mutex::new(HashMap::new()),
        learning_now: Mutex::new(std::collections::HashSet::new()),
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
        client,
        observer: observer.clone(),
        run_kind: "invoke_agent",
        run_title: Some(format!("Replay {run_id}")),
        replay_of: Some(run_id.clone()),
    });
    let control = conn.open_link().await.context("engine unavailable")?;
    *conn.control.lock().await = Some(control);

    let branched = conn
        .dispatch(
            "session.branch",
            &json!({ "session_id": session, "name": format!("Replay {}", run_id.chars().take(24).collect::<String>()) }),
        )
        .await
        .map_err(|e| anyhow!(e.message))?;
    let child = branched["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if child.is_empty() {
        bail!("branch did not return a session id");
    }

    let history = conn
        .call(json!({ "req": "get_history", "session_id": child }))
        .await
        .context("history")?;
    let messages = history["messages"].as_array().cloned().unwrap_or_default();
    let mut user_seen = 0_i64;
    let mut cut: Option<usize> = None;
    for (i, message) in messages.iter().enumerate() {
        if message["role"] == "user" {
            if user_seen == turn_index {
                cut = Some(i);
                break;
            }
            user_seen += 1;
        }
    }
    match cut {
        None if turn_index == 0 => {
            conn.call(json!({ "req": "clear", "session_id": child }))
                .await?;
        }
        Some(0) => {
            conn.call(json!({ "req": "clear", "session_id": child }))
                .await?;
        }
        Some(index) => {
            conn.call(json!({ "req": "rewind", "session_id": child, "message_index": index }))
                .await?;
        }
        None => bail!("turn index {turn_index} not found in branched session"),
    }

    let submitted = match conn
        .dispatch(
            "prompt.submit",
            &json!({ "session_id": child, "text": prompt }),
        )
        .await
    {
        Ok(submitted) => submitted,
        Err(err) => return Err(anyhow!("{}", err.message)),
    };
    let new_run = submitted["run_id"]
        .as_str()
        .ok_or_else(|| anyhow!("replay run did not start"))?
        .to_string();
    let replay_session = child.clone();
    tokio::spawn(async move {
        while let Some(msg) = ws_out.recv().await {
            let Message::Text(text) = msg else { continue };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame["method"] == "event"
                && frame["params"]["session_id"] == replay_session
                && frame["params"]["type"] == "message.complete"
            {
                break;
            }
        }
        for task in conn.link_tasks.lock().await.drain(..) {
            task.abort();
        }
    });
    Ok(json!({
        "ok": true,
        "session_id": child,
        "run_id": new_run,
        "replay_of": run_id,
        "status": "streaming",
    }))
}

pub(crate) async fn agent_run(
    config: Arc<Config>,
    hub: Arc<Hub>,
    observer: Arc<Observer>,
    prompt: &str,
    cwd: Option<&str>,
    title: Option<&str>,
    timeout: Duration,
) -> Result<Value> {
    let (to_ws, mut ws_out) = mpsc::channel::<Message>(1024);
    let client = Arc::new(Client {
        id: hub.next_client_id(),
        to_ws: to_ws.clone(),
        sessions: Mutex::new(Default::default()),
    });
    let conn = Arc::new(Conn {
        config,
        to_ws,
        control: Mutex::new(None),
        links: Mutex::new(HashMap::new()),
        link_tasks: Mutex::new(Vec::new()),
        known: Mutex::new(HashMap::new()),
        fresh: Mutex::new(std::collections::HashSet::new()),
        learn_state: Mutex::new(HashMap::new()),
        learning_now: Mutex::new(std::collections::HashSet::new()),
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
        client,
        observer,
        run_kind: "cron",
        run_title: title.map(str::to_string),
        replay_of: None,
    });

    let control = conn.open_link().await.context("engine unavailable")?;
    *conn.control.lock().await = Some(control);

    let mut create_params = json!({ "cwd": cwd });
    if let Some(title) = title {
        create_params["title"] = json!(title);
    }
    let created = conn
        .dispatch("session.create", &create_params)
        .await
        .map_err(|e| anyhow!(e.message))?;
    let session_id = created["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        bail!("engine did not return a session id");
    }

    // Every approval on this session is denied outright, before it can ever
    // reach a desktop prompt (see the two `Out::Approval`/`decide` gates).
    hub.mark_headless(&session_id).await;

    let outcome = tokio::time::timeout(timeout, async {
        conn.dispatch(
            "prompt.submit",
            &json!({ "session_id": session_id, "text": prompt }),
        )
        .await
        .map_err(|e| anyhow!(e.message))?;
        while let Some(msg) = ws_out.recv().await {
            let Message::Text(text) = msg else { continue };
            let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if frame["method"] != "event" || frame["params"]["session_id"] != session_id.as_str() {
                continue;
            }
            if frame["params"]["type"] == "message.complete" {
                let payload = frame["params"]["payload"].clone();
                return Ok(payload);
            }
        }
        bail!("engine connection closed before the turn finished")
    })
    .await;

    hub.unmark_headless(&session_id).await;
    // Only now is the session guaranteed persisted (jcode does not write a
    // session record until its first turn), so hide it from session.list
    // here rather than before the turn — the same mechanism a user's own
    // archived chats use. Best-effort: a session that never got this far
    // (e.g. jcode was unreachable) has nothing to hide.
    let _ = conn
        .dispatch(
            "session.set_hidden",
            &json!({ "session_id": session_id, "hidden": true }),
        )
        .await;
    for task in conn.link_tasks.lock().await.drain(..) {
        task.abort();
    }

    let usage = |payload: &Value| -> Value {
        let u = &payload["usage"];
        if u.is_null() {
            Value::Null
        } else {
            json!({ "input_tokens": u["input"], "output_tokens": u["output"], "cached_tokens": u["cache_read"] })
        }
    };
    Ok(match outcome {
        Ok(Ok(payload)) => {
            let ok = payload["status"] == "complete";
            let text = payload["text"].as_str().unwrap_or_default().to_string();
            json!({
                "ok": ok,
                "text": text,
                "error": if ok { Value::Null } else { json!(format!("the turn did not complete cleanly ({})", payload["status"].as_str().unwrap_or("unknown"))) },
                "session_id": session_id,
                "usage": usage(&payload),
            })
        }
        Ok(Err(err)) => {
            json!({ "ok": false, "text": "", "error": err.to_string(), "session_id": session_id })
        }
        Err(_) => {
            let _ = conn
                .dispatch("session.interrupt", &json!({ "session_id": session_id }))
                .await;
            json!({ "ok": false, "text": "", "error": "timed out waiting for the turn to finish", "session_id": session_id })
        }
    })
}

pub async fn run(
    ws: Ws,
    config: Arc<Config>,
    hub: Arc<Hub>,
    observer: Arc<Observer>,
) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (to_ws, mut ws_out) = mpsc::channel::<Message>(1024);
    let client = Arc::new(Client {
        id: hub.next_client_id(),
        to_ws: to_ws.clone(),
        sessions: Mutex::new(Default::default()),
    });
    hub.add(client.clone()).await;
    let conn = Arc::new(Conn {
        config,
        to_ws,
        control: Mutex::new(None),
        links: Mutex::new(HashMap::new()),
        link_tasks: Mutex::new(Vec::new()),
        known: Mutex::new(HashMap::new()),
        fresh: Mutex::new(std::collections::HashSet::new()),
        learn_state: Mutex::new(HashMap::new()),
        learning_now: Mutex::new(std::collections::HashSet::new()),
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
        run_kind: "invoke_agent",
        run_title: None,
        replay_of: None,
    });

    // Control link first: if the engine is unreachable, refuse the client.
    match conn.open_link().await {
        Ok(control) => *conn.control.lock().await = Some(control),
        Err(_) => {
            let _ = ws_tx
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::from(1011),
                    reason: "engine unavailable".into(),
                })))
                .await;
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

    conn.emit(
        "gateway.ready",
        None,
        json!({ "skin": {}, "change_events": false, "replay_epoch": conn.observer.replay_epoch() }),
    )
    .await;

    while let Some(msg) = ws_rx.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        if std::env::var_os("SOVEREIGN_GATEWAY_TRACE").is_some() {
            eprintln!(
                "sovereign-gateway: client {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let frame: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                let err = RpcError {
                    code: PARSE_ERROR,
                    message: "parse error".into(),
                    data: None,
                };
                conn.send_json(rpc_error(Value::Null, err)).await;
                continue;
            }
        };
        if frame.get("method").is_none()
            && (frame.get("result").is_some() || frame.get("error").is_some())
        {
            conn.on_client_reply(&frame).await;
            continue;
        }
        let Some(method) = frame["method"].as_str().map(str::to_string) else {
            let err = RpcError {
                code: -32600,
                message: "invalid request".into(),
                data: None,
            };
            conn.send_json(rpc_error(frame["id"].clone(), err)).await;
            continue;
        };
        let id = frame["id"].clone();
        let Ok(permit) = conn.in_flight.clone().try_acquire_owned() else {
            let err = RpcError {
                code: INTERNAL,
                message: "too many requests in flight".into(),
                data: None,
            };
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
