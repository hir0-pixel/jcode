//! Pure translation between jcode harness API frames and Hermes `tui_gateway`
//! JSON-RPC shapes. No I/O here, so every mapping is unit-testable.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Instant;

/// Tool output beyond this is truncated in `tool.complete` (the full output
/// stays in the jcode transcript).
const TOOL_RESULT_MAX_CHARS: usize = 8_000;

#[derive(Default)]
pub struct SessionState {
    pub model: Option<String>,
    turn: Turn,
    usage: Usage,
    seq: u64,
}

#[derive(Default)]
struct Turn {
    started: bool,
    text: String,
    reasoning: String,
    stop: Option<&'static str>,
    tools: HashMap<String, Tool>,
}

struct Tool {
    name: String,
    args: Option<Value>,
    /// Streamed input JSON (`tool_input_delta`), parsed at `tool_exec`.
    input: String,
    started_at: Instant,
    announced: bool,
}

impl Tool {
    fn new(name: String) -> Self {
        Self { name, args: None, input: String::new(), started_at: Instant::now(), announced: false }
    }

    fn args(&self) -> Option<Value> {
        self.args
            .clone()
            .or_else(|| serde_json::from_str::<Value>(&self.input).ok())
            .filter(Value::is_object)
            .map(|mut v| {
                // jcode adds an `intent` field to every tool call; it is not an argument.
                if let Some(map) = v.as_object_mut() {
                    map.remove("intent");
                }
                v
            })
    }
}

/// Emit `tool.start` once, with the arguments known so far.
fn announce(state: &mut SessionState, sid: &str, call_id: &str, out: &mut Vec<Out>) {
    ensure_started(state, sid, out);
    let Some(tool) = state.turn.tools.get_mut(call_id) else { return };
    if tool.announced {
        return;
    }
    tool.announced = true;
    tool.started_at = Instant::now();
    out.push(event("tool.start", sid, json!({ "tool_id": call_id, "name": tool.name, "args": tool.args() })));
}

#[derive(Default)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    calls: u64,
}

impl SessionState {
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    pub fn turn_active(&self) -> bool {
        self.turn.started
    }

    pub fn usage_json(&self) -> Value {
        let u = &self.usage;
        json!({
            "model": self.model.clone().unwrap_or_default(),
            "input": u.input,
            "output": u.output,
            "reasoning": 0,
            "prompt": u.input,
            "completion": u.output,
            "total": u.input + u.output,
            "calls": u.calls,
            "cache_read": u.cache_read,
            "cache_write": u.cache_write,
        })
    }
}

/// Something the WebSocket side must do in response to one harness event.
#[derive(Debug, PartialEq)]
pub enum Out {
    Event { ty: &'static str, session_id: String, payload: Value },
    Approval { session_id: String, request_id: String, tool_name: String, description: String },
}

fn event(ty: &'static str, session_id: &str, payload: Value) -> Out {
    Out::Event { ty, session_id: session_id.to_string(), payload }
}

fn ensure_started(state: &mut SessionState, sid: &str, out: &mut Vec<Out>) {
    if !state.turn.started {
        state.turn.started = true;
        out.push(event("message.start", sid, json!({})));
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}\n… [truncated]", &text[..cut]),
        None => text.to_string(),
    }
}

