//! Idle heartbeat for the agent loop (re-enter session on a timer when idle).

use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct SessionHeartbeatTool;

#[async_trait]
impl Tool for SessionHeartbeatTool {
    fn name(&self) -> &str {
        "heartbeat"
    }

    fn description(&self) -> &str {
        "Set, list, or clear idle heartbeats that re-submit a prompt when this session is idle."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "op": { "type": "string", "enum": ["set", "list", "clear"], "description": "configure, inspect, or remove heartbeats" },
                "interval": { "type": "string", "description": "Duration token for set, e.g. 5m or 1h" },
                "prompt": { "type": "string", "description": "Prompt to inject when the heartbeat fires" }
            },
            "required": ["op"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let home = jcode_base::storage::jcode_dir()?;
        let store = sovereign_prime::agent_loop::ControlStore::open_cached(&home)?;
        let op_json = json!({
            "op": input["op"].as_str().unwrap_or("list"),
            "interval": input["interval"].as_str(),
            "prompt": input["prompt"].as_str(),
        })
        .to_string();
        let text = sovereign_prime::agent_loop_host::heartbeat_host(&store, &ctx.session_id, &op_json)?;
        Ok(ToolOutput::new(text))
    }
}
