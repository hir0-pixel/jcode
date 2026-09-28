//! Headless runtime used by the Hermes desktop's `sovereign` binary.

use anyhow::Result;
use clap::ValueEnum;
use std::time::Instant;

use crate::provider::{self, Provider};
use crate::server;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ProviderChoice {
    Jcode,
    /// Native Claude (Anthropic OAuth/API). `claude-subprocess` is kept as a
    /// hidden alias for old scripts; the Claude Code CLI subprocess transport
    /// has been removed.
    #[value(alias = "claude-subprocess")]
    Claude,
    #[value(alias = "claude-api", alias = "anthropic-key", alias = "claude-key")]
    AnthropicApi,
    Openai,
    #[value(
        alias = "openai-key",
        alias = "openai-apikey",
        alias = "openai-platform"
    )]
    OpenaiApi,
    Openrouter,
    #[value(alias = "aws-bedrock", alias = "aws_bedrock")]
    Bedrock,
    #[value(alias = "azure-openai", alias = "aoai")]
    Azure,
    #[value(alias = "opencode-zen", alias = "zen")]
    Opencode,
    #[value(alias = "opencodego")]
    OpencodeGo,
    #[value(alias = "z.ai", alias = "z-ai", alias = "zai-coding")]
    Zai,
    #[value(
        alias = "kimi-code",
        alias = "kimi-coding",
        alias = "kimi-coding-plan",
        alias = "kimi-for-coding",
        alias = "moonshot-coding"
    )]
    Kimi,
    #[value(alias = "302.ai")]
    Ai302,
    Baseten,
    #[value(alias = "conifer-api")]
    Conifer,
    Cortecs,
    #[value(alias = "cgc", alias = "comtegra-gpu-cloud")]
    Comtegra,
    Deepseek,
    #[value(alias = "fpt-ai", alias = "fptcloud", alias = "fpt-cloud")]
    Fpt,
    Firmware,
    #[value(alias = "hugging-face", alias = "hf")]
    HuggingFace,
    #[value(alias = "moonshot")]
    MoonshotAi,
    Nebius,
    Scaleway,
    Stackit,
    Groq,
    #[value(alias = "mistralai")]
    Mistral,
    #[value(alias = "pplx")]
    Perplexity,
    #[value(alias = "together", alias = "together-ai")]
    TogetherAi,
    #[value(alias = "deep-infra")]
    Deepinfra,
    #[value(alias = "fireworks-ai", alias = "fireworks.ai")]
    Fireworks,
    #[value(alias = "novita-ai", alias = "novita.ai")]
    Novita,
    #[value(alias = "minimax-ai", alias = "minimaxi")]
    Minimax,
    #[value(alias = "x.ai", alias = "x-ai", alias = "grok")]
    Xai,
    /// Grok Build subscription via the authenticated Grok CLI ACP transport.
    #[value(name = "grok-build")]
    GrokBuild,
    #[value(alias = "nvidia", alias = "nim")]
    NvidiaNim,
    #[value(alias = "xiaomi", alias = "mimo", alias = "xiaomi-mimo-api")]
    XiaomiMimo,
    #[value(
        alias = "meta",
        alias = "muse",
        alias = "muse-spark",
        alias = "meta-model-api",
        alias = "meta-ai"
    )]
    MetaMuse,
    #[value(alias = "celeris-ai", alias = "celeris1", alias = "celeris-1")]
    Celeris,
    YoloAuto,
    #[value(alias = "lm-studio")]
    Lmstudio,
    Ollama,
    Chutes,
    #[value(alias = "cerebrascode", alias = "cerberascode")]
    Cerebras,
    #[value(alias = "belvedir.ai", alias = "belvedir-ai")]
    Belvedir,
    #[value(alias = "orca-router")]
    Orcarouter,
    #[value(
        alias = "bailian",
        alias = "aliyun-bailian",
        alias = "coding-plan",
        alias = "alibaba-coding"
    )]
    AlibabaCodingPlan,
    #[value(alias = "compat", alias = "custom")]
    OpenaiCompatible,
    Cursor,
    Copilot,
    Gemini,
    #[value(
        alias = "gemini-key",
        alias = "gemini-apikey",
        alias = "google-ai-studio",
        alias = "ai-studio"
    )]
    GeminiApi,
    Antigravity,
    Google,
    Auto,
}

