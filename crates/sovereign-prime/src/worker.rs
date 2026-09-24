//! The REPL worker: one Monty interpreter per process, driven over stdio.
//!
//! Protocol (one JSON object per line):
//!   parent → worker  {"op":"run","code":"..."}
//!                    {"op":"reply","value":"..."} | {"op":"reply","error":"..."}
//!   worker → parent  {"op":"ready"}                      once, after arming limits
//!                    {"op":"call","fn":"llm_query","args":["..."]}
//!                    {"op":"done","stdout":"...","value":"...","error":null}
//!
//! The sandbox has no host access: OS/filesystem calls and unknown external
//! functions are refused. Only the functions in `HOST_FUNCTIONS` reach the
//! parent, which decides what they may do.

use monty::{MontyRepl, ReplProgress};
use monty_types::{CompileOptions, ExcType, MontyException, MontyObject, PrintWriter, ResourceLimits, ResourceTracker};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::time::Duration;

/// Functions the sandboxed code may call; the parent implements them.
pub const HOST_FUNCTIONS: &[&str] = &["llm_query", "load"];

pub struct Limits {
    pub max_memory: usize,
    pub max_duration: Duration,
    pub max_recursion: usize,
    pub max_stdout: usize,
    /// Host-function calls allowed per `run`.
    pub max_calls: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_memory: 256 * 1024 * 1024,
            max_duration: Duration::from_secs(20),
            max_recursion: 200,
            max_stdout: 64 * 1024,
            max_calls: 16,
        }
    }
}

fn tracker(limits: &Limits) -> ResourceTracker {
    ResourceTracker::new(ResourceLimits {
        max_duration: Some(limits.max_duration),
        max_memory: Some(limits.max_memory),
        max_recursion_depth: limits.max_recursion,
        ..ResourceLimits::default()
    })
}

fn exception(kind: ExcType, message: impl Into<String>) -> MontyException {
    MontyException::new(kind, Some(message.into()))
}

/// Host → sandbox value. Strings pass through; anything else is refused.
fn to_monty(value: &Value) -> MontyObject {
    match value {
        Value::String(s) => MontyObject::String(s.clone()),
        Value::Null => MontyObject::None,
        other => MontyObject::String(other.to_string()),
    }
}

fn arg_to_json(arg: &MontyObject) -> Value {
    match arg {
        MontyObject::String(s) => Value::String(s.clone()),
        other => Value::String(other.py_repr()),
    }
}

/// Run the worker loop on stdin/stdout until stdin closes.
pub fn run(limits: Limits) -> std::io::Result<()> {
    // Arm the hard memory ceiling for this process (the parent never arms it).
    let _ = monty_alloc::set_limit(Some(limits.max_memory), false);
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let send = |out: &mut std::io::StdoutLock<'_>, v: Value| -> std::io::Result<()> {
        writeln!(out, "{v}")?;
        out.flush()
    };
    send(&mut out, json!({"op": "ready"}))?;

    let mut repl = Some(MontyRepl::new("repl.py", tracker(&limits), CompileOptions::default()));
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if msg["op"] != "run" {
            continue;
        }
        let code = msg["code"].as_str().unwrap_or_default().to_string();
        let mut session = repl.take().unwrap_or_else(|| MontyRepl::new("repl.py", tracker(&limits), CompileOptions::default()));
        // Fresh time budget per run: Monty's clock is cumulative per session.
        *session.tracker_mut() = tracker(&limits);

        let mut printed = String::new();
        let mut calls = 0usize;
        let mut progress = session.feed_start(&code, vec![], PrintWriter::CollectString(&mut printed, Some(limits.max_stdout)));
        let (value, error) = loop {
            let step = match progress {
                Ok(step) => step,
                Err(failed) => {
                    let failed = *failed;
                    repl = Some(failed.repl);
                    break (None, Some(failed.error.to_string()));
                }
            };
            progress = match step {
                ReplProgress::Complete { repl: done, value } => {
                    repl = Some(done);
                    let value = (!matches!(value, MontyObject::None)).then(|| value.py_repr());
                    break (value, None);
                }
                ReplProgress::FunctionCall(call) => {
                    let name = call.function_name.clone();
                    let print = PrintWriter::CollectString(&mut printed, Some(limits.max_stdout));
                    if !HOST_FUNCTIONS.contains(&name.as_str()) || call.object_id.is_some() {
                        call.abort(exception(ExcType::NameError, format!("name '{name}' is not defined")), print)
                    } else if calls >= limits.max_calls {
                        call.abort(exception(ExcType::RuntimeError, format!("host call budget ({}) exhausted", limits.max_calls)), print)
                    } else {
                        calls += 1;
                        let args: Vec<Value> = call.args.iter().map(arg_to_json).collect();
                        send(&mut out, json!({"op": "call", "fn": name, "args": args}))?;
                        let mut reply_line = String::new();
                        if input.read_line(&mut reply_line)? == 0 {
                            return Ok(());
                        }
                        let reply: Value = serde_json::from_str(reply_line.trim()).unwrap_or_default();
                        match reply["error"].as_str() {
                            Some(err) => call.abort(exception(ExcType::RuntimeError, err), print),
                            None => call.resume(to_monty(&reply["value"]), print),
                        }
                    }
                }
                ReplProgress::OsCall(call) => {
                    let print = PrintWriter::CollectString(&mut printed, Some(limits.max_stdout));
                    call.abort(exception(ExcType::PermissionError, "filesystem and OS access is not available; use load(path)"), print)
                }
                ReplProgress::NameLookup(lookup) => {
                    let print = PrintWriter::CollectString(&mut printed, Some(limits.max_stdout));
                    let name = lookup.name.clone();
                    lookup.abort(exception(ExcType::NameError, format!("name '{name}' is not defined")), print)
                }
                ReplProgress::ResolveFutures(pending) => {
                    let print = PrintWriter::CollectString(&mut printed, Some(limits.max_stdout));
                    pending.abort(exception(ExcType::RuntimeError, "async host calls are not supported"), print)
                }
            };
        };
        send(&mut out, json!({"op": "done", "stdout": printed, "value": value, "error": error}))?;
    }
}
