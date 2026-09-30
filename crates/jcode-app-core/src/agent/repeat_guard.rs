//! Repeated tool-call guard for the turn loops.
//!
//! Thresholds follow Hermes' `agent/tool_guardrails.py`: the identical-call
//! notice fires at `STALL_GUARD_IDENTICAL_CALL_THRESHOLD = 3` consecutive
//! identical (tool, args, result) calls, and the streak halt at 5
//! (`no_progress_block_after`, `exact_failure_block_after`). The wording is
//! adapted from `_IDENTICAL_CALL_NOTICE` and `identical_call_streak_halt`.
//! A second streak tracks the same failing result from one tool even when the
//! arguments vary slightly. Pollers (`bg`, `*_poll`) are exempt, as Hermes'
//! `is_stall_guard_repeatable`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub(super) const WARN_AFTER: u32 = 3;
pub(super) const STOP_AFTER: u32 = 5;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum GuardVerdict {
    Ok,
    /// Nth consecutive repeat; inject a reminder before the next request.
    Warn { tool: String, count: u32 },
    /// End the turn.
    Stop { tool: String, count: u32 },
}

#[derive(Default)]
pub(super) struct RepeatGuard {
    call_sig: u64,
    call_count: u32,
    fail_sig: u64,
    fail_count: u32,
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

impl RepeatGuard {
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
        let sig = hash_of(&[tool, &input.to_string(), result]);
        if sig == self.call_sig && self.call_count > 0 {
            self.call_count += 1;
        } else {
            self.call_sig = sig;
            self.call_count = 1;
        }
        if is_error {
            let fsig = hash_of(&[tool, result]);
            if fsig == self.fail_sig && self.fail_count > 0 {
                self.fail_count += 1;
            } else {
                self.fail_sig = fsig;
                self.fail_count = 1;
            }
        } else {
            self.fail_count = 0;
        }
        let count = self.call_count.max(self.fail_count);
        if count >= STOP_AFTER {
            GuardVerdict::Stop { tool: tool.to_string(), count }
        } else if count == WARN_AFTER {
            let reminder = format!(
                "<system-reminder>You are repeating yourself: {tool} has now been called {count} times in a row with the same outcome{}. Do not repeat it. Change the arguments, use a different tool, or proceed with what you already have. The turn will be stopped if this continues.</system-reminder>",
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
    fn identical_calls_warn_at_three_and_stop_at_five() {
        let mut g = RepeatGuard::default();
        let args = json!({"path": "a"});
        assert_eq!(g.observe("read", &args, "x", false), GuardVerdict::Ok);
        assert_eq!(g.observe("read", &args, "x", false), GuardVerdict::Ok);
        assert!(matches!(g.observe("read", &args, "x", false), GuardVerdict::Warn { count: 3, .. }));
        assert!(g.take_reminder().unwrap().contains("repeating yourself"));
        assert!(g.take_reminder().is_none());
        assert_eq!(g.observe("read", &args, "x", false), GuardVerdict::Ok);
        assert!(matches!(g.observe("read", &args, "x", false), GuardVerdict::Stop { count: 5, .. }));
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
