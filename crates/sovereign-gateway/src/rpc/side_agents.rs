//! `prompt.background`, `prompt.btw` and `preview.restart`, served by the
//! Rust engine instead of a Hermes Python `AIAgent`. Each replies `{task_id}`
//! at once and delivers its text later as `background.complete`,
//! `btw.complete` or `preview.restart.complete` (+ `.progress`) on the parent
//! session, the same events Hermes's `_spawn_side_agent` emitted.
//!
//! Background and preview runs are ordinary hidden engine sessions
//! (`agent_run`, the path cron and `/api/agent/run` use), so they are
//! headless: approvals are denied rather than prompted. `/btw` is one
//! tool-less model call over a transcript snapshot, like Hermes's one-shot
//! fallback. Nothing here runs Python.

use super::*;
use rand::Rng;
use sovereign_prime::refine::{Turn, transcript};

const RUN_TIMEOUT: Duration = Duration::from_secs(30 * 60);

const BTW_INSTRUCTIONS: &str = "You are the same AI assistant that is currently working inside the conversation \
transcribed below. The user has asked a quick SIDE question with /btw while the main work continues.\n\
Rules:\n- Answer ONLY the side question. Do not continue, redo, or critique the main task.\n\
- Use the transcript as your primary context; it is a snapshot and may not include the very latest activity.\n\
- If the transcript does not contain enough information to answer, say so plainly instead of guessing.\n\
- Be concise and direct.";

const PREVIEW_RULES: &[&str] = &[
    "Restart exactly the app intended for the Preview URL, not Hermes Desktop itself.",
    "The Preview URL and port are the target. Preserve that target unless you conclude it is impossible.",
    "If the prior conversation shows a specific command that bound this URL/port, prefer re-running THAT exact command (in the same cwd) over guessing a new one.",
    "First inspect what process, if any, owns the Preview URL port. If a stale server exists, inspect its cwd and prefer that cwd over the Hermes/Desktop process cwd.",
    "The Current working directory is only a hint. Do not assume it is the preview app root when the port owner or files indicate another root.",
    "If the console shows a module-script MIME error for src/main.tsx or similar, a static server is serving source files. Do not restart python -m http.server or any dumb static server for that app.",
    "For module-script MIME failures, inspect package.json/vite config in the candidate app root and start the real dev server/bundler (for example npm/pnpm/yarn dev) so module transforms happen.",
    "Before declaring success, verify the Preview URL responds with the intended app, not Hermes Desktop. If it serves Hermes/Desktop UI or another unrelated app, stop that process and report failure.",
    "Do not modify files. Do not ask the user unless blocked.",
    "Prefer existing project scripts or commands when they are clear.",
    "If a stale process owns the needed port, handle it safely.",
    "Start long-running servers detached/in the background, then return immediately.",
    "Do not run a foreground dev server command that blocks this background task.",
    "Keep the final response short: what command/server was started, or why it could not be restarted.",
];

const PREVIEW_HISTORY_NOTE: &str = "The conversation history below is from the user's main session, including the commands you (the assistant) previously ran to start servers, edit files, or check ports. Use it to figure out exactly which server should be running at this Preview URL. The user did not start a brand new task; recover what they had working.";

fn task_id(prefix: &str) -> String {
    format!("{prefix}_{:06x}", rand::thread_rng().gen_range(0..0x100_0000u32))
}

