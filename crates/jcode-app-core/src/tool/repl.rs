//! `repl`: Prime-style recursive REPL (sovereign engine only).
//!
//! Large context stays in sandboxed REPL variables instead of the transcript;
//! the model inspects it with code and asks focused sub-questions through
//! `llm_query`. Execution happens in a sandboxed CPython worker process (see
//! the `sovereign-prime` crate), using Hermes's bundled Python runtime.

use super::{Tool, ToolContext, ToolOutput};
use anyhow::{Context, Result};
use async_trait::async_trait;
#[cfg(not(target_os = "macos"))]
use jcode_tool_core::{StdinInputRequest, ToolExecutionMode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(not(target_os = "macos"))]
use tokio::time::{Duration, timeout};

const MAX_OUTPUT_CHARS: usize = 8_000;
const SUBQUERY_SYSTEM: &str = "You are a focused sub-agent. Answer the request using only the text it contains. Be concise and exact.";
static PENDING_COMPACTION: OnceLock<StdMutex<HashMap<String, Option<String>>>> = OnceLock::new();

pub(crate) fn take_pending_compaction(session_id: &str) -> Option<Option<String>> {
    PENDING_COMPACTION
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(session_id)
}

fn queue_compaction(session_id: &str, instructions: Option<String>) -> bool {
    let mut pending = PENDING_COMPACTION
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if pending.contains_key(session_id) {
        return false;
    }
    pending.insert(session_id.to_owned(), instructions);
    true
}

fn compaction_pending(session_id: &str) -> bool {
    PENDING_COMPACTION
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains_key(session_id)
}

#[cfg(not(target_os = "macos"))]
async fn approve_cell(ctx: &ToolContext, code: &str) -> Result<bool> {
    if ctx.execution_mode != ToolExecutionMode::AgentTurn {
        return Ok(false);
    }
    let Some(sender) = &ctx.stdin_request_tx else {
        return Ok(false);
    };
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let preview = code.chars().take(2_000).collect::<String>();
    sender
        .send(StdinInputRequest {
            request_id: format!("python-{}", ctx.tool_call_id),
            prompt: format!("Approve this Python REPL cell? Type APPROVE to run it.\n\n{preview}"),
            is_password: false,
            response_tx,
        })
        .map_err(|_| anyhow::anyhow!("Python REPL approval channel closed; cell denied"))?;
    let answer = timeout(Duration::from_secs(300), response_rx)
        .await
        .map_err(|_| anyhow::anyhow!("Python REPL approval timed out; cell denied"))?
        .map_err(|_| anyhow::anyhow!("Python REPL approval was cancelled; cell denied"))?;
    Ok(answer.trim() == "APPROVE")
}

pub struct ReplTool {
    host: Arc<sovereign_prime::ReplHost>,
}

#[cfg(test)]
mod compaction_request_tests {
    use super::{compaction_pending, queue_compaction, take_pending_compaction};

    #[test]
    fn compact_intent_is_session_scoped_and_consumed_once() {
        let session = format!("compact-test-{}", std::process::id());
        assert!(!compaction_pending(&session));
        assert!(queue_compaction(&session, Some("keep decisions".into())));
        assert!(!queue_compaction(&session, None));
        assert!(compaction_pending(&session));
        assert_eq!(
            take_pending_compaction(&session),
            Some(Some("keep decisions".into()))
        );
        assert!(!compaction_pending(&session));
        assert_eq!(take_pending_compaction(&session), None);
    }
}

static HOST: OnceLock<Option<Arc<sovereign_prime::ReplHost>>> = OnceLock::new();

pub async fn stop_session(session_id: &str) {
    if let Some(Some(host)) = HOST.get() {
        host.stop_session(session_id).await;
    }
}

