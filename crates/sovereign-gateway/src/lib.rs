//! Hermes `tui_gateway`-compatible front end for the jcode engine.
//!
//! The Hermes desktop app speaks JSON-RPC 2.0 over `ws://host/api/ws?token=…`
//! plus a few HTTP probes (`/api/health`, `/api/status`). This crate serves that
//! surface and translates it onto the stable jcode harness API, running the
//! harness bridge in-process over an in-memory duplex per WebSocket client.
//! Contract: hermes-agent `apps/shared/src/gateway-contract.openrpc.json`
//! (vendored under `contract/`).

pub mod approvals;
pub mod auth;
pub mod cron_wake;
pub mod features;
pub mod map;
pub mod observability;
mod rpc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 64;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Method names in the vendored Hermes gateway contract (parsed once).
pub(crate) fn contract_methods() -> &'static std::collections::HashSet<String> {
    static METHODS: std::sync::OnceLock<std::collections::HashSet<String>> = std::sync::OnceLock::new();
    METHODS.get_or_init(|| {
        let contract: Value = serde_json::from_str(include_str!("../contract/gateway-contract.openrpc.json")).unwrap_or_default();
        contract["methods"]
            .as_array()
            .map(|list| list.iter().filter_map(|m| m["name"].as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    })
}

/// Log each unsupported method/route once per process (stderr), so parity
/// gaps show up in the engine log without flooding it.
pub(crate) fn note_unsupported(kind: &str, what: &str) {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
    let key = format!("{kind} {what}");
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.get_or_insert_with(Default::default).insert(key.clone()) {
        eprintln!("sovereign-gateway: unsupported {}", key.chars().take(200).collect::<String>());
    }
}

pub struct Config {
    pub bind: SocketAddr,
    pub token: String,
    pub version: String,
    /// jcode daemon socket the in-process bridge dials.
    pub legacy_socket: PathBuf,
    /// Working directory for new sessions when the client names none.
    pub default_cwd: String,
    /// Binding a non-loopback address must be asked for explicitly.
    pub allow_non_loopback: bool,
    /// Provider and model the engine started with (reported to setup screens).
    pub provider: String,
    pub model: String,
    /// Engine state directory, reported as the single profile's path.
    pub home: String,
    /// One-shot model call `(system, user) -> text`, used by `/refine`.
    pub complete: Option<Complete>,
    /// Lets the `pre_tool` hook create approval prompts (never answer them).
    pub approval_secret: String,
    /// Hermes's Python backend for everything the Rust harness does not own.
    pub features: Option<Arc<features::Features>>,
}

pub type Complete = std::sync::Arc<
    dyn Fn(String, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>> + Send + Sync,
>;

pub struct Gateway {
    listener: TcpListener,
    local: SocketAddr,
    config: Arc<Config>,
    hub: Arc<approvals::Hub>,
    observer: Arc<observability::Observer>,
}

impl Gateway {
    pub async fn bind(config: Config) -> Result<Self> {
        if !config.bind.ip().is_loopback() && !config.allow_non_loopback {
            bail!("refusing to bind {} without --allow-remote", config.bind);
        }
        if config.token.len() < 32 {
            bail!("gateway token must be at least 32 characters");
        }
        let listener = TcpListener::bind(config.bind).await.context("binding gateway")?;
        let local = listener.local_addr()?;
        let observer = observability::Observer::open(std::path::Path::new(&config.home), &config.provider, &config.model)?;
        Ok(Self { listener, local, config: Arc::new(config), hub: Arc::default(), observer })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub async fn serve(self) -> Result<()> {
        if let Some(features) = self.config.features.clone() {
            let idle = features.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(features::idle_stop_after().clamp(Duration::from_secs(1), Duration::from_secs(60)));
                loop {
                    tick.tick().await;
                    idle.stop_if_idle().await;
                }
            });
            tokio::spawn(cron_wake::run(features));
        }
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (stream, _) = self.listener.accept().await?;
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                drop(stream); // over the connection cap: refuse without reading
                continue;
            };
            let config = self.config.clone();
            let hub = self.hub.clone();
            let observer = self.observer.clone();
            let local = self.local;
            tokio::spawn(async move {
                let _permit = permit;
                let _ = handle(stream, local, config, hub, observer).await;
            });
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    /// Body bytes read along with the headers.
    body_prefix: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed before headers");
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEADER_BYTES {
            bail!("headers too large");
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let header_len = match req.parse(&buf)? {
        httparse::Status::Complete(n) => n,
        httparse::Status::Partial => bail!("incomplete request"),
    };
    let target = req.path.unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (target.to_string(), None),
    };
    Ok(Request {
        method: req.method.unwrap_or("GET").to_string(),
        path,
        query,
        headers: req
            .headers
            .iter()
            .map(|h| (h.name.to_string(), String::from_utf8_lossy(h.value).into_owned()))
            .collect(),
        body_prefix: buf[header_len..].to_vec(),
    })
}

const MAX_BODY_BYTES: usize = 64 * 1024;

async fn read_body(stream: &mut TcpStream, req: &Request) -> Result<Vec<u8>> {
    let len: usize = req.header("content-length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    if len > MAX_BODY_BYTES {
        bail!("body too large");
    }
    let mut body = req.body_prefix.clone();
    body.truncate(len);
    while body.len() < len {
        let mut chunk = vec![0u8; len - body.len()];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("connection closed mid-body");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(body)
}

async fn respond(stream: &mut TcpStream, status: &str, body: &Value) -> Result<()> {
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Reverse-proxy one request (or WebSocket upgrade) to the Hermes feature
/// backend. The client's credential is replaced by the backend's private
/// token; bytes then flow both ways untouched.
async fn proxy_http(mut client: TcpStream, req: &Request, features: &features::Features, upgrade: bool) -> Result<()> {
    if std::env::var_os("SOVEREIGN_TRACE_FORWARD").is_some() {
        eprintln!("sovereign: forward HTTP {} {}", req.method, req.path);
    }
    let port = features.port().await?;
    let mut upstream = TcpStream::connect(("127.0.0.1", port)).await?;
    let query: Vec<&str> = req
        .query
        .as_deref()
        .unwrap_or_default()
        .split('&')
        .filter(|kv| !kv.is_empty() && !kv.starts_with("token=") && !kv.starts_with("ticket="))
        .collect();
    let target = if query.is_empty() { req.path.clone() } else { format!("{}?{}", req.path, query.join("&")) };
    let mut head = format!("{} {} HTTP/1.1\r\n", req.method, target);
    for (name, value) in &req.headers {
        let lower = name.to_ascii_lowercase();
        let dropped = matches!(lower.as_str(), "host" | "x-hermes-session-token" | "authorization" | "origin")
            || (!upgrade && lower == "connection");
        if !dropped {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    head.push_str(&format!("Host: 127.0.0.1:{port}\r\nX-Hermes-Session-Token: {}\r\n", features.token));
    if upgrade {
        // WebSocket routes authenticate by query token on the backend.
        let sep = if target.contains('?') { '&' } else { '?' };
        head = head.replacen(&target, &format!("{target}{sep}token={}", features.token), 1);
    } else {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    upstream.write_all(head.as_bytes()).await?;
    upstream.write_all(&req.body_prefix).await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    features.touch();
    Ok(())
}

fn query_u64(req: &Request, key: &str) -> Option<u64> {
    auth::query_param(req.query.as_deref()?, key)?.parse().ok()
}

fn bundled_startup_request(req: &Request) -> bool {
    std::env::var_os("SOVEREIGN_HERMES_PYTHON").is_some()
        && auth::query_param(req.query.as_deref().unwrap_or_default(), "feature").as_deref() != Some("1")
}

fn bundled_defaults() -> Value {
    std::env::var("SOVEREIGN_HERMES_DEFAULTS")
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

/// One request/reply against the engine over a short-lived in-process bridge,
/// for HTTP routes that have no WebSocket connection to ride on.
async fn harness_request(legacy_socket: &std::path::Path, request: Value) -> Result<Value> {
    let (ours, theirs) = tokio::io::duplex(MAX_FRAME_BYTES);
    let (their_read, their_write) = tokio::io::split(theirs);
    let bridge = tokio::spawn(jcode_harness_api_server::run_bridge_stream(
        their_read,
        their_write,
        legacy_socket.to_path_buf(),
    ));
    let (our_read, mut our_write) = tokio::io::split(ours);
    let mut lines = BufReader::new(our_read).lines();
    let result = async {
        let hello = json!({"v": 1, "id": 0, "req": "hello", "min_version": 1, "max_version": 1, "client": "sovereign-gateway-http"});
        our_write.write_all(format!("{hello}\n").as_bytes()).await?;
        lines.next_line().await?.context("engine closed")?;
        let mut frame = request;
        frame["v"] = json!(1);
        frame["id"] = json!(1);
        our_write.write_all(format!("{frame}\n").as_bytes()).await?;
        loop {
            let line = lines.next_line().await?.context("engine closed")?;
            let reply: Value = serde_json::from_str(&line)?;
            if reply["reply_to"] == 1 {
                if reply["ev"] == "error" {
                    bail!("{}", reply["message"].as_str().unwrap_or("engine error"));
                }
                return Ok(reply);
            }
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(20), result).await.context("engine timed out")?;
    bridge.abort();
    result
}

async fn session_infos(config: &Config, limit: u64, include_archived: bool) -> Result<Vec<Value>> {
    let reply = harness_request(
        &config.legacy_socket,
        json!({"req": "list_sessions", "limit": limit, "include_archived": include_archived}),
    )
    .await?;
    Ok(reply["sessions"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|s| s["parent_session_id"].is_null())
                .filter(|s| include_archived || s["archived"] != true)
                .map(map::session_info)
                .collect()
        })
        .unwrap_or_default())
}

async fn handle(mut stream: TcpStream, local: SocketAddr, config: Arc<Config>, hub: Arc<approvals::Hub>, observer: Arc<observability::Observer>) -> Result<()> {
    let req = tokio::time::timeout(HEADER_TIMEOUT, read_request(&mut stream)).await??;
    let bound_ip = local.ip().to_string();
    let host_reason = auth::host_origin_reason(req.header("host"), req.header("origin"), local.port(), &bound_ip);
    let token_ok = auth::token_matches(
        &config.token,
        auth::presented_token(req.query.as_deref(), req.header("authorization"), req.header("x-hermes-session-token"))
            .as_deref(),
    );
    let wants_ws = req.header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    if wants_ws && req.path != "/api/ws" {
        if host_reason.is_some() || !token_ok {
            return respond(&mut stream, "401 Unauthorized", &json!({"detail": "unauthorized"})).await;
        }
        return match config.features.clone() {
            Some(features) => proxy_http(stream, &req, &features, true).await,
            None => respond(&mut stream, "404 Not Found", &json!({"detail": "not supported by engine"})).await,
        };
    }
    if wants_ws && req.path == "/api/ws" {
        let Some(key) = req.header("sec-websocket-key") else {
            return respond(&mut stream, "400 Bad Request", &json!({"detail": "missing websocket key"})).await;
        };
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let head = format!(
            "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await?;
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(MAX_FRAME_BYTES);
        ws_config.max_frame_size = Some(MAX_FRAME_BYTES);
        let ws = WebSocketStream::from_raw_socket(stream, Role::Server, Some(ws_config)).await;
        // Close after accepting, with Hermes's codes, so the desktop reports
        // an auth failure rather than a network error.
        if let Some(reason) = host_reason {
            return rpc::close(ws, 4403, &reason).await;
        }
        if !token_ok {
            return rpc::close(ws, 4401, "unauthorized").await;
        }
        return rpc::run(ws, config, hub, observer).await;
    }

    if host_reason.is_some() {
        return respond(&mut stream, "403 Forbidden", &json!({"detail": "host not allowed"})).await;
    }
    if req.method == "POST" && req.path == "/api/sovereign/approve" {
        let secret_ok = auth::token_matches(&config.approval_secret, req.header("x-sovereign-approval-secret"));
        if !secret_ok {
            return respond(&mut stream, "401 Unauthorized", &json!({"detail": "unauthorized"})).await;
        }
        let body = tokio::time::timeout(HEADER_TIMEOUT, read_body(&mut stream, &req)).await??;
        let Some((session, tool, command, reason)) = approvals::parse_request(&body) else {
            return respond(&mut stream, "400 Bad Request", &json!({"detail": "bad approval request"})).await;
        };
        let choice = hub.decide(&session, &tool, &command, &reason).await;
        return respond(&mut stream, "200 OK", &json!({"choice": choice})).await;
    }
    let public = json!({"ok": true, "version": config.version, "auth_required": false});
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/health") => respond(&mut stream, "200 OK", &public).await,
        ("GET", "/api/status") => {
            let body = json!({
                "version": config.version,
                "engine": "sovereign",
                "gateway_running": true,
                "gateway_state": "running",
                "auth_required": false,
            });
            respond(&mut stream, "200 OK", &body).await
        }
        _ if !token_ok => respond(&mut stream, "401 Unauthorized", &json!({"detail": "unauthorized"})).await,
        ("GET", "/api/model/info") => {
            let body = json!({"model": config.model, "provider": config.provider, "capabilities": {}});
            respond(&mut stream, "200 OK", &body).await
        }
        ("GET", "/api/config" | "/api/config/defaults") if bundled_startup_request(&req) => {
            let body = if auth::query_param(req.query.as_deref().unwrap_or_default(), "include_defaults").as_deref() == Some("false") {
                json!({})
            } else {
                bundled_defaults()
            };
            respond(&mut stream, "200 OK", &body).await
        }
        ("GET", "/api/local-models/status") if bundled_startup_request(&req) => {
            respond(&mut stream, "200 OK", &json!({
                "enabled": false, "runtime_installed": false, "server_running": false,
                "update_available": false, "loaded_models": {}, "loading": {}, "models": []
            })).await
        }
        ("GET", "/api/local-models/jobs") if bundled_startup_request(&req) => {
            respond(&mut stream, "200 OK", &json!({"jobs": []})).await
        }
        ("GET", "/api/tools/terminal/backends") if bundled_startup_request(&req) => {
            respond(&mut stream, "200 OK", &json!({"active": "local", "backends": [{
                "name": "local", "label": "Local", "description": "Run on this computer",
                "active": true, "status": "ready", "detail": null
            }]})).await
        }
        ("POST", "/api/sessions/owner-backfill") if bundled_startup_request(&req) => {
            respond(&mut stream, "200 OK", &json!({"ok": true, "stamped": 0, "profile": "default"})).await
        }
        ("GET", "/api/profiles") => {
            let body = json!({"profiles": [{
                "name": "default", "display_name": "Default", "path": config.home, "is_default": true,
                "has_env": false, "model": config.model, "provider": config.provider, "skill_count": 0,
            }]});
            respond(&mut stream, "200 OK", &body).await
        }
        ("GET", "/api/profiles/active") => {
            respond(&mut stream, "200 OK", &json!({"active": "default", "default": "default"})).await
        }
        ("GET", "/api/profiles/sessions") => {
            let limit = query_u64(&req, "limit").unwrap_or(50).clamp(1, 1000);
            let archived = auth::query_param(req.query.as_deref().unwrap_or_default(), "archived").as_deref() == Some("true");
            match session_infos(&config, limit, archived).await {
                Ok(sessions) => {
                    let total = sessions.len();
                    let body = json!({"sessions": sessions, "total": total, "limit": limit, "offset": 0});
                    respond(&mut stream, "200 OK", &body).await
                }
                Err(err) => respond(&mut stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
            }
        }
        ("GET", "/api/profiles/sessions/sidebar") => {
            let limit = query_u64(&req, "recents_limit").unwrap_or(50).clamp(1, 1000);
            match session_infos(&config, limit, false).await {
                Ok(sessions) => {
                    let empty = json!({"sessions": []});
                    let body = json!({"recents": {"sessions": sessions}, "cron": empty, "messaging": empty});
                    respond(&mut stream, "200 OK", &body).await
                }
                Err(err) => respond(&mut stream, "503 Service Unavailable", &json!({"detail": err.to_string()})).await,
            }
        }
        ("GET", "/api/hermes/update/check") => {
            let body = json!({
                "install_method": "sovereign-engine", "current_version": config.version, "behind": 0,
                "update_available": false, "can_apply": false, "update_command": null,
                "message": "The engine is updated with its own release, not the Hermes updater.",
            });
            respond(&mut stream, "200 OK", &body).await
        }
        ("GET", "/api/fs/default-cwd") => {
            respond(&mut stream, "200 OK", &json!({"cwd": config.default_cwd, "branch": null})).await
        }
        ("GET", "/api/cron/jobs") if bundled_startup_request(&req) => respond(&mut stream, "200 OK", &json!([])).await,
        ("GET", "/api/host/identity") => {
            let body = json!({"ok": true, "protocolVersion": 1, "pid": std::process::id(), "role": "serve"});
            respond(&mut stream, "200 OK", &body).await
        }
        ("GET", "/api/sovereign/observability/runs") => {
            let limit = query_u64(&req, "limit").unwrap_or(50).clamp(1, 200);
            let result = tokio::task::spawn_blocking(move || observer.list(limit)).await?;
            match result {
                Ok(body) => respond(&mut stream, "200 OK", &body).await,
                Err(err) => respond(&mut stream, "503 Service Unavailable", &json!({"detail":err.to_string()})).await,
            }
        }
        ("GET", "/api/sovereign/observability/run") => {
            let Some(id) = auth::query_param(req.query.as_deref().unwrap_or_default(), "id") else {
                return respond(&mut stream, "400 Bad Request", &json!({"detail":"id is required"})).await;
            };
            let result = tokio::task::spawn_blocking(move || observer.detail(&id)).await?;
            match result {
                Ok(body) => respond(&mut stream, "200 OK", &body).await,
                Err(rusqlite::Error::QueryReturnedNoRows) => respond(&mut stream, "404 Not Found", &json!({"detail":"run not found"})).await,
                Err(err) => respond(&mut stream, "503 Service Unavailable", &json!({"detail":err.to_string()})).await,
            }
        }
        (_, path) if path.starts_with("/api/") && config.features.is_some() => {
            let features = config.features.clone().expect("checked");
            proxy_http(stream, &req, &features, false).await
        }
        (method, path) => {
            note_unsupported("http", &format!("{method} {path}"));
            let body = json!({"detail": "not supported by engine", "reason": "not_supported_by_engine"});
            respond(&mut stream, "404 Not Found", &body).await
        }
    }
}

#[cfg(test)]
mod contract_gate_tests {
    #[test]
    fn only_contract_methods_are_forwardable() {
        let methods = super::contract_methods();
        assert_eq!(methods.len(), 235);
        assert!(methods.contains("cron.manage") && methods.contains("config.show"));
        assert!(!methods.contains("definitely.not.a.method"));
    }
}