fn turns(history: &Value) -> Vec<Turn> {
    history["messages"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|m| Turn {
                    role: m["role"].as_str().unwrap_or_default().to_string(),
                    text: m["content"].as_str().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Conn {
    /// Run `prompt` as a hidden headless engine session and deliver its final
    /// text (or `error: ...`) on `parent` as `event`.
    fn spawn_side_run(
        self: &Arc<Self>,
        parent: String,
        kind: &'static str,
        event: &'static str,
        task_id: String,
        prompt: String,
        cwd: Option<String>,
        title: &'static str,
    ) {
        let conn = self.clone();
        tokio::spawn(async move {
            let result = agent_run(
                conn.config.clone(),
                conn.hub.clone(),
                conn.observer.clone(),
                kind,
                &prompt,
                cwd.as_deref(),
                Some(title),
                None,
                RUN_TIMEOUT,
            )
            .await;
            let text = match result {
                Ok(done) if done["ok"] == true => done["text"].as_str().unwrap_or_default().to_string(),
                Ok(done) => format!("error: {}", done["error"].as_str().unwrap_or("the run failed")),
                Err(err) => format!("error: {err:#}"),
            };
            conn.emit(event, Some(&parent), json!({ "task_id": task_id, "text": text })).await;
        });
    }

    pub(super) async fn prompt_background(self: &Arc<Self>, p: &Value) -> Result<Value, RpcError> {
        let (parent, text) = side_args(p)?;
        let task_id = task_id("bg");
        let cwd = self.session_cwd(&parent).await;
        self.spawn_side_run(parent, "background", "background.complete", task_id.clone(), text, cwd, "Background task");
        Ok(json!({ "task_id": task_id }))
    }

    pub(super) async fn prompt_btw(self: &Arc<Self>, p: &Value) -> Result<Value, RpcError> {
        let (parent, question) = side_args(p)?;
        let complete = self
            .config
            .complete
            .clone()
            .ok_or_else(|| RpcError::internal(anyhow!("no model is available for /btw")))?;
        let history = self.history(&parent).await.map_err(RpcError::internal)?;
        let snapshot = transcript(&turns(&history));
        let task_id = task_id("btw");
        let conn = self.clone();
        let reply_task = task_id.clone();
        tokio::spawn(async move {
            let user = format!("Conversation transcript:\n{snapshot}\n\nSide question: {question}");
            let started = crate::observability::now();
            let reply = complete(BTW_INSTRUCTIONS.to_string(), user).await;
            conn.observer.record_aux(
                &parent, "other", Some("Side question"), None, None, started,
                reply.as_ref().ok().and_then(|d| d.usage),
                reply.as_ref().err().map(|e| e.to_string()).as_deref(),
            );
            let text = match reply {
                Ok(done) => done.text.trim().to_string(),
                Err(err) => format!("error: {err:#}"),
            };
            conn.emit(
                "btw.complete",
                Some(&parent),
                json!({ "task_id": reply_task, "question": question, "text": text }),
            )
            .await;
        });
        Ok(json!({ "task_id": task_id }))
    }

    pub(super) async fn preview_restart(self: &Arc<Self>, p: &Value) -> Result<Value, RpcError> {
        let parent = p["session_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RpcError::params("session_id is required"))?
            .to_string();
        let field = |k: &str| p[k].as_str().unwrap_or_default().trim().to_string();
        let (url, cwd, context) = (field("url"), field("cwd"), field("context"));
        if url.is_empty() {
            return Err(RpcError::params("url required"));
        }
        // A malformed client path is "no validated cwd": fall back to the parent's.
        let preview_cwd = std::fs::canonicalize(&cwd).ok().filter(|d| d.is_dir()).map(|d| d.to_string_lossy().into_owned());
        let cwd_for_run = match &preview_cwd {
            Some(dir) => Some(dir.clone()),
            None => self.session_cwd(&parent).await,
        };
        // Last 24 messages back to the last user turn; tool output truncated.
        let mut recent = self.history(&parent).await.map(|h| turns(&h)).unwrap_or_default();
        let last_user = recent.iter().rposition(|t| t.role == "user");
        let start = recent.len().saturating_sub(24).min(last_user.unwrap_or(usize::MAX));
        recent.drain(..start.min(recent.len()));
        for turn in &mut recent {
            if turn.role == "tool" && turn.text.chars().count() > 1200 {
                turn.text = turn.text.chars().take(1200).collect::<String>() + "\n... (truncated)";
            }
        }
        let mut lines = vec![
            "The desktop preview pane cannot load a local server URL.".to_string(),
            format!("Preview URL: {url}"),
            format!("Current working directory: {}", if cwd.is_empty() { "(unknown)" } else { &cwd }),
        ];
        if !context.is_empty() {
            lines.push(format!("Preview console:\n{context}"));
        }
        lines.extend(PREVIEW_RULES.iter().map(|r| r.to_string()));
        if !recent.is_empty() {
            lines.push(PREVIEW_HISTORY_NOTE.to_string());
            lines.push(transcript(&recent));
        }
        let task_id = task_id("preview");
        let note = if recent.is_empty() {
            String::new()
        } else {
            format!(" (with {} parent-session messages of context)", recent.len())
        };
        self.emit(
            "preview.restart.progress",
            Some(&parent),
            json!({ "task_id": task_id, "text": format!("Starting hidden restart agent{note}") }),
        )
        .await;
        self.spawn_side_run(
            parent,
            "preview",
            "preview.restart.complete",
            task_id.clone(),
            lines.join("\n"),
            cwd_for_run,
            "Preview restart",
        );
        Ok(json!({ "task_id": task_id }))
    }
}

fn side_args(p: &Value) -> Result<(String, String), RpcError> {
    let parent = p["session_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::params("session_id is required"))?;
    let text = p["text"].as_str().unwrap_or_default();
    if text.is_empty() {
        return Err(RpcError::params("text required"));
    }
    Ok((parent.to_string(), text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_args_require_a_session_and_text() {
        assert!(side_args(&json!({ "text": "hi" })).is_err());
        assert!(side_args(&json!({ "session_id": "s1", "text": "" })).is_err());
        assert_eq!(side_args(&json!({ "session_id": "s1", "text": "hi" })).ok(), Some(("s1".into(), "hi".into())));
    }

    #[test]
    fn task_ids_carry_the_hermes_prefix() {
        assert!(task_id("bg").starts_with("bg_") && task_id("btw").len() == "btw_".len() + 6);
    }
}
