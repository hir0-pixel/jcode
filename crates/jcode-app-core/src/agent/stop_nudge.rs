//! Stop nudges when a turn ends with no tool calls.
//!
//! Ported from Hermes `agent/turn_stop_gates.py` + `verification_stop.py`
//! (nudge after code edits unless tests ran) and its tool-use-enforcement path
//! (a text-only turn that announces an action). The test-command detection is
//! the idea of `coding_context.py`. At most two nudges per turn (the second only after tool calls in between), and never in a
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
const ACTION_PHRASES: [&str; 17] = [
    "i will", "i'll", "let me", "now i'll", "now, i'll", "next i'll", "next, i'll", "first, i'll",
    "first i'll", "then i'll", "i am going to", "i'm going to", "i need to", "i should", "i can now",
    "plan:", "next steps:",
];

const VERIFY_NUDGE: &str = "<system-reminder>You edited files this turn but never ran the tests. Run the project's tests and read any failures before finishing (if pytest is not installed, use `python3 -m unittest`).</system-reminder>";
const ACTION_NUDGE: &str = "<system-reminder>Continue by calling the tool now, or state the final result.</system-reminder>";

type Runner<'a> = &'a dyn Fn(&str, &std::path::Path, std::time::Duration) -> sovereign_prime::agent_loop::GateResult;

#[derive(Default)]
pub(super) struct StopNudge {
    edited: bool,
    used_tools: bool,
    sent: u8,
    sent_seq: u32,
    pending: Option<String>,
    // Auto-verify gate state. `seq` orders edits against test runs.
    seq: u32,
    last_edit: u32,
    last_test: u32,
    last_test_exit: i64,
    tests_touched: bool,
    cwd: Option<std::path::PathBuf>,
    gate_ok: bool,
    baseline: Vec<String>,
    rounds: u32,
    last_fail: Option<u64>,
    gate_ran: bool,
    passed: bool,
    last_fail_stop: bool,
    no_marker_logged: bool,
}

fn enabled() -> bool {
    std::env::var("JCODE_VERIFY_ON_STOP").map_or(true, |v| v != "0")
        && crate::config::config().agents.verify_on_stop
}

fn auto_verify_enabled() -> bool {
    std::env::var("JCODE_AUTO_VERIFY").map_or(true, |v| v != "0")
        && crate::config::config().agents.auto_verify
}

fn looks_like_test_run(command: &str) -> bool {
    let c = command.to_ascii_lowercase();
    TEST_MARKERS.iter().any(|m| c.contains(m))
}

