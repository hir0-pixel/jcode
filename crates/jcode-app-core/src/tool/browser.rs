//! Model-facing `browser` tool (M8a).
//!
//! One owner for the browser implementation: Hermes's Python agent already
//! drives it (`hermes-agent/tools/browser_tool.py`, backed by local headless
//! Chromium, Browser Use / Browserbase / Firecrawl cloud, a CDP endpoint, or
//! Camofox). This tool does not reimplement any of that — it forwards one
//! action to the feature-backend REST route `POST /api/browser/act`
//! (`hermes_cli/web_routers/browser.py`), the same on-demand path every other
//! forwarded Hermes route uses (see `sovereign-gateway::features`).
//!
//! The `sovereign serve` binary calls [`set_bridge`] once, after the gateway
//! binds, with that listener's route and token; its catch-all `/api/*`
//! handler wakes the Python backend on demand and reverse-proxies. The token
//! is held in memory, not in the environment, so commands the model runs
//! never inherit it.

use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

static BRIDGE: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

/// Point the tool at the feature backend's browser route (`url`, `token`).
pub fn set_bridge(url: String, token: String) {
    let _ = BRIDGE.set((url, token));
}

const ACTIONS: &[&str] = &[
    "navigate",
    "snapshot",
    "click",
    "type",
    "scroll",
    "back",
    "press",
    "get_images",
    "vision",
    "console",
];

pub struct BrowserTool {
    client: reqwest::Client,
    /// Test-only override for `(url, token)`; production uses [`set_bridge`].
    endpoint: Option<(String, String)>,
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: None,
        }
    }

    #[cfg(test)]
    fn with_endpoint(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: Some((url.into(), token.into())),
        }
    }

    fn endpoint(&self) -> Result<(String, String)> {
        if let Some(pair) = &self.endpoint {
            return Ok(pair.clone());
        }
        BRIDGE
            .get()
            .cloned()
            .context("browser tool unavailable: the engine's feature backend is not configured")
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Control a headless browser. Call action='navigate' first."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ACTIONS,
                    "description": "Which operation to run; see the matching field below for its params."
                },
                "url": {"type": "string", "description": "navigate: URL."},
                "ref": {"type": "string", "description": "click/type: ref from snapshot, e.g. '@e5'."},
                "text": {"type": "string", "description": "type: text to enter."},
                "direction": {"type": "string", "enum": ["up", "down"], "description": "scroll direction."},
                "key": {"type": "string", "description": "press: key name, e.g. 'Enter'."},
                "full": {"type": "boolean", "description": "snapshot: full content vs compact."},
                "question": {"type": "string", "description": "vision: what to look for."},
                "expression": {"type": "string", "description": "console: JS to evaluate."},
                "clear": {"type": "boolean", "description": "console: clear buffers after reading."}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let action = input["action"]
            .as_str()
            .context("browser: 'action' is required")?
            .to_string();
        if !ACTIONS.contains(&action.as_str()) {
            bail!(
                "browser: unknown action '{action}'. Valid actions: {}",
                ACTIONS.join(", ")
            );
        }
        let (url, token) = self.endpoint()?;

        let mut params = input;
        if let Value::Object(map) = &mut params {
            map.remove("action");
            map.remove("intent");
        }
        let body = json!({ "action": action, "task_id": ctx.session_id, "params": params });

        let resp = self
            .client
            .post(&url)
            .header("X-Hermes-Session-Token", token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("browser: request to the feature backend failed ({url})"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("browser: reading the feature backend's response")?;
        if !status.is_success() {
            bail!("browser: feature backend returned {status}: {text}");
        }
        Ok(ToolOutput::new(text))
    }
}

#[cfg(test)]
#[path = "browser_tests.rs"]
mod browser_tests;
