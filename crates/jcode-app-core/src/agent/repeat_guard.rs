//! Repeated tool-call guard for the turn loops.
//!
//! Thresholds follow Hermes' `agent/tool_guardrails.py`: the identical-call
//! notice fires at `STALL_GUARD_IDENTICAL_CALL_THRESHOLD = 3` consecutive
//! identical (tool, args, result) calls, and the streak halt at 5
//! (`no_progress_block_after`, `exact_failure_block_after`). The wording is
//! adapted from `_IDENTICAL_CALL_NOTICE` and `identical_call_streak_halt`.
//! Like Hermes, the 5th identical call is not executed: `block` answers it
//! with a tool error and the turn goes on; only a runaway (10) ends the turn.
//! Read-only repeats (read/ls/glob/grep, `ls`/`git status` polls) are warned
//! about but never blocked.
//! A second streak tracks the same failing result from one tool even when the
//! arguments vary slightly. Pollers (`bg`, `*_poll`) are exempt, as Hermes'
//! `is_stall_guard_repeatable`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub(super) const WARN_AFTER: u32 = 3;
pub(super) const BLOCK_AFTER: u32 = 5;
pub(super) const STOP_AFTER: u32 = 10;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum GuardVerdict {
    Ok,
    /// Nth consecutive repeat; inject a reminder before the next request.
    Warn { tool: String, count: u32 },
    /// End the turn (only produced by `block`).
    Stop { tool: String, count: u32 },
}

#[derive(Default)]
pub(super) struct RepeatGuard {
    call_sig: u64,
    call_count: u32,
    fail_sig: u64,
    fail_count: u32,
    fail_tool: String,
    last_input_sig: u64,
    last_tool: String,
    blocked: u32,
    pending_reminder: Option<String>,
}

fn hash_of(parts: &[&str]) -> u64 {
    let mut h = DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
        0u8.hash(&mut h);
    }
    h.finish()
}

fn is_poller(tool: &str) -> bool {
    tool == "bg" || tool.ends_with("_poll") || tool.ends_with("_get_result")
}

fn is_block_exempt(tool: &str, input: &serde_json::Value) -> bool {
    match tool {
        "read" | "ls" | "glob" | "grep" | "agentgrep" => true,
        "bash" => input["command"]
            .as_str()
            .map(|c| {
                let c = c.trim_start();
                c == "ls" || c.starts_with("ls ") || c.starts_with("git status")
            })
            .unwrap_or(false),
        _ => false,
    }
}

