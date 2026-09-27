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

#[cfg(test)]
mod tests {
    use super::parse_config;

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
