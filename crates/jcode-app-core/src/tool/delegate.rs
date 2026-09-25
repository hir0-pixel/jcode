//! Compact subagent delegation (spawn / message / list / stop / status) via swarm internals.

use super::{Tool, ToolContext, ToolOutput};
use super::communicate::CommunicateTool;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct DelegateTool {
    inner: CommunicateTool,
}

impl DelegateTool {
    pub fn new() -> Self {
        Self { inner: CommunicateTool::new() }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Spawn or manage child agents: spawn (optional spec: name prefix), message, list, stop, status."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": { "type": "string", "enum": ["spawn", "message", "list", "stop", "status"] },
                "label": { "type": "string", "description": "Short chip label for spawn" },
                "prompt": { "type": "string", "description": "Task for spawn or body for message" },
                "target_session": { "type": "string", "description": "Child session or label for message/stop" },
                "to_session": { "type": "string", "description": "Alias of target_session" },
                "model": { "type": "string" },
                "working_dir": { "type": "string" }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let action = input["action"].as_str().context("action is required")?;
        let mut mapped = input.clone();
        mapped["action"] = json!(match action {
            "message" => "message",
            "list" => "list",
            "stop" => "stop",
            "status" => "status",
            "spawn" => "spawn",
            other => other,
        });
        if action == "spawn" {
            let mut prompt = input["prompt"].as_str().unwrap_or("").to_string();
            if let Some(rest) = prompt.strip_prefix("spec:").map(str::trim) {
                let name = rest.split_whitespace().next().unwrap_or_default();
                if !name.is_empty() {
                    let home = jcode_base::storage::jcode_dir()?;
                    let store = sovereign_prime::entries::EntryStore::open_cached(&home)?;
                    if let Some(spec) = store.resolve_subagent_spec(&ctx.session_id, name)? {
                        prompt = format!("{}\n\n{}", spec.content.trim(), rest[name.len()..].trim());
                    }
                }
            }
            if mapped.get("label").and_then(Value::as_str).is_none_or(str::is_empty) {
                mapped["label"] = json!("delegate");
            }
            mapped["prompt"] = json!(prompt);
        }
        if action == "message" {
            let target = input["target_session"].as_str().or(input["to_session"].as_str());
            if let Some(t) = target {
                mapped["to_session"] = json!(t);
            }
            if let Some(body) = input["prompt"].as_str() {
                mapped["message"] = json!(body);
            }
        }
        self.inner.execute(mapped, ctx).await
    }
}