impl ProviderChoice {
    #[allow(deprecated)]
    pub fn as_arg_value(&self) -> &'static str {
        match self {
            Self::Jcode => "jcode",
            Self::Claude => "claude",
            Self::AnthropicApi => "anthropic-api",
            Self::Openai => "openai",
            Self::OpenaiApi => "openai-api",
            Self::Openrouter => "openrouter",
            Self::Bedrock => "bedrock",
            Self::Azure => "azure",
            Self::Opencode => "opencode",
            Self::OpencodeGo => "opencode-go",
            Self::Zai => "zai",
            Self::Kimi => "kimi",
            Self::Ai302 => "302ai",
            Self::Baseten => "baseten",
            Self::Conifer => "conifer",
            Self::Cortecs => "cortecs",
            Self::Comtegra => "comtegra",
            Self::Deepseek => "deepseek",
            Self::Fpt => "fpt",
            Self::Firmware => "firmware",
            Self::HuggingFace => "huggingface",
            Self::MoonshotAi => "moonshotai",
            Self::Nebius => "nebius",
            Self::Scaleway => "scaleway",
            Self::Stackit => "stackit",
            Self::Groq => "groq",
            Self::Mistral => "mistral",
            Self::Perplexity => "perplexity",
            Self::TogetherAi => "togetherai",
            Self::Deepinfra => "deepinfra",
            Self::Fireworks => "fireworks",
            Self::Novita => "novita",
            Self::Minimax => "minimax",
            Self::Xai => "xai",
            Self::GrokBuild => "grok-build",
            Self::NvidiaNim => "nvidia-nim",
            Self::XiaomiMimo => "xiaomi-mimo",
            Self::MetaMuse => "meta-muse",
            Self::Celeris => "celeris",
            Self::YoloAuto => "yolo-auto",
            Self::Lmstudio => "lmstudio",
            Self::Ollama => "ollama",
            Self::Chutes => "chutes",
            Self::Cerebras => "cerebras",
            Self::Belvedir => "belvedir",
            Self::Orcarouter => "orcarouter",
            Self::AlibabaCodingPlan => "alibaba-coding-plan",
            Self::OpenaiCompatible => "openai-compatible",
            Self::Cursor => "cursor",
            Self::Copilot => "copilot",
            Self::Gemini => "gemini",
            Self::GeminiApi => "gemini-api",
            Self::Antigravity => "antigravity",
            Self::Google => "google",
            Self::Auto => "auto",
        }
    }
}

