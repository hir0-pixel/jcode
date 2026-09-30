//! One WebSocket client: JSON-RPC 2.0 in, harness API calls out, harness
//! events translated back into Hermes `event` notifications.

use crate::approvals::{Client, Hub};
use crate::learn::Trigger;
use crate::map::{self, Out, SessionState};
use crate::observability::Observer;
use crate::{Config, MAX_FRAME_BYTES};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use jcode_base::obs_sink::{Span, emit};
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

mod driver;
pub(crate) use driver::start as start_driver;
pub(crate) mod attach;
mod local_state;
mod provider_state;
mod side_agents;
mod spawn_tree;
mod toolsets;

const HARNESS_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a closing session (or a finished headless run) waits for its last learning review.
const DISPOSE_LEARNING_WAIT: Duration = Duration::from_secs(30);
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

fn send_message_request(session_id: &str, text: &str, reminder: Option<&str>, images: Vec<(String, String)>) -> Value {
    json!({ "req": "send_message", "session_id": session_id, "content": text, "system_reminder": reminder, "images": images })
}

#[derive(Debug)]
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

static STORE_WARNED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
    std::sync::LazyLock::new(Default::default);

/// A store failed to open: say so once (log line plus an error `status.update` to every window),
/// not on every retry. Goals and learning stay off until it opens again.
async fn store_unavailable(hub: &Hub, what: &'static str, err: &anyhow::Error) {
    error_once(hub, what, format!("The {what} store could not be opened, so those features are off: {err:#}")).await;
}

/// One log line and one error `status.update` per `key` per process.
async fn error_once(hub: &Hub, key: &'static str, text: String) {
    if !STORE_WARNED.lock().unwrap_or_else(|e| e.into_inner()).insert(key) {
        return;
    }
    eprintln!("sovereign: {text}");
    let event = json!({ "jsonrpc": "2.0", "method": "event", "params": { "type": "status.update", "payload": { "kind": "error", "text": text } } });
    hub.broadcast_text(event.to_string()).await;
}

/// Log `what` failing to open once per process (no window to tell: callers without a `Conn`).
/// True the first time.
fn log_once(what: &'static str, err: &anyhow::Error) -> bool {
    let first = STORE_WARNED.lock().unwrap_or_else(|e| e.into_inner()).insert(what);
    if first {
        eprintln!("sovereign: {what} unavailable: {err:#}");
    }
    first
}

/// The entry store, or None after logging once why (for callers that have no `Conn` to report through).
pub(crate) fn entries_or_log(home: &str) -> Option<Arc<sovereign_prime::entries::EntryStore>> {
    sovereign_prime::entries::EntryStore::open_cached(Path::new(home)).map_err(|e| log_once("learning store (no window)", &e)).ok()
}

/// The goal-control store, or None after logging once why.
pub(crate) fn control_or_log(home: &str) -> Option<Arc<sovereign_prime::agent_loop::ControlStore>> {
    sovereign_prime::agent_loop::ControlStore::open_cached(Path::new(home)).map_err(|e| log_once("goal control store (no window)", &e)).ok()
}

/// The daily database backup failed (`migrate::backup_error`): say so once.
pub(crate) async fn report_backup_error(hub: &Hub, err: Option<String>) {
    if let Some(err) = err {
        error_once(hub, "backup", format!("The daily database backup failed: {err}")).await;
    }
}

