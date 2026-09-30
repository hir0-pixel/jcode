//! The engine's side of the Hermes tool bridge: how the Rust agent's `hermes` and `clarify` tools
//! reach the Python feature backend, the approval hub and the person.

use crate::approvals::Hub;
use crate::features::Features;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use jcode_app_core::tool::hermes_bridge::{ClarifyReply, HermesHost, UNAVAILABLE};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Image and video generation run for minutes.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Host {
    /// `None` when the engine runs without the Hermes backend (`feature=0`).
    pub features: Option<Arc<Features>>,
    pub hub: Arc<Hub>,
}

#[async_trait]
impl HermesHost for Host {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let features = self.features.clone().ok_or_else(|| anyhow!(UNAVAILABLE))?;
        // Held so the idle reaper cannot stop the backend under a long-running tool.
        let _lease = features.lease();
        let port = features.port().await.map_err(|e| anyhow!("{UNAVAILABLE}: {e}"))?;
        features.touch(path, "agent-tool");
        let url = format!("http://127.0.0.1:{port}{path}");
        let client = reqwest::Client::builder().timeout(CALL_TIMEOUT).build()?;
        let request = if method == "POST" { client.post(&url) } else { client.get(&url) };
        let request = request.header("X-Hermes-Session-Token", &features.token);
        let response = match body {
            Some(body) => request.json(&body).send().await,
            None => request.send().await,
        }
        .map_err(|e| anyhow!("{UNAVAILABLE}: {e}"))?;
        let status = response.status();
        let reply: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let detail = reply["detail"].as_str().map(str::to_string).unwrap_or_else(|| reply.to_string());
            return Err(anyhow!("Hermes backend answered {status}: {detail}"));
        }
        Ok(reply)
    }

    async fn approve(&self, session_id: &str, tool: &str, summary: &str, reason: &str) -> bool {
        matches!(self.hub.decide(session_id, tool, summary, reason).await.as_str(), "once" | "session" | "always")
    }

    async fn clarify(&self, session_id: &str, question: &str, choices: &[String]) -> ClarifyReply {
        self.hub.clarify(session_id, question, choices).await
    }
}

/// Make the bridge tools usable by this engine's agent turns.
pub fn install(features: Option<Arc<Features>>, hub: Arc<Hub>) {
    jcode_app_core::tool::hermes_bridge::install_host(Arc::new(Host { features, hub }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::Client;
    use std::collections::HashSet;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{Mutex, mpsc};
    use tokio_tungstenite::tungstenite::Message;

    /// A Hermes stand-in: an HTTP server that requires the session token, and a process that announces its port.
    async fn fake_hermes(dir: &std::path::Path) -> (Arc<Features>, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let script = dir.join("fake-hermes");
        std::fs::write(&script, format!("#!/bin/sh\necho 'HERMES_BACKEND_READY port={port}'\nexec sleep 30\n")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let features = Arc::new(Features::new(vec![script.to_string_lossy().into_owned()]));
        let token = features.token.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap();
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let first = text.lines().next().unwrap_or("").to_string();
                log.lock().await.push(first.clone());
                let (status, body) = if !text.contains(&format!("x-hermes-session-token: {token}")) && !text.contains(&format!("X-Hermes-Session-Token: {token}")) {
                    ("401 Unauthorized", r#"{"detail":"bad token"}"#)
                } else if first.starts_with("GET /api/agent-tools/nope") {
                    ("404 Not Found", r#"{"detail":"tool 'nope' is unknown or unavailable"}"#)
                } else if first.starts_with("POST /api/agent-tools/invoke") {
                    ("200 OK", r#"{"result":"{\"success\":true}"}"#)
                } else {
                    ("200 OK", r#"{"tools":[]}"#)
                };
                let reply = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        (features, seen)
    }

    #[tokio::test]
    async fn calls_reach_the_backend_with_its_token_and_errors_carry_its_detail() {
        let dir = std::env::temp_dir().join(format!("hermes-host-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (features, seen) = fake_hermes(&dir).await;
        let host = Host { features: Some(features.clone()), hub: Arc::new(Hub::default()) };
        assert_eq!(host.call("GET", "/api/agent-tools", None).await.unwrap()["tools"], serde_json::json!([]));
        let out = host.call("POST", "/api/agent-tools/invoke", Some(serde_json::json!({"name": "x"}))).await.unwrap();
        assert_eq!(out["result"], "{\"success\":true}");
        let err = host.call("GET", "/api/agent-tools/nope", None).await.unwrap_err().to_string();
        assert!(err.contains("404") && err.contains("unknown or unavailable"), "{err}");
        assert_eq!(seen.lock().await.len(), 3);
        features.stop().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn no_backend_is_the_clean_unavailable_error() {
        let hub = Arc::new(Hub::default());
        let off = Host { features: None, hub: hub.clone() };
        assert_eq!(off.call("GET", "/api/agent-tools", None).await.unwrap_err().to_string(), UNAVAILABLE);
        let broken = Host { features: Some(Arc::new(Features::new(vec!["/nonexistent/hermes".into()]))), hub };
        let err = broken.call("GET", "/api/agent-tools", None).await.unwrap_err().to_string();
        assert!(err.starts_with(UNAVAILABLE), "{err}");
    }

    #[tokio::test]
    async fn clarify_reaches_the_window_and_headless_runs_never_wait() {
        let hub = Arc::new(Hub::default());
        let host = Arc::new(Host { features: None, hub: hub.clone() });
        // Nobody has the session open: answered at once.
        assert!(matches!(host.clarify("s1", "Which one?", &[]).await, ClarifyReply::NoUser));
        let (tx, mut rx) = mpsc::channel(8);
        let window = Arc::new(Client { id: 1, to_ws: tx, sessions: Mutex::new(HashSet::from(["s1".to_string()])) });
        hub.add(window.clone()).await;
        let asker = { let host = host.clone(); tokio::spawn(async move { host.clarify("s1", "Which one?", &["a".into(), "b".into()]).await }) };
        let Message::Text(frame) = rx.recv().await.unwrap() else { panic!() };
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!((frame["method"].as_str(), frame["params"]["question"].as_str()), (Some("clarify"), Some("Which one?")));
        assert_eq!(frame["params"]["choices"], serde_json::json!(["a", "b"]));
        let id = frame["id"].as_str().unwrap();
        let answer = serde_json::json!({"result": {"answer": "b"}});
        let (tx2, _rx2) = mpsc::channel(8);
        let stranger = Client { id: 2, to_ws: tx2, sessions: Mutex::new(HashSet::new()) };
        assert!(!hub.answer_clarify(&stranger, id, &answer).await, "a client not on the session cannot answer");
        assert!(hub.answer_clarify(&window, id, &answer).await);
        assert!(matches!(asker.await.unwrap(), ClarifyReply::Answer(a) if a == "b"));
        // A window that closes cancels its open question.
        let asker = { let host = host.clone(); tokio::spawn(async move { host.clarify("s1", "Gone?", &[]).await }) };
        let _ = rx.recv().await.unwrap();
        hub.remove(1).await;
        assert!(matches!(asker.await.unwrap(), ClarifyReply::TimedOut));
        hub.add(window.clone()).await;
        // A cron or bot run has a window-less session marked headless: no question is sent.
        hub.mark_headless("s1", "cron").await;
        assert!(matches!(host.clarify("s1", "Still there?", &[]).await, ClarifyReply::NoUser));
        assert!(rx.try_recv().is_err(), "a headless run sent nothing to the window");
    }
}