/// Translate one streaming harness event (a frame without `reply_to`).
pub fn map_event(ev: &Value, sessions: &mut HashMap<String, SessionState>) -> Vec<Out> {
    let mut out = Vec::new();
    let Some(sid) = ev["session_id"].as_str() else {
        return out;
    };
    let state = sessions.entry(sid.to_string()).or_default();
    let text = |key: &str| ev[key].as_str().unwrap_or_default().to_string();

    match ev["ev"].as_str().unwrap_or_default() {
        "text_delta" => {
            ensure_started(state, sid, &mut out);
            let delta = text("text");
            state.turn.text.push_str(&delta);
            out.push(event("message.delta", sid, json!({ "text": delta })));
        }
        "text_replace" => state.turn.text = text("text"),
        "reasoning_delta" => {
            ensure_started(state, sid, &mut out);
            let delta = text("text");
            state.turn.reasoning.push_str(&delta);
            out.push(event("reasoning.delta", sid, json!({ "text": delta })));
        }
        "tool_call" => {
            let call_id = text("call_id");
            let tool = state.turn.tools.entry(call_id).or_insert_with(|| Tool::new(text("name")));
            tool.args = Some(ev["input"].clone());
        }
        "tool_start" => {
            ensure_started(state, sid, &mut out);
            state.turn.tools.entry(text("call_id")).or_insert_with(|| Tool::new(text("name")));
        }
        "tool_input_delta" => {
            let tool = state.turn.tools.entry(text("call_id")).or_insert_with(|| Tool::new(text("name")));
            if tool.input.len() < 256 * 1024 {
                tool.input.push_str(&text("delta"));
            }
        }
        "tool_exec" => {
            let call_id = text("call_id");
            state.turn.tools.entry(call_id.clone()).or_insert_with(|| Tool::new(text("name")));
            announce(state, sid, &call_id, &mut out);
        }
        "tool_done" => {
            let call_id = text("call_id");
            state.turn.tools.entry(call_id.clone()).or_insert_with(|| Tool::new(text("name")));
            announce(state, sid, &call_id, &mut out);
            let tool = state.turn.tools.remove(&call_id);
            let name = tool.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| text("name"));
            let args = tool.as_ref().and_then(Tool::args);
            let duration = tool.as_ref().map(|t| t.started_at.elapsed().as_secs_f64());
            let result = match ev["error"].as_str() {
                Some(err) => format!("Error: {err}\n{}", text("output")),
                None => text("output"),
            };
            out.push(event(
                "tool.complete",
                sid,
                json!({
                    "tool_id": call_id,
                    "name": name,
                    "args": args,
                    "duration_s": duration,
                    // The desktop renders `result`; `result_text` is the
                    // plain-text twin other clients read.
                    "result": truncate_chars(&result, TOOL_RESULT_MAX_CHARS),
                    "result_text": truncate_chars(&result, TOOL_RESULT_MAX_CHARS),
                }),
            ));
        }
        "token_usage" => {
            let u = &mut state.usage;
            u.input += ev["input"].as_u64().unwrap_or(0);
            u.output += ev["output"].as_u64().unwrap_or(0);
            u.cache_read += ev["cache_read_input"].as_u64().unwrap_or(0);
            u.cache_write += ev["cache_creation_input"].as_u64().unwrap_or(0);
            u.calls += 1;
            out.push(event("session.usage", sid, json!({ "usage": state.usage_json() })));
        }
        "turn_stopped" => {
            let status = match ev["reason"].as_str() {
                Some("interrupted") => "interrupted",
                _ => "error",
            };
            state.turn.stop = Some(status);
            let message = text("message");
            if status == "error" && !message.is_empty() {
                out.push(event("error", sid, json!({ "message": message })));
            }
        }
        "turn_done" => {
            let turn = std::mem::take(&mut state.turn);
            let reasoning = (!turn.reasoning.is_empty()).then_some(turn.reasoning);
            out.push(event(
                "message.complete",
                sid,
                json!({
                    "text": turn.text,
                    "status": turn.stop.unwrap_or("complete"),
                    "reasoning": reasoning,
                    "usage": state.usage_json(),
                }),
            ));
        }
        "session_renamed" => {
            let title = text("display_title");
            out.push(event("session.title", sid, json!({ "session_id": sid, "title": title })));
        }
        "compacted" => {
            out.push(event("status.update", sid, json!({ "kind": "compress", "text": text("message") })));
        }
        "model_info" | "runtime_info" => {
            if let Some(model) = ev["model"].as_str() {
                state.model = Some(model.to_string());
            }
        }
        "permission_request" => out.push(Out::Approval {
            session_id: sid.to_string(),
            request_id: text("request_id"),
            tool_name: text("tool_name"),
            description: text("description"),
        }),
        _ => {}
    }
    out
}

/// Hermes approval choice → jcode permission decision. `session` maps to a
/// one-time allow: jcode's `allow_always` persists beyond the session, which
/// would silently widen what the user granted.
pub fn approval_decision(choice: &str) -> &'static str {
    match choice {
        "once" | "session" => "allow",
        "always" => "allow_always",
        _ => "deny",
    }
}

