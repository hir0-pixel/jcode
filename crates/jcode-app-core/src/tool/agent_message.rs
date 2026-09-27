use super::communicate::CommunicateTool;
use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

fn map_request(action: &str, target: Value, message: Value) -> Result<Value> {
    Ok(match action {
        "send" if message.as_str().is_some_and(|s| !s.is_empty()) => {
            json!({"action":"message", "message":message, "target_session":target})
        }
        "send" => bail!("message is required for send"),
        "read" if target.as_str().is_some_and(|s| !s.is_empty()) => {
            json!({"action":"read_context", "target_session":target})
        }
        "read" => bail!("target is required for read"),
        "list" => json!({"action":"list"}),
        other => bail!("unknown action: {other}"),
    })
}

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
        let mapped = map_request(action, input["target"].clone(), input["message"].clone())?;
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

    #[test]
    fn read_targets_the_agent_transcript_not_shared_context() {
        let mapped = map_request("read", serde_json::json!("agent-1"), Value::Null).unwrap();
        assert_eq!(mapped["action"], "read_context");
    }
}