fn register_external_provider_runtimes() {
    crate::provider::external::register_external_provider(
        crate::provider::external::GROK_BUILD_RUNTIME,
        || std::sync::Arc::new(jcode_provider_grok_build_runtime::GrokBuildProvider::new()),
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::GEMINI_RUNTIME,
        || std::sync::Arc::new(jcode_provider_gemini_runtime::GeminiProvider::new()),
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::CURSOR_RUNTIME,
        || std::sync::Arc::new(jcode_provider_cursor_runtime::CursorCliProvider::new()),
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::ANTIGRAVITY_RUNTIME,
        || std::sync::Arc::new(jcode_provider_antigravity_runtime::AntigravityProvider::new()),
    );
    crate::provider::external::register_external_provider(
        crate::provider::external::ANTHROPIC_RUNTIME,
        || std::sync::Arc::new(jcode_provider_anthropic_runtime::AnthropicProvider::new()),
    );
    // OpenRouter serves several identities (aggregator, pinned API-key
    // runtime, direct OpenAI-compatible profiles, named config profiles)
    // through one concrete type, so it registers a parameterized factory.
    crate::provider::external::register_openrouter_factory(|spec| {
        use crate::provider::external::OpenRouterRuntimeSpec;
        use jcode_provider_openrouter_runtime::OpenRouterProvider;
        let provider: std::sync::Arc<dyn crate::provider::Provider> = match spec {
            OpenRouterRuntimeSpec::Default => std::sync::Arc::new(OpenRouterProvider::new()?),
            OpenRouterRuntimeSpec::OpenRouterApiKey => {
                std::sync::Arc::new(OpenRouterProvider::new_openrouter_api_key_runtime()?)
            }
            OpenRouterRuntimeSpec::CompatibleProfile(profile) => std::sync::Arc::new(
                OpenRouterProvider::new_openai_compatible_profile_runtime(profile)?,
            ),
            OpenRouterRuntimeSpec::NamedProfile { name, config } => std::sync::Arc::new(
                OpenRouterProvider::new_named_openai_compatible(&name, &config)?,
            ),
        };
        Ok(provider)
    });
    crate::provider::external::register_profile_catalog_refresh(
        jcode_provider_openrouter_runtime::maybe_schedule_openai_compatible_profile_catalog_refresh,
    );
    crate::provider::external::register_standard_openrouter_catalog_refresh(
        jcode_provider_openrouter_runtime::maybe_schedule_standard_openrouter_catalog_refresh,
    );
    // API-backed OpenAI routes use Codex/platform credentials. The runtime is
    // still registered without them so browser-backed ChatGPT models remain
    // usable through the logged-in Firefox session.
    crate::provider::external::register_external_provider_fallible(
        crate::provider::external::OPENAI_RUNTIME,
        || {
            let provider = match crate::auth::codex::load_credentials() {
                Ok(credentials) => jcode_provider_openai_runtime::OpenAIProvider::new(credentials),
                Err(_) => jcode_provider_openai_runtime::OpenAIProvider::new_browser_only(),
            };
            Some(std::sync::Arc::new(provider) as std::sync::Arc<dyn crate::provider::Provider>)
        },
    );
    // Copilot's constructor is fallible (needs a GitHub token) and the runtime
    // wants tier detection scheduled right after construction, eagerly for
    // interactive sessions and deferred for non-interactive ones. That policy
    // lives here in the composition root so base stays provider-agnostic.
    crate::provider::external::register_external_provider_fallible(
        crate::provider::external::COPILOT_RUNTIME,
        || {
            let provider = std::sync::Arc::new(
                jcode_provider_copilot_runtime::CopilotApiProvider::new().ok()?,
            );
            let eager_tier_detection = std::env::var("JCODE_NON_INTERACTIVE").is_err();
            if eager_tier_detection && tokio::runtime::Handle::try_current().is_ok() {
                let p_clone = std::sync::Arc::clone(&provider);
                tokio::spawn(async move {
                    p_clone.detect_tier_and_set_default().await;
                });
            } else {
                provider.complete_init_without_tier_detection();
            }
            Some(provider as std::sync::Arc<dyn crate::provider::Provider>)
        },
    );
}


async fn server_is_running_at(path: &std::path::Path) -> bool {
    // Check liveness before performing a protocol handshake. On Windows the
    // named pipe may be busy while another client is connecting; that already
    // proves a daemon exists, while a handshake connect can otherwise wait in
    // the transport's ERROR_PIPE_BUSY retry loop and block server startup.
    server::has_live_listener(path).await || server::is_server_ready(path).await
}