fn is_code_path(p: &str) -> bool {
    let p = p.trim().trim_matches(['"', '\'']);
    p.rsplit_once('.')
        .is_some_and(|(_, e)| CODE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Paths an edit-tool call touched; `None` when the call names no readable
/// path (apply_patch bodies are scanned for source-file paths instead).
fn edit_paths(input: &serde_json::Value) -> Vec<String> {
    for k in ["file_path", "path", "filePath"] {
        if let Some(p) = input[k].as_str() {
            return vec![p.to_string()];
        }
    }
    input["patch_text"]
        .as_str()
        .or_else(|| input["patch"].as_str())
        .or_else(|| input["input"].as_str())
        .map(|t| {
            t.lines()
                .filter(|l| l.starts_with("***") || l.starts_with("+++"))
                .filter_map(|l| l.split_whitespace().last().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether an edit-tool call touched a source file. Calls with no readable
/// path are assumed to.
fn edits_code(input: &serde_json::Value) -> bool {
    let paths = edit_paths(input);
    if paths.is_empty() {
        return input["file_path"].is_null()
            && input["path"].is_null()
            && input["filePath"].is_null()
            && !(input["patch_text"].is_string() || input["patch"].is_string() || input["input"].is_string());
    }
    paths.iter().any(|p| is_code_path(p))
}

/// Hash of a test run's output with timing tokens (`0.02s`, `(0.00s)`, `1.5`) dropped,
/// so the same failure compares equal across runs ("1 failed in 0.02s" vs "0.03s").
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for line in s.lines() {
        for tok in line.split_whitespace() {
            let t = tok.trim_matches(|c: char| "()[],".contains(c));
            let t = t.strip_suffix("ms").or_else(|| t.strip_suffix('s')).unwrap_or(t);
            if t.contains('.') && t.parse::<f64>().is_ok() {
                continue;
            }
            tok.hash(&mut h);
        }
        '\n'.hash(&mut h);
    }
    h.finish()
}

impl super::Agent {
    /// Per-turn nudge/gate state. The gate is off for subagents, goal and
    /// autonomous sessions (their own gates), past a hard deadline, and when
    /// the cwd is missing, `/`, or a home directory.
    pub(super) fn new_stop_nudge(&self) -> StopNudge {
        let mut n = StopNudge::default();
        let cwd = self.session.working_dir.as_deref().map(std::path::PathBuf::from);
        let is_home = |d: &std::path::Path| {
            d == std::path::Path::new("/")
                || std::env::var_os("HOME").is_some_and(|h| d == std::path::Path::new(&h))
                || jcode_base::storage::jcode_dir().is_ok_and(|h| d == h)
        };
        let past_deadline = std::env::var("JCODE_HARD_DEADLINE_UNIX")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|d| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .is_ok_and(|t| t.as_secs() >= d)
            });
        n.gate_ok = auto_verify_enabled()
            && self.session.parent_id.is_none()
            && !past_deadline
            && cwd.as_deref().is_some_and(|d| d.is_dir() && !is_home(d))
            && !jcode_base::storage::jcode_dir()
                .ok()
                .and_then(|h| sovereign_prime::agent_loop::ControlStore::open_cached(&h).ok())
                .is_some_and(|st| {
                    use sovereign_prime::agent_loop::{AutonomousStatus, GoalStatus};
                    let id = &self.session.id;
                    st.get_goal(id).ok().flatten().is_some_and(|g| g.status == GoalStatus::Active)
                        || st.get_autonomous(id).ok().flatten().is_some_and(|a| a.status == AutonomousStatus::Active)
                });
        if n.gate_ok {
            n.baseline = super::auto_verify::git_changed(cwd.as_deref().unwrap());
            n.cwd = cwd;
        }
        n
    }
}

impl StopNudge {
    /// Record a finished tool call. `exit_code` is the tool's exit status
    /// (see `auto_verify::exit_code_of`).
    pub(super) fn observe(&mut self, tool: &str, input: &serde_json::Value, exit_code: i64) {
        self.used_tools = true;
        self.seq += 1;
        if exit_code == 0 && EDIT_TOOLS.contains(&tool) {
            if edits_code(input) {
                self.edited = true;
                self.last_edit = self.seq;
                self.passed = false;
            }
            if edit_paths(input).iter().any(|p| super::auto_verify::is_test_path(p)) {
                self.tests_touched = true;
            }
        }
        if tool == "bash" && input["command"].as_str().is_some_and(looks_like_test_run) {
            self.last_test = self.seq;
            self.last_test_exit = exit_code;
        }
    }

    /// Fallback for edits made through a non-edit tool (python, shell):
    /// code files that became changed in git since the turn began.
    fn git_edited(&mut self) -> bool {
        let Some(dir) = self.cwd.as_deref().filter(|_| self.used_tools) else { return false };
        let changed: Vec<String> = super::auto_verify::git_changed(dir)
            .into_iter()
            .filter(|p| !self.baseline.contains(p))
            .collect();
        if changed.iter().any(|p| super::auto_verify::is_test_path(p)) {
            self.tests_touched = true;
        }
        changed.iter().any(|p| is_code_path(p))
    }

    fn guard_span(&self, session_id: &str, reason: &str, cmd: &str, ms: u64) -> jcode_base::obs_sink::Span {
        jcode_base::obs_sink::Span::new("loop.guard")
            .session(session_id)
            .attr("reason", reason)
            .attr("round", self.rounds)
            .attr("command", cmd)
            .attr("tests_touched", self.tests_touched)
            .took_ms(ms)
    }

    /// Run the project's tests once more when the turn ends after edits.
    /// `None`: the gate does not apply (caller falls back to the nudge).
    /// `Some(true)`: failure feedback queued, continue the turn.
    /// `Some(false)`: end the turn (passed, or stopped early).
    fn auto_verify(&mut self, session_id: &str, run: Runner) -> Option<bool> {
        if !self.gate_ok {
            return None;
        }
        let tested_ok = self.last_test > self.last_edit && self.last_test_exit == 0;
        if self.edited {
            if tested_ok {
                return Some(false);
            }
        } else if self.passed || !self.git_edited() {
            return None;
        }
        let dir = self.cwd.clone()?;
        let Some((name, cmd)) = super::auto_verify::detect_test_command(&dir) else {
            if !self.no_marker_logged {
                self.no_marker_logged = true;
                jcode_base::obs_sink::emit(self.guard_span(session_id, "auto_verify_skip_no_marker", "", 0));
            }
            return None;
        };
        self.gate_ran = true;
        let cfg = &crate::config::config().agents;
        if self.rounds >= cfg.auto_verify_rounds.max(1) || self.last_fail_stop {
            return Some(false);
        }
        self.rounds += 1;
        let started = std::time::Instant::now();
        let r = run(&cmd, &dir, std::time::Duration::from_secs(cfg.auto_verify_timeout_s.max(1)));
        let ms = started.elapsed().as_millis() as u64;
        let reason = match (r.passed, r.exit_code) {
            (true, _) => "auto_verify_pass",
            (_, 124) => "auto_verify_timeout",
            _ => "auto_verify_fail",
        };
        jcode_base::obs_sink::emit(self.guard_span(session_id, reason, name, ms));
        if r.passed {
            // The pass is now the latest test run; later edits re-arm the gate.
            self.seq += 1;
            self.last_test = self.seq;
            self.last_test_exit = 0;
            self.passed = true;
            return Some(false);
        }
        let h = hash_str(&r.output);
        if self.last_fail.replace(h) == Some(h) {
            self.last_fail_stop = true;
            return Some(false);
        }
        self.pending = Some(format!(
            "<system-reminder>Auto-verify: `{cmd}` failed (exit {}, round {}/{}). Last output:\n{}\nFix the failures, then finish.</system-reminder>",
            r.exit_code, self.rounds, cfg.auto_verify_rounds.max(1), r.output
        ));
        Some(true)
    }

    /// Called when the model ended its turn with no tool calls. Returns true
    /// when a nudge or gate feedback is queued and the loop should continue.
    pub(super) fn on_text_only_stop(&mut self, session_id: &str, text: &str) -> bool {
        self.on_text_only_stop_with(session_id, text, &real_runner)
    }

    fn on_text_only_stop_with(&mut self, session_id: &str, text: &str, run: Runner) -> bool {
        if self.auto_verify(session_id, run) == Some(true) {
            return true;
        }
        // Max 2 nudges per turn; the second only after tool calls made progress.
        if self.sent >= 2 || (self.sent == 1 && self.seq == self.sent_seq) || !enabled() {
            return false;
        }
        let lower = text.to_ascii_lowercase();
        let (nudge, reason) = if self.edited && self.last_test <= self.last_edit && !self.gate_ran {
            (VERIFY_NUDGE, "verify_nudge")
        } else if self.used_tools && announces_action(&lower) {
            (ACTION_NUDGE, "action_nudge")
        } else {
            return false;
        };
        self.sent += 1;
        self.sent_seq = self.seq;
        self.pending = Some(nudge.to_string());
        jcode_base::obs_sink::emit(
            jcode_base::obs_sink::Span::new("loop.guard")
                .session(session_id)
                .attr("reason", reason)
                .attr("tests_touched", self.tests_touched),
        );
        true
    }

    pub(super) fn take_pending(&mut self) -> Option<String> {
        self.pending.take()
    }
}

fn real_runner(cmd: &str, dir: &std::path::Path, t: std::time::Duration) -> sovereign_prime::agent_loop::GateResult {
    let run = || sovereign_prime::agent_loop::run_command(cmd, Some(dir), t);
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(run),
        _ => run(),
    }
}

/// The reply ends on a promise to act: its last sentence, or the lead-in line
/// of its last paragraph (a numbered plan), carries the phrase.
fn announces_action(lower: &str) -> bool {
    let body = lower.trim_end();
    let para = body.rsplit("\n\n").next().unwrap_or(body);
    if para.trim_end().ends_with('?') || para.contains("summary") {
        return false;
    }
    let last = body.rsplit(['.', '!', '?', '\n']).find(|s| !s.trim().is_empty()).unwrap_or("").trim();
    let lead = para.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let promises = |s: &str| !s.starts_with("let me know") && ACTION_PHRASES.iter().any(|p| s.starts_with(p));
    promises(last) || promises(lead) || body.ends_with(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn failure_hash_ignores_timings() {
        assert_eq!(hash_str("1 failed in 0.02s\nok (0.00s)"), hash_str("1 failed in 0.03s\nok (0.01s)"));
        assert_ne!(hash_str("1 failed in 0.02s"), hash_str("2 failed in 0.02s"));
    }

    #[test]
    fn verify_nudge_once_after_edit_without_tests() {
        let mut n = StopNudge::default();
        n.observe("edit", &json!({}), 0);
        assert!(n.on_text_only_stop("s", "Done."));
        assert_eq!(n.take_pending(), Some(VERIFY_NUDGE.to_string()));
        assert!(!n.on_text_only_stop("s", "Done."));
    }

    #[test]
    fn no_nudge_when_tests_ran_or_no_edits() {
        let mut n = StopNudge::default();
        n.observe("write", &json!({}), 0);
        n.observe("bash", &json!({"command": "cd x && cargo test -p foo"}), 0);
        assert!(!n.on_text_only_stop("s", "Done."));
        let mut n = StopNudge::default();
        n.observe("read", &json!({}), 0);
        assert!(!n.on_text_only_stop("s", "All good."));
        let mut n = StopNudge::default();
        n.observe("edit", &json!({}), 1);
        assert!(!n.on_text_only_stop("s", "failed."));
    }

    #[test]
    fn action_nudge_needs_prior_tools_and_trailing_promise() {
        let mut n = StopNudge::default();
        assert!(!n.on_text_only_stop("s", "Now I'll fix it."));
        n.observe("read", &json!({}), 0);
        assert!(n.on_text_only_stop("s", "Found the bug. Now I'll fix it."));
        assert_eq!(n.take_pending(), Some(ACTION_NUDGE.to_string()));
    }

    #[test]
    fn plan_only_endings_are_caught() {
        for t in [
            "I'll implement the parser, then run the tests.",
            "Next, I'll fix the bug and run the tests",
            "I need to update the config.",
            "Plan:\n1. edit a.py\n2. run the tests",
            "I'll do this:\n1. edit a.py\n2. run the tests.",
            "Found it. First, I'll patch the loader.",
        ] {
            assert!(announces_action(&t.to_ascii_lowercase()), "{t}");
        }
        for t in [
            "Let me know if you want more.",
            "Fixed and tested. Summary: all good.",
            "Should I continue?",
            "I'll implement it.\n\nDone. Anything else?",
        ] {
            assert!(!announces_action(&t.to_ascii_lowercase()), "{t}");
        }
    }

    #[test]
    fn second_nudge_only_after_progress_max_two() {
        let mut n = StopNudge::default();
        n.observe("read", &json!({}), 0);
        assert!(n.on_text_only_stop("s", "I'll fix it."));
        assert!(!n.on_text_only_stop("s", "I'll fix it."), "no progress");
        n.observe("read", &json!({}), 0);
        assert!(n.on_text_only_stop("s", "Next, I'll fix it."));
        n.observe("read", &json!({}), 0);
        assert!(!n.on_text_only_stop("s", "I'll fix it."), "max 2");
    }

    #[test]
    fn let_me_know_is_not_a_promise() {
        let mut n = StopNudge::default();
        n.observe("read", &json!({}), 0);
        assert!(!n.on_text_only_stop("s", "All fixed. Let me know if you need anything else."));
        assert!(!n.on_text_only_stop("s", "I think it works, so I'll stop here."));
    }

    #[test]
    fn docs_edits_do_not_need_tests_and_new_runners_count() {
        let mut n = StopNudge::default();
        n.observe("edit", &json!({"file_path": "README.md"}), 0);
        n.observe("write", &json!({"file_path": "a.json"}), 0);
        assert!(!n.on_text_only_stop("s", "Done."));
        let mut n = StopNudge::default();
        n.observe("edit", &json!({"file_path": "src/a.ts"}), 0);
        n.observe("bash", &json!({"command": "bun test"}), 0);
        assert!(!n.on_text_only_stop("s", "Done."));
    }

    use sovereign_prime::agent_loop::GateResult;
    use std::cell::RefCell;

    fn gate_nudge(files: &[(&str, &str)]) -> (StopNudge, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("sn-{}-{}", std::process::id(), std::thread::current().name().unwrap_or("t").replace("::", "_")));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        for (f, c) in files {
            std::fs::write(d.join(f), c).unwrap();
        }
        let mut n = StopNudge::default();
        n.gate_ok = true;
        n.cwd = Some(d.clone());
        (n, d)
    }

    fn fail(out: &str) -> GateResult {
        GateResult { passed: false, exit_code: 1, output: out.into() }
    }
    const OK: fn() -> GateResult = || GateResult { passed: true, exit_code: 0, output: String::new() };

    #[test]
    fn gate_feeds_back_failure_then_ends_on_pass() {
        let (mut n, _d) = gate_nudge(&[("test_a.py", "")]);
        n.observe("edit", &json!({"file_path": "a.py"}), 0);
        let results = RefCell::new(vec![OK(), fail("FAIL: test_x")]);
        let run = |_: &str, _: &std::path::Path, _: std::time::Duration| results.borrow_mut().pop().unwrap();
        assert!(n.on_text_only_stop_with("s", "Done.", &run));
        let msg = n.take_pending().unwrap();
        assert!(msg.contains("FAIL: test_x") && msg.contains("round 1/3"));
        n.observe("edit", &json!({"file_path": "a.py"}), 0);
        assert!(!n.on_text_only_stop_with("s", "Fixed.", &run));
        assert!(n.take_pending().is_none(), "gate replaces the verify nudge");
        assert!(!n.on_text_only_stop_with("s", "Fixed.", &run));
        assert!(results.borrow().is_empty());
    }

    #[test]
    fn gate_stops_on_identical_failure_and_after_max_rounds() {
        let (mut n, _d) = gate_nudge(&[("Cargo.toml", "")]);
        n.observe("edit", &json!({"file_path": "a.rs"}), 0);
        let calls = RefCell::new(0);
        let run = |_: &str, _: &std::path::Path, _: std::time::Duration| {
            *calls.borrow_mut() += 1;
            fail("same")
        };
        assert!(n.on_text_only_stop_with("s", "x", &run));
        assert!(n.on_text_only_stop_with("s", "x", &run) == false);
        assert_eq!(*calls.borrow(), 2);
        assert!(!n.on_text_only_stop_with("s", "x", &run));
        assert_eq!(*calls.borrow(), 2, "no more runs after the identical-failure stop");

        let (mut n, _d) = gate_nudge(&[("Cargo.toml", "")]);
        n.observe("edit", &json!({"file_path": "a.rs"}), 0);
        let k = RefCell::new(0);
        let run = |_: &str, _: &std::path::Path, _: std::time::Duration| {
            *k.borrow_mut() += 1;
            fail(&format!("different {}", k.borrow()))
        };
        for _ in 0..3 {
            assert!(n.on_text_only_stop_with("s", "x", &run));
        }
        assert!(!n.on_text_only_stop_with("s", "x", &run));
        assert_eq!(*k.borrow(), 3);
    }

    #[test]
    fn no_marker_falls_back_to_nudge() {
        let (mut n, _d) = gate_nudge(&[("main.py", "")]);
        n.observe("edit", &json!({"file_path": "a.py"}), 0);
        let run = |_: &str, _: &std::path::Path, _: std::time::Duration| -> GateResult { panic!("ran") };
        assert!(n.on_text_only_stop_with("s", "Done.", &run));
        assert_eq!(n.take_pending(), Some(VERIFY_NUDGE.to_string()));
    }

    #[test]
    fn exit_code_aware_skip_and_edit_resets_tested() {
        let (mut n, _d) = gate_nudge(&[("Cargo.toml", "")]);
        let calls = RefCell::new(0);
        let run = |_: &str, _: &std::path::Path, _: std::time::Duration| {
            *calls.borrow_mut() += 1;
            OK()
        };
        n.observe("edit", &json!({"file_path": "a.rs"}), 0);
        n.observe("bash", &json!({"command": "cargo test"}), 0);
        assert!(!n.on_text_only_stop_with("s", "Done.", &run));
        assert_eq!(*calls.borrow(), 0, "green test after last edit: skip");
        // A later edit makes the earlier run stale.
        n.observe("edit", &json!({"file_path": "a.rs"}), 0);
        n.on_text_only_stop_with("s", "Done.", &run);
        assert_eq!(*calls.borrow(), 1);
        // A failing test run after the edit does not skip the gate.
        let (mut n, _d) = gate_nudge(&[("Cargo.toml", "")]);
        n.observe("edit", &json!({"file_path": "a.rs"}), 0);
        n.observe("bash", &json!({"command": "cargo test"}), 101);
        n.on_text_only_stop_with("s", "Done.", &run);
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn tests_touched_flag() {
        let mut n = StopNudge::default();
        n.observe("edit", &json!({"file_path": "src/a.rs"}), 0);
        assert!(!n.tests_touched);
        n.observe("edit", &json!({"file_path": "tests/a.rs"}), 0);
        assert!(n.tests_touched);
    }
}