/// jcode `SessionInfo` → Hermes `SessionListRow`.
pub fn session_row(info: &Value) -> Value {
    let id = info["session_id"].as_str().unwrap_or_default();
    let title = info["title"]
        .as_str()
        .or_else(|| info["display_title"].as_str())
        .unwrap_or("New chat");
    let ms = info["last_active_at_ms"].as_i64().or_else(|| info["updated_at_ms"].as_i64()).unwrap_or(0);
    json!({
        "id": id,
        "resolved_id": id,
        "title": title,
        "preview": "",
        "started_at": ms as f64 / 1000.0,
        "message_count": 0,
        "source": "desktop",
    })
}

/// jcode `SessionInfo` → desktop REST `SessionInfo` (`types/hermes.ts`).
pub fn session_info(info: &Value) -> Value {
    let row = session_row(info);
    let secs = row["started_at"].clone();
    json!({
        "id": row["id"],
        "title": row["title"],
        "preview": "",
        "source": "desktop",
        "started_at": secs,
        "last_active": secs,
        "ended_at": null,
        "is_active": info["status"] == "running",
        "message_count": 0,
        "tool_call_count": 0,
        "input_tokens": 0,
        "output_tokens": 0,
        "model": null,
        "cwd": info["working_dir"],
        "parent_session_id": info["parent_session_id"],
        "archived": info["archived"].as_bool().unwrap_or(false),
        "profile": "default",
        "is_default_profile": true,
    })
}

/// jcode history message → Hermes `TranscriptMessage`.
pub fn transcript(messages: &Value) -> Vec<Value> {
    messages
        .as_array()
        .map(|list| {
            list.iter()
                .map(|m| json!({ "role": m["role"], "text": m["content"] }))
                .collect()
        })
        .unwrap_or_default()
}

/// `SessionLiveInfo` for create/resume results.
pub fn live_info(session_id: &str, state: Option<&SessionState>, cwd: &str, version: &str, model: &str, provider: &str) -> Value {
    json!({
        "model": state.and_then(|s| s.model.clone()).unwrap_or_else(|| model.to_string()),
        "provider": provider,
        "cwd": cwd,
        "running": state.is_some_and(SessionState::turn_active),
        "title": "",
        "stored_session_id": session_id,
        "version": version,
        // The desktop warns below contract 8. The engine speaks the v8
        // protocol; methods it lacks answer `not_supported_by_engine`.
        "desktop_contract": 8,
    })
}

/// Complete a filesystem path fragment relative to `cwd` (at most 50 items;
/// dotfiles only when the fragment asks for them).
pub fn complete_path(word: &str, cwd: &str) -> Vec<Value> {
    let (dir_part, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let dir = if let Some(rest) = dir_part.strip_prefix("~/") {
        std::env::var("HOME").map(|h| std::path::Path::new(&h).join(rest)).unwrap_or_default()
    } else if dir_part.starts_with('/') {
        std::path::PathBuf::from(dir_part)
    } else {
        std::path::Path::new(cwd).join(dir_part)
    };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut items: Vec<(String, bool)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let hidden_ok = !name.starts_with('.') || prefix.starts_with('.');
            (name.starts_with(prefix) && hidden_ok).then(|| (name, e.file_type().is_ok_and(|t| t.is_dir())))
        })
        .collect();
    items.sort();
    items
        .into_iter()
        .take(50)
        .map(|(name, is_dir)| {
            let suffix = if is_dir { "/" } else { "" };
            json!({
                "text": format!("{dir_part}{name}{suffix}"),
                "display": format!("{name}{suffix}"),
                "meta": if is_dir { "dir" } else { "file" },
                "kind": if is_dir { "dir" } else { "file" },
            })
        })
        .collect()
}