#[allow(deprecated)]
async fn init_provider_for_serve(
    choice: ProviderChoice,
    model: Option<&str>,
) -> Result<Arc<dyn provider::Provider>> {
    register_external_provider_runtimes();
    if let Ok(profile_name) = std::env::var("JCODE_PROVIDER_PROFILE_NAME")
        && !profile_name.trim().is_empty()
    {
        crate::provider_catalog::apply_named_provider_profile_env(profile_name.trim())?;
        crate::env::set_var("JCODE_PROVIDER_PROFILE_ACTIVE", "1");
    }
    let profile = crate::provider_catalog::resolve_openai_compatible_profile_selection(
        choice.as_arg_value(),
    );
    let provider: Arc<dyn provider::Provider> = if let Some(profile) = profile {
        if std::env::var_os("JCODE_NAMED_PROVIDER_PROFILE").is_none() {
            crate::provider_catalog::force_apply_openai_compatible_profile_env(Some(profile));
        }
        let named_profile = std::env::var("JCODE_NAMED_PROVIDER_PROFILE").ok();
        let runtime_model = if let Some(name) = named_profile.as_deref() {
            let cfg = crate::config::config();
            cfg
                .providers
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("Unknown provider profile '{name}'"))?
                .default_model
                .clone()
        } else {
            crate::provider_catalog::resolve_openai_compatible_profile(profile).default_model
        };
        provider::activation::apply_openai_compatible_runtime(runtime_model)?;
        if let Some(name) = named_profile {
            let cfg = crate::config::config();
            let profile = cfg
                .providers
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("Unknown provider profile '{name}'"))?;
            Arc::new(
                jcode_provider_openrouter_runtime::OpenRouterProvider::new_named_openai_compatible(
                    &name, profile,
                )?,
            )
        } else {
            Arc::new(jcode_provider_openrouter_runtime::OpenRouterProvider::new()?)
        }
    } else {
        let multi = || Arc::new(provider::MultiProvider::new_fast());
        match choice {
            ProviderChoice::Jcode => Arc::new(provider::jcode::JcodeProvider::new()),
            ProviderChoice::Claude => {
                provider::activation::select_initial_runtime_provider_key("claude");
                Arc::new(provider::MultiProvider::with_preference_fast(false))
            }
            ProviderChoice::AnthropicApi => {
                provider::activation::select_initial_runtime_provider_key("claude");
                Arc::new(provider::MultiProvider::with_preference_fast(false))
            }
            ProviderChoice::Openai | ProviderChoice::OpenaiApi => {
                provider::activation::select_initial_runtime_provider_key("openai");
                Arc::new(provider::MultiProvider::with_preference_fast(true))
            }
            ProviderChoice::Openrouter => {
                provider::activation::select_initial_runtime_provider_key("openrouter");
                multi()
            }
            ProviderChoice::Bedrock => {
                provider::activation::select_initial_runtime_provider_key("bedrock");
                multi()
            }
            ProviderChoice::Azure => {
                let azure_model = provider::activation::apply_azure_openai_runtime()?;
                let provider = multi();
                if let Some(model) = azure_model {
                    let _ = provider.set_model(&model);
                }
                provider
            }
            ProviderChoice::Cursor => {
                crate::env::set_var("JCODE_ACTIVE_PROVIDER", "cursor");
                Arc::new(jcode_provider_cursor_runtime::CursorCliProvider::new())
            }
            ProviderChoice::Copilot => {
                provider::activation::select_initial_runtime_provider_key("copilot");
                multi()
            }
            ProviderChoice::Gemini => {
                crate::env::set_var("JCODE_ACTIVE_PROVIDER", "gemini");
                Arc::new(jcode_provider_gemini_runtime::GeminiProvider::new())
            }
            ProviderChoice::GrokBuild => {
                crate::provider::external::instantiate_external_provider(
                    crate::provider::external::GROK_BUILD_RUNTIME,
                )
                .ok_or_else(|| anyhow::anyhow!("Grok Build runtime is not registered"))?
            }
            ProviderChoice::Antigravity => {
                crate::env::set_var("JCODE_ACTIVE_PROVIDER", "antigravity");
                Arc::new(jcode_provider_antigravity_runtime::AntigravityProvider::new())
            }
            ProviderChoice::Google | ProviderChoice::Auto => {
                let auto = provider::MultiProvider::from_auth_status(
                    crate::auth::AuthStatus::check_fast(),
                );
                crate::env::set_var("JCODE_ACTIVE_PROVIDER", auto.name().to_lowercase());
                Arc::new(auto)
            }
            ProviderChoice::YoloAuto => unreachable!("YoloAuto is an OpenAI-compatible profile"),
            _ => anyhow::bail!("unsupported sovereign provider {}", choice.as_arg_value()),
        }
    };
    if matches!(choice, ProviderChoice::AnthropicApi | ProviderChoice::OpenaiApi) {
        provider.set_credential_mode(provider::CredentialMode::ApiKey)?;
    }
    if let Some(model) = model {
        provider.set_model(model)?;
    }
    Ok(provider)
}

