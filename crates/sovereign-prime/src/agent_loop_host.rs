//! REPL host-function handlers for session goal / heartbeat (JSON op per call).

use crate::agent_loop::{ControlStore, handle_goal_command, handle_heartbeat_command};
use anyhow::{Context, Result};
use serde_json::{Value, json};

pub fn goal_host(store: &ControlStore, session_id: &str, op_json: &str) -> Result<String> {
    let op: Value = serde_json::from_str(op_json).context("goal() expects JSON")?;
    match op["op"].as_str().unwrap_or("get") {
        "get" => {
            let text = store
                .get_goal(session_id)?
                .map(|g| g.status_text())
                .unwrap_or_else(|| "No active session goal.".into());
            Ok(json!({ "goal": text }).to_string())
        }
        "complete" => Ok(handle_goal_command(store, session_id, "complete")?),
        "create" => {
            let text = op["text"]
                .as_str()
                .or(op["title"].as_str())
                .unwrap_or_default();
            if text.trim().is_empty() {
                anyhow::bail!("goal create requires text");
            }
            Ok(handle_goal_command(store, session_id, text.trim())?)
        }
        other => anyhow::bail!("unknown goal op: {other}"),
    }
}

pub fn heartbeat_host(store: &ControlStore, session_id: &str, op_json: &str) -> Result<String> {
    let op: Value = serde_json::from_str(op_json).context("heartbeat() expects JSON")?;
    match op["op"].as_str().unwrap_or("list") {
        "list" => Ok(handle_heartbeat_command(store, session_id, "list")?),
        "clear" => Ok(handle_heartbeat_command(store, session_id, "clear")?),
        "set" => {
            let interval = op["interval"]
                .as_str()
                .or(op["every"].as_str())
                .unwrap_or_default();
            let prompt = op["prompt"].as_str().unwrap_or_default();
            if interval.is_empty() || prompt.trim().is_empty() {
                anyhow::bail!("heartbeat set requires interval and prompt");
            }
            Ok(handle_heartbeat_command(
                store,
                session_id,
                &format!("every {interval} {prompt}"),
            )?)
        }
        other => anyhow::bail!("unknown heartbeat op: {other}"),
    }
}
