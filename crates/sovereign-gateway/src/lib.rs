//! Hermes `tui_gateway`-compatible front end for the jcode engine.
//!
//! The Hermes desktop app speaks JSON-RPC 2.0 over `ws://host/api/ws?token=…`
//! plus a few HTTP probes (`/api/health`, `/api/status`). This crate serves that
//! surface and translates it onto the stable jcode harness API, running the
//! harness bridge in-process over an in-memory duplex per WebSocket client.
//! Contract: hermes-agent `apps/shared/src/gateway-contract.openrpc.json`
//! (vendored under `contract/`).

pub mod auth;
pub mod map;
mod rpc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 64;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

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
}

pub struct Gateway {
    listener: TcpListener,
    local: SocketAddr,
    config: Arc<Config>,
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
        Ok(Self { listener, local, config: Arc::new(config) })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub async fn serve(self) -> Result<()> {
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (stream, _) = self.listener.accept().await?;
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                drop(stream); // over the connection cap: refuse without reading
                continue;
            };
            let config = self.config.clone();
            let local = self.local;
            tokio::spawn(async move {
                let _permit = permit;
                let _ = handle(stream, local, config).await;
            });
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
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
    req.parse(&buf)?;
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
    })
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

async fn handle(mut stream: TcpStream, local: SocketAddr, config: Arc<Config>) -> Result<()> {
    let req = tokio::time::timeout(HEADER_TIMEOUT, read_request(&mut stream)).await??;
    let bound_ip = local.ip().to_string();
    let host_reason = auth::host_origin_reason(req.header("host"), req.header("origin"), local.port(), &bound_ip);
    let token_ok = auth::token_matches(
        &config.token,
        auth::presented_token(req.query.as_deref(), req.header("authorization")).as_deref(),
    );
    let wants_ws = req.header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

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
        return rpc::run(ws, config).await;
    }

    if host_reason.is_some() {
        return respond(&mut stream, "403 Forbidden", &json!({"detail": "host not allowed"})).await;
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
        ("GET", "/api/host/identity") => {
            let body = json!({"ok": true, "protocolVersion": 1, "pid": std::process::id(), "role": "serve"});
            respond(&mut stream, "200 OK", &body).await
        }
        _ => {
            let body = json!({"detail": "not supported by engine", "reason": "not_supported_by_engine"});
            respond(&mut stream, "404 Not Found", &body).await
        }
    }
}