pub async fn run_gateway(
    provider_choice: &ProviderChoice,
    model: Option<&str>,
    host: &str,
    port: u16,
    allow_remote: bool,
) -> Result<()> {
    let profile_defaults = sovereign_gateway::profile::current();
    let mut effective_provider = *provider_choice;
    if let Some(provider) = profile_defaults.provider.as_deref() {
        effective_provider = match ProviderChoice::from_str(provider, true) {
            Ok(choice) => choice,
            Err(_) if crate::config::config().providers.contains_key(provider) => {
                crate::env::set_var("JCODE_NAMED_PROVIDER_PROFILE", provider);
                ProviderChoice::OpenaiCompatible
            }
            Err(_) => {
                anyhow::bail!("Hermes profile selects unsupported engine provider `{provider}`")
            }
        };
    }
    if matches!(effective_provider, ProviderChoice::OpenaiCompatible)
        && std::env::var_os("JCODE_NAMED_PROVIDER_PROFILE").is_none()
        && let Some(provider) = crate::config::config()
            .provider
            .default_provider
            .as_deref()
            .filter(|name| crate::config::config().providers.contains_key(*name))
    {
        // Hermes owns provider selection; JCode's named profile owns endpoint
        // details and credentials for the selected OpenAI-compatible transport.
        crate::env::set_var("JCODE_NAMED_PROVIDER_PROFILE", provider);
    }
    let effective_model = profile_defaults.model.as_deref().or(model);
    let socket =
        crate::storage::runtime_dir().join(format!("sovereign-{}.sock", std::process::id()));
    server::set_socket_path(&socket.to_string_lossy());

    let launch = sovereign_gateway::auth::launch_token()
        .map(str::to_owned)
        .or_else(|| std::env::var("HERMES_DASHBOARD_SESSION_TOKEN").ok());
    let token = match launch {
        Some(token) if token.len() >= 32 => token,
        _ => {
            let token = sovereign_gateway::auth::generate_token();
            let path = crate::storage::jcode_dir()?.join("sovereign-gateway.token");
            write_private_file(&path, &token)?;
            eprintln!("sovereign: token written to {}", path.display());
            token
        }
    };
    let bind: std::net::SocketAddr = format!("{host}:{port}")
        .parse()
        .or_else(|_| format!("[{host}]:{port}").parse())
        .map_err(|_| anyhow::anyhow!("invalid --host {host}"))?;

    let approval_secret = sovereign_gateway::auth::generate_token();
    // Ollama's OpenAI-compat `/v1` path ignores per-request num_ctx and reloads
    // the base model at its trained window (262k here), wiping a warm load.
    // Pin num_ctx on a local alias (`sovereign/…`) so warm-up and chat share
    // one serving size, then load it for the life of this process.
    let mut serve_model = effective_model.map(str::to_owned);
    if matches!(effective_provider, ProviderChoice::Ollama)
        || std::env::var("SOVEREIGN_PROVIDER").ok().as_deref() == Some("ollama")
    {
        let base = effective_model.unwrap_or("qwen3.8:27b");
        match warm_ollama_serving_context(base).await {
            Some(alias) => {
                serve_model = Some(alias);
            }
            None => {
                eprintln!("sovereign: Ollama warm failed; continuing with {base}");
            }
        }
    }
    let provider = init_provider_for_serve(effective_provider, serve_model.as_deref()).await?;
    // Catalog enrichment (GET /api/ps) only runs on fetch_models. Until then
    // Ollama's context_window() hard-falls back to 4096 and every tool-heavy
    // turn emergency-compacts. Refresh now that the model is warm.
    if matches!(effective_provider, ProviderChoice::Ollama)
        || std::env::var("SOVEREIGN_PROVIDER").ok().as_deref() == Some("ollama")
    {
        match provider.refresh_model_catalog().await {
            Ok(_) => {
                eprintln!(
                    "sovereign: Ollama context_window={} after catalog refresh",
                    provider.context_window()
                );
            }
            Err(err) => {
                eprintln!(
                    "sovereign: Ollama catalog refresh failed ({err}); context may stay at 4k"
                );
            }
        }
    }
    let (provider_name, provider_model) = (provider.name().to_string(), provider.model());
    let refine_provider = provider.clone();
    let complete: sovereign_gateway::Complete =
        std::sync::Arc::new(move |system: String, user: String| {
            let provider = refine_provider.clone();
            Box::pin(async move { provider.complete_simple_with_usage(&user, &system).await })
        });
    let learning = Some(sovereign_learning());
    let server = server::Server::new_with_name(provider, Some("sovereign".to_string()));

    let default_cwd = std::env::var("HERMES_DESKTOP_CWD")
        .or_else(|_| std::env::var("TERMINAL_CWD"))
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".into())
        });

    let ollama_unload = serve_model.clone().filter(|_| {
        matches!(effective_provider, ProviderChoice::Ollama)
            || std::env::var("SOVEREIGN_PROVIDER").ok().as_deref() == Some("ollama")
    });

    let features = hermes_feature_command()
        .map(|cmd| std::sync::Arc::new(sovereign_gateway::features::Features::new(cmd)));

    let gateway = async {
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while !server_is_running_at(&socket).await {
            if Instant::now() > deadline {
                anyhow::bail!("engine server did not start");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let gateway = sovereign_gateway::Gateway::bind(sovereign_gateway::Config {
            bind,
            token: token.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            legacy_socket: socket.clone(),
            default_cwd,
            allow_non_loopback: allow_remote,
            provider: provider_name,
            model: provider_model,
            home: crate::storage::jcode_dir()?.to_string_lossy().into_owned(),
            complete: Some(complete),
            approval_secret: approval_secret.clone(),
            features: features.clone(),
            learning: learning.clone(),
        })
        .await?;
        let port = gateway.local_addr().port();
        if let Some(features) = &features {
            features.set_engine_env(format!("http://127.0.0.1:{port}"), token.clone());
            crate::tool::set_browser_bridge(
                format!("http://127.0.0.1:{port}/api/browser/act"),
                token.clone(),
            );
        }
        // Where the pre_tool hook (`sovereign __pre-tool`) asks for approval.
        let approval = serde_json::json!({ "addr": gateway.local_addr().to_string(), "secret": approval_secret });
        write_private_file(
            &crate::storage::jcode_dir()?.join("sovereign-approval.json"),
            &approval.to_string(),
        )?;
        if let Ok(ready_file) = std::env::var("HERMES_DESKTOP_READY_FILE") {
            write_private_file(
                std::path::Path::new(&ready_file),
                &format!("{{\"port\":{port}}}"),
            )?;
        }
        // The exact line the Hermes desktop waits for.
        println!("HERMES_BACKEND_READY port={port}");
        use std::io::Write as _;
        std::io::stdout().flush()?;
        gateway.serve().await
    };
    let result = tokio::select! {
        result = server.run() => result,
        result = gateway => result,
        _ = shutdown_signal() => Ok(()),
    };
    let _ = std::fs::remove_file(&socket);
    if let Some(model) = ollama_unload {
        unload_ollama_model(&model).await;
    }
    result
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
        return;
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

fn ollama_num_ctx() -> u64 {
    std::env::var("SOVEREIGN_OLLAMA_NUM_CTX")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n >= 8192)
        .unwrap_or(32_768)
}

fn ollama_alias_name(base_model: &str) -> String {
    // Ollama model names: namespace/name; strip tags' ':' for the alias leaf.
    // Always include `:latest` so it matches `/v1/models` / catalog ids —
    // without it context_window() misses the cache and falls back to 4096.
    let leaf = base_model.replace(':', "-");
    format!("sovereign/{leaf}:latest")
}

/// Pin `num_ctx` on a local Ollama alias and load it for the life of this
/// process (`keep_alive: -1`). Returns the alias to use for chat, or `None`
/// if Ollama is unreachable (deferred auth / later failure).
async fn warm_ollama_serving_context(base_model: &str) -> Option<String> {
    let num_ctx = ollama_num_ctx();
    let alias = ollama_alias_name(base_model);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .ok()?;

    // Integer num_ctx required — a string value makes later loads fail with
    // `option "num_ctx" must be of type integer`.
    let create = serde_json::json!({
        "model": alias,
        "from": base_model,
        "stream": false,
        "parameters": { "num_ctx": num_ctx },
    });
    match client
        .post("http://127.0.0.1:11434/api/create")
        .json(&create)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            eprintln!(
                "sovereign: Ollama create {} returned HTTP {}; chat may reload at a different num_ctx",
                alias,
                resp.status()
            );
            return None;
        }
        Err(err) => {
            eprintln!("sovereign: Ollama create skipped ({err})");
            return None;
        }
    }

    // -1 = stay loaded until we send keep_alive:0 (app quit). Do not use a
    // wall-clock TTL that keeps the model resident after Hermes exits.
    let body = serde_json::json!({
        "model": alias,
        "prompt": ".",
        "stream": false,
        "keep_alive": -1,
    });
    match client
        .post("http://127.0.0.1:11434/api/generate")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            eprintln!(
                "sovereign: warmed Ollama {alias} (from {base_model}) with num_ctx={num_ctx}, keep_alive=-1"
            );
            if let Ok(home) = crate::storage::jcode_dir() {
                let meta =
                    serde_json::json!({ "model": alias, "base": base_model, "num_ctx": num_ctx });
                let _ =
                    write_private_file(&home.join("sovereign-ollama-warm.json"), &meta.to_string());
            }
            Some(alias)
        }
        Ok(resp) => {
            eprintln!(
                "sovereign: Ollama warm returned HTTP {}; chat may emergency-compact until num_ctx is pinned",
                resp.status()
            );
            None
        }
        Err(err) => {
            eprintln!("sovereign: Ollama warm skipped ({err})");
            None
        }
    }
}

