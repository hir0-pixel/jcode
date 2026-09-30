//! The Hermes tools only Hermes's Python backend implements, reached from the Rust agent.
//!
//! One tool, `hermes`, lists, describes and calls them. Schemas come from the running backend's
//! tool registry when the model asks (`describe`), so nothing is copied here and the system prompt
//! carries one small definition however many tools Hermes has. Tools the engine already has natively
//! (files, shell, web, memory, todo, delegation, skills, browser) are never offered. The engine that
//! embeds this crate installs a [`HermesHost`] (the feature backend connection and the approval hub);
//! without one, every call answers "Hermes feature backend not available".
//!
//! `clarify` is native, not bridged: it must pause this turn and ask the person, which the
//! Python process cannot do for a Rust turn.

use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};

pub const UNAVAILABLE: &str = "Hermes feature backend not available";

/// Hermes toolsets the engine already implements natively.
const NATIVE_TOOLSETS: &[&str] = &[
    "file", "terminal", "web", "memory", "todo", "session_search", "code_execution", "delegation", "skills", "browser",
    "clarify",
    // Renderer-driven or Hermes-session-bound tools that mean nothing in a Rust turn.
    "desktop_ui", "project", "setup", "connections",
];

/// The answer to a `clarify` question.
pub enum ClarifyReply {
    Answer(String),
    /// Unattended run, or no window shows this session: nobody can answer.
    NoUser,
    TimedOut,
}

/// What the embedding engine provides: the feature backend, and the person.
#[async_trait]
pub trait HermesHost: Send + Sync {
    /// A JSON request to the feature backend's `/api/agent-tools` routes. An error is shown to the model as is.
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value>;
    /// Ask the person to allow a side-effecting call (the engine's approval flow); false denies.
    async fn approve(&self, session_id: &str, tool: &str, summary: &str, reason: &str) -> bool;
    /// Ask the person a question and wait for the answer.
    async fn clarify(&self, session_id: &str, question: &str, choices: &[String]) -> ClarifyReply;
}

static HOST: RwLock<Option<Arc<dyn HermesHost>>> = RwLock::new(None);

/// Called once by the embedding engine (the gateway) at startup.
pub fn install_host(host: Arc<dyn HermesHost>) {
    *HOST.write().unwrap_or_else(|e| e.into_inner()) = Some(host);
}

