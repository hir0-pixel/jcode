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
//!
//! Unattended runs (cron, bot turns, goals with no window) cannot wait for an
//! answer: they follow the Hermes approval config (`approvals.mode: off`,
//! `cron_mode` / `unattended_mode: approve`) and otherwise deny, parking the
//! blocked command as a normal `approval` prompt on the desktop so the user can
//! approve it later (see docs/SAFETY_SYSTEM.md).

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// How long a prompt waits for the user. Shorter than the hook's own limit.
pub const DECISION_TIMEOUT: Duration = Duration::from_secs(240);
/// How long a denied unattended command stays open for a late approval.
const PARK_TTL: Duration = Duration::from_secs(24 * 3600);
const MAX_PARKED: usize = 50;

/// Whether the user's Hermes config (`$HERMES_HOME/config.yaml`) lets an unattended `surface`
/// ("cron", "bot", "goal") run approval-gated commands: `approvals.mode: off`, or `cron_mode`
/// (cron) / `unattended_mode` (everything else) set to `approve`. Default and any read error: no.
fn policy_allows(home: &Path, surface: &str) -> bool {
    let approvals = std::fs::read_to_string(home.join("config.yaml"))
        .ok()
        .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(&raw).ok())
        .map(|cfg| cfg["approvals"].clone())
        .unwrap_or_default();
    let key = if surface == "cron" { "cron_mode" } else { "unattended_mode" };
    approvals["mode"].as_str() == Some("off") || approvals[key].as_str() == Some("approve")
}

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
    /// Sessions running unattended (`/api/agent/run`) -> their surface ("cron" | "bot"). No desktop
    /// prompt ever waits on them; see [`Hub::unattended`].
    headless: Mutex<HashMap<String, &'static str>>,
    /// Exact commands the user approved after the fact: consumed by the next unattended run needing
    /// it (`once`), or good until the engine restarts (`session` / `always`).
    once_grants: Mutex<HashSet<String>>,
    sticky_grants: Mutex<HashSet<String>>,
    observer: std::sync::Mutex<Option<std::sync::Arc<crate::observability::Observer>>>,
}

impl Hub {
    pub fn set_observer(&self, observer: std::sync::Arc<crate::observability::Observer>) {
        *self.observer.lock().unwrap() = Some(observer);
    }

    fn audit(&self, session_id: &str, tool: &str, command: &str, decision: &str, actor: &str) {
        if let Some(observer) = self.observer.lock().unwrap().as_ref() {
            observer.record_approval(session_id, tool, command, decision, actor);
        }
    }
    pub async fn broadcast_text(&self, frame: String) {
        for client in self.clients.lock().await.iter() {
            let _ = client.to_ws.send(Message::Text(frame.clone())).await;
        }
    }

    pub async fn add(&self, client: Arc<Client>) {
        // A desktop that connects later still sees what unattended runs were denied meanwhile.
        for (id, (_, params)) in self.shown.lock().await.iter() {
            if params["unattended"] == true {
                let frame = json!({ "jsonrpc": "2.0", "id": id, "method": "approval", "params": params });
                let _ = client.to_ws.send(Message::Text(frame.to_string())).await;
            }
        }
        self.clients.lock().await.push(client);
    }

    /// Whether a desktop window has `session` open (the driver's engine link
    /// yields to it: the window renders, observes and prompts).
    pub async fn has_window(&self, session_id: &str) -> bool {
        for client in self.clients.lock().await.iter() {
            if client.sessions.lock().await.contains(session_id) {
                return true;
            }
        }
        false
    }

    pub async fn remove(&self, id: u64) {
        self.clients.lock().await.retain(|c| c.id != id);
    }

    pub fn next_client_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn mark_headless(&self, session_id: &str, surface: &'static str) {
        self.headless.lock().await.insert(session_id.to_string(), surface);
    }

    pub async fn unmark_headless(&self, session_id: &str) {
        self.headless.lock().await.remove(session_id);
    }