/// Whether any child of `parent` (other than `except`) is running: the engine's own status
/// (whichever connection started the child) or a turn this connection sees running.
fn children_running(list: &[Value], parent: &str, except: Option<&str>, live: &HashMap<String, SessionState>) -> bool {
    list.iter().any(|s| {
        let id = s["session_id"].as_str().unwrap_or_default();
        s["parent_session_id"].as_str() == Some(parent)
            && Some(id) != except
            && (["status", "swarm_status"].iter().any(|f| matches!(s[*f].as_str(), Some("running" | "processing")))
                || live.get(id).is_some_and(SessionState::turn_active))
    })
}

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
    /// Sessions with a learning pass running (std mutex: the guard releases on drop, even when the pass is cancelled).
    learning_now: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Sessions whose context was compacted and not yet reviewed (Prime's `_compactAutoRefinePending`).
    compact_pending: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Learning passes started here; a headless run waits for them before closing its links.
    learning_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The engine-level driver's link (see `driver.rs`), not a desktop window.
    driver: bool,
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
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: Arc<Config>,
        to_ws: mpsc::Sender<Message>,
        hub: Arc<Hub>,
        client: Arc<Client>,
        observer: Arc<Observer>,
        driver: bool,
        run_kind: &'static str,
        run_title: Option<String>,
        replay_of: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            to_ws,
            control: Mutex::new(None),
            links: Mutex::new(HashMap::new()),
            link_tasks: Mutex::new(Vec::new()),
            known: Mutex::new(HashMap::new()),
            fresh: Mutex::new(Default::default()),
            learning_now: Default::default(),
            compact_pending: Default::default(),
            learning_tasks: Default::default(),
            driver,
            next_id: AtomicU64::new(1),
            next_server_request: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
            accept_waiters: Mutex::new(HashMap::new()),
            upstream: Mutex::new(None),
            next_forward: AtomicU64::new(1),
            hub,
            client,
            observer,
            run_kind,
            run_title,
            replay_of,
        })
    }

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
            if let Some(link) = self.links.lock().await.get(sid).filter(|l| !l.is_closed()) {
                return Ok(link.clone());
            }
        }
        self.control
            .lock()
            .await
            .clone()
            .filter(|l| !l.is_closed())
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
        let mine = tx.clone();
        let reader = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(conn) = weak.upgrade() else { return };
                if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                    conn.on_harness_frame(frame).await;
                }
            }
            // The bridge closed: forget this link so the next attach or tick opens a fresh one.
            if let Some(conn) = weak.upgrade() {
                conn.link_lost(&mine).await;
            }
        });
        self.link_tasks.lock().await.extend([
            bridge.abort_handle(),
            writer.abort_handle(),
            reader.abort_handle(),
        ]);
        Ok(tx)
    }

    /// A bridge link closed: forget it, and end the turns that were running on it, whose
    /// `message.complete` is lost. Otherwise `busy` stays set and the driver skips the goal forever.
    async fn link_lost(&self, link: &mpsc::Sender<String>) {
        let lost: Vec<String> = {
            let mut links = self.links.lock().await;
            let lost = links.iter().filter(|(_, l)| l.same_channel(link)).map(|(sid, _)| sid.clone()).collect();
            links.retain(|_, l| !l.same_channel(link));
            lost
        };
        {
            let mut control = self.control.lock().await;
            if control.as_ref().is_some_and(|l| l.same_channel(link)) {
                *control = None;
            }
        }
        for sid in lost {
            let was_running = self.sessions.lock().await.get_mut(&sid).is_some_and(SessionState::end_turn);
            // A driver that yields the session to a window leaves the run to that window.
            let closed = !(self.driver && self.hub.has_window(&sid).await) && self.observer.has_active_run(&sid);
            if closed {
                self.observer.event(&sid, "message.complete", &json!({ "status": "interrupted", "text": "" }));
            }
            if was_running && !self.driver {
                self.emit("message.complete", Some(&sid), json!({ "status": "interrupted", "text": "" })).await;
            }
            if self.driver && (was_running || closed) {
                driver::resume_soon(&sid);
            }
        }
        driver::poke();
    }

    async fn control_store(&self) -> Option<Arc<sovereign_prime::agent_loop::ControlStore>> {
        match sovereign_prime::agent_loop::ControlStore::open_cached(Path::new(&self.config.home)) {
            Ok(store) => Some(store),
            Err(err) => {
                store_unavailable(&self.hub, "goal control", &err).await;
                None
            }
        }
    }

    async fn entry_store(&self) -> Option<Arc<sovereign_prime::entries::EntryStore>> {
        match sovereign_prime::entries::EntryStore::open_cached(Path::new(&self.config.home)) {
            Ok(store) => Some(store),
            Err(err) => {
                store_unavailable(&self.hub, "learning", &err).await;
                None
            }
        }
    }

    /// Open the control link when there is none or its bridge has closed.
    async fn ensure_control(self: &Arc<Self>) -> Result<()> {
        if self.control.lock().await.as_ref().is_some_and(|l| !l.is_closed()) {
            return Ok(());
        }
        let link = self.open_link().await?;
        *self.control.lock().await = Some(link);
        Ok(())
    }

    /// Attach this connection to `session_id` once.
    async fn ensure_attached(self: &Arc<Self>, session_id: &str) -> Result<Value> {
        if self.links.lock().await.get(session_id).is_some_and(|l| !l.is_closed()) {
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
                // A turn the engine still runs (the old link only dropped) must not be doubled.
                if reply["session"]["status"].as_str() == Some("processing") {
                    self.sessions.lock().await.entry(session_id.to_string()).or_default().mark_running();
                }
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
    /// Send a user message. `reminder` rides the turn's uncached system-reminder slot (not the
    /// cached static prefix, not the transcript) and lasts for this turn only.
    async fn submit(self: &Arc<Self>, session_id: &str, text: &str, reminder: Option<&str>, images: Vec<(String, String)>) -> Result<()> {
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
                send_message_request(session_id, text, reminder, images),
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
        // The driver only keeps its session state while a window has the
        // session open: the window observes, renders and prompts for it.
        let yields = self.driver
            && match frame["session_id"].as_str() {
                Some(sid) => self.hub.has_window(sid).await,
                None => false,
            };
        if !yields {
            self.observer.harness_event(&frame);
        }
        let outs = map::map_event(&frame, &mut *self.sessions.lock().await);
        if yields {
            return;
        }
        for out in outs {
            match out {
                Out::Event {
                    ty,
                    session_id,
                    payload,
                } => {
                    self.observer.event(&session_id, ty, &payload);
                    let completed = ty == "message.complete";
                    // Tool-call boundaries are the only natural checkpoint
                    // inside a busy, possibly long, multi-tool-call turn:
                    // `message.complete` only fires once at the very end.
                    // Check for a due RLM "steer" heartbeat there so it can
                    // reach the session at its next turn boundary instead of
                    // waiting for the whole turn to finish.
                    if ty == "tool.complete" {
                        sovereign_prime::agent_loop::observe_tool(
                            &session_id,
                            payload["name"].as_str().unwrap_or(""),
                            &payload["args"],
                            payload["result_text"].as_str().unwrap_or(""),
                        );
                        self.clone().maybe_steer_heartbeat(session_id.clone());
                    }
                    let payload_for_loop = completed.then(|| payload.clone());
                    let compacted = ty == "status.update" && payload["kind"] == "compress";
                    self.emit(ty, Some(&session_id), payload).await;
                    if compacted {
                        // Prime reviews after a compaction whatever the count, once the cooldown allows.
                        self.compact_pending.lock().unwrap_or_else(|e| e.into_inner()).insert(session_id.clone());
                        self.schedule_learning(session_id.clone(), Trigger::Compact);
                    }
                    if let Some(loop_payload) = payload_for_loop {
                        self.schedule_learning(session_id.clone(), Trigger::TurnInterval);
                        driver::turn_done(self.clone(), session_id, loop_payload);
                    }
                }
                Out::Approval {
                    session_id,
                    request_id,
                    tool_name,
                    description,
                } => {
                    if self.driver || self.hub.is_headless(&session_id).await {
                        // Unattended (`/api/agent/run`, or the driver with no window open): no prompt waits;
                        // Hermes's approval config decides, else deny and park it for the desktop.
                        let conn = self.clone();
                        tokio::spawn(async move {
                            let choice = conn.hub.unattended(&session_id, &tool_name, &description, "").await;
                            let _ = conn.resolve_approval(&session_id, &request_id, &choice).await;
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

    /// Check, right after a completed turn or a compaction, whether this chat is due an auto-refine
    /// gate call (Prime's `_maybeAutoRefine`: an assistant-message counter, a compaction flag and a
    /// cooldown, no idle wait). Runs as a spawned task only so it never blocks the turn's own
    /// response; the check itself happens immediately. The task is remembered so a headless run can
    /// let it finish before it closes its links ([`close_links`]).
    fn schedule_learning(self: &Arc<Self>, session: String, trigger: Trigger) {
        if self.config.learning.is_none() {
            return;
        }
        let conn = self.clone();
        let task = tokio::spawn(async move { conn.learning_check(&session, trigger).await });
        let mut tasks = self.learning_tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.retain(|t| !t.is_finished());
        tasks.push(task);
    }

    /// One learning check for `session`: skip (with a `learning.skip` span saying why) or run the pass.
    async fn learning_check(self: &Arc<Self>, session: &str, trigger: Trigger) {
        let Some(learning) = self.config.learning.clone() else {
            return;
        };
        let skip = |reason: &str, assistants: Option<usize>| {
            let span = Span::new("learning.skip").session(session).attr("trigger", trigger.as_str()).attr("reason", reason);
            emit(match assistants {
                Some(n) => span.attr("assistants", n as u64).attr("interval", learning.turn_interval as u64),
                None => span,
            });
        };
        // The model-callable `refine` tool / REPL `refine` schedule a
        // request that runs at the end of the turn, independent of the
        // checkpoint counter (Prime runs those immediately, too).
        let store = self.entry_store().await;
        let scheduled = store.as_ref().and_then(|store| store.refine_pending(session).ok()).unwrap_or(false);
        // The `learning.enabled` switch lives in sovereign.db.
        let enabled = store.as_ref().is_some_and(|store| store.learning_enabled());
        if !enabled && !scheduled {
            return skip("disabled", None);
        }
        // A closing session is reviewed even mid-turn bookkeeping; otherwise wait for the turn to end.
        if trigger != Trigger::Dispose && self.sessions.lock().await.get(session).is_some_and(SessionState::turn_active) {
            return skip("busy", None);
        }
        struct Running<'a>(&'a Conn, String);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.learning_now.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.1);
            }
        }
        if !self.learning_now.lock().unwrap_or_else(|e| e.into_inner()).insert(session.to_string()) {
            return skip("in_progress", None);
        }
        let _running = Running(self, session.to_string());
        let mut gate = None;
        if let (true, Some(store)) = (enabled, &store) {
            let history = match self.history(session).await {
                Ok(history) => history,
                Err(err) => return eprintln!("sovereign: learning check for {session} failed: {err:#}"),
            };
            let raw = history["messages"].as_array().map(Vec::as_slice).unwrap_or_default();
            let compact = self.compact_pending.lock().unwrap_or_else(|e| e.into_inner()).contains(session);
            match crate::learn::due(store, session, &learning, raw, trigger, compact, crate::observability::now()) {
                // Prime reviews top-level sessions only (`_rlmDepth === 0`).
                Ok(_) if self.is_child_session(session).await => skip("depth>0", None),
                Ok(due) => gate = Some(due),
                Err((reason, assistants)) => skip(reason, Some(assistants)),
            }
        }
        if gate.is_none() && !scheduled {
            return;
        }
        let compacted = gate.is_some_and(|d| d.trigger == Trigger::Compact);
        match crate::learn::pass(self, session, gate).await {
            Ok(result) => {
                if compacted {
                    self.compact_pending.lock().unwrap_or_else(|e| e.into_inner()).remove(session);
                }
                if let Some(text) = result {
                    self.emit("status.update", Some(session), json!({ "kind": "learning", "text": text })).await;
                }
            }
            Err(err) => eprintln!("sovereign: learning pass for {session} failed: {err:#}"),
        }
    }

    /// Whether `session` is a sub-agent or fork (it has a parent); Prime reviews top-level sessions only.
    async fn is_child_session(&self, session: &str) -> bool {
        if self.known.lock().await.get(session).is_some_and(|info| !info["parent_session_id"].is_null()) {
            return true;
        }
        let id = session.to_string();
        tokio::task::spawn_blocking(move || jcode_base::session::Session::load_startup_stub(&id).ok().and_then(|s| s.parent_id).is_some())
            .await
            .unwrap_or(false)
    }

    /// Prime's review before dispose (`_drainPendingRefinementForDisposal`): wait (bounded) for a
    /// pass already running, then run the review if it is due, before the session's state goes.
    pub(crate) async fn learn_before_dispose(self: &Arc<Self>, session: &str) {
        if self.config.learning.is_none() {
            return;
        }
        let review = async {
            while self.learning_now.lock().unwrap_or_else(|e| e.into_inner()).contains(session) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            self.learning_check(session, Trigger::Dispose).await;
        };
        if tokio::time::timeout(DISPOSE_LEARNING_WAIT, review).await.is_err() {
            emit(Span::new("learning.skip").session(session).attr("trigger", "dispose").attr("reason", "timeout"));
        }
    }

    /// Deliver a due RLM "steer" heartbeat to a busy session right now, via
    /// the same soft-interrupt primitive `session.steer` uses — not a hard
    /// cancel, and not a wait for the turn to end. Plain `follow_up`
    /// heartbeats are unaffected: they stay on the idle-only path in
    /// `driver::turn_done`. A cheap local SQLite read per tool-call
    /// boundary; a no-op unless this session has a due steer-mode heartbeat.
    fn maybe_steer_heartbeat(self: Arc<Self>, session_id: String) {
        tokio::spawn(async move {
            let Some(store) = self.control_store().await else { return };
            let due = sovereign_prime::agent_loop::due_steer_heartbeat(&store, &session_id);
            let Ok(Some(sovereign_prime::agent_loop::Continuation::Heartbeat { prompt, .. })) = due
            else {
                return;
            };
            if let Err(err) = self
                .call(json!({
                    "req": "soft_interrupt",
                    "session_id": session_id,
                    "content": prompt,
                }))
                .await
            {
                eprintln!("sovereign: steer heartbeat delivery for {session_id}: {err:#}");
            }
        });
    }

    async fn child_sessions_running(self: &Arc<Self>, parent_id: &str) -> bool {
        let Ok(reply) = self.call(json!({ "req": "list_sessions" })).await else {
            return false;
        };
        let sessions = self.sessions.lock().await;
        children_running(reply["sessions"].as_array().map_or(&[][..], Vec::as_slice), parent_id, None, &sessions)
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
            m if local_state::handles(m) => self.local_state(m, p).await,
            m if attach::handles(m) => {
                // Only a session this connection has open or the engine has stored may stage files.
                if let Some(id) = p["session_id"].as_str().filter(|s| !s.is_empty()) {
                    if !self.client.sessions.lock().await.contains(id) && !self.known.lock().await.contains_key(id) && !jcode_base::session::session_exists(id) {
                        return Err(RpcError::params("unknown session_id"));
                    }
                }
                let cwd = match p["session_id"].as_str() {
                    Some(id) => self.session_cwd(id).await,
                    None => None,
                }
                .unwrap_or_else(|| self.config.default_cwd.clone());
                attach::handle(m, &self.config.home, &cwd, p)
                    .map_err(|(code, message)| RpcError { code, message, data: None })
            }
            "setup.status" => Ok(provider_state::setup_status(&self.config.provider)),
            "setup.runtime_check" => Ok(provider_state::runtime_check(&self.config.provider, &self.config.model, p["provider"].as_str().filter(|r| !r.is_empty()))),
            "model.options" => {
                // The served provider's models come from the session's catalog when one is named.
                let listed = match p["session_id"].as_str().filter(|s| !s.is_empty()) {
                    Some(id) => tokio::time::timeout(Duration::from_secs(3), self.call(json!({ "req": "list_models", "session_id": id }))).await.ok().and_then(Result::ok),
                    None => None,
                };
                let models = listed.as_ref().and_then(|r| r["models"].as_array()).map(|m| m.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                Ok(provider_state::model_options(&self.config.provider, &self.config.model, models, p["include_unconfigured"] == true, &self.config.reasoning_efforts))
            }
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
                let engine = json!({
                    "pairs": pairs, "sub": {}, "canon": {}, "commands": {},
                    "categories": [{ "name": "Harness", "pairs": pairs }],
                    "skills": {}, "skill_count": jcode_base::skill::SkillRegistry::shared_snapshot().list().len(), "warning": "",
                });
                // Hermes's own commands, skills and quick/plugin commands, when its backend answers.
                Ok(match self.forward("commands.catalog", &json!({})).await {
                    Ok(hermes) => crate::slash_forward::merge_catalog(engine, &hermes),
                    Err(_) => engine,
                })
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
            "spawn_tree.list" | "spawn_tree.save" | "spawn_tree.load" => self.spawn_tree(method, p).await,
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
                driver::poke();
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
                // Not the engine's: Hermes answers quick/plugin/bundle/skill commands and prompt
                // builders without a session. Commands on chat state are never forwarded.
                let (name, arg) = match command.trim_start_matches('/').split_once(char::is_whitespace) {
                    Some((name, arg)) => (name, arg.trim()),
                    None => (command.trim_start_matches('/'), ""),
                };
                if !name.is_empty() && !crate::slash_forward::SESSION_BOUND.contains(&name) {
                    if let Ok(done) = self
                        .forward("command.dispatch", &json!({ "name": name, "arg": arg }))
                        .await
                    {
                        return Ok(done);
                    }
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
                    .or(profile.model.as_deref().filter(|_| self.config.profile_model_applies));
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
                // A remembered effort the served model cannot take (a local Ollama
                // model has no effort levels) is a preference that does not apply
                // here, not a reason to refuse the chat: skip it and say so.
                let mut applied_effort = None;
                if let Some(effort) = selected_effort {
                    let session_link =
                        self.links.lock().await.get(&id).cloned().ok_or_else(|| {
                            RpcError::internal(anyhow!("new session link was lost"))
                        })?;
                    match self.call_on(&session_link, json!({
                        "req": "set_reasoning_effort", "session_id": id.clone(), "effort": effort,
                    })).await {
                        Ok(_) => applied_effort = Some(effort),
                        Err(err) => eprintln!("sovereign: reasoning effort {effort:?} not applied to {id}: {err:#}"),
                    }
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
                if let Some(effort) = applied_effort {
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
                self.learn_before_dispose(id).await;
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
                self.learn_before_dispose(&id).await;
                self.links.lock().await.remove(&id);
                self.client.sessions.lock().await.remove(&id);
                crate::sessions_rest::delete_everywhere(&self.config, &id).await.map_err(RpcError::internal)?;
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
                let run =
                    self.observer
                        .start_turn(&id, &text, self.run_kind, self.run_title.as_deref());
                if let Some(original) = &self.replay_of {
                    self.observer.link_replay(&run, original);
                }
                // Images staged by `image.attach` ride along on this turn.
                let (staged, images, unreadable) = {
                    let (home, id) = (self.config.home.clone(), id.clone());
                    tokio::task::spawn_blocking(move || attach::staged_images(&home, &id)).await.map_err(|e| RpcError::internal(e.into()))?
                };
                if let Err(err) = self.submit(&id, &text, p["system_reminder"].as_str(), images).await {
                    self.observer.failed_submit(&id, &run, &err.to_string());
                    attach::restore_staged(&self.config.home, &id, &staged, &unreadable);
                    return Err(RpcError::internal(err));
                }
                let mut response = json!({ "status": if busy { "queued" } else { "streaming" } });
                if !unreadable.is_empty() {
                    response["dropped_images"] = json!(unreadable);
                }
                if self.replay_of.is_some() {
                    response["run_id"] = json!(run);
                }
                Ok(response)
            }
            "prompt.background" => self.prompt_background(p).await,
            "prompt.btw" => self.prompt_btw(p).await,
            "preview.restart" => self.preview_restart(p).await,
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
                    "learning.enabled" => Ok(json!({ "value": crate::learn::learning_enabled(&self.config.home) })),
                    _ => Ok(json!({ "value": null })),
                }
            }
            "config.set" if p["key"] == "learning.enabled" => {
                let on = crate::learn::set_learning_enabled(&self.config.home, &p["value"]).map_err(RpcError::internal)?;
                Ok(json!({ "value": on }))
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
            // One model path: the engine's own provider and key, traced like any other call.
            "llm.oneshot" => {
                let (system, user) = crate::oneshot::prompts(p).map_err(|(code, message)| RpcError { code, message, data: None })?;
                let complete = self.config.complete.clone().ok_or_else(|| RpcError::internal(anyhow!("no model available")))?;
                let started = crate::observability::now();
                let reply = complete(system, user).await;
                self.observer.record_aux(
                    p["session_id"].as_str().unwrap_or(""), "other", Some("One-shot"), None, None, started,
                    reply.as_ref().ok().and_then(|d| d.usage), reply.as_ref().err().map(|e| e.to_string()).as_deref(),
                );
                let done = reply.map_err(|e| RpcError { code: 5030, message: format!("one-shot generation failed: {e}"), data: None })?;
                Ok(json!({ "text": crate::oneshot::strip_code_fence(&done.text) }))
            }
            // Python would run these outside the approval hook: ask first.
            "shell.exec" | "cli.exec" => {
                if let Some(command) = ungated_exec_command(method, p) {
                    let session = p["session_id"].as_str().unwrap_or("");
                    let choice = self.hub.decide(session, method, &command, "runs a command outside the agent loop").await;
                    if !matches!(choice.as_str(), "once" | "session" | "always") {
                        return Err(RpcError::params("denied: the command was not approved"));
                    }
                }
                self.forward(method, p).await
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
        let result: anyhow::Result<String> = match words {
            ["harness", ..] => {
                let mut text =
                    "No learned instructions yet. Run /refine after a session worth learning from.".to_string();
                if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
                    if let Ok(store) =
                        EntryStore::open_cached(std::path::Path::new(&self.config.home))
                    {
                        let addenda = store.render_prompt(sid).unwrap_or_default();
                        if !addenda.trim().is_empty() {
                            text = format!(
                                "Learned instructions (applied to new sessions):\n\n{}",
                                addenda.trim()
                            );
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
                    let turns: Vec<sovereign_prime::refine::Turn> = history["messages"]
                        .as_array()
                        .map(|list| {
                            list.iter()
                                .map(|m| sovereign_prime::refine::Turn {
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
                    match sovereign_prime::refine::apply(&store, sid, &reply, global, "refine") {
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
        driver::poke(); // /goal, /autonomous and /heartbeat may have started work
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
        if reply.get("error").is_none() && crate::changes_credentials(method) {
            let (config, provider) = (self.config.clone(), params["provider"].as_str().map(str::to_string));
            tokio::spawn(async move { crate::notify_auth_changed(&config, provider.as_deref()).await });
        }
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

/// `/api/agent/run` (`kind` "cron") and the side agents ("background", "preview"): one headless prompt on its own hidden session, run to
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
    let conn = Conn::new(config.clone(), to_ws, hub.clone(), client, observer.clone(), false, "invoke_agent", Some(format!("Replay {run_id}")), Some(run_id.clone()));
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

/// What differs between headless callers of [`agent_run`].
pub(crate) struct RunOpts<'a> {
    /// "cron" | "bot" | "goal": which Hermes unattended-approval setting applies.
    pub surface: &'static str,
    /// Per-turn system context (a bot's platform, user and formatting rules), sent as the turn's
    /// system reminder so it never enters the cached static prefix or the transcript.
    pub instructions: Option<&'a str>,
    /// A cron job's own model / provider, applied when the run creates its session.
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    /// Hermes toolset policy for the run: a per-job allowlist and the always-on denylist.
    pub enabled_toolsets: Option<Vec<String>>,
    pub disabled_toolsets: Vec<String>,
}

impl Default for RunOpts<'_> {
    fn default() -> Self {
        Self { surface: "goal", instructions: None, model: None, provider: None, enabled_toolsets: None, disabled_toolsets: Vec::new() }
    }
}

/// The command text to put in front of the approval gate, or None for the fixed
/// profile edits the desktop's own dialogs issue (`profile delete|describe`,
/// `config unset model`). Anything else Hermes's CLI can do, `chat -q` included,
/// drives an agent or a shell, so it needs a yes.
fn ungated_exec_command(method: &str, p: &Value) -> Option<String> {
    if method == "shell.exec" {
        return Some(p["command"].as_str().unwrap_or("").to_string());
    }
    let argv: Vec<&str> = p["argv"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let rest = match argv.as_slice() {
        ["--profile", _, rest @ ..] => rest,
        rest => rest,
    };
    match rest {
        ["profile", "delete" | "describe", ..] | ["config", "unset", "model"] => None,
        _ => Some(format!("hermes {}", argv.join(" "))),
    }
}

/// Bot chat key -> engine session, persisted in `sovereign.db` (`engine_settings`) so a bot
/// conversation survives an engine restart.
fn bot_session_setting(key: &str) -> String {
    format!("bot_session:{key}")
}

fn load_bot_session(home: &str, key: &str) -> Option<String> {
    entries_or_log(home)?.setting(&bot_session_setting(key))
}

fn save_bot_session(home: &str, key: &str, session_id: &str) {
    if let Some(store) = entries_or_log(home) {
        let _ = store.set_setting(&bot_session_setting(key), session_id);
    }
}

/// Bot chat key -> the connection and engine session of the turn it is running now, so
/// `/stop` from the chat can interrupt it.
static ACTIVE_RUNS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, (Arc<Conn>, String)>>> =
    std::sync::LazyLock::new(Default::default);

/// One turn at a time per bot chat: two messages sent together would otherwise run two turns on
/// one engine session, and each would take the other's `message.complete` for its own reply.
static CHAT_TURNS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    std::sync::LazyLock::new(Default::default);

async fn chat_turn(key: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = CHAT_TURNS.lock().unwrap_or_else(|e| e.into_inner()).entry(key.to_string()).or_default().clone();
    lock.lock_owned().await
}

/// Interrupt the turn a chat is running (Hermes `/stop`); false when it has none.
pub(crate) async fn interrupt_run(session_key: &str) -> bool {
    let entry = ACTIVE_RUNS.lock().unwrap_or_else(|e| e.into_inner()).get(session_key).cloned();
    let Some((conn, session)) = entry else { return false };
    conn.dispatch("session.interrupt", &json!({ "session_id": session })).await.is_ok()
}

/// Hermes `/new`: stop what the chat is running and forget its engine session, so the next
/// message starts a fresh conversation. The old transcript stays in the sidebar.
pub(crate) async fn reset_bot_session(home: &str, key: &str) {
    interrupt_run(key).await;
    if let Ok(store) = sovereign_prime::entries::EntryStore::open_cached(Path::new(home)) {
        let _ = store.delete_setting(&bot_session_setting(key));
    }
}

/// The user approved a command an unattended run was denied: a goal session picks its work back up.
pub(crate) fn resume_after_approval(session_id: &str, command: &str) {
    driver::resume(session_id, command);
}

/// `session.create` params for a headless run, carrying a cron job's model / provider override.
fn run_session_params(cwd: Option<&str>, title: Option<&str>, opts: &RunOpts<'_>) -> Value {
    let mut params = json!({ "cwd": cwd });
    for (field, value) in [("title", title), ("model", opts.model), ("provider", opts.provider)] {
        if let Some(value) = value {
            params[field] = json!(value);
        }
    }
    params
}

pub(crate) async fn agent_run(
    config: Arc<Config>,
    hub: Arc<Hub>,
    observer: Arc<Observer>,
    kind: &'static str,
    prompt: &str,
    cwd: Option<&str>,
    title: Option<&str>,
    session_key: Option<&str>,
    opts: RunOpts<'_>,
    timeout: Duration,
) -> Result<Value> {
    // Queued behind the chat's running turn (its `timeout` starts once this turn does).
    let turn = match session_key {
        Some(key) => Some(chat_turn(key).await),
        None => None,
    };
    let home = config.home.clone();
    let (to_ws, mut ws_out) = mpsc::channel::<Message>(1024);
    let client = Arc::new(Client {
        id: hub.next_client_id(),
        to_ws: to_ws.clone(),
        sessions: Mutex::new(Default::default()),
    });
    let conn = Conn::new(config, to_ws, hub.clone(), client, observer, false, kind, title.map(str::to_string), None);

    // Every exit path, including a failed create or tool policy, ends the run's registration
    // and its link tasks below.
    let result: Result<Value> = async {
        let _ = conn.entry_store().await; // report an unopenable store once, up front
        let control = conn.open_link().await.context("engine unavailable")?;
        *conn.control.lock().await = Some(control);

        // A `session_key` (one bot chat) resumes its engine session; otherwise (or on
        // first use) a fresh one is created and remembered under the key.
        let resumed = session_key.and_then(|key| load_bot_session(&home, key));
        let resumed = match resumed {
            Some(id) => conn
                .dispatch("session.activate", &json!({ "session_id": id, "omit_messages": true }))
                .await
                .ok()
                .map(|_| id),
            None => None,
        };
        let session_id = match resumed {
            Some(id) => id,
            None => {
                let create_params = run_session_params(cwd, title, &opts);
                let created = conn
                    .dispatch("session.create", &create_params)
                    .await
                    .map_err(|e| anyhow!(e.message))?;
                let id = created["session_id"].as_str().unwrap_or_default().to_string();
                if id.is_empty() {
                    bail!("engine did not return a session id");
                }
                if let Some(key) = session_key {
                    save_bot_session(&home, key, &id);
                }
                id
            }
        };

        if let Some(key) = session_key {
            ACTIVE_RUNS.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), (conn.clone(), session_id.clone()));
        }
        // A cron job's toolset policy holds for the whole run (fails closed: no policy, no run).
        if let Some(request) = toolsets::request(&session_id, opts.enabled_toolsets.as_deref(), &opts.disabled_toolsets) {
            conn.call(request).await.context("could not apply the job's tool policy")?;
        }
        // Nobody waits on an approval here: both gates (`Out::Approval`, `decide`) apply
        // the unattended policy (Hermes config, else deny and park for the desktop).
        hub.mark_headless(&session_id, opts.surface).await;

        let outcome = tokio::time::timeout(timeout, async {
            conn.dispatch(
                "prompt.submit",
                &json!({ "session_id": session_id, "text": prompt, "system_reminder": opts.instructions }),
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

        // Stop a timed-out turn first: while it can still ask, it must stay unattended.
        if outcome.is_err() {
            let _ = conn.dispatch("session.interrupt", &json!({ "session_id": session_id })).await;
        }
        hub.unmark_headless(&session_id).await;
        // The turn is over and its transcript is the record: free its buffered live events.
        conn.observer.release_replay(&session_id);
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
        // Tag it so the sidebar lists the transcript, and let old one-shot cron ones expire.
        if matches!(opts.surface, "cron" | "bot") {
            crate::surface_sessions::tag(&home, &session_id, opts.surface, crate::observability::now(), title);
        }
        if opts.surface == "cron" {
            crate::surface_sessions::prune_cron(&conn.config, crate::observability::now()).await;
        }
        Ok(match outcome {
            Ok(Ok(payload)) => turn_reply(&payload, &session_id, opts.surface),
            Ok(Err(err)) => {
                json!({ "ok": false, "text": "", "error": err.to_string(), "session_id": session_id })
            }
            Err(_) => {
                json!({ "ok": false, "text": "", "error": "timed out waiting for the turn to finish", "session_id": session_id })
            }
        })
    }
    .await;
    end_run(&conn, session_key).await;
    drop(turn);
    if let Some(key) = session_key {
        prune_chat_turn(key);
    }
    result
}

/// The `/api/agent/run` reply for a finished turn. A bot chat's `/stop` interrupts the turn: Hermes
/// takes `interrupted: true` (with `ok`) as "stopped" and posts nothing; every other surface
/// keeps it a failed run, since a cron job that was cut short did not do its work.
fn turn_reply(payload: &Value, session_id: &str, surface: &str) -> Value {
    let status = payload["status"].as_str().unwrap_or("unknown");
    let interrupted = status == "interrupted";
    let ok = status == "complete" || (interrupted && surface == "bot");
    let usage = &payload["usage"];
    json!({
        "ok": ok,
        "interrupted": interrupted,
        "text": payload["text"].as_str().unwrap_or_default(),
        "error": if ok { Value::Null } else { json!(format!("the turn did not complete cleanly ({status})")) },
        "session_id": session_id,
        "usage": if usage.is_null() { Value::Null } else { json!({ "input_tokens": usage["input"], "output_tokens": usage["output"], "cached_tokens": usage["cache_read"] }) },
    })
}

/// Forget an idle chat's turn lock (nothing holds or awaits it), so the map does not grow with every chat.
fn prune_chat_turn(key: &str) {
    let mut turns = CHAT_TURNS.lock().unwrap_or_else(|e| e.into_inner());
    if turns.get(key).is_some_and(|lock| Arc::strong_count(lock) == 1) {
        turns.remove(key);
    }
}

/// End a headless run's registration and link tasks, whatever way it ended.
async fn end_run(conn: &Arc<Conn>, session_key: Option<&str>) {
    if let Some(key) = session_key {
        let mut runs = ACTIVE_RUNS.lock().unwrap_or_else(|e| e.into_inner());
        if runs.get(key).is_some_and(|(c, _)| Arc::ptr_eq(c, conn)) {
            runs.remove(key);
        }
    }
    close_links(conn).await;
}

/// Abort a connection's link tasks. A learning pass it started still needs its history link, so
/// while one is running the abort waits (bounded) in the background; the caller is not delayed.
async fn close_links(conn: &Arc<Conn>) {
    let running: Vec<_> = {
        let mut tasks = conn.learning_tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.drain(..).filter(|t| !t.is_finished()).collect()
    };
    if running.is_empty() {
        for task in conn.link_tasks.lock().await.drain(..) {
            task.abort();
        }
        return;
    }
    let conn = conn.clone();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + DISPOSE_LEARNING_WAIT;
        for task in running {
            let _ = tokio::time::timeout_at(deadline, task).await;
        }
        for task in conn.link_tasks.lock().await.drain(..) {
            task.abort();
        }
    });
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
    let conn = Conn::new(config, to_ws, hub.clone(), client.clone(), observer, false, "invoke_agent", None, None);

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
            Ok(Message::Close(_)) => break,
            Err(tokio_tungstenite::tungstenite::Error::Capacity(_)) => {
                // Tell the client why (close 1009) instead of dropping the socket unexplained.
                conn.emit("status.update", None, json!({ "kind": "error", "text": "That message is larger than the gateway accepts and was refused." })).await;
                let _ = conn.to_ws.send(Message::Close(Some(CloseFrame { code: CloseCode::Size, reason: "message too big".into() }))).await;
                tokio::time::sleep(Duration::from_millis(200)).await; // let the writer flush it
                break;
            }
            Err(_) => break,
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
    // The window is gone: review the chats it had open that are due, then let the links go.
    let open: Vec<String> = conn.links.lock().await.keys().cloned().collect();
    for session in open {
        if !conn.sessions.lock().await.get(&session).is_some_and(SessionState::turn_active) {
            conn.schedule_learning(session, Trigger::Dispose);
        }
    }
    close_links(&conn).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn test_conn(name: &str) -> Arc<Conn> {
        test_conn_as(name, false)
    }

    pub(super) fn test_conn_as(name: &str, driver: bool) -> Arc<Conn> {
        let (config, home) = test_config(name);
        let observer = Observer::open(&home, "p", "m", None).unwrap();
        let hub = Arc::new(Hub::default());
        let (to_ws, _rx) = mpsc::channel::<Message>(8);
        let client = Arc::new(Client { id: hub.next_client_id(), to_ws: to_ws.clone(), sessions: Mutex::new(Default::default()) });
        Conn::new(config, to_ws, hub, client, observer, driver, "invoke_agent", None, None)
    }

    fn test_config(name: &str) -> (Arc<Config>, std::path::PathBuf) {
        let home = std::env::temp_dir().join(format!("c{name}{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // A stand-in daemon: accepts the bridge's dials and holds them open.
        let socket = home.join("d.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let config = Arc::new(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: "t".repeat(32),
            version: "test".into(),
            legacy_socket: socket,
            default_cwd: "/".into(),
            allow_non_loopback: false,
            provider: "p".into(),
            model: "m".into(),
            reasoning_efforts: Vec::new(),
            profile_model_applies: true,
            home: home.to_string_lossy().into(),
            complete: None,
            approval_secret: String::new(),
            features: None,
            learning: None,
        });
        (config, home)
    }

    /// A real socket pair: the gateway's `run` on one end, a raw client on the other.
    async fn gateway_socket(name: &str) -> WebSocketStream<TcpStream> {
        let (config, home) = test_config(name);
        let observer = Observer::open(&home, "p", "m", None).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = WebSocketStream::from_raw_socket(stream, tokio_tungstenite::tungstenite::protocol::Role::Server, Some(crate::ws_config())).await;
            let _ = run(ws, config, Arc::new(Hub::default()), observer).await;
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        WebSocketStream::from_raw_socket(stream, tokio_tungstenite::tungstenite::protocol::Role::Client, None).await
    }

    async fn reply_to(ws: &mut WebSocketStream<TcpStream>, id: u64) -> Value {
        loop {
            let Message::Text(text) = tokio::time::timeout(Duration::from_secs(20), ws.next()).await.expect("a reply").expect("socket open").expect("no error") else { continue };
            let frame: Value = serde_json::from_str(&text).unwrap();
            if frame["id"] == id {
                return frame;
            }
        }
    }

    #[tokio::test]
    async fn a_6_5_mb_image_attach_gets_a_reply_and_the_socket_stays_open() {
        let mut ws = gateway_socket("big-attach").await;
        // 6.5 MB of image is ~8.7 MB of base64: over the old 8 MiB message cap.
        let payload = "A".repeat(6_500_000 / 3 * 4);
        let request = json!({ "jsonrpc": "2.0", "id": 1, "method": "image.attach_bytes", "params": { "session_id": "nobody", "content_base64": payload } });
        ws.send(Message::Text(request.to_string())).await.unwrap();
        let reply = reply_to(&mut ws, 1).await;
        assert_eq!(reply["error"]["message"], "unknown session_id", "answered, not disconnected: {reply}");
        ws.send(Message::Text(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }).to_string())).await.unwrap();
        assert!(reply_to(&mut ws, 2).await["result"].is_object(), "the same connection still works");
    }

    #[tokio::test]
    async fn a_message_past_the_cap_ends_the_connection_without_hanging() {
        let mut ws = gateway_socket("too-big").await;
        // The gateway refuses it from the frame header and closes (1009 when the close frame beats
        // the TCP reset); the client's own write may fail once that happens.
        let _ = ws.send(Message::Text("x".repeat(crate::MAX_WS_MESSAGE_BYTES + 1))).await;
        let ended = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Close(frame) = msg {
                    return frame.map(|f| u16::from(f.code));
                }
            }
            None
        })
        .await
        .expect("the gateway ended the connection");
        assert!(matches!(ended, None | Some(1009)), "{ended:?}");
    }

    #[tokio::test]
    async fn a_link_whose_bridge_closed_is_dropped_and_reopened() {
        let conn = test_conn("dead-link");
        conn.ensure_control().await.unwrap();
        let first = conn.control.lock().await.clone().unwrap();
        conn.link_tasks.lock().await[0].abort(); // the bridge dies
        tokio::time::timeout(Duration::from_secs(5), async {
            while conn.control.lock().await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the reader forgets the closed control link");
        assert!(conn.route(&json!({ "req": "list_sessions" })).await.is_err(), "not trusted while closed");
        conn.ensure_control().await.unwrap();
        let second = conn.control.lock().await.clone().unwrap();
        assert!(!second.same_channel(&first) && !second.is_closed());
        conn.route(&json!({ "req": "list_sessions" })).await.unwrap();
        // A session link whose writer is gone is likewise not trusted.
        conn.link_tasks.lock().await[1].abort();
        tokio::time::timeout(Duration::from_secs(5), first.closed()).await.expect("writer gone");
        conn.links.lock().await.insert("s".into(), first);
        assert!(conn.route(&json!({ "session_id": "s" })).await.unwrap().same_channel(&second), "falls back to the control link");
    }

    #[tokio::test]
    async fn a_failed_run_still_ends_its_registration_and_link_tasks() {
        let conn = test_conn("end-run");
        conn.ensure_control().await.unwrap();
        ACTIVE_RUNS.lock().unwrap().insert("telegram:end-run".into(), (conn.clone(), "s".into()));
        end_run(&conn, Some("telegram:end-run")).await;
        assert!(!ACTIVE_RUNS.lock().unwrap().contains_key("telegram:end-run"));
        assert!(conn.link_tasks.lock().await.is_empty());
    }

    #[tokio::test]
    async fn a_headless_run_lets_its_learning_pass_finish_before_its_links_close() {
        let conn = test_conn("end-run-learning");
        conn.ensure_control().await.unwrap();
        let (release, held) = oneshot::channel::<()>();
        conn.learning_tasks.lock().unwrap().push(tokio::spawn(async move {
            let _ = held.await;
        }));
        end_run(&conn, None).await; // returns at once, links stay up for the pass
        assert!(!conn.link_tasks.lock().await.is_empty(), "the pass still needs its history link");
        release.send(()).unwrap();
        for _ in 0..50 {
            if conn.link_tasks.lock().await.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the links were never closed after the pass finished");
    }

    #[tokio::test]
    async fn closing_a_session_with_nothing_due_returns_at_once_and_frees_the_running_mark() {
        let conn = test_conn("dispose-nothing");
        // No learning configured in the test config: nothing to review, nothing left marked.
        conn.learn_before_dispose("s1").await;
        assert!(conn.learning_now.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unopenable_store_is_reported_once() {
        let conn = test_conn("store-warn");
        let (to_ws, mut rx) = mpsc::channel::<Message>(8);
        conn.hub.add(Arc::new(Client { id: 99, to_ws, sessions: Mutex::new(Default::default()) })).await;
        let err = anyhow!("database is locked");
        store_unavailable(&conn.hub, "test store", &err).await;
        store_unavailable(&conn.hub, "test store", &err).await;
        let Message::Text(text) = rx.try_recv().unwrap() else { panic!("text frame") };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!((event["params"]["type"].as_str(), event["params"]["payload"]["kind"].as_str()), (Some("status.update"), Some("error")));
        assert!(rx.try_recv().is_err(), "the second failure is silent");
    }

    #[test]
    fn an_unopenable_store_without_a_window_is_logged_once() {
        let file = std::env::temp_dir().join(format!("not-a-dir-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let home = file.to_string_lossy().to_string();
        assert!(entries_or_log(&home).is_none() && control_or_log(&home).is_none());
        assert!(!log_once("learning store (no window)", &anyhow!("again")), "already logged");
        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn a_failed_backup_is_reported_once() {
        let conn = test_conn("backup-warn");
        let (to_ws, mut rx) = mpsc::channel::<Message>(8);
        conn.hub.add(Arc::new(Client { id: 98, to_ws, sessions: Mutex::new(Default::default()) })).await;
        report_backup_error(&conn.hub, None).await;
        assert!(rx.try_recv().is_err(), "nothing to report");
        report_backup_error(&conn.hub, Some("disk full".into())).await;
        report_backup_error(&conn.hub, Some("disk full".into())).await;
        let Message::Text(text) = rx.try_recv().unwrap() else { panic!("text frame") };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(event["params"]["payload"]["kind"], "error");
        assert!(event["params"]["payload"]["text"].as_str().unwrap().contains("disk full"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_cron_jobs_model_and_provider_reach_the_run_session() {
        let opts = RunOpts { model: Some("llama-3.3-70b"), provider: Some("groq"), ..RunOpts::default() };
        let params = run_session_params(Some("/w"), Some("nightly"), &opts);
        assert_eq!((params["model"].as_str(), params["provider"].as_str(), params["title"].as_str()), (Some("llama-3.3-70b"), Some("groq"), Some("nightly")));
        assert!(run_session_params(None, None, &RunOpts::default()).get("model").is_none());
    }

    #[tokio::test]
    async fn a_chats_turns_run_one_at_a_time_while_other_chats_are_not_held_up() {
        let first = chat_turn("telegram:serial").await;
        let second = tokio::spawn(async { drop(chat_turn("telegram:serial").await) });
        drop(chat_turn("telegram:elsewhere").await); // another chat is free while the first is busy
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!second.is_finished(), "the second message waits for the first turn");
        drop(first);
        tokio::time::timeout(Duration::from_secs(2), second).await.expect("released").unwrap();
    }

    #[tokio::test]
    async fn a_stopped_bot_turn_replies_stopped_not_failed_and_the_queued_message_still_runs() {
        let stopped = json!({ "status": "interrupted", "text": "partial", "usage": null });
        let bot = turn_reply(&stopped, "s", "bot");
        assert_eq!((bot["ok"].clone(), bot["interrupted"].clone(), bot["error"].clone()), (json!(true), json!(true), Value::Null));
        let cron = turn_reply(&stopped, "s", "cron");
        assert_eq!((cron["ok"].clone(), cron["interrupted"].clone()), (json!(false), json!(true)));
        assert!(cron["error"].as_str().unwrap().contains("interrupted"));
        let done = turn_reply(&json!({ "status": "complete", "text": "hi" }), "s", "bot");
        assert_eq!((done["ok"].clone(), done["interrupted"].clone(), done["text"].clone()), (json!(true), json!(false), json!("hi")));
        // The next message for the chat waits for the stopped turn, then gets the lock; idle chats are pruned.
        let key = "telegram:stop-queue";
        let first = chat_turn(key).await;
        let second = tokio::spawn(async move { drop(chat_turn(key).await) });
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(first);
        prune_chat_turn(key);
        tokio::time::timeout(Duration::from_secs(2), second).await.expect("the queued turn runs").unwrap();
        prune_chat_turn(key);
        assert!(!CHAT_TURNS.lock().unwrap().contains_key(key), "idle chat pruned");
        let held = chat_turn(key).await;
        prune_chat_turn(key);
        assert!(CHAT_TURNS.lock().unwrap().contains_key(key), "a held lock is kept");
        drop(held);
        prune_chat_turn(key);
    }

    #[tokio::test]
    async fn new_in_a_bot_chat_starts_a_fresh_engine_session() {
        let home = std::env::temp_dir().join(format!("bot-reset-{}", std::process::id()));
        let home_str = home.to_string_lossy().to_string();
        save_bot_session(&home_str, "telegram:9", "session_old");
        save_bot_session(&home_str, "telegram:8", "session_other");
        assert!(!interrupt_run("telegram:9").await, "nothing running");
        reset_bot_session(&home_str, "telegram:9").await;
        assert_eq!(load_bot_session(&home_str, "telegram:9"), None, "the next message creates a new session");
        assert_eq!(load_bot_session(&home_str, "telegram:8").as_deref(), Some("session_other"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn exec_rpcs_are_gated_unless_they_are_the_desktops_profile_edits() {
        let cli = |argv: Value| ungated_exec_command("cli.exec", &json!({ "argv": argv }));
        assert_eq!(cli(json!(["profile", "delete", "w", "--yes"])), None);
        assert_eq!(cli(json!(["--profile", "w", "config", "unset", "model"])), None);
        assert_eq!(cli(json!(["chat", "-q", "rm -rf /"])).as_deref(), Some("hermes chat -q rm -rf /"));
        assert_eq!(cli(json!([])).as_deref(), Some("hermes "));
        assert_eq!(ungated_exec_command("shell.exec", &json!({ "command": "ls" })).as_deref(), Some("ls"));
    }

    #[test]
    fn a_bot_chat_keeps_its_engine_session_across_an_engine_restart() {
        let home = std::env::temp_dir().join(format!("bot-sessions-{}", std::process::id()));
        let home_str = home.to_string_lossy().to_string();
        assert_eq!(load_bot_session(&home_str, "telegram:1"), None);
        save_bot_session(&home_str, "telegram:1", "session_a");
        save_bot_session(&home_str, "telegram:2", "session_b");
        save_bot_session(&home_str, "telegram:1", "session_c"); // a chat re-mapped
        // "Restart": a fresh store on the same sovereign.db, not the process-cached one.
        let reopened = sovereign_prime::entries::EntryStore::open(&home).unwrap();
        assert_eq!(reopened.setting(&bot_session_setting("telegram:1")).as_deref(), Some("session_c"));
        assert_eq!(reopened.setting(&bot_session_setting("telegram:2")).as_deref(), Some("session_b"));
        assert_eq!(load_bot_session(&home_str, "telegram:3"), None);
        let _ = std::fs::remove_dir_all(home);
    }
}
