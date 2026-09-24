//! Human approval for risky shell commands (Hermes parity).
//!
//! jcode gates destructive commands deterministically but never asks a
//! person. Its `pre_tool` hook runs `sovereign __pre-tool` (see [`hook`]),
//! which assesses the command with jcode's own risk classifier and, when it is
//! not plainly safe, asks this hub. The hub shows a Hermes `approval` request
//! on the desktop windows that have the session open and waits for the answer.
//!
//! Fail closed everywhere on our side: no desktop client, a timeout, a bad
//! secret, a malformed request or a hook error all mean "deny". The secret
//! only lets a caller *create* a prompt; answers come only from an
//! authenticated desktop socket.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// How long a prompt waits for the user. Shorter than the hook's own limit.
pub const DECISION_TIMEOUT: Duration = Duration::from_secs(240);

/// A desktop connection able to show prompts.
pub struct Client {
    pub id: u64,
    pub to_ws: mpsc::Sender<Message>,
    /// Sessions this client has open (created or attached).
    pub sessions: Mutex<HashSet<String>>,
}

#[derive(Default)]
pub struct Hub {
    clients: Mutex<Vec<Arc<Client>>>,
    pending: Mutex<HashMap<String, (String, oneshot::Sender<String>)>>,
    /// Params of open prompts, for `approval.pending`.
    shown: Mutex<HashMap<String, (String, Value)>>,
    /// Sessions where the user chose "session" (allow for the rest of it).
    session_grants: Mutex<HashSet<String>>,
    /// The user chose "always": allow until the engine restarts.
    always: std::sync::atomic::AtomicBool,
    next: AtomicU64,
}

impl Hub {
    pub async fn add(&self, client: Arc<Client>) {
        self.clients.lock().await.push(client);
    }

    pub async fn remove(&self, id: u64) {
        self.clients.lock().await.retain(|c| c.id != id);
    }