impl RepeatGuard {
    /// Called before executing a call. If the same call (or the same failing
    /// tool) has already repeated `BLOCK_AFTER` times, return the tool-error
    /// text to answer with instead of executing, plus the turn-ending error
    /// once the runaway reaches `STOP_AFTER`.
    pub(super) fn block(
        &mut self,
        session_id: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> Option<(String, Option<anyhow::Error>)> {
        if is_poller(tool) || is_block_exempt(tool, input) {
            return None;
        }
        let same_call =
            self.call_count >= BLOCK_AFTER && hash_of(&[tool, &input.to_string()]) == self.last_input_sig && tool == self.last_tool;
        let same_fail = self.fail_count >= BLOCK_AFTER && tool == self.fail_tool;
        if !(same_call || same_fail) {
            return None;
        }
        self.blocked += 1;
        let count = self.call_count.max(self.fail_count) + self.blocked;
        let verdict = if count >= STOP_AFTER {
            GuardVerdict::Stop { tool: tool.to_string(), count }
        } else {
            GuardVerdict::Ok
        };
        jcode_base::obs_sink::emit(
            jcode_base::obs_sink::Span::new("loop.guard")
                .session(session_id)
                .attr("action", "block")
                .attr("tool", tool)
                .attr("count", count),
        );
        let msg = format!(
            "blocked: identical call repeated {count} times, change your approach. {tool} was not executed."
        );
        Some((msg, self.handle(session_id, &verdict)))
    }

    pub(super) fn observe(
        &mut self,
        tool: &str,
        input: &serde_json::Value,
        result: &str,
        is_error: bool,
    ) -> GuardVerdict {
        if is_poller(tool) {
            self.call_count = 0;
            self.fail_count = 0;
            return GuardVerdict::Ok;
        }
        self.last_input_sig = hash_of(&[tool, &input.to_string()]);
        self.last_tool = tool.to_string();
        let sig = hash_of(&[tool, &input.to_string(), result]);
        if sig == self.call_sig && self.call_count > 0 {
            self.call_count += 1;
        } else {
            self.call_sig = sig;
            self.call_count = 1;
            self.blocked = 0;
        }
        if is_error {
            let fsig = hash_of(&[tool, result]);
            if fsig == self.fail_sig && self.fail_count > 0 {
                self.fail_count += 1;
            } else {
                self.fail_sig = fsig;
                self.fail_count = 1;
                self.fail_tool = tool.to_string();
            }
        } else {
            self.fail_count = 0;
        }
        let count = self.call_count.max(self.fail_count);
        if count == WARN_AFTER {
            let reminder = format!(
                "<system-reminder>You are repeating yourself: {tool} has now been called {count} times in a row with the same outcome{}. Do not repeat it. Change the arguments, use a different tool, or proceed with what you already have. The call will be blocked if this continues.</system-reminder>",
                if is_error { " (it keeps failing)" } else { "" }
            );
            self.pending_reminder = Some(reminder);
            GuardVerdict::Warn { tool: tool.to_string(), count }
        } else {
            GuardVerdict::Ok
        }
    }

    pub(super) fn take_reminder(&mut self) -> Option<String> {
        self.pending_reminder.take()
    }

    /// Record the verdict as an obs span and return the stop error if any.
    pub(super) fn handle(&self, session_id: &str, verdict: &GuardVerdict) -> Option<anyhow::Error> {
        let (action, tool, count) = match verdict {
            GuardVerdict::Ok => return None,
            GuardVerdict::Warn { tool, count } => ("warn", tool, count),
            GuardVerdict::Stop { tool, count } => ("stop", tool, count),
        };
        jcode_base::obs_sink::emit(
            jcode_base::obs_sink::Span::new("loop.guard")
                .session(session_id)
                .attr("action", action)
                .attr("tool", tool.as_str())
                .attr("count", *count),
        );
        matches!(verdict, GuardVerdict::Stop { .. }).then(|| {
            anyhow::anyhow!(
                "Stopped: {tool} was repeated {count} times in a row with identical results. The model is stuck in a loop; rephrase the request or give it a hint."
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_calls_warn_at_three_then_block_then_stop_at_ten() {
        let mut g = RepeatGuard::default();
        let args = json!({"command": "cargo test"});
        assert_eq!(g.observe("bash", &args, "x", false), GuardVerdict::Ok);
        assert_eq!(g.observe("bash", &args, "x", false), GuardVerdict::Ok);
        assert!(matches!(g.observe("bash", &args, "x", false), GuardVerdict::Warn { count: 3, .. }));
        assert!(g.take_reminder().unwrap().contains("repeating yourself"));
        assert!(g.take_reminder().is_none());
        assert!(g.block("s", "bash", &args).is_none());
        assert_eq!(g.observe("bash", &args, "x", false), GuardVerdict::Ok);
        assert_eq!(g.observe("bash", &args, "x", false), GuardVerdict::Ok);
        // 5 executed: the 6th is blocked with a tool error, turn continues.
        let (msg, stop) = g.block("s", "bash", &args).unwrap();
        assert!(msg.starts_with("blocked: identical call repeated 6 times"));
        assert!(stop.is_none());
        for _ in 0..3 {
            assert!(g.block("s", "bash", &args).unwrap().1.is_none());
        }
        // 5 executed + 5 blocked = 10: runaway, end the turn.
        assert!(g.block("s", "bash", &args).unwrap().1.is_some());
        // A different call is never blocked.
        assert!(g.block("s", "edit", &json!({"a": 1})).is_none());
    }

    #[test]
    fn read_only_repeats_and_polls_are_never_blocked() {
        let mut g = RepeatGuard::default();
        for _ in 0..8 {
            g.observe("read", &json!({"path": "a"}), "same", false);
            assert!(g.block("s", "read", &json!({"path": "a"})).is_none());
        }
        let ls = json!({"command": "git status --short"});
        for _ in 0..8 {
            g.observe("bash", &ls, "clean", false);
            assert!(g.block("s", "bash", &ls).is_none());
        }
    }

    #[test]
    fn success_of_edit_resets_failure_streak() {
        let mut g = RepeatGuard::default();
        let t = json!({"command": "pytest"});
        for _ in 0..4 {
            g.observe("bash", &t, "FAILED", true);
        }
        g.observe("edit", &json!({"file_path": "a"}), "ok", false);
        g.observe("bash", &t, "FAILED", true);
        assert!(g.block("s", "bash", &t).is_none());
    }

    #[test]
    fn different_result_or_call_resets_streak() {
        let mut g = RepeatGuard::default();
        let args = json!({"c": 1});
        g.observe("bash", &args, "a", false);
        g.observe("bash", &args, "a", false);
        assert_eq!(g.observe("bash", &args, "b", false), GuardVerdict::Ok);
        g.observe("bash", &args, "b", false);
        assert_eq!(g.observe("read", &args, "b", false), GuardVerdict::Ok);
    }

    #[test]
    fn same_failing_result_with_varied_args_counts() {
        let mut g = RepeatGuard::default();
        for i in 0..2 {
            assert_eq!(g.observe("edit", &json!({"n": i}), "Error: no match", true), GuardVerdict::Ok);
        }
        assert!(matches!(
            g.observe("edit", &json!({"n": 9}), "Error: no match", true),
            GuardVerdict::Warn { .. }
        ));
    }

    #[test]
    fn pollers_are_exempt() {
        let mut g = RepeatGuard::default();
        for _ in 0..9 {
            assert_eq!(g.observe("bg", &json!({}), "running", false), GuardVerdict::Ok);
        }
    }
}
