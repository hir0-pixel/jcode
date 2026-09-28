//! REPL host-function handlers for session goal / heartbeat (JSON op per call).

use crate::agent_loop::{
    ControlStore, GoalStatus, Heartbeat, HeartbeatStatus, SessionGoal, handle_heartbeat_command,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};

pub fn goal_host(store: &ControlStore, session_id: &str, op_json: &str) -> Result<String> {
    let op: Value = serde_json::from_str(op_json).context("goal() expects JSON")?;
    match op["op"].as_str().unwrap_or("get") {
        "get" => Ok(goal_result(store.get_goal(session_id)?).to_string()),
        "complete" => {
            let mut goal = store
                .get_goal(session_id)?
                .ok_or_else(|| anyhow::anyhow!("No goal to complete."))?;
            goal.status = GoalStatus::Done;
            goal.last_verdict = Some("done".into());
            goal.updated_at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or(goal.updated_at_ms);
            store.set_goal(session_id, Some(&goal))?;
            Ok(goal_result(Some(goal)).to_string())
        }
        "create" => {
            let text = op["text"]
                .as_str()
                .or(op["title"].as_str())
                .unwrap_or_default();
            if text.trim().is_empty() {
                anyhow::bail!("goal create requires text");
            }
            if store
                .get_goal(session_id)?
                .is_some_and(|goal| goal.status != GoalStatus::Done)
            {
                anyhow::bail!("an active or paused goal already exists");
            }
            let token_budget = match op.get("token_budget") {
                None | Some(Value::Null) => None,
                Some(value) => {
                    let budget = value.as_i64().filter(|budget| *budget > 0).ok_or_else(|| {
                        anyhow::anyhow!("goal token_budget must be a positive integer")
                    })?;
                    Some(budget)
                }
            };
            let mut goal = SessionGoal::new(text.trim());
            goal.token_budget = token_budget;
            store.set_goal(session_id, Some(&goal))?;
            Ok(goal_result(Some(goal)).to_string())
        }
        other => anyhow::bail!("unknown goal op: {other}"),
    }
}

fn goal_result(goal: Option<SessionGoal>) -> Value {
    let remaining_tokens = goal.as_ref().and_then(|goal| {
        goal.token_budget
            .map(|budget| budget.saturating_sub(goal.tokens_used))
    });
    let completion_budget_report = goal.as_ref().map(|goal| {
        json!({
            "token_budget": goal.token_budget,
            "tokens_used": goal.tokens_used,
            "remaining_tokens": remaining_tokens,
            "turns_used": goal.turns_used,
            "max_turns": goal.max_turns,
        })
    });
    let goal = goal.map(|goal| {
        json!({
            "objective": goal.title,
            "status": match goal.status { GoalStatus::Active => "active", GoalStatus::Done => "done", GoalStatus::Paused => "paused" },
            "token_budget": goal.token_budget,
            "tokens_used": goal.tokens_used,
            "remaining_tokens": remaining_tokens,
            "turns_used": goal.turns_used,
            "max_turns": goal.max_turns,
            "started_at_ms": goal.started_at_ms,
            "created_at_ms": goal.created_at_ms,
            "updated_at_ms": goal.updated_at_ms,
        })
    });
    json!({
        "goal": goal,
        "remaining_tokens": remaining_tokens,
        "completion_budget_report": completion_budget_report,
    })
}

