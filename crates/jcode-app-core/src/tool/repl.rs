//! `repl`: Prime-style recursive REPL (sovereign engine only).
//!
//! Large context stays in sandboxed REPL variables instead of the transcript;
//! the model inspects it with code and asks focused sub-questions through
//! `llm_query`. Execution happens in a memory-capped worker process (see the
//! `sovereign-prime` crate). Registered only when `SOVEREIGN_REPL_WORKER`
//! names a worker binary.

use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_OUTPUT_CHARS: usize = 8_000;
const SUBQUERY_SYSTEM: &str = "You are a focused sub-agent. Answer the request using only the text it contains. Be concise and exact.";

pub struct ReplTool {
    host: Arc<sovereign_prime::ReplHost>,
}

impl ReplTool {
    /// `None` outside the sovereign engine (no worker binary configured).
    pub fn from_env() -> Option<Self> {
        static HOST: OnceLock<Option<Arc<sovereign_prime::ReplHost>>> = OnceLock::new();
        let host = HOST.get_or_init(|| {
            let exe = std::env::var_os("SOVEREIGN_REPL_WORKER").map(PathBuf::from)?;
            exe.is_file().then(|| sovereign_prime::ReplHost::new(exe))
        });
        host.clone().map(|host| Self { host })
    }
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_OUTPUT_CHARS) {
        Some((cut, _)) => format!("{}\n… [output truncated]", &text[..cut]),
        None => text.to_string(),
    }
}

#[async_trait]
impl Tool for ReplTool {
    fn name(&self) -> &str {
        "repl"
    }

    fn description(&self) -> &str {
        sovereign_prime::TOOL_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "code": { "type": "string", "description": "Python (Monty subset) to run" }
            },
            "required": ["code"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let code = input["code"].as_str().context("`code` is required")?;
        let session_id = ctx.session_id.clone();
        let llm_query: sovereign_prime::LlmQuery = Arc::new(move |prompt: String| {
            let session_id = session_id.clone();
            Box::pin(async move {
                let provider =
                    crate::provider::active_provider_fork().context("no active model provider")?;
                let provider_name = provider.name().to_string();
                let model = provider.model();
                let started = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let result = provider
                    .complete_simple_with_usage(&prompt, SUBQUERY_SYSTEM)
                    .await;
                if let Some(observer) = super::repl_llm_query_observer() {
                    match &result {
                        Ok(reply) => observer(
                            &session_id,
                            provider_name,
                            model,
                            started,
                            reply.usage,
                            None,
                        ),
                        Err(error) => observer(
                            &session_id,
                            provider_name,
                            model,
                            started,
                            None,
                            Some(&error.to_string()),
                        ),
                    }
                }
                result.map(|reply| reply.text)
            })
        });
        let out = self
            .host
            .run(&ctx.session_id, code, ctx.working_dir.as_deref(), llm_query)
            .await?;
        let mut text = String::new();
        if out.fresh_state {
            text.push_str("[new REPL state]\n");
        }
        if !out.stdout.is_empty() {
            text.push_str(&out.stdout);
            if !out.stdout.ends_with('\n') {
                text.push('\n');
            }
        }
        if let Some(value) = &out.value {
            text.push_str(&format!("=> {value}\n"));
        }
        if let Some(error) = &out.error {
            text.push_str(&format!("Error: {error}\n"));
        }
        if text.is_empty() {
            text.push_str("(no output)");
        }
        Ok(ToolOutput::new(clip(&text)).with_metadata(json!({ "host_calls": out.host_calls })))
    }
}