impl ReplTool {
    /// `None` when the REPL is off or no interpreter is available: Hermes's
    /// bundled one, else (macOS sandbox only) the system `python3`.
    pub fn from_env() -> Option<Self> {
        let host = HOST.get_or_init(|| {
            let python = match std::env::var_os("SOVEREIGN_HERMES_PYTHON") {
                Some(path) => PathBuf::from(path),
                None => {
                    let enabled = std::env::var("JCODE_REPL").map_or(true, |v| v != "0")
                        && crate::config::config().agents.repl;
                    if !enabled {
                        return None;
                    }
                    sovereign_prime::host::system_python()?
                }
            };
            python
                .is_file()
                .then(|| sovereign_prime::ReplHost::new(python))
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
                "code": { "type": "string", "description": "Python to run in the persistent REPL" }
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
                match &result {
                    Ok(reply) => super::report_aux_model_call(
                        "REPL subquery",
                        &session_id,
                        provider_name,
                        model,
                        started,
                        reply.usage,
                        None,
                    ),
                    Err(error) => super::report_aux_model_call(
                        "REPL subquery",
                        &session_id,
                        provider_name,
                        model,
                        started,
                        None,
                        Some(&error.to_string()),
                    ),
                }
                result.map(|reply| reply.text)
            })
        });
        let session_id = ctx.session_id.clone();
        let session_for_host = ctx.session_id.clone();
        let goal: sovereign_prime::host::HostFn = Arc::new(move |op_json: String| {
            let session_id = session_for_host.clone();
            Box::pin(async move {
                let home = jcode_base::storage::jcode_dir()?;
                let store = sovereign_prime::agent_loop::ControlStore::open_cached(&home)?;
                sovereign_prime::agent_loop_host::goal_host(&store, &session_id, &op_json)
            })
        });
        let session_for_hb = ctx.session_id.clone();
        let heartbeat: sovereign_prime::host::HostFn = Arc::new(move |op_json: String| {
            let session_id = session_for_hb.clone();
            Box::pin(async move {
                let home = jcode_base::storage::jcode_dir()?;
                let store = sovereign_prime::agent_loop::ControlStore::open_cached(&home)?;
                sovereign_prime::agent_loop_host::heartbeat_host(&store, &session_id, &op_json)
            })
        });
        let refine: sovereign_prime::host::Refine = Arc::new(move |op_json: String| {
            let session_id = session_id.clone();
            Box::pin(async move {
                let op: Value = serde_json::from_str(&op_json).unwrap_or_default();
                let home = jcode_base::storage::jcode_dir()?;
                let store = sovereign_prime::entries::EntryStore::open_cached(&home)?;
                match op["op"].as_str().unwrap_or("run") {
                    "status" => {
                        Ok(json!({ "pending": store.refine_pending(&session_id)? }).to_string())
                    }
                    _ => {
                        store.schedule_refine(
                            &session_id,
                            op["instructions"].as_str(),
                            op["global"].as_bool().unwrap_or(false),
                        )?;
                        Ok(json!({ "scheduled": true }).to_string())
                    }
                }
            })
        });
        let extra = sovereign_prime::host::ExtraHostFns {
            goal,
            heartbeat,
            spawn_subagent: {
                let context = ctx.clone();
                Arc::new(move |op_json| {
                    let context = context.clone();
                    Box::pin(async move {
                        let op: Value = serde_json::from_str(&op_json).unwrap_or_default();
                        let prompt = op["prompt"].as_str().unwrap_or_default();
                        let label = op["label"].as_str().unwrap_or("worker");
                        if op["action"].as_str() == Some("await") {
                            let target = op["target"].as_str().unwrap_or_default();
                            let timeout = op["timeout"].as_u64().unwrap_or(20);
                            let result = super::communicate::prime_await_subagent(
                                &context.session_id,
                                target,
                                timeout,
                            )
                            .await?;
                            return Ok(result.to_string());
                        }
                        let output = super::delegate::DelegateTool::new()
                            .execute(
                                json!({"action":"spawn","prompt":prompt,"label":label}),
                                context,
                            )
                            .await?;
                        let session_id = output
                            .output
                            .split_once("Spawned new agent:")
                            .map(|(_, id)| id.trim())
                            .filter(|id| !id.is_empty())
                            .ok_or_else(|| {
                                anyhow::anyhow!("delegate did not return a spawned session id")
                            })?;
                        Ok(json!({ "session_id": session_id, "status": "spawned" }).to_string())
                    })
                })
            },
            agent_message: {
                let context = ctx.clone();
                Arc::new(move |op_json| {
                    let context = context.clone();
                    Box::pin(async move {
                        let op: Value = serde_json::from_str(&op_json).unwrap_or_default();
                        if let Some(operation) = op["host_request"].as_str() {
                            let result = super::communicate::prime_agent_host_request(
                                &context.session_id,
                                operation,
                                &op["payload"],
                            )
                            .await?;
                            return Ok(result.to_string());
                        }
                        let action = op["action"].as_str().unwrap_or("list");
                        let mut request = json!({"action":action});
                        match action {
                            "send" => {
                                request["action"] = json!("message");
                                request["prompt"] = op["message"].clone();
                                request["target_session"] = op["target"].clone();
                            }
                            "read" => {
                                request["action"] = json!("read_context");
                                request["target_session"] = op["target"].clone();
                            }
                            "list" => {}
                            other => anyhow::bail!("unknown agent_message action: {other}"),
                        }
                        let output = super::communicate::CommunicateTool::new()
                            .execute(request, context)
                            .await?;
                        Ok(output.output)
                    })
                })
            },
            websearch: {
                let context = ctx.clone();
                Arc::new(move |op_json| {
                    let context = context.clone();
                    Box::pin(async move {
                        let op: Value = serde_json::from_str(&op_json).unwrap_or_default();
                        let output = super::websearch::WebSearchTool::new()
                            .execute(
                                json!({
                                    "query": op["query"],
                                    "num_results": op["num_results"],
                                    "engine": "duckduckgo"
                                }),
                                context,
                            )
                            .await?;
                        Ok(output.output)
                    })
                })
            },
            compact: {
                let session_id = ctx.session_id.clone();
                Arc::new(move |op_json| {
                    let session_id = session_id.clone();
                    Box::pin(async move {
                        let op: serde_json::Value = serde_json::from_str(&op_json)?;
                        match op["op"].as_str().unwrap_or_default() {
                            "status" => {
                                Ok(json!({ "scheduled": compaction_pending(&session_id) })
                                    .to_string())
                            }
                            "run" => {
                                let instructions = op["instructions"].as_str().map(str::to_owned);
                                let scheduled = queue_compaction(&session_id, instructions);
                                Ok(json!({ "scheduled": scheduled, "phase": "end_of_turn" })
                                    .to_string())
                            }
                            other => anyhow::bail!("unsupported compact operation: {other}"),
                        }
                    })
                })
            },
            skill: Arc::new(move |input| {
                Box::pin(async move {
                    let input: Value = serde_json::from_str(&input)?;
                    let path = super::skill::create_skill_for_repl(input).await?;
                    let src = path.join("src");
                    Ok(json!({ "path": path, "src": src }).to_string())
                })
            }),
            ..sovereign_prime::host::ExtraHostFns::default()
        };
        #[cfg(target_os = "macos")]
        let approved = true;
        #[cfg(not(target_os = "macos"))]
        let approved = approve_cell(&ctx, code).await?;
        let out = self
            .host
            .run(
                &ctx.session_id,
                code,
                ctx.working_dir.as_deref(),
                llm_query,
                refine,
                extra,
                approved,
            )
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