pub fn heartbeat_host(store: &ControlStore, session_id: &str, op_json: &str) -> Result<String> {
    let op: Value = serde_json::from_str(op_json).context("heartbeat() expects JSON")?;
    match op["op"].as_str().unwrap_or("list") {
        "rlm_list" => {
            let include_inactive = op["include_inactive"].as_bool().unwrap_or(false);
            let items: Vec<_> = store
                .list_heartbeats(session_id)?
                .into_iter()
                .filter(|h| {
                    h.source == "rlm"
                        && (include_inactive || h.status == HeartbeatStatus::Active)
                })
                .map(|h| json!({
                    "id": h.id,
                    "label": h.label,
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
            heartbeat.label = op["label"].as_str().map(str::to_owned);
            match op["delivery_mode"].as_str() {
                None | Some("follow_up") => {}
                Some("steer") => anyhow::bail!(
                    "RLM heartbeats run when the session is idle; steer delivery is unsupported"
                ),
                Some(_) => anyhow::bail!("RLM heartbeat delivery_mode must be follow_up"),
            }
            store.upsert_heartbeat(&heartbeat)?;
            Ok(
                json!({"id": heartbeat.id, "status": "active", "label": heartbeat.label})
                    .to_string(),
            )
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
                if op.get("label").is_some() {
                    heartbeat.label = op["label"].as_str().map(str::to_owned);
                }
                match op["delivery_mode"].as_str() {
                    None | Some("follow_up") => {}
                    Some("steer") => anyhow::bail!(
                        "RLM heartbeats run when the session is idle; steer delivery is unsupported"
                    ),
                    Some(_) => anyhow::bail!("RLM heartbeat delivery_mode must be follow_up"),
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
    fn goal_skill_host_returns_structured_state_and_enforces_budget_and_completion() {
        let store = ControlStore::memory().unwrap();
        let create: Value = serde_json::from_str(
            &goal_host(
                &store,
                "s",
                r#"{"op":"create","text":"ship parity","token_budget":9000}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(create["goal"]["objective"], "ship parity");
        assert_eq!(create["goal"]["token_budget"], 9000);
        assert_eq!(create["remaining_tokens"], 9000);
        assert!(goal_host(&store, "s", r#"{"op":"create","text":"replace"}"#).is_err());

        let complete: Value =
            serde_json::from_str(&goal_host(&store, "s", r#"{"op":"complete"}"#).unwrap()).unwrap();
        assert_eq!(complete["goal"]["status"], "done");
        assert_eq!(complete["completion_budget_report"]["token_budget"], 9000);
    }

    #[test]
    fn goal_skill_host_rejects_malformed_explicit_budgets() {
        let store = ControlStore::memory().unwrap();
        for budget in ["true", "\"500\"", "0", "-1", "9223372036854775808"] {
            let op = format!(r#"{{"op":"create","text":"goal","token_budget":{budget}}}"#);
            assert!(goal_host(&store, "s", &op).is_err(), "accepted {budget}");
        }
        let valid: Value = serde_json::from_str(
            &goal_host(
                &store,
                "s",
                r#"{"op":"create","text":"goal","token_budget":null}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(valid["goal"]["token_budget"].is_null());
    }

    #[test]
    fn rlm_heartbeat_skill_filters_paused_items_and_keeps_labels() {
        let store = ControlStore::memory().unwrap();
        let created: Value = serde_json::from_str(
            &heartbeat_host(
                &store,
                "s",
                r#"{"op":"rlm_create","instruction":"check progress","interval":"5m","label":"tests","delivery_mode":"follow_up"}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let id = created["id"].as_str().unwrap();
        assert_eq!(created["label"], "tests");
        heartbeat_host(
            &store,
            "s",
            &json!({"op":"rlm_update","id":id,"status":"pause"}).to_string(),
        )
        .unwrap();
        let active: Value =
            serde_json::from_str(&heartbeat_host(&store, "s", r#"{"op":"rlm_list"}"#).unwrap())
                .unwrap();
        assert_eq!(active["heartbeats"].as_array().unwrap().len(), 0);
        let all: Value = serde_json::from_str(
            &heartbeat_host(&store, "s", r#"{"op":"rlm_list","include_inactive":true}"#).unwrap(),
        )
        .unwrap();
        assert_eq!(all["heartbeats"][0]["status"], "paused");
        assert_eq!(all["heartbeats"][0]["label"], "tests");
        assert!(
            heartbeat_host(
                &store,
                "s",
                r#"{"op":"rlm_create","instruction":"interrupt","delivery_mode":"steer"}"#,
            )
            .is_err()
        );
    }

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
        let listed: Value = serde_json::from_str(
            &heartbeat_host(&store, "s", r#"{"op":"rlm_list","include_inactive":true}"#).unwrap(),
        )
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