async fn unload_ollama_model(model: &str) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let body = serde_json::json!({ "model": model, "keep_alive": 0 });
    match client
        .post("http://127.0.0.1:11434/api/generate")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            eprintln!("sovereign: unloaded Ollama {model}");
        }
        Ok(resp) => {
            eprintln!("sovereign: Ollama unload HTTP {}", resp.status());
        }
        Err(err) => {
            eprintln!("sovereign: Ollama unload skipped ({err})");
        }
    }
    if let Ok(home) = crate::storage::jcode_dir() {
        let _ = std::fs::remove_file(home.join("sovereign-ollama-warm.json"));
    }
}

/// Command that starts Hermes's Python backend for the features the Rust
/// harness does not own: `SOVEREIGN_HERMES_CMD` (empty disables), else the
/// managed install, else `hermes` on PATH.
fn hermes_feature_command() -> Option<Vec<String>> {
    if let Ok(python) = std::env::var("SOVEREIGN_HERMES_PYTHON") {
        return (!python.is_empty()).then(|| vec![python, "-m".into(), "hermes_cli.main".into()]);
    }
    if let Ok(cmd) = std::env::var("SOVEREIGN_HERMES_CMD") {
        let parts: Vec<String> = cmd.split_whitespace().map(str::to_owned).collect();
        return (!parts.is_empty()).then_some(parts);
    }
    let managed = dirs::home_dir()?.join(".hermes/hermes-agent/venv/bin/hermes");
    if managed.is_file() {
        return Some(vec![managed.to_string_lossy().into_owned()]);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("hermes"))
            .find(|p| p.is_file())
            .map(|p| vec![p.to_string_lossy().into_owned()])
    })
}

