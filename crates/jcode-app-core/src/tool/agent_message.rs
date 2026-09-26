use super::communicate::CommunicateTool;
use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct AgentMessageTool {
    inner: CommunicateTool,
}

impl AgentMessageTool {
    pub fn new() -> Self {
        Self {
            inner: CommunicateTool::new(),
        }
    }
}

#[async_trait]
impl Tool for AgentMessageTool {
    fn name(&self) -> &str {
        "agent_message"
    }

    fn description(&self) -> &str {
        "Send/read agent messages and list the delegated-agent roster."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {"type": "string", "enum": ["send", "read", "list"]},
                "target": {"type": "string", "description": "Agent session or label"},
                "message": {"type": "string", "description": "Message to send"}
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let action = input["action"].as_str().context("action is required")?;
        let target = input["target"].clone();
        let mapped = match action {
            "send" => {
                if input["message"].as_str().is_none_or(str::is_empty) {
                    bail!("message is required for send")
                }
                json!({"action":"message", "message":input["message"], "target_session":target})
            }
            "read" => {
                if target.as_str().is_none_or(str::is_empty) {
                    bail!("target is required for read")
                }
                json!({"action":"read", "target_session":target})
            }
            "list" => json!({"action":"list"}),
            other => bail!("unknown agent_message action: {other}"),
        };
        self.inner.execute(mapped, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messaging_schema_stays_under_two_hundred_tokens() {
        let schema = AgentMessageTool::new().parameters_schema().to_string();
        assert!(schema.len() / 4 < 200, "schema too large: {schema}");
    }
}
