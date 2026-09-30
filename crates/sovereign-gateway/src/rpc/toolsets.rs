//! Hermes toolset names (a cron job's `enabled_toolsets` / the cron denylist) as the engine's tool names.

/// The engine's tools behind one Hermes toolset. An unknown name passes through as-is, so a
/// job can also name an engine tool or an MCP server directly.
fn tools_of(toolset: &str) -> Vec<&str> {
    match toolset {
        // Loop prevention: anything that re-arms unattended work for later.
        "cronjob" => vec!["heartbeat", "session_goal", "cronjob_manage"],
        "messaging" => vec!["send_message"],
        "terminal" => vec!["bash", "bg"],
        "file" => vec!["read", "write", "edit", "patch", "apply_patch", "replace", "ls", "agentgrep"],
        "web" => vec!["webfetch", "websearch"],
        "browser" => vec!["browser"],
        "skills" => vec!["skill", "skill_manage"],
        "memory" => vec!["memory"],
        "todo" => vec!["todo"],
        "delegation" => vec!["delegate", "swarm", "agent_message"],
        "session_search" => vec!["session_search", "conversation_search"],
        "code_execution" => vec!["repl", "batch"],
        // Native: pauses the turn and asks the person; an unattended run is told nobody is there.
        "clarify" => vec!["clarify"],
        other => vec![other],
    }
}

/// Whether a toolset's tools live in Hermes's Python backend, reached through the `hermes` bridge tool
/// (image_gen, vision, tts, homeassistant, kanban, ...). The bridge matches a tool's own toolset name
/// against the run's policy, so those names pass through as-is; only the tool that carries them,
/// `hermes`, has to be allowed too. A name the engine maps itself, or an MCP server, is not one (a direct
/// engine-tool name also passes this test, which only makes `hermes` visible: it still runs nothing unnamed).
fn bridged(toolset: &str) -> bool {
    toolset == "cronjob"
        || (tools_of(toolset) == [toolset] && !toolset.starts_with("mcp") && toolset != "hermes")
}

/// jcode refuses a whole tool policy over one odd name (`no_mcp`-style markers, dots).
fn valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && name.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

pub(super) fn tools(toolsets: &[String]) -> Vec<String> {
    let mut out: Vec<String> = toolsets.iter().flat_map(|t| tools_of(t)).filter(|t| valid(t)).map(str::to_string).collect();
    out.sort();
    out.dedup();
    out
}

/// The `configure_tools` request for a run's policy, or `None` when it names nothing.
pub(super) fn request(session_id: &str, enabled: Option<&[String]>, disabled: &[String]) -> Option<serde_json::Value> {
    if enabled.is_none() && disabled.is_empty() {
        return None;
    }
    let mut tools = serde_json::json!({ "disabled": self::tools(disabled) });
    if let Some(enabled) = enabled {
        // Hermes's denylist wins over its allowlist, as in the AIAgent path.
        let blocked = self::tools(disabled);
        let mut allowed: Vec<String> = self::tools(enabled).into_iter().filter(|t| !blocked.contains(t)).collect();
        if enabled.iter().any(|t| bridged(t) && !disabled.contains(t)) {
            allowed.push("hermes".into());
        }
        allowed.sort();
        allowed.dedup();
        tools["enabled"] = serde_json::json!(allowed);
    }
    Some(serde_json::json!({ "req": "configure_tools", "session_id": session_id, "tools": tools }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_cron_denylist_blocks_the_engines_self_scheduling_tools() {
        let req = request("s1", None, &names(&["cronjob", "messaging", "clarify"])).unwrap();
        assert_eq!(req["tools"]["disabled"], serde_json::json!(["clarify", "cronjob_manage", "heartbeat", "send_message", "session_goal"]));
        assert!(req["tools"].get("enabled").is_none(), "no allowlist unless the job set one");
        assert!(request("s1", None, &[]).is_none());
    }

    #[test]
    fn a_per_job_allowlist_maps_toolsets_and_the_denylist_still_wins() {
        let req = request("s1", Some(&names(&["web", "cronjob", "mcp__docs"])), &names(&["cronjob"])).unwrap();
        assert_eq!(req["tools"]["enabled"], serde_json::json!(["mcp__docs", "webfetch", "websearch"]));
    }

    #[test]
    fn hermes_only_toolsets_enable_the_bridge_and_the_denylist_reaches_its_tools() {
        let req = request("s1", Some(&names(&["web", "image_gen", "clarify"])), &[]).unwrap();
        assert_eq!(req["tools"]["enabled"], serde_json::json!(["clarify", "hermes", "image_gen", "webfetch", "websearch"]));
        let req = request("s1", Some(&names(&["cronjob"])), &names(&["cronjob"])).unwrap();
        assert_eq!(req["tools"]["enabled"], serde_json::json!([]), "a denied bridge toolset does not enable the bridge");
        let req = request("s1", None, &names(&["cronjob", "image_gen"])).unwrap();
        assert_eq!(req["tools"]["disabled"], serde_json::json!(["cronjob_manage", "heartbeat", "image_gen", "session_goal"]));
    }
}