/// Write a file readable only by the current user (tokens, ready files).
fn write_private_file(path: &std::path::Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    // A pre-existing tmp file would keep its old permissions; start fresh.
    let _ = std::fs::remove_file(&tmp);
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write as _;
        let mut file = options.open(&tmp)?;
        file.write_all(contents.as_bytes())?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}



fn sovereign_learning() -> sovereign_gateway::learn::Learning {
    // Prime's `settings.autoRefine.turnInterval` / `.cooldownMs` (defaults
    // 25 turns / 20 minutes); no idle wait.
    let turn_interval = std::env::var("SOVEREIGN_LEARN_TURN_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);
    let cooldown_ms = std::env::var("SOVEREIGN_LEARN_COOLDOWN_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20 * 60_000);
    let remember: sovereign_gateway::learn::Remember = std::sync::Arc::new(
        |text: &str, category: &str, user_stated: bool, cwd: Option<&str>| {
            use crate::memory::{MemoryCategory, MemoryEntry, MemoryManager, TrustLevel};
            // Learned lessons are about the user and how they work: global scope,
            // with the chat's project attached so project scope can be added later.
            let manager = match cwd {
                Some(dir) => MemoryManager::new().with_project_dir(dir.to_string()),
                None => MemoryManager::new(),
            };
            let category = match category {
                "preference" => MemoryCategory::Preference,
                "correction" => MemoryCategory::Correction,
                _ => MemoryCategory::Fact,
            };
            let mut entry = MemoryEntry::new(category, text.to_string());
            entry.trust = if user_stated {
                TrustLevel::High
            } else {
                TrustLevel::Medium
            };
            entry.source = Some("prime-learning".to_string());
            manager.remember_global(entry)
        },
    );
    let forget: sovereign_gateway::learn::Forget = std::sync::Arc::new(|id: &str| {
        use crate::memory::{MemoryCategory, MemoryManager};
        let manager = MemoryManager::new();
        // Hand the text back so a rolled-back refine delete can restore it.
        let saved = manager.list_all().ok().and_then(|all| all.into_iter().find(|m| m.id == id));
        let _ = manager.forget(id);
        saved.map(|m| {
            let category = match m.category {
                MemoryCategory::Preference => "preference",
                MemoryCategory::Correction => "correction",
                _ => "fact",
            };
            (m.content, category.to_string())
        })
    });
    sovereign_gateway::learn::Learning {
        turn_interval,
        cooldown: std::time::Duration::from_millis(cooldown_ms),
        remember,
        forget,
    }
}