/// Current git branch for `cwd` (walks up to the repository root; worktrees
/// and `.git` files supported). Short commit id when detached. No subprocess.
pub fn git_branch(cwd: &str) -> Option<String> {
    let mut dir = std::path::Path::new(cwd).to_path_buf();
    loop {
        let dot_git = dir.join(".git");
        let git_dir = if dot_git.is_dir() {
            Some(dot_git)
        } else if dot_git.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim().to_string();
            let path = std::path::Path::new(&target);
            Some(if path.is_absolute() { path.to_path_buf() } else { dir.join(path) })
        } else {
            None
        };
        if let Some(git_dir) = git_dir {
            let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
            let head = head.trim();
            return Some(match head.strip_prefix("ref: refs/heads/") {
                Some(branch) => branch.to_string(),
                None => head.chars().take(7).collect(),
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Title for an untitled session from its first prompt: the desktop's
/// `title_preview` when given, else the first non-empty line, capped at 60
/// characters on a word boundary. No model call.
pub fn derive_title(preview: Option<&str>, text: &str) -> Option<String> {
    let source = preview.filter(|p| !p.trim().is_empty()).unwrap_or(text);
    let line = source.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= 60 {
        return Some(line.to_string());
    }
    let cut: String = line.chars().take(60).collect();
    let trimmed = cut.rsplit_once(' ').map(|(head, _)| head).filter(|h| h.len() >= 20).unwrap_or(&cut);
    Some(format!("{}…", trimmed.trim_end()))
}

/// Text of a `prompt.submit` `text` field, which may be a string or a list of
/// content parts.
pub fn prompt_text(text: &Value) -> String {
    match text {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.as_str().map(str::to_string).or_else(|| p["text"].as_str().map(str::to_string)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(events: &[Value]) -> Vec<Out> {
        let mut sessions = HashMap::new();
        events.iter().flat_map(|e| map_event(e, &mut sessions)).collect()
    }

    fn types(out: &[Out]) -> Vec<&'static str> {
        out.iter()
            .filter_map(|o| match o {
                Out::Event { ty, .. } => Some(*ty),
                Out::Approval { .. } => Some("approval"),
            })
            .collect()
    }

    #[test]
    fn a_text_turn_maps_to_start_delta_complete() {
        let out = run(&[
            json!({"ev":"text_delta","session_id":"s","text":"Hel"}),
            json!({"ev":"text_delta","session_id":"s","text":"lo"}),
            json!({"ev":"token_usage","session_id":"s","input":10,"output":2}),
            json!({"ev":"text_done","session_id":"s"}),
            json!({"ev":"turn_done","session_id":"s"}),
        ]);
        assert_eq!(types(&out), ["message.start", "message.delta", "message.delta", "session.usage", "message.complete"]);
        let Out::Event { payload, .. } = out.last().unwrap() else { panic!() };
        assert_eq!(payload["text"], "Hello");
        assert_eq!(payload["status"], "complete");
        assert_eq!(payload["usage"]["total"], 12);
    }

    #[test]
    fn tools_carry_streamed_args_and_results() {
        // jcode's real order: start, streamed input, exec, done (no tool_call).
        let out = run(&[
            json!({"ev":"tool_start","session_id":"s","call_id":"c1","name":"bash"}),
            json!({"ev":"tool_input_delta","session_id":"s","call_id":"c1","delta":"{\"command\":\"ls\","}),
            json!({"ev":"tool_input_delta","session_id":"s","call_id":"c1","delta":"\"intent\":\"list\"}"}),
            json!({"ev":"tool_exec","session_id":"s","call_id":"c1","name":"bash"}),
            json!({"ev":"tool_exec","session_id":"s","call_id":"c1","name":"bash"}),
            json!({"ev":"tool_done","session_id":"s","call_id":"c1","name":"bash","output":"a.txt","error":null}),
        ]);
        assert_eq!(types(&out), ["message.start", "tool.start", "tool.complete"]);
        let Out::Event { payload, .. } = &out[1] else { panic!() };
        assert_eq!(payload["args"]["command"], "ls");
        assert!(payload["args"].get("intent").is_none(), "jcode's intent field is not an argument");
        let Out::Event { payload, .. } = &out[2] else { panic!() };
        assert_eq!(payload["result_text"], "a.txt");
        assert_eq!(payload["result"], "a.txt");
    }

    #[test]
    fn stops_map_to_turn_status() {
        let out = run(&[
            json!({"ev":"text_delta","session_id":"s","text":"x"}),
            json!({"ev":"turn_stopped","session_id":"s","reason":"failure","message":"rate limited"}),
            json!({"ev":"turn_done","session_id":"s"}),
        ]);
        assert_eq!(types(&out), ["message.start", "message.delta", "error", "message.complete"]);
        let Out::Event { payload, .. } = out.last().unwrap() else { panic!() };
        assert_eq!(payload["status"], "error");

        let out = run(&[
            json!({"ev":"turn_stopped","session_id":"s","reason":"interrupted","message":""}),
            json!({"ev":"turn_done","session_id":"s"}),
        ]);
        let Out::Event { payload, .. } = out.last().unwrap() else { panic!() };
        assert_eq!(payload["status"], "interrupted");
    }

    #[test]
    fn a_new_turn_starts_fresh() {
        let mut sessions = HashMap::new();
        for e in [json!({"ev":"text_delta","session_id":"s","text":"one"}), json!({"ev":"turn_done","session_id":"s"})] {
            map_event(&e, &mut sessions);
        }
        let out = map_event(&json!({"ev":"text_delta","session_id":"s","text":"two"}), &mut sessions);
        assert_eq!(types(&out), ["message.start", "message.delta"]);
    }

    #[test]
    fn permission_requests_become_approvals() {
        let out = run(&[json!({"ev":"permission_request","session_id":"s","request_id":"r","tool_name":"bash","description":"rm -rf x"})]);
        assert_eq!(
            out,
            [Out::Approval {
                session_id: "s".into(),
                request_id: "r".into(),
                tool_name: "bash".into(),
                description: "rm -rf x".into()
            }]
        );
    }

    #[test]
    fn approval_session_scope_never_widens() {
        assert_eq!(approval_decision("once"), "allow");
        assert_eq!(approval_decision("session"), "allow");
        assert_eq!(approval_decision("always"), "allow_always");
        assert_eq!(approval_decision("deny"), "deny");
        assert_eq!(approval_decision("anything-else"), "deny");
    }

    #[test]
    fn long_tool_output_is_truncated_on_a_char_boundary() {
        let long = "é".repeat(TOOL_RESULT_MAX_CHARS + 5);
        let t = truncate_chars(&long, TOOL_RESULT_MAX_CHARS);
        assert!(t.ends_with("[truncated]"));
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn events_without_a_session_are_ignored() {
        assert!(run(&[json!({"ev":"text_delta","text":"x"})]).is_empty());
    }

    #[test]
    fn git_branch_reads_head_without_git() {
        let root = std::env::temp_dir().join(format!("gb-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_branch(root.join("src/deep").to_str().unwrap()).as_deref(), Some("feature/x"));
        std::fs::write(root.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_branch(root.to_str().unwrap()).as_deref(), Some("0123456"));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(git_branch("/definitely/not/a/repo"), None);
    }

    #[test]
    fn titles_come_from_the_first_prompt() {
        assert_eq!(derive_title(None, "\n  Fix the login bug\nmore").as_deref(), Some("Fix the login bug"));
        assert_eq!(derive_title(Some("Preview title"), "ignored").as_deref(), Some("Preview title"));
        let long = derive_title(None, &"word ".repeat(40)).unwrap();
        assert!(long.ends_with('…') && long.chars().count() <= 61);
        assert_eq!(derive_title(None, "   "), None);
    }

    #[test]
    fn prompt_text_accepts_strings_and_parts() {
        assert_eq!(prompt_text(&json!("hi")), "hi");
        assert_eq!(prompt_text(&json!([{"type":"text","text":"a"}, "b"])), "a\nb");
        assert_eq!(prompt_text(&json!(null)), "");
    }
}

#[cfg(test)]
mod path_tests {
    use super::complete_path;

    #[test]
    fn completes_relative_paths_and_hides_dotfiles() {
        let dir = std::env::temp_dir().join(format!("sov-complete-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("setup.py"), "").unwrap();
        std::fs::write(dir.join(".secret"), "").unwrap();
        let cwd = dir.to_string_lossy();
        let texts: Vec<String> = complete_path("s", &cwd).iter().map(|i| i["text"].as_str().unwrap().to_string()).collect();
        assert_eq!(texts, ["setup.py", "src/"]);
        assert!(complete_path("", &cwd).iter().all(|i| !i["text"].as_str().unwrap().starts_with('.')));
        assert_eq!(complete_path(".s", &cwd).len(), 1);
        assert!(complete_path("nope/x", &cwd).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod tool_order_tests {
    use super::*;

    #[test]
    fn a_tool_that_never_executes_is_still_announced_before_completing() {
        let mut sessions = HashMap::new();
        let mut types = Vec::new();
        for e in [
            json!({"ev":"tool_start","session_id":"s","call_id":"c","name":"bash"}),
            json!({"ev":"tool_done","session_id":"s","call_id":"c","name":"bash","output":"","error":"blocked"}),
        ] {
            for o in map_event(&e, &mut sessions) {
                if let Out::Event { ty, .. } = o {
                    types.push(ty);
                }
            }
        }
        assert_eq!(types, ["message.start", "tool.start", "tool.complete"]);
    }
}
