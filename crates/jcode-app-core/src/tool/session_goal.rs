//! Session-level goal (agent loop), not the todo/initiative `goal` module.

use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct SessionGoalTool;

#[async_trait]
impl Tool for SessionGoalTool {
    fn name(&self) -> &str {
        "session_goal"
    }

    fn description(&self) -> &str {
        "Get, set, or complete the unattended session goal (persists across turns; the gateway continues the session until done or budget hit)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "op": { "type": "string", "enum": ["get", "create", "complete"], "description": "get status, create/replace goal text, or mark complete" },
                "text": { "type": "string", "description": "Goal text when op=create" }
            },
            "required": ["op"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let home = jcode_base::storage::jcode_dir()?;
        let store = sovereign_prime::agent_loop::ControlStore::open_cached(&home)?;
        let op_json = match input["op"].as_str().unwrap_or("get") {
            "create" => {
                json!({ "op": "create", "text": input["text"].as_str().unwrap_or_default() })
                    .to_string()
            }
            "complete" => json!({ "op": "complete" }).to_string(),
            _ => json!({ "op": "get" }).to_string(),
        };
        let text = sovereign_prime::agent_loop_host::goal_host(&store, &ctx.session_id, &op_json)?;
        Ok(ToolOutput::new(text))
    }
}
