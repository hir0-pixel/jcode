//! `setup.status`, `setup.runtime_check` and `model.options` from the engine's real
//! credential state (Hermes `.env` first, then jcode's own auth), in Hermes's shapes.

use jcode_base::auth::AuthStatus;
use jcode_base::provider_catalog::{
    LoginProviderDescriptor, LoginProviderTarget, load_api_key_from_env_or_config, login_providers, resolve_login_provider_loose,
};
use serde_json::{Value, json};

fn authenticated(d: LoginProviderDescriptor, status: &AuthStatus) -> bool {
    match d.target {
        LoginProviderTarget::OpenAiCompatible(p) => {
            !p.requires_api_key || load_api_key_from_env_or_config(p.api_key_env, p.env_file).is_some()
        }
        _ => status.state_for_provider(d) != jcode_base::auth::AuthState::NotConfigured,
    }
}

fn default_model(d: LoginProviderDescriptor) -> Option<&'static str> {
    match d.target {
        LoginProviderTarget::OpenAiCompatible(p) => p.default_model,
        _ => None,
    }
}

/// The served provider has usable credentials. A label the catalog does not know
/// (a named profile, an external runtime) counts when anything is configured.
pub(super) fn configured(provider: &str) -> bool {
    let status = AuthStatus::check_fast();
    match resolve_login_provider_loose(provider) {
        Some(d) => authenticated(d, &status),
        None => login_providers().iter().any(|d| authenticated(*d, &status)),
    }
}

fn other_configured(provider: &str) -> bool {
    let status = AuthStatus::check_fast();
    let current = resolve_login_provider_loose(provider).map(|d| d.id);
    login_providers().iter().any(|d| Some(d.id) != current && authenticated(*d, &status))
}

pub(super) fn setup_status(provider: &str) -> Value {
    json!({
        "provider_configured": configured(provider), "ready": true, "free_tier": false,
        "other_providers": other_configured(provider), "inference_provider": provider,
    })
}

/// `requested`: an explicit provider to check strictly, else the served one.
pub(super) fn runtime_check(provider: &str, model: &str, requested: Option<&str>) -> Value {
    let (provider, model) = match requested {
        Some(r) if resolve_login_provider_loose(r).map(|d| d.id) != resolve_login_provider_loose(provider).map(|d| d.id) => {
            (r, resolve_login_provider_loose(r).and_then(default_model).unwrap_or(model))
        }
        _ => (provider, model),
    };
    if configured(provider) {
        json!({ "ok": true, "provider": provider, "model": model, "source": "engine", "free_tier": false })
    } else {
        json!({ "ok": false, "provider": provider, "model": model, "source": "engine", "error": format!("No usable credentials found for {provider}.") })
    }
}

/// Provider rows: the served one (with `current_models` when known), every provider with
/// credentials, and with `include_unconfigured` the rest of the catalog.
pub(super) fn model_options(provider: &str, model: &str, current_models: Vec<String>, include_unconfigured: bool, efforts: &[String]) -> Value {
    let status = AuthStatus::check_fast();
    let current_id = resolve_login_provider_loose(provider).map(|d| d.id);
    let mut rows: Vec<Value> = login_providers()
        .iter()
        .filter_map(|d| {
            let (is_current, auth) = (Some(d.id) == current_id, authenticated(*d, &status));
            if !(is_current || auth || include_unconfigured) {
                return None;
            }
            let models: Vec<String> = if is_current && !current_models.is_empty() {
                current_models.clone()
            } else if is_current {
                vec![model.to_string()]
            } else {
                default_model(*d).map(str::to_string).into_iter().collect()
            };
            let mut row = json!({ "slug": d.id, "name": d.display_name, "total_models": models.len(), "models": models, "is_current": is_current, "authenticated": auth });
            if is_current {
                row["capabilities"] = served_capabilities(&models, efforts);
            }
            Some(row)
        })
        .collect();
    if current_id.is_none() {
        // Named profile or runtime the catalog has no row for: it is what the engine serves.
        let models = if current_models.is_empty() { vec![model.to_string()] } else { current_models };
        let capabilities = served_capabilities(&models, efforts);
        rows.insert(0, json!({ "slug": provider, "name": provider, "total_models": models.len(), "models": models, "is_current": true, "authenticated": configured(provider), "capabilities": capabilities }));
    }
    rows.sort_by_key(|r| r["is_current"] != true);
    json!({ "providers": rows, "model": model, "provider": provider })
}

/// What the served provider can honour, per model of its row: reasoning (and
/// switching it off) only when it accepts effort levels. Without this the
/// desktop assumed reasoning everywhere, offered an Effort control for e.g. a
/// local Ollama model, and the effort it saved then made session start fail.
fn served_capabilities(models: &[String], efforts: &[String]) -> Value {
    let reasoning = !efforts.is_empty();
    let caps = json!({ "fast": false, "reasoning": reasoning, "can_disable_reasoning": reasoning });

    Value::Object(models.iter().map(|m| (m.clone(), caps.clone())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated<T>(f: impl FnOnce() -> T) -> T {
        let _lock = crate::hermes_env::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("provider-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("hermes")).unwrap();
        // SAFETY: env is only touched under ENV_LOCK.
        unsafe {
            std::env::set_var("JCODE_HOME", dir.join("jcode"));
            std::env::set_var("HERMES_HOME", dir.join("hermes"));
            std::env::remove_var("GROQ_API_KEY");
        }
        crate::hermes_env::register();
        let out = f();
        unsafe {
            std::env::remove_var("JCODE_HOME");
            std::env::remove_var("HERMES_HOME");
        }
        let _ = std::fs::remove_dir_all(dir);
        out
    }

    #[test]
    fn the_served_row_reports_reasoning_only_when_the_provider_takes_efforts() {
        isolated(|| {
            let none = model_options("ollama", "qwen3", vec!["qwen3".into()], false, &[]);
            let caps = &none["providers"][0]["capabilities"]["qwen3"];
            assert_eq!((caps["reasoning"].clone(), caps["can_disable_reasoning"].clone()), (json!(false), json!(false)));

            let efforts = ["low".to_string(), "high".to_string()];
            let some = model_options("ollama", "qwen3", vec!["qwen3".into()], false, &efforts);
            assert_eq!(some["providers"][0]["capabilities"]["qwen3"]["reasoning"], json!(true));
        });
    }

    #[test]
    fn a_missing_key_asks_for_one_and_a_hermes_env_key_satisfies_it() {
        isolated(|| {
            assert_eq!(setup_status("groq")["provider_configured"], false);
            let check = runtime_check("groq", "llama-3.3-70b", None);
            assert_eq!((check["ok"].clone(), check["error"].as_str()), (json!(false), Some("No usable credentials found for groq.")));
            let options = model_options("groq", "llama-3.3-70b", vec![], false, &[]);
            assert_eq!((options["providers"][0]["slug"].as_str(), options["providers"][0]["authenticated"].clone()), (Some("groq"), json!(false)));

            std::fs::write(std::path::Path::new(&std::env::var("HERMES_HOME").unwrap()).join(".env"), "GROQ_API_KEY=test-key\n").unwrap();
            assert_eq!(setup_status("groq")["provider_configured"], true);
            assert_eq!(runtime_check("groq", "llama-3.3-70b", None)["ok"], true);
            let options = model_options("groq", "llama-3.3-70b", vec!["llama-3.3-70b".into(), "qwen3".into()], false, &[]);
            let row = &options["providers"][0];
            assert_eq!((row["is_current"].clone(), row["authenticated"].clone(), row["total_models"].clone()), (json!(true), json!(true), json!(2)));
        });
    }
}