    pub async fn is_headless(&self, session_id: &str) -> bool {
        self.headless.lock().await.contains_key(session_id)
    }

    /// An approval nobody can answer now (a cron / bot turn, or a goal with no desktop): allowed by
    /// the user's Hermes config or an earlier late approval (`once`), otherwise denied and parked as
    /// a normal desktop `approval` prompt so the user can approve it later.
    pub(crate) async fn unattended(self: &Arc<Self>, session_id: &str, tool: &str, command: &str, reason: &str) -> String {
        let surface = self.headless.lock().await.get(session_id).copied().unwrap_or("goal");
        let granted = self.once_grants.lock().await.remove(command) || self.sticky_grants.lock().await.contains(command);
        let home = std::env::var_os("HERMES_HOME").map(std::path::PathBuf::from);
        if granted || home.is_some_and(|home| policy_allows(&home, surface)) {
            self.audit(session_id, tool, command, "once", if granted { "user-later" } else { "policy" });
            return "once".into();
        }
        self.audit(session_id, tool, command, "deny", "headless-deny");
        self.park(session_id, tool, command, reason, surface).await;
        "deny".into()
    }

    /// Show a denied unattended command on the desktop; an approval records a late grant and, for a
    /// goal session, resumes it.
    async fn park(self: &Arc<Self>, session_id: &str, tool: &str, command: &str, reason: &str, surface: &str) {
        let request_id = format!("approval-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        let params = json!({
            "session_id": session_id,
            "request_id": request_id,
            "command": command.chars().take(4000).collect::<String>(),
            "description": format!("Blocked while unattended ({surface}): {reason}. Approve to let it run next time."),
            "tool_name": tool,
            "choices": ["once", "session", "always", "deny"],
            "allow_permanent": true,
            "allow_session": true,
            "unattended": true,
        });
        {
            let mut shown = self.shown.lock().await;
            let parked = shown.values().filter(|(_, p)| p["unattended"] == true);
            if parked.clone().count() >= MAX_PARKED || parked.into_iter().any(|(sid, p)| sid == session_id && p["command"] == command) {
                return;
            }
            shown.insert(request_id.clone(), (session_id.to_string(), params.clone()));
        }
        self.pending.lock().await.insert(request_id.clone(), (session_id.to_string(), tx));
        let frame = json!({ "jsonrpc": "2.0", "id": request_id, "method": "approval", "params": params }).to_string();
        self.broadcast_text(frame).await;
        let (hub, session, tool, command) = (self.clone(), session_id.to_string(), tool.to_string(), command.to_string());
        tokio::spawn(async move {
            let choice = tokio::time::timeout(PARK_TTL, rx).await.ok().and_then(Result::ok).unwrap_or_default();
            hub.pending.lock().await.remove(&request_id);
            hub.shown.lock().await.remove(&request_id);
            match choice.as_str() {
                "once" => drop(hub.once_grants.lock().await.insert(command.clone())),
                "session" | "always" => drop(hub.sticky_grants.lock().await.insert(command.clone())),
                _ => return,
            }
            hub.audit(&session, &tool, &command, &choice, "user-later");
            crate::rpc::resume_after_approval(&session, &command);
        });
    }

    /// Ask the user whether `command` may run. Returns the Hermes choice
    /// (`once` / `session` / `always` / `deny`).
    pub async fn decide(self: &Arc<Self>, session_id: &str, tool: &str, command: &str, reason: &str) -> String {
        if self.headless.lock().await.contains_key(session_id) {
            return self.unattended(session_id, tool, command, reason).await;
        }
        if self.always.load(Ordering::Relaxed) {
            let choice = "session".to_string();
            self.audit(session_id, tool, command, &choice, "session-grant");
            return choice;
        }
        if self.session_grants.lock().await.contains(session_id) {
            let choice = "session".to_string();
            self.audit(session_id, tool, command, &choice, "session-grant");
            return choice;
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
            return self.unattended(session_id, tool, command, reason).await;
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
        let timed_out = choice == "deny";
        let final_choice = if matches!(choice.as_str(), "once" | "session" | "always") {
            choice
        } else {
            "deny".into()
        };
        let actor = if final_choice == "deny" && timed_out { "user" } else { "user" };
        self.audit(session_id, tool, command, &final_choice, actor);
        final_choice
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
        assert_eq!(Arc::new(Hub::default()).decide("s", "bash", "rm -rf x", "r").await, "deny");
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
    async fn unattended_denial_is_parked_for_the_desktop_and_a_late_approval_lets_the_next_run_through() {
        let hub = Arc::new(Hub::default());
        let (_c, mut rx) = client(&hub, "other").await;
        hub.mark_headless("cron-run", "cron").await;
        // Nobody waits: the run is denied at once, and the desktop gets the prompt anyway.
        assert_eq!(hub.decide("cron-run", "bash", "rm -rf build", "r").await, "deny");
        let Message::Text(frame) = rx.recv().await.unwrap() else { panic!() };
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame["params"]["unattended"], true);
        assert_eq!(frame["params"]["command"], "rm -rf build");
        // The same command is not parked twice; a later desktop connection is shown the open prompt.
        assert_eq!(hub.decide("cron-run", "bash", "rm -rf build", "r").await, "deny");
        assert!(rx.try_recv().is_err());
        assert_eq!(hub.pending_for("cron-run").await.len(), 1);
        let (_late, mut late_rx) = client(&hub, "x").await;
        assert_eq!(request_id(late_rx.recv().await.unwrap()), frame["id"].as_str().unwrap());
        // The user approves it once: the next unattended run passes, exactly once.
        assert!(hub.answer(frame["id"].as_str().unwrap(), "once").await);
        for _ in 0..50 {
            if hub.once_grants.lock().await.contains("rm -rf build") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(hub.decide("cron-run", "bash", "rm -rf build", "r").await, "once");
        assert_eq!(hub.decide("cron-run", "bash", "rm -rf build", "r").await, "deny");
    }

    #[test]
    fn hermes_approval_config_decides_what_unattended_surfaces_may_run() {
        let home = std::env::temp_dir().join(format!("approvals-policy-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let allows = |yaml: &str, surface: &str| {
            std::fs::write(home.join("config.yaml"), yaml).unwrap();
            policy_allows(&home, surface)
        };
        assert!(!policy_allows(&home, "cron"), "no config: deny");
        assert!(!allows("approvals:\n  mode: smart\n  cron_mode: deny\n", "cron"));
        assert!(allows("approvals:\n  cron_mode: approve\n", "cron"));
        assert!(!allows("approvals:\n  cron_mode: approve\n", "bot"), "cron_mode is for cron only");
        assert!(allows("approvals:\n  unattended_mode: approve\n", "bot"));
        assert!(allows("approvals:\n  unattended_mode: approve\n", "goal"));
        assert!(allows("approvals:\n  mode: \"off\"\n", "cron"));
        assert!(!allows(": not yaml [", "cron"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn headless_sessions_never_wait_on_a_prompt_and_ordinary_sessions_still_ask_afterwards() {
        let hub = Arc::new(Hub::default());
        let (_c, mut rx) = client(&hub, "s").await;
        hub.mark_headless("s", "bot").await;
        assert_eq!(hub.decide("s", "bash", "rm -rf x", "r").await, "deny");
        hub.unmark_headless("s").await;
        while rx.try_recv().is_ok() {}
        let h = hub.clone();
        let asked = tokio::spawn(async move { h.decide("s", "bash", "rm -rf x", "r").await });
        let id = request_id(rx.recv().await.unwrap());
        assert!(hub.answer(&id, "once").await);
        assert_eq!(asked.await.unwrap(), "once");
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