fn installed() -> Option<Arc<dyn HermesHost>> {
    HOST.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Tools that act on the outside world (messages, devices, the desktop) ask first, like a risky shell command.
fn needs_approval(name: &str) -> bool {
    matches!(name, "send_message" | "computer_use" | "ha_call_service")
        || name.starts_with("discord")
        || name.starts_with("yb_send_")
        || matches!(name, "feishu_drive_reply_comment" | "feishu_drive_add_comment")
}

pub struct HermesTool {
    host: Option<Arc<dyn HermesHost>>,
}

impl HermesTool {
    pub fn new() -> Self {
        Self { host: None }
    }

    #[cfg(test)]
    fn with_host(host: Arc<dyn HermesHost>) -> Self {
        Self { host: Some(host) }
    }

    fn host(&self) -> Result<Arc<dyn HermesHost>> {
        self.host.clone().or_else(installed).ok_or_else(|| anyhow::anyhow!(UNAVAILABLE))
    }

    async fn list(&self, host: &dyn HermesHost, session_id: &str) -> Result<String> {
        let reply = host.call("GET", "/api/agent-tools", None).await?;
        let mut lines = Vec::new();
        for tool in reply["tools"].as_array().into_iter().flatten() {
            let (name, toolset) = (tool["name"].as_str().unwrap_or(""), tool["toolset"].as_str().unwrap_or(""));
            if name.is_empty() || NATIVE_TOOLSETS.contains(&toolset) || !super::session_tool_allows(session_id, name, toolset) {
                continue;
            }
            lines.push(format!("{name}: {}", tool["description"].as_str().unwrap_or("")));
        }
        Ok(if lines.is_empty() {
            "No Hermes tools are available (each needs its own credentials or setup in Hermes).".to_string()
        } else {
            format!("{}\n\nUse action=describe for a tool's parameters, then action=call.", lines.join("\n"))
        })
    }

    /// The tool's toolset, from the backend, after the run's policy and the native-tool rule allowed it.
    async fn checked(host: &dyn HermesHost, name: &str, session_id: &str) -> Result<Value> {
        anyhow::ensure!(!name.is_empty(), "`tool` is required");
        let reply = host.call("GET", &format!("/api/agent-tools/{}", urlencoding::encode(name)), None).await?;
        let toolset = reply["toolset"].as_str().unwrap_or("");
        anyhow::ensure!(!NATIVE_TOOLSETS.contains(&toolset), "'{name}' is a native tool; call it directly");
        anyhow::ensure!(super::session_tool_allows(session_id, name, toolset), "Tool '{name}' is disabled in this run");
        Ok(reply)
    }
}

#[async_trait]
impl Tool for HermesTool {
    fn name(&self) -> &str {
        "hermes"
    }

    fn description(&self) -> &str {
        "Hermes-only tools (cron jobs, image/video generation, vision, text to speech, Home Assistant, kanban, x_search, computer use, CDP browser). action=list shows what is available, describe gives a tool's parameters, call runs it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "action": { "type": "string", "enum": ["list", "describe", "call"] },
                "tool": { "type": "string", "description": "Tool name, for describe and call" },
                "args": { "type": "object", "description": "Arguments for call, as described by describe" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let host = self.host()?;
        let tool = input["tool"].as_str().unwrap_or("").trim();
        match input["action"].as_str().unwrap_or("list") {
            "list" => Ok(ToolOutput::new(self.list(host.as_ref(), &ctx.session_id).await?)),
            "describe" => {
                let reply = Self::checked(host.as_ref(), tool, &ctx.session_id).await?;
                Ok(ToolOutput::new(serde_json::to_string_pretty(&reply["schema"])?))
            }
            "call" => {
                Self::checked(host.as_ref(), tool, &ctx.session_id).await?;
                let args = if input["args"].is_object() { input["args"].clone() } else { json!({}) };
                if needs_approval(tool) {
                    let summary: String = format!("{tool} {args}").chars().take(600).collect();
                    let reason = "a Hermes tool that acts outside this session";
                    if !host.approve(&ctx.session_id, "hermes", &summary, reason).await {
                        anyhow::bail!("denied: '{tool}' was not approved");
                    }
                }
                let body = json!({ "name": tool, "args": args, "session_id": ctx.session_id });
                let reply = host.call("POST", "/api/agent-tools/invoke", Some(body)).await?;
                Ok(ToolOutput::new(reply["result"].as_str().map(str::to_string).unwrap_or_else(|| reply.to_string())))
            }
            other => anyhow::bail!("unknown action '{other}'; use list, describe or call"),
        }
    }
}

/// Ask the person a question mid-run.
pub struct ClarifyTool {
    host: Option<Arc<dyn HermesHost>>,
}

impl ClarifyTool {
    pub fn new() -> Self {
        Self { host: None }
    }

    #[cfg(test)]
    fn with_host(host: Arc<dyn HermesHost>) -> Self {
        Self { host: Some(host) }
    }
}

#[async_trait]
impl Tool for ClarifyTool {
    fn name(&self) -> &str {
        "clarify"
    }

    fn description(&self) -> &str {
        "Ask the user one question when you cannot proceed without their answer. Not available in unattended runs."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "question": { "type": "string" },
                "choices": { "type": "array", "items": { "type": "string" }, "maxItems": 4, "description": "Optional short answers to offer" }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let question = input["question"].as_str().unwrap_or("").trim();
        anyhow::ensure!(!question.is_empty(), "`question` is required");
        let choices: Vec<String> = input["choices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_string))
            .take(4)
            .collect();
        let reply = match self.host.clone().or_else(installed) {
            Some(host) => host.clarify(&ctx.session_id, question, &choices).await,
            None => ClarifyReply::NoUser,
        };
        Ok(ToolOutput::new(match reply {
            ClarifyReply::Answer(a) if a.trim().is_empty() => "The user skipped the question. Continue with your best judgment.".to_string(),
            ClarifyReply::Answer(a) => format!("The user answered: {a}"),
            ClarifyReply::NoUser => "No user is available to answer (unattended run). Do not wait: proceed with your best judgment and state the assumption you made.".to_string(),
            ClarifyReply::TimedOut => "The user did not answer in time. Proceed with your best judgment and state the assumption you made.".to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_tool_core::ToolExecutionMode;
    use std::sync::Mutex;

    struct Fake {
        up: bool,
        approve: bool,
        clarify: Mutex<Option<ClarifyReply>>,
        calls: Mutex<Vec<(String, String, Option<Value>)>>,
        approvals: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(up: bool, approve: bool) -> Arc<Self> {
            Arc::new(Self { up, approve, clarify: Mutex::new(None), calls: Mutex::default(), approvals: Mutex::default() })
        }
    }

    #[async_trait]
    impl HermesHost for Fake {
        async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
            anyhow::ensure!(self.up, UNAVAILABLE);
            self.calls.lock().unwrap().push((method.into(), path.into(), body));
            Ok(match path {
                "/api/agent-tools" => json!({ "tools": [
                    { "name": "image_generate", "toolset": "image_gen", "description": "Make an image" },
                    { "name": "cronjob_manage", "toolset": "cronjob", "description": "Manage cron jobs" },
                    { "name": "send_message", "toolset": "messaging", "description": "Send a message" },
                    { "name": "read_file", "toolset": "file", "description": "Native duplicate" },
                ]}),
                "/api/agent-tools/invoke" => json!({ "result": "{\"ok\":true}" }),
                other => {
                    let name = other.rsplit('/').next().unwrap_or("");
                    let toolset = match name { "cronjob_manage" => "cronjob", "read_file" => "file", "ha_call_service" => "homeassistant", _ => "image_gen" };
                    json!({ "name": name, "toolset": toolset, "schema": { "name": name, "parameters": { "type": "object" } } })
                }
            })
        }
        async fn approve(&self, _: &str, _: &str, summary: &str, _: &str) -> bool {
            self.approvals.lock().unwrap().push(summary.to_string());
            self.approve
        }
        async fn clarify(&self, _: &str, _: &str, _: &[String]) -> ClarifyReply {
            self.clarify.lock().unwrap().take().unwrap_or(ClarifyReply::NoUser)
        }
    }

    fn ctx(session: &str) -> ToolContext {
        ToolContext {
            session_id: session.into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::AgentTurn,
        }
    }

    async fn run(tool: &HermesTool, session: &str, input: Value) -> Result<String> {
        tool.execute(input, ctx(session)).await.map(|o| o.output)
    }

    #[tokio::test]
    async fn list_fetches_from_the_backend_and_hides_native_tools() {
        let fake = Fake::new(true, true);
        let out = run(&HermesTool::with_host(fake.clone()), "hb-list", json!({"action": "list"})).await.unwrap();
        assert!(out.contains("image_generate: Make an image") && out.contains("cronjob_manage"));
        assert!(!out.contains("read_file"), "native tools are not offered twice");
        assert_eq!(fake.calls.lock().unwrap()[0].1, "/api/agent-tools");
    }

    #[tokio::test]
    async fn describe_returns_the_backends_schema_and_call_runs_the_tool() {
        let fake = Fake::new(true, true);
        let tool = HermesTool::with_host(fake.clone());
        let schema = run(&tool, "hb-call", json!({"action": "describe", "tool": "image_generate"})).await.unwrap();
        assert!(schema.contains("\"name\": \"image_generate\""));
        let out = run(&tool, "hb-call", json!({"action": "call", "tool": "image_generate", "args": {"prompt": "a fox"}})).await.unwrap();
        assert_eq!(out, "{\"ok\":true}");
        let calls = fake.calls.lock().unwrap();
        let body = calls.last().unwrap().2.clone().unwrap();
        assert_eq!((body["name"].as_str(), body["args"]["prompt"].as_str(), body["session_id"].as_str()), (Some("image_generate"), Some("a fox"), Some("hb-call")));
        assert!(fake.approvals.lock().unwrap().is_empty(), "a harmless tool does not prompt");
    }

    #[tokio::test]
    async fn a_missing_backend_is_a_clean_error() {
        let down = HermesTool::with_host(Fake::new(false, true));
        let err = run(&down, "hb-down", json!({"action": "list"})).await.unwrap_err();
        assert_eq!(err.to_string(), UNAVAILABLE);
        let none = HermesTool { host: None };
        if installed().is_none() {
            assert_eq!(run(&none, "hb-none", json!({"action": "call", "tool": "x"})).await.unwrap_err().to_string(), UNAVAILABLE);
        }
    }

    #[tokio::test]
    async fn the_run_policy_denylist_blocks_the_tool_and_hides_it() {
        let fake = Fake::new(true, true);
        let tool = HermesTool::with_host(fake.clone());
        super::super::set_session_tool_policy("hb-deny", None, ["cronjob_manage".to_string()].into());
        let err = run(&tool, "hb-deny", json!({"action": "call", "tool": "cronjob_manage", "args": {"action": "create"}})).await.unwrap_err();
        assert!(err.to_string().contains("disabled"));
        assert!(!fake.calls.lock().unwrap().iter().any(|c| c.1.ends_with("invoke")), "the backend never ran it");
        // Naming the toolset in the denylist blocks its tools too, without a copied tool table.
        super::super::set_session_tool_policy("hb-deny", None, ["cronjob".to_string()].into());
        assert!(run(&tool, "hb-deny", json!({"action": "call", "tool": "cronjob_manage"})).await.is_err());
        super::super::set_session_tool_policy("hb-deny", None, ["cronjob_manage".to_string()].into());
        let listed = run(&tool, "hb-deny", json!({"action": "list"})).await.unwrap();
        assert!(!listed.contains("cronjob_manage") && listed.contains("image_generate"));
        super::super::clear_session_tool_policy("hb-deny");
    }

    #[tokio::test]
    async fn an_allowlist_limits_which_hermes_tools_run() {
        let tool = HermesTool::with_host(Fake::new(true, true));
        super::super::set_session_tool_policy("hb-allow", Some(["hermes".to_string(), "image_generate".to_string()].into()), Default::default());
        assert!(run(&tool, "hb-allow", json!({"action": "call", "tool": "image_generate"})).await.is_ok());
        assert!(run(&tool, "hb-allow", json!({"action": "call", "tool": "ha_call_service"})).await.is_err());
        super::super::set_session_tool_policy("hb-allow", Some(["hermes".to_string(), "homeassistant".to_string()].into()), Default::default());
        assert!(run(&tool, "hb-allow", json!({"action": "call", "tool": "ha_call_service"})).await.is_ok(), "allowed through its toolset");
        super::super::clear_session_tool_policy("hb-allow");
    }

    #[tokio::test]
    async fn side_effecting_tools_need_approval() {
        let denied = Fake::new(true, false);
        let call = json!({"action": "call", "tool": "ha_call_service", "args": {"domain": "light"}});
        let err = run(&HermesTool::with_host(denied.clone()), "hb-ap", call.clone()).await.unwrap_err();
        assert!(err.to_string().contains("not approved"));
        assert_eq!(denied.approvals.lock().unwrap().len(), 1);
        assert!(!denied.calls.lock().unwrap().iter().any(|c| c.1.ends_with("invoke")), "denied means never invoked");
        let allowed = Fake::new(true, true);
        assert!(run(&HermesTool::with_host(allowed.clone()), "hb-ap", call).await.is_ok());
        assert!(allowed.calls.lock().unwrap().iter().any(|c| c.1.ends_with("invoke")));
        for name in ["send_message", "computer_use", "discord_admin"] {
            assert!(needs_approval(name), "{name}");
        }
        assert!(!needs_approval("image_generate") && !needs_approval("cronjob_manage"));
    }

    #[tokio::test]
    async fn clarify_returns_the_answer_and_never_hangs_without_a_user() {
        let fake = Fake::new(true, true);
        let tool = ClarifyTool::with_host(fake.clone());
        *fake.clarify.lock().unwrap() = Some(ClarifyReply::Answer("blue".into()));
        let out = tool.execute(json!({"question": "Which colour?", "choices": ["blue", "red"]}), ctx("c1")).await.unwrap().output;
        assert_eq!(out, "The user answered: blue");
        // Headless: the host answers NoUser at once and the tool says so.
        let out = tool.execute(json!({"question": "Which colour?"}), ctx("c1")).await.unwrap().output;
        assert!(out.starts_with("No user is available"), "{out}");
        assert!(tool.execute(json!({"question": " "}), ctx("c1")).await.is_err());
        if installed().is_none() {
            let bare = ClarifyTool::new().execute(json!({"question": "q"}), ctx("c2")).await.unwrap().output;
            assert!(bare.starts_with("No user is available"));
        }
    }
}
