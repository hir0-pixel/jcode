//! Stop nudges when a turn ends with no tool calls.
//!
//! Ported from Hermes `agent/turn_stop_gates.py` + `verification_stop.py`
//! (nudge after code edits unless tests ran) and its tool-use-enforcement path
//! (a text-only turn that announces an action). The test-command detection is
//! the idea of `coding_context.py`. At most one nudge per turn, and never in a
//! turn without edits (verify) or without prior tool use (action).

const EDIT_TOOLS: [&str; 4] = ["write", "edit", "apply_patch", "replace"];
const TEST_MARKERS: [&str; 23] = [
    "jest", "vitest", "bun test", "deno test", "node --test", "swift test", "dotnet test",
    "gradle test", "tox",
    "pytest", "unittest", "cargo test", "cargo nextest", "go test", "npm test", "npm run test",
    "yarn test", "pnpm test", "make test", "gradlew test", "ctest", "mvn test", "rspec",
];
const CODE_EXTS: [&str; 26] = [
    "rs", "py", "js", "jsx", "ts", "tsx", "mjs", "cjs", "go", "java", "kt", "swift", "c", "h",
    "cc", "cpp", "hpp", "cs", "rb", "php", "scala", "sh", "lua", "dart", "zig", "ex",
];
const ACTION_PHRASES: [&str; 6] = ["i will", "i'll", "let me", "now i'll", "i am going to", "i'm going to"];

const VERIFY_NUDGE: &str = "<system-reminder>You edited files this turn but never ran the tests. Run the project's tests and read any failures before finishing.</system-reminder>";
const ACTION_NUDGE: &str = "<system-reminder>Continue by calling the tool now, or state the final result.</system-reminder>";

#[derive(Default)]
pub(super) struct StopNudge {
    edited: bool,
    tested: bool,
    used_tools: bool,
    sent: bool,
    pending: Option<&'static str>,
}

fn enabled() -> bool {
    std::env::var("JCODE_VERIFY_ON_STOP").map_or(true, |v| v != "0")
        && crate::config::config().agents.verify_on_stop
}

fn looks_like_test_run(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    TEST_MARKERS.iter().any(|m| c.contains(m))
}

/// Whether an edit-tool call touched a source file. Calls with no readable
/// path (apply_patch bodies etc.) are scanned for source-file paths.
fn edits_code(input: &serde_json::Value) -> bool {
    let is_code = |p: &str| {
        let p = p.trim().trim_matches(['"', '\'']);
        p.rsplit_once('.')
            .is_some_and(|(_, e)| CODE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
    };
    for k in ["file_path", "path", "filePath"] {
        if let Some(p) = input[k].as_str() {
            return is_code(p);
        }
    }
    match input["patch_text"].as_str().or_else(|| input["patch"].as_str()).or_else(|| input["input"].as_str()) {
        Some(t) => t.lines().filter(|l| l.starts_with("***") || l.starts_with("+++")).any(|l| {
            l.split_whitespace().last().is_some_and(is_code)
        }),
        None => true,
    }
}

impl StopNudge {
    /// Record a finished tool call.
    pub(super) fn observe(&mut self, tool: &str, input: &serde_json::Value, is_error: bool) {
        self.used_tools = true;
        if !is_error && EDIT_TOOLS.contains(&tool) && edits_code(input) {
            self.edited = true;
        }
        if tool == "bash" && input["command"].as_str().is_some_and(looks_like_test_run) {
            self.tested = true;
        }
    }

    /// Called when the model ended its turn with no tool calls. Returns true
    /// when a nudge is queued and the loop should continue.
    pub(super) fn on_text_only_stop(&mut self, session_id: &str, text: &str) -> bool {
        if self.sent || !enabled() {
            return false;
        }
        let lower = text.to_ascii_lowercase();
        let (nudge, reason) = if self.edited && !self.tested {
            (VERIFY_NUDGE, "verify_nudge")
        } else if self.used_tools && announces_action(&lower) {
            (ACTION_NUDGE, "action_nudge")
        } else {
            return false;
        };
        self.sent = true;
        self.pending = Some(nudge);
        jcode_base::obs_sink::emit(
            jcode_base::obs_sink::Span::new("loop.guard")
                .session(session_id)
                .attr("reason", reason),
        );
        true
    }

    pub(super) fn take_pending(&mut self) -> Option<&'static str> {
        self.pending.take()
    }
}

/// The reply ends on a promise to act (its last sentence carries the phrase).
fn announces_action(lower: &str) -> bool {
    let last = lower
        .trim_end()
        .rsplit(['.', '!', '?', '\n'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or("");
    let last = last.trim();
    if last.starts_with("let me know") || lower.contains('?') || lower.contains("summary") {
        return false;
    }
    ACTION_PHRASES.iter().any(|p| last.starts_with(p)) || lower.trim_end().ends_with(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verify_nudge_once_after_edit_without_tests() {
        let mut n = StopNudge::default();
        n.observe("edit", &json!({}), false);
        assert!(n.on_text_only_stop("s", "Done."));
        assert_eq!(n.take_pending(), Some(VERIFY_NUDGE));
        assert!(!n.on_text_only_stop("s", "Done."));
    }

    #[test]
    fn no_nudge_when_tests_ran_or_no_edits() {
        let mut n = StopNudge::default();
        n.observe("write", &json!({}), false);
        n.observe("bash", &json!({"command": "cd x && cargo test -p foo"}), false);
        assert!(!n.on_text_only_stop("s", "Done."));
        let mut n = StopNudge::default();
        n.observe("read", &json!({}), false);
        assert!(!n.on_text_only_stop("s", "All good."));
        let mut n = StopNudge::default();
        n.observe("edit", &json!({}), true);
        assert!(!n.on_text_only_stop("s", "failed."));
    }

    #[test]
    fn action_nudge_needs_prior_tools_and_trailing_promise() {
        let mut n = StopNudge::default();
        assert!(!n.on_text_only_stop("s", "Now I'll fix it."));
        n.observe("read", &json!({}), false);
        assert!(n.on_text_only_stop("s", "Found the bug. Now I'll fix it."));
        assert_eq!(n.take_pending(), Some(ACTION_NUDGE));
    }

    #[test]
    fn let_me_know_is_not_a_promise() {
        let mut n = StopNudge::default();
        n.observe("read", &json!({}), false);
        assert!(!n.on_text_only_stop("s", "All fixed. Let me know if you need anything else."));
        assert!(!n.on_text_only_stop("s", "I think it works, so I'll stop here."));
    }

    #[test]
    fn docs_edits_do_not_need_tests_and_new_runners_count() {
        let mut n = StopNudge::default();
        n.observe("edit", &json!({"file_path": "README.md"}), false);
        n.observe("write", &json!({"file_path": "a.json"}), false);
        assert!(!n.on_text_only_stop("s", "Done."));
        let mut n = StopNudge::default();
        n.observe("edit", &json!({"file_path": "src/a.ts"}), false);
        n.observe("bash", &json!({"command": "bun test"}), false);
        assert!(!n.on_text_only_stop("s", "Done."));
    }
}
