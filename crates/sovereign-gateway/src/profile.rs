//! Defaults for the Hermes profile selected by the desktop's `--profile` arg.
//! HERMES_HOME is scoped by the sovereign entrypoint before the gateway starts.

use serde_yaml::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileDefaults {
    pub name: Option<String>,
    pub home: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub system_prompt: Option<String>,
}

pub fn current() -> ProfileDefaults {
    let name = std::env::var("SOVEREIGN_PROFILE")
        .ok()
        .filter(|s| !s.is_empty());
    let Some(home) = std::env::var_os("HERMES_HOME").map(PathBuf::from) else {
        return ProfileDefaults { name, ..Default::default() };
    };
    let (provider, model, reasoning_effort) = std::fs::read_to_string(home.join("config.yaml"))
        .map(|raw| parse_config(&raw))
        .unwrap_or_default();
    let system_prompt = std::fs::read_to_string(home.join("SOUL.md"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    ProfileDefaults {
        name,
        home: Some(home),
        provider,
        model,
        reasoning_effort,
        system_prompt,
    }
}

pub fn parse_config(raw: &str) -> (Option<String>, Option<String>, Option<String>) {
    let config = serde_yaml::from_str::<Value>(raw).unwrap_or(Value::Null);
    let model = &config["model"];
    let model_name = model["default"]
        .as_str()
        .or_else(|| model["model"].as_str())
        .or_else(|| model["name"].as_str())
        .or_else(|| model["default"]["model"].as_str())
        .or_else(|| model["default"]["default"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let provider = model["provider"]
        .as_str()
        .or_else(|| model["default"]["provider"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let effort = config["agent"]["reasoning_effort"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    (provider, model_name, effort)
}

/// The `GET /api/mcp/servers` body from `mcp_servers` in a Hermes config.yaml, the
/// same map the engine's MCP client loads. Env values are never echoed.
pub fn mcp_servers_body(raw: &str) -> serde_json::Value {
    let config = serde_yaml::from_str::<Value>(raw).unwrap_or(Value::Null);
    let mut rows: Vec<_> = config["mcp_servers"]
        .as_mapping()
        .into_iter()
        .flatten()
        .filter_map(|(name, cfg)| Some((name.as_str()?.to_owned(), cfg)))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let servers: Vec<_> = rows
        .into_iter()
        .map(|(name, cfg)| {
            let (url, command) = (cfg["url"].as_str(), cfg["command"].as_str());
            let env: serde_json::Map<_, _> = cfg["env"]
                .as_mapping()
                .into_iter()
                .flatten()
                .filter_map(|(k, _)| Some((k.as_str()?.to_owned(), "***".into())))
                .collect();
            serde_json::json!({
                "name": name,
                "transport": if url.is_some() { "http" } else if command.is_some() { "stdio" } else { "unknown" },
                "url": url, "command": command,
                "args": cfg["args"].as_sequence().map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()).unwrap_or_default(),
                "env": env, "auth": cfg["auth"].as_str(),
                "enabled": !matches!(cfg["enabled"].as_bool(), Some(false)),
                "tools": cfg["tools"].as_sequence().map(|t| t.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()),
                "source": "config", "plugin": null,
            })
        })
        .collect();
    serde_json::json!({ "servers": servers })
}

#[cfg(test)]
mod tests {
    use super::{mcp_servers_body, parse_config};

    #[test]
    fn mcp_list_mirrors_config_yaml_without_env_values() {
        let body = mcp_servers_body("mcp_servers:\n  b:\n    url: http://x/mcp\n    enabled: false\n  a:\n    command: /bin/a\n    args: [--x]\n    env: {K: secret}\n");
        assert_eq!(body["servers"][0]["name"], "a");
        assert_eq!(body["servers"][0]["transport"], "stdio");
        assert_eq!(body["servers"][0]["env"]["K"], "***");
        assert_eq!(body["servers"][1]["enabled"], false);
        assert_eq!(mcp_servers_body("")["servers"], serde_json::json!([]));
    }

    #[test]
    fn reads_hermes_main_model_and_effort_shapes() {
        assert_eq!(
            parse_config(
                "model:\n  provider: ollama\n  default: qwen3\nagent:\n  reasoning_effort: low\n"
            ),
            (
                Some("ollama".into()),
                Some("qwen3".into()),
                Some("low".into())
            )
        );
        assert_eq!(
            parse_config("model:\n  default:\n    provider: ollama\n    model: qwen3\n"),
            (Some("ollama".into()), Some("qwen3".into()), None)
        );
    }
}
