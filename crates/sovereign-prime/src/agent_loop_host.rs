//! REPL host-function handlers for session goal / heartbeat (JSON op per call).

use crate::agent_loop::{
    ControlStore, Heartbeat, HeartbeatStatus, handle_goal_command, handle_heartbeat_command,
};
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
        "rlm_list" => {
            let items: Vec<_> = store
                .list_heartbeats(session_id)?
                .into_iter()
                .filter(|h| h.source == "rlm")
                .map(|h| json!({
                    "id": h.id,
                    "instruction": h.prompt,
                    "status": match h.status { HeartbeatStatus::Active => "active", HeartbeatStatus::Paused => "paused" },
                    "interval_seconds": h.interval_seconds,
                    "fire_count": h.fire_count,
                }))
                .collect();
            Ok(json!({"heartbeats": items}).to_string())
        }
        "rlm_create" => {
            let interval = parse_interval(op["interval"].as_str().unwrap_or("5m"))?;
            let instruction = op["instruction"].as_str().unwrap_or_default().trim();
            if instruction.is_empty() {
                anyhow::bail!("RLM heartbeat instruction is required");
            }
            let mut heartbeat = Heartbeat::new(session_id, instruction, interval);
            heartbeat.source = "rlm".into();
            store.upsert_heartbeat(&heartbeat)?;
            Ok(json!({"id": heartbeat.id, "status": "active"}).to_string())
        }
        "rlm_update" | "rlm_delete" => {
            let id = op["id"].as_str().context("RLM heartbeat id is required")?;
            let mut heartbeat = store
                .list_heartbeats(session_id)?
                .into_iter()
                .find(|h| h.id == id && h.source == "rlm")
                .context("RLM heartbeat not found")?;
            if op["op"] == "rlm_delete" {
                store.delete_heartbeat(id)?;
            } else {
                if let Some(instruction) = op["instruction"].as_str() {
                    heartbeat.prompt = instruction.to_string();
                }
                if let Some(interval) = op["interval"].as_str() {
                    heartbeat.interval_seconds = parse_interval(interval)?;
                }
                match op["status"].as_str() {
                    Some("pause") => heartbeat.status = HeartbeatStatus::Paused,
                    Some("resume") => heartbeat.status = HeartbeatStatus::Active,
                    Some(_) => anyhow::bail!("RLM heartbeat status must be pause or resume"),
                    None => {}
                }
                store.upsert_heartbeat(&heartbeat)?;
            }
            Ok(json!({"id": id, "status": "updated"}).to_string())
        }
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

fn parse_interval(value: &str) -> Result<i64> {
    let (amount, multiplier) = if let Some(amount) = value.strip_suffix('s') {
        (amount, 1)
    } else if let Some(amount) = value.strip_suffix('m') {
        (amount, 60)
    } else if let Some(amount) = value.strip_suffix('h') {
        (amount, 3600)
    } else {
        (value, 1)
    };
    let seconds = amount
        .parse::<i64>()
        .context("invalid heartbeat interval")?
        .checked_mul(multiplier)
        .context("heartbeat interval is too large")?;
    anyhow::ensure!(seconds > 0, "heartbeat interval must be positive");
    Ok(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rlm_heartbeat_crud_is_separate_from_user_heartbeat() {
        let store = ControlStore::memory().unwrap();
        let user = Heartbeat::new("s", "user reminder", 60);
        store.upsert_heartbeat(&user).unwrap();
        let created: Value = serde_json::from_str(
            &heartbeat_host(
                &store,
                "s",
                r#"{"op":"rlm_create","instruction":"check progress","interval":"5m"}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let id = created["id"].as_str().unwrap();
        assert_eq!(store.user_heartbeat("s").unwrap().unwrap().id, user.id);
        assert_eq!(store.list_heartbeats("s").unwrap().len(), 2);
        heartbeat_host(
            &store,
            "s",
            &json!({"op":"rlm_update","id":id,"status":"pause"}).to_string(),
        )
        .unwrap();
        let listed: Value =
            serde_json::from_str(&heartbeat_host(&store, "s", r#"{"op":"rlm_list"}"#).unwrap())
                .unwrap();
        assert_eq!(listed["heartbeats"][0]["status"], "paused");
        heartbeat_host(&store, "s", &json!({"op":"rlm_delete","id":id}).to_string()).unwrap();
        assert_eq!(store.list_heartbeats("s").unwrap().len(), 1);
    }

    #[test]
    fn heartbeat_interval_parser_rejects_bad_and_overflow_values() {
        assert_eq!(parse_interval("5m").unwrap(), 300);
        assert!(parse_interval("0m").is_err());
        assert!(parse_interval("999999999999999999999h").is_err());
        assert!(parse_interval("💥").is_err());
    }
}