    pub fn next_client_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Ask the user whether `command` may run. Returns the Hermes choice
    /// (`once` / `session` / `always` / `deny`).
    pub async fn decide(&self, session_id: &str, tool: &str, command: &str, reason: &str) -> String {
        if self.always.load(Ordering::Relaxed) || self.session_grants.lock().await.contains(session_id) {
            return "session".into();
        }
        let clients: Vec<Arc<Client>> = {
            let all = self.clients.lock().await.clone();
            let mut showing = Vec::new();
            for client in &all {
                if client.sessions.lock().await.contains(session_id) {
                    showing.push(client.clone());
                }
            }
            if showing.is_empty() { all } else { showing }
        };
        if clients.is_empty() {
            return "deny".into();
        }
        let request_id = format!("approval-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), (session_id.to_string(), tx));
        let params = json!({
            "session_id": session_id,
            "request_id": request_id,
            "command": command.chars().take(4000).collect::<String>(),
            "description": reason,
            "tool_name": tool,
            "choices": ["once", "session", "always", "deny"],
            "allow_permanent": true,
            "allow_session": true,
        });
        self.shown.lock().await.insert(request_id.clone(), (session_id.to_string(), params.clone()));
        let frame = json!({ "jsonrpc": "2.0", "id": request_id, "method": "approval", "params": params }).to_string();
        for client in &clients {
            let _ = client.to_ws.send(Message::Text(frame.clone())).await;
        }
        let choice = match tokio::time::timeout(DECISION_TIMEOUT, rx).await {
            Ok(Ok(choice)) => choice,
            _ => "deny".into(),
        };
        self.pending.lock().await.remove(&request_id);
        self.shown.lock().await.remove(&request_id);
        match choice.as_str() {
            "session" => {
                self.session_grants.lock().await.insert(session_id.to_string());
            }
            "always" => self.always.store(true, Ordering::Relaxed),
            _ => {}
        }
        if matches!(choice.as_str(), "once" | "session" | "always") { choice } else { "deny".into() }
    }

    /// Open prompts for a session (`approval.pending`, e.g. after a reload).
    pub async fn pending_for(&self, session_id: &str) -> Vec<Value> {
        self.shown
            .lock()
            .await
            .iter()
            .filter(|(_, (sid, _))| sid == session_id)
            .map(|(id, (_, params))| {
                // PendingApproval carries no session_id (the caller named it).
                let mut p = params.clone();
                if let Some(map) = p.as_object_mut() {
                    map.remove("session_id");
                }
                p["request_id"] = json!(id);
                p
            })
            .collect()
    }

    /// A desktop answered a prompt by replying to the server request.
    pub async fn answer(&self, request_id: &str, choice: &str) -> bool {
        match self.pending.lock().await.remove(request_id) {
            Some((_, tx)) => tx.send(choice.to_string()).is_ok(),
            None => false,
        }
    }

    /// `approval.respond`: resolve this session's prompts (one, or all).
    pub async fn answer_session(&self, session_id: &str, request_id: Option<&str>, choice: &str) -> usize {
        let mut pending = self.pending.lock().await;
        let keys: Vec<String> = pending
            .iter()
            .filter(|(id, (sid, _))| sid == session_id && request_id.is_none_or(|r| r == id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        let mut resolved = 0;
        for key in keys {
            if let Some((_, tx)) = pending.remove(&key) {
                resolved += usize::from(tx.send(choice.to_string()).is_ok());
            }
        }
        resolved
    }
}

/// The `pre_tool` gate process: `sovereign __pre-tool`. Returns the exit code
/// (0 allow, 2 block). Everything that goes wrong blocks.
pub mod hook {
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    /// Exit code jcode treats as "block the call".
    pub const BLOCK: i32 = 2;
    /// Below jcode's configured hook timeout so we, not jcode, decide.
    const HOOK_TIMEOUT: Duration = Duration::from_secs(270);

    pub fn run(approval_file: &std::path::Path) -> i32 {
        let tool = std::env::var("JCODE_HOOK_TOOL_NAME").unwrap_or_default();
        if tool != "bash" {
            return 0;
        }
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        let Some(command) = serde_json::from_str::<Value>(&input).ok().and_then(|v| v["command"].as_str().map(str::to_owned)) else {
            return block("could not read the command to assess");
        };
        let cwd = std::env::var("JCODE_HOOK_CWD").ok().map(std::path::PathBuf::from);
        let assessment = jcode_command_risk::assess(&command, &jcode_command_risk::RiskContext::from_env(cwd));
        match assessment.level {
            jcode_command_risk::RiskLevel::Safe => return 0,
            jcode_command_risk::RiskLevel::Catastrophic => {
                return block("blocked: this command would destroy the home directory, root, or credentials");
            }
            _ => {}
        }
        let session = std::env::var("JCODE_HOOK_SESSION_ID").unwrap_or_default();
        let reason = assessment
            .findings
            .first()
            .map(|f| format!("{f:?}"))
            .unwrap_or_else(|| "potentially destructive command".into());
        match ask(approval_file, &session, &command, &reason) {
            Ok(choice) if matches!(choice.as_str(), "once" | "session" | "always") => 0,
            Ok(_) => block("The user declined this command. Do not retry it; ask the user how to proceed."),
            Err(err) => block(&format!("approval unavailable ({err}); the command was not run")),
        }
    }

    fn block(message: &str) -> i32 {
        eprintln!("{message}");
        BLOCK
    }

    fn ask(approval_file: &std::path::Path, session: &str, command: &str, reason: &str) -> Result<String, String> {
        let config: Value = std::fs::read_to_string(approval_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .ok_or("no approval endpoint")?;
        let addr = config["addr"].as_str().ok_or("no address")?;
        let secret = config["secret"].as_str().ok_or("no secret")?;
        let body = json!({ "session_id": session, "tool": "bash", "command": command, "reason": reason }).to_string();
        let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(HOOK_TIMEOUT)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
        let request = format!(
            "POST /api/sovereign/approve HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nX-Sovereign-Approval-Secret: {secret}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        let mut response = String::new();
        stream.read_to_string(&mut response).map_err(|e| e.to_string())?;
        let (head, payload) = response.split_once("\r\n\r\n").ok_or("bad response")?;
        if !head.starts_with("HTTP/1.1 200") {
            return Err(head.lines().next().unwrap_or("error").to_string());
        }
        let reply: Value = serde_json::from_str(payload).map_err(|e| e.to_string())?;
        Ok(reply["choice"].as_str().unwrap_or("deny").to_string())
    }
}

/// Validate and parse an approval request body.
pub fn parse_request(body: &[u8]) -> Option<(String, String, String, String)> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let text = |k: &str| v[k].as_str().map(str::to_owned);
    Some((text("session_id")?, text("tool").unwrap_or_else(|| "bash".into()), text("command")?, text("reason").unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn client(hub: &Hub, session: &str) -> (Arc<Client>, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(8);
        let c = Arc::new(Client { id: hub.next_client_id(), to_ws: tx, sessions: Mutex::new(HashSet::from([session.to_string()])) });
        hub.add(c.clone()).await;
        (c, rx)
    }

    fn request_id(msg: Message) -> String {
        let Message::Text(t) = msg else { panic!() };
        serde_json::from_str::<Value>(&t).unwrap()["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn no_desktop_means_deny() {
        assert_eq!(Hub::default().decide("s", "bash", "rm -rf x", "r").await, "deny");
    }

    #[tokio::test]
    async fn the_user_decides_and_session_grants_stick() {
        let hub = Arc::new(Hub::default());
        let (_c, mut rx) = client(&hub, "s").await;
        let h = hub.clone();
        let asked = tokio::spawn(async move { h.decide("s", "bash", "rm -rf build", "r").await });
        let id = request_id(rx.recv().await.unwrap());
        assert!(hub.answer(&id, "session").await);
        assert_eq!(asked.await.unwrap(), "session");
        // Granted for the session: no second prompt.
        assert_eq!(hub.decide("s", "bash", "rm -rf dist", "r").await, "session");
        // Another session still asks, and a denial is final.
        let (_c2, mut rx2) = client(&hub, "t").await;
        let h = hub.clone();
        let asked = tokio::spawn(async move { h.decide("t", "bash", "rm -rf x", "r").await });
        let id = request_id(rx2.recv().await.unwrap());
        assert!(hub.answer(&id, "deny").await);
        assert_eq!(asked.await.unwrap(), "deny");
    }

    #[tokio::test]
    async fn unknown_choices_are_denials_and_respond_resolves_by_session() {
        let hub = Arc::new(Hub::default());
        let (_c, mut rx) = client(&hub, "s").await;
        let h = hub.clone();
        let asked = tokio::spawn(async move { h.decide("s", "bash", "x", "r").await });
        rx.recv().await.unwrap();
        assert_eq!(hub.answer_session("s", None, "yes-please").await, 1);
        assert_eq!(asked.await.unwrap(), "deny");
        assert_eq!(hub.answer_session("s", None, "once").await, 0, "nothing left pending");
    }

    #[test]
    fn request_bodies_are_validated() {
        assert!(parse_request(br#"{"session_id":"s","command":"rm x"}"#).is_some());
        assert!(parse_request(br#"{"command":"rm x"}"#).is_none());
        assert!(parse_request(b"not json").is_none());
    }
}
