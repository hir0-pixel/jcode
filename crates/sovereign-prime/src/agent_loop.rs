//! Session goals, autonomous mode, and heartbeats (Prime long-running agents
//! semantics, Hermes `session.control` wire shape).
//!
//! Persisted in `sovereign.db` so state survives engine restart and desktop
//! disconnect/reattach. The desktop reads `session.control.read`; autonomous
//! mode is projected into the `loop` field (`mode=self_paced`).

use crate::goal_ratchet::{Checkpoint, Score, command_key, score_from_json, score_of, score_to_json};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SCHEMA: &str = "
    PRAGMA journal_mode=WAL;
    PRAGMA busy_timeout=5000;
    CREATE TABLE IF NOT EXISTS session_goals(
        session_id TEXT PRIMARY KEY,
        state TEXT NOT NULL,
        updated_at_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS session_autonomous(
        session_id TEXT PRIMARY KEY,
        state TEXT NOT NULL,
        updated_at_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS session_heartbeats(
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        source TEXT NOT NULL DEFAULT 'user',
        state TEXT NOT NULL,
        updated_at_ms INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS session_heartbeats_session ON session_heartbeats(session_id, updated_at_ms);
";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_secs_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Empty completion-contract fields the desktop parser requires.
fn empty_contract() -> Value {
    json!({
        "outcome": "",
        "verification": "",
        "constraints": "",
        "boundaries": "",
        "stop_when": ""
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct QualityGate {
    pub command: String,
    pub timeout_seconds: i64,
    pub max_retries: i64,
    pub attempts: i64,
    pub last_exit_code: Option<i64>,
}

impl QualityGate {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout_seconds: 120,
            max_retries: 3,
            attempts: 0,
            last_exit_code: None,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "command": self.command,
            "timeout_seconds": self.timeout_seconds,
            "max_retries": self.max_retries,
            "attempts": self.attempts,
            "last_exit_code": self.last_exit_code,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            command: v["command"].as_str()?.to_string(),
            timeout_seconds: v["timeout_seconds"].as_i64().unwrap_or(120),
            max_retries: v["max_retries"].as_i64().unwrap_or(3),
            attempts: v["attempts"].as_i64().unwrap_or(0),
            last_exit_code: v["last_exit_code"].as_i64(),
        })
    }
}

/// Run a quality gate. A passed gate means only that gate passed.
pub fn run_gate(gate: &mut QualityGate) -> GateResult {
    gate.attempts += 1;
    let timeout = Duration::from_secs(gate.timeout_seconds.max(1) as u64);
    let started = Instant::now();
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(&gate.command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            gate.last_exit_code = Some(127);
            return GateResult {
                passed: false,
                exit_code: 127,
                output: format!("failed to spawn gate: {err}"),
            };
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(1) as i64;
                gate.last_exit_code = Some(code);
                let stdout = {
                    let mut buf = String::new();
                    if let Some(mut out) = child.stdout.take() {
                        let _ = std::io::Read::read_to_string(&mut out, &mut buf);
                    }
                    if let Some(mut err) = child.stderr.take() {
                        let _ = std::io::Read::read_to_string(&mut err, &mut buf);
                    }
                    buf.chars()
                        .rev()
                        .take(3000)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect()
                };
                return GateResult {
                    passed: code == 0,
                    exit_code: code,
                    output: stdout,
                };
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                gate.last_exit_code = Some(124);
                return GateResult {
                    passed: false,
                    exit_code: 124,
                    output: format!("gate timed out after {}s", gate.timeout_seconds),
                };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => {
                gate.last_exit_code = Some(1);
                return GateResult {
                    passed: false,
                    exit_code: 1,
                    output: err.to_string(),
                };
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct GateResult {
    pub passed: bool,
    pub exit_code: i64,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GoalStatus {
    Active,
    Done,
    Paused,
}

impl GoalStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Done => "done",
            Self::Paused => "paused",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "done" => Some(Self::Done),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionGoal {
    pub title: String,
    pub status: GoalStatus,
    pub turns_used: i64,
    pub max_turns: i64,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub wall_budget_ms: Option<i64>,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub created_at_ms: i64,
    pub paused_reason: Option<String>,
    pub last_verdict: Option<String>,
    pub last_reason: Option<String>,
    pub subgoals: Vec<String>,
    pub gates: Vec<QualityGate>,
    pub contract: Value,
    /// Pause continuations while waiting on subagents.
    pub waiting_on_subagents: bool,
    /// Capped, one-line-per-turn attempt log: what was tried, whether
    /// verification passed, and the key error if it failed. Only ever
    /// injected into the per-turn continuation prompt (never the cached
    /// static prefix), and capped at `MAX_ATTEMPT_LOG` entries of at most
    /// `MAX_ATTEMPT_LEN` chars each so it stays a small, bounded addition.
    pub attempt_log: Vec<String>,
    /// The verification the model cited when it called `complete` (what it
    /// ran and the result). Required for completion; also surfaced in the
    /// budget-exhaustion report so a paused/expired goal still shows what
    /// was verified so far.
    pub completion_verification: Option<String>,
    /// AVO ratchet (see `goal_ratchet`): best score per test command,
    /// checkpoint lineage (cap 20), regression note, supervisor rate limits.
    pub best: Score,
    pub lineage: Vec<Checkpoint>,
    pub checkpoint_seq: u32,
    pub regressed: Option<String>,
    pub sup_turn: Option<i64>,
    pub sup_episode: bool,
}

/// Attempt-log bounds (NVIDIA AVO long-horizon harness: a small, bounded
/// per-turn note, not unbounded history).
pub const MAX_ATTEMPT_LOG: usize = 8;
pub const MAX_ATTEMPT_LEN: usize = 80;
/// Consecutive auto-recorded turns examined for plateau detection.
pub const PLATEAU_TURNS: usize = 3;

/// What the engine observed in one turn, accumulated from `tool.complete`
/// events (no model call, no prompt tokens).
#[derive(Default)]
struct TurnObs {
    tools: u32,
    failed: u32,
    files_changed: bool,
    verified: bool,
    first_err: Option<String>,
    scores: Score,
}

fn turn_obs() -> &'static Mutex<HashMap<String, TurnObs>> {
    static OBS: OnceLock<Mutex<HashMap<String, TurnObs>>> = OnceLock::new();
    OBS.get_or_init(Default::default)
}

/// Strip paths and digits so identical failures compare equal.
fn normalize_error(err: &str) -> String {
    let first = err.trim().trim_start_matches("Error:").trim().lines().next().unwrap_or("");
    let words: Vec<String> = first
        .split_whitespace()
        .map(|w| {
            if w.contains('/') || w.contains('\\') {
                "<p>".to_string()
            } else {
                w.chars().filter(|c| !c.is_ascii_digit()).collect()
            }
        })
        .collect();
    words.join(" ").to_lowercase().chars().take(30).collect()
}

/// Record one completed tool call for `session_id`'s current turn. Called by
/// the gateway at `tool.complete`; drained by `after_turn`.
pub fn observe_tool(session_id: &str, name: &str, args: &Value, result: &str) {
    // jcode's bash tool reports a non-zero exit as a trailing "Exit code: N"
    // (or "finished with exit code: N" for detached runs), not an "Error:" prefix.
    let nonzero_exit = result.lines().rev().find(|l| !l.trim().is_empty()).is_some_and(|l| {
        let l = l.trim().trim_end_matches(" ---");
        l.rsplit_once("xit code: ").is_some_and(|(_, code)| code.trim() != "0")
    });
    let failed = result.starts_with("Error:") || nonzero_exit;
    let cmd = args["command"].as_str().unwrap_or("").to_lowercase();
    let verify_cmd = name == "bash"
        && ["test", "build", "check", "lint", "clippy", "pytest", "tsc"].iter().any(|k| cmd.contains(k));
    let mut map = turn_obs().lock().unwrap();
    let obs = map.entry(session_id.to_string()).or_default();
    obs.tools += 1;
    if verify_cmd {
        obs.scores.insert(command_key(&cmd), score_of(!failed, result));
    }
    if failed {
        obs.failed += 1;
        obs.first_err.get_or_insert_with(|| normalize_error(result));
        return;
    }
    if matches!(name, "edit" | "write" | "patch" | "apply_patch" | "multiedit") {
        obs.files_changed = true;
    }
    if verify_cmd {
        obs.verified = true;
    }
}

impl SessionGoal {
    pub fn new(title: impl Into<String>) -> Self {
        let now = now_ms();
        Self {
            title: title.into(),
            status: GoalStatus::Active,
            turns_used: 0,
            max_turns: 20,
            token_budget: None,
            tokens_used: 0,
            wall_budget_ms: None,
            started_at_ms: now,
            updated_at_ms: now,
            created_at_ms: now,
            paused_reason: None,
            last_verdict: None,
            last_reason: None,
            subgoals: Vec::new(),
            gates: Vec::new(),
            contract: empty_contract(),
            waiting_on_subagents: false,
            attempt_log: Vec::new(),
            completion_verification: None,
            best: Score::new(),
            lineage: Vec::new(),
            checkpoint_seq: 0,
            regressed: None,
            sup_turn: None,
            sup_episode: false,
        }
    }

    /// Record one capped, one-line attempt-log entry for this turn. Never
    /// ends the goal on a failed tool call or failed verification — only
    /// completion, budget exhaustion, or a user cancel do that.
    pub fn record_attempt(&mut self, line: impl Into<String>) {
        let mut line = line.into();
        if line.chars().count() > MAX_ATTEMPT_LEN {
            line = line
                .chars()
                .take(MAX_ATTEMPT_LEN.saturating_sub(1))
                .collect::<String>();
            line.push('…');
        }
        self.attempt_log.push(line);
        if self.attempt_log.len() > MAX_ATTEMPT_LOG {
            let excess = self.attempt_log.len() - MAX_ATTEMPT_LOG;
            self.attempt_log.drain(0..excess);
        }
    }

    /// Cheap, no-model-call plateau heuristic. Over the last
    /// `PLATEAU_TURNS` engine-recorded `auto` lines: no successful
    /// verification AND (same normalized error signature OR no file
    /// changes). Also trips when the last 3 entries of any kind are
    /// identical (model-supplied notes that repeat).
    pub fn plateaued(&self) -> bool {
        let n = self.attempt_log.len();
        if n >= PLATEAU_TURNS {
            let last = &self.attempt_log[n - PLATEAU_TURNS..];
            if last.iter().all(|line| line == &last[0] && !line.starts_with("auto ")) {
                return true;
            }
        }
        let auto: Vec<&str> = self
            .attempt_log
            .iter()
            .filter(|l| l.starts_with("auto "))
            .map(String::as_str)
            .collect();
        if auto.len() < PLATEAU_TURNS {
            return false;
        }
        let last = &auto[auto.len() - PLATEAU_TURNS..];
        let field = |l: &str, key: &str| -> String {
            l.split(" | ")
                .find_map(|p| p.strip_prefix(key))
                .unwrap_or("")
                .to_string()
        };
        if last.iter().any(|l| field(l, "ver=") == "pass") {
            return false;
        }
        let err0 = field(last[0], "err=");
        let same_err = !err0.is_empty() && last.iter().all(|l| field(l, "err=") == err0);
        let no_files = last.iter().all(|l| field(l, "files=") == "no");
        same_err || no_files
    }

    pub fn out_of_budget(&self) -> Option<&'static str> {
        if self.max_turns > 0 && self.turns_used >= self.max_turns {
            return Some("turn budget");
        }
        if let Some(tokens) = self.token_budget {
            if tokens > 0 && self.tokens_used >= tokens {
                return Some("token budget");
            }
        }
        if let Some(wall) = self.wall_budget_ms {
            if wall > 0 && now_ms().saturating_sub(self.started_at_ms) >= wall {
                return Some("wall-clock budget");
            }
        }
        None
    }

    pub fn to_control_json(&self) -> Value {
        let mut v = json!({
            "title": self.title,
            "status": self.status.as_str(),
            "turns_used": self.turns_used,
            "max_turns": self.max_turns,
            "contract": self.contract,
            "subgoals": self.subgoals,
            "gates": self.gates.iter().map(QualityGate::to_json).collect::<Vec<_>>(),
            "created_at": self.created_at_ms as f64 / 1000.0,
            "updated_at": self.updated_at_ms as f64 / 1000.0,
        });
        if let Some(reason) = &self.paused_reason {
            v["paused_reason"] = json!(reason);
        }
        if let Some(verdict) = &self.last_verdict {
            v["last_verdict"] = json!(verdict);
        }
        if let Some(reason) = &self.last_reason {
            v["last_reason"] = json!(reason);
        }
        if !self.attempt_log.is_empty() {
            v["attempt_log"] = json!(self.attempt_log);
        }
        if let Some(verification) = &self.completion_verification {
            v["completion_verification"] = json!(verification);
        }
        v
    }

    fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "status": self.status.as_str(),
            "turns_used": self.turns_used,
            "max_turns": self.max_turns,
            "token_budget": self.token_budget,
            "tokens_used": self.tokens_used,
            "wall_budget_ms": self.wall_budget_ms,
            "started_at_ms": self.started_at_ms,
            "updated_at_ms": self.updated_at_ms,
            "created_at_ms": self.created_at_ms,
            "paused_reason": self.paused_reason,
            "last_verdict": self.last_verdict,
            "last_reason": self.last_reason,
            "subgoals": self.subgoals,
            "gates": self.gates.iter().map(QualityGate::to_json).collect::<Vec<_>>(),
            "contract": self.contract,
            "waiting_on_subagents": self.waiting_on_subagents,
            "attempt_log": self.attempt_log,
            "completion_verification": self.completion_verification,
            "best": score_to_json(&self.best),
            "lineage": self.lineage.iter().map(Checkpoint::to_json).collect::<Vec<_>>(),
            "checkpoint_seq": self.checkpoint_seq,
            "regressed": self.regressed,
            "sup_turn": self.sup_turn,
            "sup_episode": self.sup_episode,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            title: v["title"].as_str()?.to_string(),
            status: GoalStatus::parse(v["status"].as_str()?)?,
            turns_used: v["turns_used"].as_i64().unwrap_or(0),
            max_turns: v["max_turns"].as_i64().unwrap_or(20),
            token_budget: v["token_budget"].as_i64(),
            tokens_used: v["tokens_used"].as_i64().unwrap_or(0),
            wall_budget_ms: v["wall_budget_ms"].as_i64(),
            started_at_ms: v["started_at_ms"].as_i64().unwrap_or(0),
            updated_at_ms: v["updated_at_ms"].as_i64().unwrap_or(0),
            created_at_ms: v["created_at_ms"].as_i64().unwrap_or(0),
            paused_reason: v["paused_reason"].as_str().map(str::to_string),
            last_verdict: v["last_verdict"].as_str().map(str::to_string),
            last_reason: v["last_reason"].as_str().map(str::to_string),
            subgoals: v["subgoals"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            gates: v["gates"]
                .as_array()
                .map(|a| a.iter().filter_map(QualityGate::from_json).collect())
                .unwrap_or_default(),
            contract: if v["contract"].is_object() {
                v["contract"].clone()
            } else {
                empty_contract()
            },
            waiting_on_subagents: v["waiting_on_subagents"].as_bool().unwrap_or(false),
            attempt_log: v["attempt_log"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            completion_verification: v["completion_verification"]
                .as_str()
                .map(str::to_string),
            best: score_from_json(&v["best"]),
            lineage: v["lineage"].as_array().map(|a| a.iter().filter_map(Checkpoint::from_json).collect()).unwrap_or_default(),
            checkpoint_seq: v["checkpoint_seq"].as_u64().unwrap_or(0) as u32,
            regressed: v["regressed"].as_str().map(str::to_string),
            sup_turn: v["sup_turn"].as_i64(),
            sup_episode: v["sup_episode"].as_bool().unwrap_or(false),
        })
    }

    pub fn status_text(&self) -> String {
        match self.status {
            GoalStatus::Active => {
                if self.waiting_on_subagents {
                    format!(
                        "⏳ Goal (parked, {}/{} turns): {}",
                        self.turns_used, self.max_turns, self.title
                    )
                } else {
                    format!(
                        "⊙ Goal (active, {}/{} turns): {}",
                        self.turns_used, self.max_turns, self.title
                    )
                }
            }
            GoalStatus::Paused => {
                let detail = self.paused_reason.as_deref().unwrap_or("paused");
                format!(
                    "⏸ Goal paused — {}. Use /goal resume to keep going.",
                    detail
                )
            }
            GoalStatus::Done => format!(
                "✓ Goal done ({}/{} turns): {}",
                self.turns_used, self.max_turns, self.title
            ),
        }
    }

    pub fn continuation_prompt(&self) -> String {
        self.continuation_prompt_with(None)
    }

    /// `supervisor`: alternative strategies from the one rare supervisor call,
    /// injected once into this prompt (never the cached static prefix).
    pub fn continuation_prompt_with(&self, supervisor: Option<&str>) -> String {
        let mut log_block = String::new();
        if !self.attempt_log.is_empty() {
            log_block.push_str("\n\nRecent attempts (most recent last):\n");
            for line in &self.attempt_log {
                log_block.push_str("- ");
                log_block.push_str(line);
                log_block.push('\n');
            }
        }
        log_block.push_str(&self.ratchet_block());
        let steer = if let Some(alt) = supervisor {
            format!("\n[Supervisor: alternative directions]\n{}", alt.chars().take(600).collect::<String>())
        } else if self.plateaued() {
            "\n[Plateau detected] The last few turns made no new verified progress \
             (same result repeated). Abandon the current approach and try a genuinely \
             different strategy instead of repeating what already failed above.".to_string()
        } else {
            String::new()
        };
        format!(
            "[Continuing toward your standing goal]\nGoal: {}{log_block}{steer}\n\n\
             Continue working toward this goal. Take the next concrete step, then record it with \
             session_goal op=progress (note, verification, error). Verify by execution (run the \
             relevant tests/build/command) before claiming success. \
             Only call session_goal op=complete once you have actually run that verification, and \
             cite what you ran and its result. \
             If you are blocked and need input from the user, say so clearly and stop.",
            self.title
        )
    }
}

/// Record the (possibly failed) supervisor call for this plateau episode and
/// return the continuation prompt with its alternatives injected once.
pub fn apply_supervisor(store: &ControlStore, session_id: &str, alternatives: Option<&str>) -> Result<String> {
    let mut goal = store.get_goal(session_id)?.context("no goal")?;
    goal.sup_turn = Some(goal.turns_used);
    goal.sup_episode = true;
    store.set_goal(session_id, Some(&goal))?;
    Ok(goal.continuation_prompt_with(alternatives.filter(|a| !a.trim().is_empty())))
}

#[derive(Debug, Clone, PartialEq)]
pub enum AutonomousStatus {
    Active,
    Done,
    Paused,
}

impl AutonomousStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Done => "done",
            Self::Paused => "paused",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "done" => Some(Self::Done),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutonomousState {
    pub status: AutonomousStatus,
    pub prompt: String,
    pub max_continuations: i64,
    pub continuations_used: i64,
    pub max_turns: i64,
    pub turns_used: i64,
    pub max_tokens: Option<i64>,
    pub tokens_used: i64,
    pub timeout_ms: Option<i64>,
    pub started_at_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_fired_at_ms: i64,
    pub gates: Vec<QualityGate>,
    pub waiting_on_subagents: bool,
    pub paused_reason: Option<String>,
    pub last_stop_reason: Option<String>,
    /// True once every gate has passed; limit hits never set this.
    pub succeeded: bool,
}

impl AutonomousState {
    pub fn default_on() -> Self {
        let now = now_ms();
        Self {
            status: AutonomousStatus::Active,
            prompt: "Continue the current task.".to_string(),
            max_continuations: 3,
            continuations_used: 0,
            max_turns: 0,
            turns_used: 0,
            max_tokens: None,
            tokens_used: 0,
            timeout_ms: None,
            started_at_ms: now,
            created_at_ms: now,
            updated_at_ms: now,
            last_fired_at_ms: 0,
            gates: Vec::new(),
            waiting_on_subagents: false,
            paused_reason: None,
            last_stop_reason: None,
            succeeded: false,
        }
    }

    pub fn out_of_limit(&self) -> Option<&'static str> {
        if self.max_continuations > 0 && self.continuations_used >= self.max_continuations {
            return Some("continuation limit");
        }
        if self.max_turns > 0 && self.turns_used >= self.max_turns {
            return Some("turn limit");
        }
        if let Some(tokens) = self.max_tokens {
            if tokens > 0 && self.tokens_used >= tokens {
                return Some("token limit");
            }
        }
        if let Some(timeout) = self.timeout_ms {
            if timeout > 0 && now_ms().saturating_sub(self.started_at_ms) >= timeout {
                return Some("time limit");
            }
        }
        None
    }

    /// Project into Hermes `loop` snapshot (desktop SessionControlLoop).
    pub fn to_loop_json(&self) -> Value {
        let mut v = json!({
            "prompt": self.prompt,
            "status": self.status.as_str(),
            "mode": "self_paced",
            "interval_seconds": 0.0,
            "current_delay": 0.0,
            "times": 0,
            "until": "",
            "max_ticks": self.max_continuations,
            "ticks_fired": self.continuations_used,
            "created_at": self.created_at_ms as f64 / 1000.0,
            "last_fired_at": self.last_fired_at_ms as f64 / 1000.0,
            "next_due_at": if self.status == AutonomousStatus::Active { now_secs_f64() } else { 0.0 },
            "awaiting_response": false,
            "deferred_by_goal": false,
        });
        if let Some(reason) = &self.paused_reason {
            v["paused_reason"] = json!(reason);
        }
        if let Some(reason) = &self.last_stop_reason {
            v["last_stop_reason"] = json!(reason);
        }
        v
    }

    fn to_json(&self) -> Value {
        json!({
            "status": self.status.as_str(),
            "prompt": self.prompt,
            "max_continuations": self.max_continuations,
            "continuations_used": self.continuations_used,
            "max_turns": self.max_turns,
            "turns_used": self.turns_used,
            "max_tokens": self.max_tokens,
            "tokens_used": self.tokens_used,
            "timeout_ms": self.timeout_ms,
            "started_at_ms": self.started_at_ms,
            "created_at_ms": self.created_at_ms,
            "updated_at_ms": self.updated_at_ms,
            "last_fired_at_ms": self.last_fired_at_ms,
            "gates": self.gates.iter().map(QualityGate::to_json).collect::<Vec<_>>(),
            "waiting_on_subagents": self.waiting_on_subagents,
            "paused_reason": self.paused_reason,
            "last_stop_reason": self.last_stop_reason,
            "succeeded": self.succeeded,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            status: AutonomousStatus::parse(v["status"].as_str()?)?,
            prompt: v["prompt"]
                .as_str()
                .unwrap_or("Continue the current task.")
                .to_string(),
            max_continuations: v["max_continuations"].as_i64().unwrap_or(3),
            continuations_used: v["continuations_used"].as_i64().unwrap_or(0),
            max_turns: v["max_turns"].as_i64().unwrap_or(0),
            turns_used: v["turns_used"].as_i64().unwrap_or(0),
            max_tokens: v["max_tokens"].as_i64(),
            tokens_used: v["tokens_used"].as_i64().unwrap_or(0),
            timeout_ms: v["timeout_ms"].as_i64(),
            started_at_ms: v["started_at_ms"].as_i64().unwrap_or(0),
            created_at_ms: v["created_at_ms"].as_i64().unwrap_or(0),
            updated_at_ms: v["updated_at_ms"].as_i64().unwrap_or(0),
            last_fired_at_ms: v["last_fired_at_ms"].as_i64().unwrap_or(0),
            gates: v["gates"]
                .as_array()
                .map(|a| a.iter().filter_map(QualityGate::from_json).collect())
                .unwrap_or_default(),
            waiting_on_subagents: v["waiting_on_subagents"].as_bool().unwrap_or(false),
            paused_reason: v["paused_reason"].as_str().map(str::to_string),
            last_stop_reason: v["last_stop_reason"].as_str().map(str::to_string),
            succeeded: v["succeeded"].as_bool().unwrap_or(false),
        })
    }

    pub fn status_text(&self) -> String {
        let gates = if self.gates.is_empty() {
            "no gates".to_string()
        } else {
            format!("{} gate(s)", self.gates.len())
        };
        match self.status {
            AutonomousStatus::Active => format!(
                "Autonomous on ({}/{} continuations, {}, {}).",
                self.continuations_used,
                self.max_continuations,
                gates,
                if self.waiting_on_subagents {
                    "waiting on subagents"
                } else {
                    "running"
                }
            ),
            AutonomousStatus::Paused => format!(
                "Autonomous paused — {}.",
                self.paused_reason.as_deref().unwrap_or("paused")
            ),
            AutonomousStatus::Done if self.succeeded => {
                "Autonomous complete — quality gates passed.".to_string()
            }
            AutonomousStatus::Done => format!(
                "Autonomous stopped — {} (not success).",
                self.last_stop_reason.as_deref().unwrap_or("limit reached")
            ),
        }
    }

    pub fn continuation_prompt(&self) -> String {
        if !self.gates.is_empty() {
            let cmds: Vec<_> = self.gates.iter().map(|g| g.command.as_str()).collect();
            format!(
                "[Autonomous continuation #{}/{}]\nContinue the current task. Quality gates that must pass before finishing: {}.\n\
                 Do not claim success until every gate passes. If blocked, say so and stop.",
                self.continuations_used + 1,
                self.max_continuations,
                cmds.join("; ")
            )
        } else {
            format!(
                "[Autonomous continuation #{}/{}]\n{}\nIf the work is done, say so clearly.",
                self.continuations_used + 1,
                self.max_continuations,
                self.prompt
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HeartbeatStatus {
    Active,
    Paused,
}

impl HeartbeatStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Heartbeat {
    pub id: String,
    pub session_id: String,
    pub source: String,
    pub label: Option<String>,
    pub prompt: String,
    pub status: HeartbeatStatus,
    pub interval_seconds: i64,
    pub created_at_ms: i64,
    pub last_fired_at_ms: i64,
    pub fire_count: i64,
    /// "follow_up" (default; deliver only once the session is idle) or
    /// "steer" (RLM heartbeats only; deliver at the next turn boundary even
    /// while the session is busy, via a soft interrupt).
    pub delivery_mode: String,
}

impl Heartbeat {
    pub fn new(
        session_id: impl Into<String>,
        prompt: impl Into<String>,
        interval_seconds: i64,
    ) -> Self {
        let now = now_ms();
        let floor = std::env::var("SOVEREIGN_HEARTBEAT_MIN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60i64)
            .max(1);
        let interval = interval_seconds.max(floor);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            source: "user".to_string(),
            label: None,
            prompt: prompt.into(),
            status: HeartbeatStatus::Active,
            interval_seconds: interval,
            created_at_ms: now,
            // In test floors (<60s), make the first tick due immediately so e2e
            // can observe a fire without waiting a full minute.
            last_fired_at_ms: if floor < 60 {
                now - interval * 1000
            } else {
                now
            },
            fire_count: 0,
            delivery_mode: "follow_up".to_string(),
        }
    }

    pub fn is_due(&self, now: i64) -> bool {
        self.status == HeartbeatStatus::Active
            && now.saturating_sub(self.last_fired_at_ms)
                >= self.interval_seconds.saturating_mul(1000)
    }

    pub fn to_control_json(&self) -> Value {
        json!({
            "prompt": self.prompt,
            "label": self.label,
            "status": self.status.as_str(),
            "interval_seconds": self.interval_seconds,
            "created_at": self.created_at_ms as f64 / 1000.0,
            "last_fired_at": self.last_fired_at_ms as f64 / 1000.0,
            "fire_count": self.fire_count,
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "session_id": self.session_id,
            "source": self.source,
            "label": self.label,
            "prompt": self.prompt,
            "status": self.status.as_str(),
            "interval_seconds": self.interval_seconds,
            "created_at_ms": self.created_at_ms,
            "last_fired_at_ms": self.last_fired_at_ms,
            "fire_count": self.fire_count,
            "delivery_mode": self.delivery_mode,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v["id"].as_str()?.to_string(),
            session_id: v["session_id"].as_str()?.to_string(),
            source: v["source"].as_str().unwrap_or("user").to_string(),
            label: v["label"].as_str().map(str::to_string),
            prompt: v["prompt"].as_str()?.to_string(),
            status: HeartbeatStatus::parse(v["status"].as_str()?)?,
            interval_seconds: v["interval_seconds"].as_i64().unwrap_or(60),
            created_at_ms: v["created_at_ms"].as_i64().unwrap_or(0),
            last_fired_at_ms: v["last_fired_at_ms"].as_i64().unwrap_or(0),
            fire_count: v["fire_count"].as_i64().unwrap_or(0),
            delivery_mode: v["delivery_mode"]
                .as_str()
                .unwrap_or("follow_up")
                .to_string(),
        })
    }

    pub fn fire_prompt(&self) -> String {
        format!(
            "[/heartbeat]\nRecurring check: {}\n\nThis is an automatic heartbeat. Perform the check against current state and report briefly.",
            self.prompt
        )
    }
}

/// Parse `30s` / `5m` / `2h` / `1h30m` into seconds.
pub fn parse_duration_token(token: &str) -> Option<i64> {
    let re = regex_lite_duration(token)?;
    Some(re)
}

fn regex_lite_duration(token: &str) -> Option<i64> {
    let t = token.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let mut rest = t.as_str();
    let mut total = 0i64;
    let mut matched = false;
    while !rest.is_empty() {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let n: i64 = digits.parse().ok()?;
        rest = &rest[digits.len()..];
        let unit = rest.chars().next()?;
        rest = &rest[unit.len_utf8()..];
        matched = true;
        total += match unit {
            'h' => n * 3600,
            'm' => n * 60,
            's' => n,
            _ => return None,
        };
    }
    if matched && total > 0 {
        Some(total)
    } else {
        None
    }
}

pub struct ControlStore {
    conn: Mutex<Connection>,
}

impl ControlStore {
    pub fn open(home: &Path) -> Result<Self> {
        std::fs::create_dir_all(home).ok();
        let conn = Connection::open(home.join("sovereign.db")).context("opening sovereign.db")?;
        conn.execute_batch(SCHEMA)
            .context("migrating session control tables")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_cached(home: &Path) -> Result<Arc<Self>> {
        static STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<ControlStore>>>> = OnceLock::new();
        let mut map = STORES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(store) = map.get(home) {
            return Ok(store.clone());
        }
        let store = Arc::new(Self::open(home)?);
        map.insert(home.to_path_buf(), store.clone());
        Ok(store)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn get_goal(&self, session_id: &str) -> Result<Option<SessionGoal>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let state: Option<String> = conn
            .query_row(
                "SELECT state FROM session_goals WHERE session_id=?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(state
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| SessionGoal::from_json(&v)))
    }

    pub fn set_goal(&self, session_id: &str, goal: Option<&SessionGoal>) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        match goal {
            None => {
                conn.execute(
                    "DELETE FROM session_goals WHERE session_id=?1",
                    [session_id],
                )?;
            }
            Some(goal) => {
                conn.execute(
                    "INSERT INTO session_goals(session_id, state, updated_at_ms) VALUES (?1, ?2, ?3)
                     ON CONFLICT(session_id) DO UPDATE SET state=excluded.state, updated_at_ms=excluded.updated_at_ms",
                    params![session_id, goal.to_json().to_string(), goal.updated_at_ms],
                )?;
            }
        }
        Ok(())
    }

    pub fn get_autonomous(&self, session_id: &str) -> Result<Option<AutonomousState>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let state: Option<String> = conn
            .query_row(
                "SELECT state FROM session_autonomous WHERE session_id=?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(state
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| AutonomousState::from_json(&v)))
    }

    pub fn set_autonomous(&self, session_id: &str, state: Option<&AutonomousState>) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        match state {
            None => {
                conn.execute(
                    "DELETE FROM session_autonomous WHERE session_id=?1",
                    [session_id],
                )?;
            }
            Some(state) => {
                conn.execute(
                    "INSERT INTO session_autonomous(session_id, state, updated_at_ms) VALUES (?1, ?2, ?3)
                     ON CONFLICT(session_id) DO UPDATE SET state=excluded.state, updated_at_ms=excluded.updated_at_ms",
                    params![session_id, state.to_json().to_string(), state.updated_at_ms],
                )?;
            }
        }
        Ok(())
    }

    pub fn list_heartbeats(&self, session_id: &str) -> Result<Vec<Heartbeat>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT state FROM session_heartbeats WHERE session_id=?1 ORDER BY updated_at_ms",
        )?;
        let rows = stmt.query_map([session_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let s = row?;
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if let Some(hb) = Heartbeat::from_json(&v) {
                    out.push(hb);
                }
            }
        }
        Ok(out)
    }

    pub fn user_heartbeat(&self, session_id: &str) -> Result<Option<Heartbeat>> {
        Ok(self
            .list_heartbeats(session_id)?
            .into_iter()
            .find(|h| h.source == "user"))
    }

    pub fn upsert_heartbeat(&self, hb: &Heartbeat) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO session_heartbeats(id, session_id, source, state, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET state=excluded.state, updated_at_ms=excluded.updated_at_ms, source=excluded.source",
            params![hb.id, hb.session_id, hb.source, hb.to_json().to_string(), now_ms()],
        )?;
        Ok(())
    }

    pub fn delete_heartbeat(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DELETE FROM session_heartbeats WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn clear_user_heartbeat(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "DELETE FROM session_heartbeats WHERE session_id=?1 AND source='user'",
            [session_id],
        )?;
        Ok(())
    }

    /// Sessions with an active goal, autonomous loop or heartbeat: the
    /// gateway driver's work list (empty means it has nothing to wake for).
    pub fn active_sessions(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut ids = std::collections::BTreeSet::new();
        for table in ["session_goals", "session_autonomous", "session_heartbeats"] {
            let mut stmt = conn.prepare(&format!("SELECT session_id, state FROM {table}"))?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (id, state) = row?;
                if serde_json::from_str::<Value>(&state).is_ok_and(|v| v["status"] == "active") {
                    ids.insert(id);
                }
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// Full `session.control.read` snapshot the desktop parser accepts.
    pub fn control_snapshot(&self, session_id: &str) -> Result<Value> {
        let goal = self.get_goal(session_id)?;
        let autonomous = self.get_autonomous(session_id)?;
        let heartbeat = self.user_heartbeat(session_id)?;
        let mut updated_at = 0.0f64;
        let goal_json = match goal.as_ref() {
            Some(g) if g.status != GoalStatus::Done => {
                updated_at = updated_at.max(g.updated_at_ms as f64 / 1000.0);
                g.to_control_json()
            }
            Some(g) => {
                // Done goals: still return for live chips; hydrate path clears them.
                updated_at = updated_at.max(g.updated_at_ms as f64 / 1000.0);
                g.to_control_json()
            }
            None => Value::Null,
        };
        let loop_json = match autonomous.as_ref() {
            Some(a) => {
                updated_at = updated_at.max(a.updated_at_ms as f64 / 1000.0);
                a.to_loop_json()
            }
            None => Value::Null,
        };
        let hb_json = match heartbeat.as_ref() {
            Some(h) => {
                updated_at = updated_at.max(h.created_at_ms as f64 / 1000.0);
                h.to_control_json()
            }
            None => Value::Null,
        };
        if updated_at == 0.0 {
            updated_at = now_secs_f64();
        }
        let revision = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            goal_json.to_string().hash(&mut h);
            loop_json.to_string().hash(&mut h);
            hb_json.to_string().hash(&mut h);
            format!("{:x}", h.finish())
        };
        Ok(json!({
            "goal": goal_json,
            "loop": loop_json,
            "heartbeat": hb_json,
            "revision": revision,
            "updated_at": updated_at,
        }))
    }
}

/// What the gateway should inject after a completed turn, if anything.
#[derive(Debug, Clone)]
pub enum Continuation {
    Goal(String),
    Autonomous(String),
    Heartbeat { id: String, prompt: String },
}

/// After a turn ends: maybe pause for subagents, check gates/budgets, return a follow-up prompt.
pub fn after_turn(
    store: &ControlStore,
    session_id: &str,
    tokens_this_turn: i64,
    subagents_running: bool,
    user_interrupted: bool,
) -> Result<Option<Continuation>> {
    after_turn_in(store, session_id, tokens_this_turn, subagents_running, user_interrupted, None)
}

/// `after_turn` with the session workspace, so ratchet checkpoints can be taken.
pub fn after_turn_in(
    store: &ControlStore,
    session_id: &str,
    tokens_this_turn: i64,
    subagents_running: bool,
    user_interrupted: bool,
    cwd: Option<&Path>,
) -> Result<Option<Continuation>> {
    let obs = turn_obs().lock().unwrap().remove(session_id);
    if user_interrupted {
        if let Some(mut goal) = store.get_goal(session_id)? {
            if goal.status == GoalStatus::Active {
                goal.status = GoalStatus::Paused;
                goal.paused_reason = Some("stopped on user input".into());
                goal.updated_at_ms = now_ms();
                store.set_goal(session_id, Some(&goal))?;
            }
        }
        if let Some(mut auto) = store.get_autonomous(session_id)? {
            if auto.status == AutonomousStatus::Active {
                auto.status = AutonomousStatus::Paused;
                auto.paused_reason = Some("stopped on user input".into());
                auto.updated_at_ms = now_ms();
                store.set_autonomous(session_id, Some(&auto))?;
            }
        }
        return Ok(None);
    }

    // Goal takes priority over autonomous (Prime: complementary; goal drives when set).
    if let Some(mut goal) = store.get_goal(session_id)? {
        if goal.status == GoalStatus::Active {
            goal.turns_used += 1;
            goal.tokens_used += tokens_this_turn;
            let o = obs.unwrap_or_default();
            goal.record_attempt(format!(
                "auto t{} f{} | files={} | ver={} | err={}",
                o.tools,
                o.failed,
                if o.files_changed { "yes" } else { "no" },
                if o.verified { "pass" } else { "none" },
                o.first_err.unwrap_or_default()
            ));
            let line = goal.attempt_log.last().cloned().unwrap_or_default();
            goal.ratchet(&o.scores, o.files_changed, cwd, session_id, &line);
            if !goal.plateaued() {
                goal.sup_episode = false;
            }
            goal.updated_at_ms = now_ms();
            if subagents_running {
                goal.waiting_on_subagents = true;
                store.set_goal(session_id, Some(&goal))?;
                return Ok(None);
            }
            goal.waiting_on_subagents = false;
            if let Some(reason) = goal.out_of_budget() {
                goal.status = GoalStatus::Paused;
                goal.paused_reason = Some(format!(
                    "{}/{} turns used ({})",
                    goal.turns_used, goal.max_turns, reason
                ));
                goal.last_verdict = Some("blocked".into());
                goal.last_reason = Some(reason.into());
                store.set_goal(session_id, Some(&goal))?;
                return Ok(None);
            }
            // Optional gates before allowing the model to complete — do not auto-complete.
            store.set_goal(session_id, Some(&goal))?;
            return Ok(Some(Continuation::Goal(goal.continuation_prompt())));
        }
    }

    if let Some(mut auto) = store.get_autonomous(session_id)? {
        if auto.status == AutonomousStatus::Active {
            auto.turns_used += 1;
            auto.tokens_used += tokens_this_turn;
            auto.updated_at_ms = now_ms();
            if subagents_running {
                auto.waiting_on_subagents = true;
                store.set_autonomous(session_id, Some(&auto))?;
                return Ok(None);
            }
            auto.waiting_on_subagents = false;

            // Run gates: all must pass for success. Limit hit ≠ success.
            if !auto.gates.is_empty() {
                let mut all_passed = true;
                let mut failed_output = String::new();
                for gate in &mut auto.gates {
                    let result = run_gate(gate);
                    if !result.passed {
                        all_passed = false;
                        failed_output = result.output;
                        break;
                    }
                }
                if all_passed {
                    auto.status = AutonomousStatus::Done;
                    auto.succeeded = true;
                    auto.last_stop_reason = Some("gates passed".into());
                    store.set_autonomous(session_id, Some(&auto))?;
                    return Ok(None);
                }
                if let Some(reason) = auto.out_of_limit() {
                    auto.status = AutonomousStatus::Done;
                    auto.succeeded = false;
                    auto.last_stop_reason = Some(reason.into());
                    store.set_autonomous(session_id, Some(&auto))?;
                    return Ok(None);
                }
                auto.continuations_used += 1;
                auto.last_fired_at_ms = now_ms();
                let prompt = format!(
                    "[Autonomous — quality gate failed]\n{}\n\nGate output (tail):\n```\n{}\n```\nFix the problem so the gate passes.",
                    auto.continuation_prompt(),
                    failed_output.chars().take(2000).collect::<String>()
                );
                store.set_autonomous(session_id, Some(&auto))?;
                return Ok(Some(Continuation::Autonomous(prompt)));
            }

            if let Some(reason) = auto.out_of_limit() {
                auto.status = AutonomousStatus::Done;
                auto.succeeded = false;
                auto.last_stop_reason = Some(reason.into());
                store.set_autonomous(session_id, Some(&auto))?;
                return Ok(None);
            }
            auto.continuations_used += 1;
            auto.last_fired_at_ms = now_ms();
            let prompt = auto.continuation_prompt();
            store.set_autonomous(session_id, Some(&auto))?;
            return Ok(Some(Continuation::Autonomous(prompt)));
        }
    }

    Ok(None)
}

/// Poll due heartbeats for a session (idle only — caller gates on idle).
pub fn due_heartbeat(store: &ControlStore, session_id: &str) -> Result<Option<Continuation>> {
    let now = now_ms();
    for mut hb in store.list_heartbeats(session_id)? {
        if hb.is_due(now) {
            hb.fire_count += 1;
            hb.last_fired_at_ms = now;
            store.upsert_heartbeat(&hb)?;
            return Ok(Some(Continuation::Heartbeat {
                id: hb.id.clone(),
                prompt: hb.fire_prompt(),
            }));
        }
    }
    Ok(None)
}

/// Poll due RLM "steer" heartbeats for a session. Unlike `due_heartbeat`, the
/// caller does NOT need to gate on idle: steer-mode RLM heartbeats are meant
/// to reach a busy session at its next turn boundary via a soft interrupt,
/// not wait for the whole session to go idle. Only `source == "rlm"` and
/// `delivery_mode == "steer"` heartbeats are considered here; plain
/// `follow_up` heartbeats (including the user's own) stay on the
/// idle-only `due_heartbeat` path.
pub fn due_steer_heartbeat(store: &ControlStore, session_id: &str) -> Result<Option<Continuation>> {
    let now = now_ms();
    for mut hb in store.list_heartbeats(session_id)? {
        if hb.source == "rlm" && hb.delivery_mode == "steer" && hb.is_due(now) {
            hb.fire_count += 1;
            hb.last_fired_at_ms = now;
            store.upsert_heartbeat(&hb)?;
            return Ok(Some(Continuation::Heartbeat {
                id: hb.id.clone(),
                prompt: hb.fire_prompt(),
            }));
        }
    }
    Ok(None)
}

// ── Slash command helpers ─────────────────────────────────────────────

pub fn handle_goal_command(store: &ControlStore, session_id: &str, args: &str) -> Result<String> {
    let args = args.trim();
    let first = args
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match first.as_str() {
        "" | "status" => Ok(store
            .get_goal(session_id)?
            .map(|g| g.status_text())
            .unwrap_or_else(|| "No active goal. Set one with /goal <text>.".into())),
        "clear" => {
            store.set_goal(session_id, None)?;
            Ok("✓ Goal cleared.".into())
        }
        "pause" => {
            let mut goal = store
                .get_goal(session_id)?
                .ok_or_else(|| anyhow::anyhow!("No goal to pause."))?;
            goal.status = GoalStatus::Paused;
            goal.paused_reason = Some("paused by user".into());
            goal.updated_at_ms = now_ms();
            store.set_goal(session_id, Some(&goal))?;
            Ok(format!("⏸ Goal paused: {}", goal.title))
        }
        "resume" => {
            let mut goal = store
                .get_goal(session_id)?
                .ok_or_else(|| anyhow::anyhow!("No goal to resume."))?;
            goal.status = GoalStatus::Active;
            goal.paused_reason = None;
            goal.updated_at_ms = now_ms();
            store.set_goal(session_id, Some(&goal))?;
            Ok(format!("▶ Goal resumed: {}", goal.title))
        }
        "complete" => {
            let mut goal = store
                .get_goal(session_id)?
                .ok_or_else(|| anyhow::anyhow!("No goal to complete."))?;
            goal.status = GoalStatus::Done;
            goal.last_verdict = Some("done".into());
            goal.updated_at_ms = now_ms();
            store.set_goal(session_id, Some(&goal))?;
            Ok(format!("✓ Goal achieved: {}", goal.title))
        }
        _ => {
            // `/goal [--budget N] [--turns N] [--timeout-ms N] <text>`
            let mut token_budget = None;
            let mut max_turns = 20i64;
            let mut wall_budget_ms = None;
            let mut title_parts = Vec::new();
            let mut iter = args.split_whitespace().peekable();
            while let Some(w) = iter.next() {
                match w {
                    "--budget" | "--token-budget" | "--max-tokens" => {
                        token_budget = iter
                            .next()
                            .and_then(|n| n.replace(',', "").replace('_', "").parse().ok());
                    }
                    "--turns" | "--max-turns" => {
                        max_turns = iter.next().and_then(|n| n.parse().ok()).unwrap_or(20);
                    }
                    "--timeout-ms" | "--wall-ms" => {
                        wall_budget_ms = iter.next().and_then(|n| n.replace(',', "").parse().ok());
                    }
                    other if other.starts_with("--budget=") => {
                        token_budget = other
                            .trim_start_matches("--budget=")
                            .replace(',', "")
                            .parse()
                            .ok();
                    }
                    other if other.starts_with("--turns=") => {
                        max_turns = other.trim_start_matches("--turns=").parse().unwrap_or(20);
                    }
                    other => title_parts.push(other.to_string()),
                }
            }
            let title = title_parts.join(" ").trim().to_string();
            if title.is_empty() {
                bail!("Usage: /goal <text> [--budget N] [--turns N]");
            }
            let mut goal = SessionGoal::new(title);
            goal.max_turns = max_turns.max(1);
            goal.token_budget = token_budget;
            goal.wall_budget_ms = wall_budget_ms;
            store.set_goal(session_id, Some(&goal))?;
            Ok(format!(
                "⊙ Goal set ({}-turn budget): {}",
                goal.max_turns, goal.title
            ))
        }
    }
}

pub fn handle_autonomous_command(
    store: &ControlStore,
    session_id: &str,
    args: &str,
) -> Result<String> {
    let args = args.trim();
    let mut words: Vec<&str> = args.split_whitespace().collect();
    if words.is_empty() {
        words.push("status");
    }
    match words[0].to_ascii_lowercase().as_str() {
        "status" => Ok(store
            .get_autonomous(session_id)?
            .map(|a| a.status_text())
            .unwrap_or_else(|| "Autonomous off.".into())),
        "off" | "stop" => {
            store.set_autonomous(session_id, None)?;
            Ok("Autonomous off.".into())
        }
        "on" | "start" => {
            let mut state = AutonomousState::default_on();
            let mut i = 1;
            let mut named_budget = false;
            while i < words.len() {
                let w = words[i];
                let val = |i: usize| words.get(i + 1).copied();
                match w {
                    "--max-continuations" | "--autonomous-max-continuations" => {
                        named_budget = true;
                        state.max_continuations = val(i).and_then(parse_limit).unwrap_or(3);
                        i += 2;
                    }
                    "--max-turns" | "--autonomous-max-turns" => {
                        named_budget = true;
                        state.max_turns = val(i).and_then(parse_limit).unwrap_or(0);
                        i += 2;
                    }
                    "--max-tokens" | "--autonomous-max-tokens" => {
                        named_budget = true;
                        state.max_tokens = val(i).and_then(parse_limit);
                        i += 2;
                    }
                    "--timeout-ms" | "--autonomous-timeout-ms" => {
                        named_budget = true;
                        state.timeout_ms = val(i).and_then(parse_limit);
                        i += 2;
                    }
                    "--gate" | "--autonomous-gate" => {
                        if let Some(cmd) = val(i) {
                            state.gates.push(QualityGate::new(cmd));
                        }
                        i += 2;
                    }
                    "--gate-retries" => {
                        if let Some(n) = val(i).and_then(|s| s.parse().ok()) {
                            for g in &mut state.gates {
                                g.max_retries = n;
                            }
                        }
                        i += 2;
                    }
                    "--gate-timeout-ms" => {
                        if let Some(n) = val(i).and_then(|s| s.parse::<i64>().ok()) {
                            for g in &mut state.gates {
                                g.timeout_seconds = (n / 1000).max(1);
                            }
                        }
                        i += 2;
                    }
                    other if other.starts_with("--") => {
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            if named_budget {
                // Named budget flags define the whole budget: unnamed limits become unlimited.
                // (Already set only the ones we saw; defaults for unset stay as default_on unless we clear.)
            }
            store.set_autonomous(session_id, Some(&state))?;
            Ok(state.status_text())
        }
        // Hermes `/loop` alias: treat remaining as on with a prompt
        _ => {
            // `/autonomous <prompt>` → on with custom prompt
            let mut state = AutonomousState::default_on();
            state.prompt = args.to_string();
            store.set_autonomous(session_id, Some(&state))?;
            Ok(state.status_text())
        }
    }
}

fn parse_limit(s: &str) -> Option<i64> {
    if s.eq_ignore_ascii_case("unlimited") {
        return Some(0);
    }
    s.replace(',', "").replace('_', "").parse().ok()
}

pub fn handle_heartbeat_command(
    store: &ControlStore,
    session_id: &str,
    args: &str,
) -> Result<String> {
    let args = args.trim();
    let mut words: Vec<&str> = args.split_whitespace().collect();
    if words.is_empty() {
        words.push("status");
    }
    match words[0].to_ascii_lowercase().as_str() {
        "status" | "list" => {
            let list = store.list_heartbeats(session_id)?;
            if list.is_empty() {
                return Ok(
                    "No heartbeat. Set one with /heartbeat every <interval> <prompt>.".into(),
                );
            }
            let lines: Vec<_> = list
                .iter()
                .map(|h| {
                    format!(
                        "- [{}] every {}s ({}, fired {}) — {}",
                        h.id.chars().take(8).collect::<String>(),
                        h.interval_seconds,
                        h.status.as_str(),
                        h.fire_count,
                        h.prompt
                    )
                })
                .collect();
            Ok(lines.join("\n"))
        }
        "pause" => {
            let mut hb = store
                .user_heartbeat(session_id)?
                .ok_or_else(|| anyhow::anyhow!("No heartbeat to pause."))?;
            hb.status = HeartbeatStatus::Paused;
            store.upsert_heartbeat(&hb)?;
            Ok("Heartbeat paused.".into())
        }
        "resume" => {
            let mut hb = store
                .user_heartbeat(session_id)?
                .ok_or_else(|| anyhow::anyhow!("No heartbeat to resume."))?;
            hb.status = HeartbeatStatus::Active;
            store.upsert_heartbeat(&hb)?;
            Ok("Heartbeat resumed.".into())
        }
        "clear" | "cancel" => {
            let id = words.get(1).copied();
            if let Some(id) = id {
                store.delete_heartbeat(id)?;
            } else {
                store.clear_user_heartbeat(session_id)?;
            }
            Ok("Heartbeat cleared.".into())
        }
        "every" => {
            let interval = words
                .get(1)
                .and_then(|t| parse_duration_token(t))
                .ok_or_else(|| anyhow::anyhow!("Usage: /heartbeat every <interval> <prompt>"))?;
            let prompt = words.get(2..).map(|w| w.join(" ")).unwrap_or_default();
            if prompt.trim().is_empty() {
                bail!("Usage: /heartbeat every <interval> <prompt> — the prompt is required.");
            }
            store.clear_user_heartbeat(session_id)?;
            let hb = Heartbeat::new(session_id, prompt, interval);
            store.upsert_heartbeat(&hb)?;
            Ok(format!(
                "Heartbeat set: every {} — will re-enter this session when idle.",
                format_duration(interval)
            ))
        }
        _ => {
            // Allow `/heartbeat 5m check CI` without "every"
            if let Some(interval) = words.first().and_then(|t| parse_duration_token(t)) {
                let prompt = words.get(1..).map(|w| w.join(" ")).unwrap_or_default();
                if prompt.trim().is_empty() {
                    bail!("Usage: /heartbeat every <interval> <prompt>");
                }
                store.clear_user_heartbeat(session_id)?;
                let hb = Heartbeat::new(session_id, prompt, interval);
                store.upsert_heartbeat(&hb)?;
                return Ok(format!(
                    "Heartbeat set: every {}.",
                    format_duration(interval)
                ));
            }
            bail!("Usage: /heartbeat every <interval> <prompt> | status | pause | resume | clear")
        }
    }
}

fn format_duration(seconds: i64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    parts.join("")
}

/// Dispatch envelope for `session.control` actions.
pub fn control_action(
    store: &ControlStore,
    session_id: &str,
    action: &str,
    args: &Value,
) -> Result<(Value, Value)> {
    let message = match action {
        "goal.clear" => handle_goal_command(store, session_id, "clear")?,
        "goal.pause" => handle_goal_command(store, session_id, "pause")?,
        "goal.resume" => handle_goal_command(store, session_id, "resume")?,
        "goal.unwait" => {
            if let Some(mut g) = store.get_goal(session_id)? {
                g.waiting_on_subagents = false;
                g.updated_at_ms = now_ms();
                store.set_goal(session_id, Some(&g))?;
            }
            "Goal wait cleared.".into()
        }
        "subgoal.add" => {
            let text = args["text"].as_str().unwrap_or_default();
            if let Some(mut g) = store.get_goal(session_id)? {
                g.subgoals.push(text.to_string());
                g.updated_at_ms = now_ms();
                store.set_goal(session_id, Some(&g))?;
                format!("Added subgoal: {text}")
            } else {
                bail!("No active goal");
            }
        }
        "subgoal.remove" => {
            let index = args["index"].as_u64().unwrap_or(0) as usize;
            if let Some(mut g) = store.get_goal(session_id)? {
                if index == 0 || index > g.subgoals.len() {
                    bail!("Invalid subgoal index");
                }
                let removed = g.subgoals.remove(index - 1);
                g.updated_at_ms = now_ms();
                store.set_goal(session_id, Some(&g))?;
                format!("Removed subgoal: {removed}")
            } else {
                bail!("No active goal");
            }
        }
        "subgoal.clear" => {
            if let Some(mut g) = store.get_goal(session_id)? {
                g.subgoals.clear();
                g.updated_at_ms = now_ms();
                store.set_goal(session_id, Some(&g))?;
            }
            "Subgoals cleared.".into()
        }
        "heartbeat.clear" => handle_heartbeat_command(store, session_id, "clear")?,
        "heartbeat.pause" => handle_heartbeat_command(store, session_id, "pause")?,
        "heartbeat.resume" => handle_heartbeat_command(store, session_id, "resume")?,
        "loop.pause" => {
            if let Some(mut a) = store.get_autonomous(session_id)? {
                a.status = AutonomousStatus::Paused;
                a.paused_reason = Some("paused by user".into());
                a.updated_at_ms = now_ms();
                store.set_autonomous(session_id, Some(&a))?;
            }
            "Autonomous paused.".into()
        }
        "loop.resume" => {
            if let Some(mut a) = store.get_autonomous(session_id)? {
                a.status = AutonomousStatus::Active;
                a.paused_reason = None;
                a.updated_at_ms = now_ms();
                store.set_autonomous(session_id, Some(&a))?;
            }
            "Autonomous resumed.".into()
        }
        "loop.stop" => {
            store.set_autonomous(session_id, None)?;
            "Autonomous stopped.".into()
        }
        other => bail!("Unknown session.control action: {other}"),
    };
    let control = store.control_snapshot(session_id)?;
    let dispatch = json!({
        "type": "exec",
        "output": message,
        "notice": null,
        "message": null,
        "display": null,
    });
    Ok((control, dispatch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_sessions_lists_only_active_goals_loops_and_heartbeats() {
        let store = ControlStore::memory().unwrap();
        assert!(store.active_sessions().unwrap().is_empty());
        store.set_goal("g", Some(&SessionGoal::new("ship"))).unwrap();
        let mut paused = SessionGoal::new("later");
        paused.status = GoalStatus::Paused;
        store.set_goal("paused", Some(&paused)).unwrap();
        store.upsert_heartbeat(&Heartbeat::new("h", "check", 600)).unwrap();
        let mut off = Heartbeat::new("off", "check", 600);
        off.status = HeartbeatStatus::Paused;
        store.upsert_heartbeat(&off).unwrap();
        assert_eq!(store.active_sessions().unwrap(), ["g", "h"]);
    }

    #[test]
    fn goal_stops_at_turn_budget() {
        let store = ControlStore::memory().unwrap();
        let mut goal = SessionGoal::new("ship it");
        goal.max_turns = 2;
        store.set_goal("s1", Some(&goal)).unwrap();
        let c1 = after_turn(&store, "s1", 10, false, false).unwrap();
        assert!(matches!(c1, Some(Continuation::Goal(_))));
        let c2 = after_turn(&store, "s1", 10, false, false).unwrap();
        assert!(c2.is_none());
        let g = store.get_goal("s1").unwrap().unwrap();
        assert_eq!(g.status, GoalStatus::Paused);
        assert!(g.paused_reason.as_deref().unwrap().contains("turn"));
    }

    #[test]
    fn autonomous_gate_pass_is_success_limit_is_not() {
        let store = ControlStore::memory().unwrap();
        let mut auto = AutonomousState::default_on();
        auto.max_continuations = 5;
        auto.gates = vec![QualityGate::new("true")];
        store.set_autonomous("s1", Some(&auto)).unwrap();
        let c = after_turn(&store, "s1", 0, false, false).unwrap();
        assert!(c.is_none());
        let a = store.get_autonomous("s1").unwrap().unwrap();
        assert!(a.succeeded);
        assert_eq!(a.status, AutonomousStatus::Done);

        let mut auto = AutonomousState::default_on();
        auto.max_continuations = 1;
        auto.gates = vec![QualityGate::new("false")];
        store.set_autonomous("s2", Some(&auto)).unwrap();
        let _ = after_turn(&store, "s2", 0, false, false).unwrap();
        // After one continuation attempt with failed gate and limit 1 used...
        // continuations_used becomes 1, so next after_turn hits limit
        let c2 = after_turn(&store, "s2", 0, false, false).unwrap();
        assert!(c2.is_none());
        let a = store.get_autonomous("s2").unwrap().unwrap();
        assert!(!a.succeeded);
    }

    #[test]
    fn heartbeat_persists_and_fires_once() {
        let store = ControlStore::memory().unwrap();
        let mut hb = Heartbeat::new("s1", "check CI", 60);
        hb.last_fired_at_ms = now_ms() - 61_000;
        store.upsert_heartbeat(&hb).unwrap();
        let due = due_heartbeat(&store, "s1").unwrap().unwrap();
        assert!(matches!(due, Continuation::Heartbeat { .. }));
        let loaded = store.user_heartbeat("s1").unwrap().unwrap();
        assert_eq!(loaded.fire_count, 1);
        // Not immediately due again
        assert!(due_heartbeat(&store, "s1").unwrap().is_none());
    }

    #[test]
    fn control_read_shape_has_exact_fields() {
        let store = ControlStore::memory().unwrap();
        store.set_goal("s1", Some(&SessionGoal::new("x"))).unwrap();
        let auto = AutonomousState::default_on();
        store.set_autonomous("s1", Some(&auto)).unwrap();
        store
            .upsert_heartbeat(&Heartbeat::new("s1", "ping", 60))
            .unwrap();
        let snap = store.control_snapshot("s1").unwrap();
        assert!(snap["goal"].is_object());
        assert!(snap["loop"].is_object());
        assert!(snap["heartbeat"].is_object());
        assert!(snap["revision"].is_string());
        assert!(snap["updated_at"].as_f64().is_some());
        // Goal required fields
        for key in [
            "title",
            "status",
            "turns_used",
            "max_turns",
            "contract",
            "subgoals",
            "gates",
        ] {
            assert!(snap["goal"].get(key).is_some(), "missing goal.{key}");
        }
        for key in [
            "prompt",
            "status",
            "mode",
            "interval_seconds",
            "current_delay",
            "times",
            "until",
            "max_ticks",
            "ticks_fired",
            "created_at",
            "last_fired_at",
            "next_due_at",
            "awaiting_response",
            "deferred_by_goal",
        ] {
            assert!(snap["loop"].get(key).is_some(), "missing loop.{key}");
        }
        for key in [
            "prompt",
            "status",
            "interval_seconds",
            "created_at",
            "last_fired_at",
            "fire_count",
        ] {
            assert!(
                snap["heartbeat"].get(key).is_some(),
                "missing heartbeat.{key}"
            );
        }
    }

    #[test]
    fn parse_duration_and_slash_goal() {
        assert_eq!(parse_duration_token("5m"), Some(300));
        assert_eq!(parse_duration_token("1h30m"), Some(5400));
        let store = ControlStore::memory().unwrap();
        let msg = handle_goal_command(&store, "s1", "--turns 3 ship the feature").unwrap();
        assert!(msg.contains("Goal set"));
        assert!(msg.contains("ship the feature"));
        let g = store.get_goal("s1").unwrap().unwrap();
        assert_eq!(g.max_turns, 3);
    }

    #[test]
    fn waiting_on_subagents_pauses_continuation() {
        let store = ControlStore::memory().unwrap();
        store.set_goal("s1", Some(&SessionGoal::new("x"))).unwrap();
        let c = after_turn(&store, "s1", 0, true, false).unwrap();
        assert!(c.is_none());
        assert!(store.get_goal("s1").unwrap().unwrap().waiting_on_subagents);
    }

    #[test]
    fn user_interrupt_pauses_goal() {
        let store = ControlStore::memory().unwrap();
        store.set_goal("s1", Some(&SessionGoal::new("x"))).unwrap();
        let c = after_turn(&store, "s1", 0, false, true).unwrap();
        assert!(c.is_none());
        assert_eq!(
            store.get_goal("s1").unwrap().unwrap().status,
            GoalStatus::Paused
        );
    }

    #[test]
    fn attempt_log_is_capped_and_truncated() {
        let mut goal = SessionGoal::new("x");
        for i in 0..12 {
            goal.record_attempt(format!("attempt number {i}"));
        }
        assert_eq!(goal.attempt_log.len(), MAX_ATTEMPT_LOG);
        // Oldest entries are dropped, newest kept.
        assert_eq!(goal.attempt_log.first().unwrap(), "attempt number 4");
        assert_eq!(goal.attempt_log.last().unwrap(), "attempt number 11");

        let long = "x".repeat(200);
        goal.record_attempt(long);
        let last = goal.attempt_log.last().unwrap();
        assert!(last.chars().count() <= MAX_ATTEMPT_LEN);
        assert!(last.ends_with('…'));
    }

    #[test]
    fn plateau_detection_needs_no_model_call() {
        let mut goal = SessionGoal::new("x");
        assert!(!goal.plateaued(), "empty log is never a plateau");
        goal.record_attempt("fail: same build error");
        goal.record_attempt("pass: different step");
        assert!(!goal.plateaued(), "two distinct entries is not a plateau");
        goal.record_attempt("fail: same build error");
        goal.record_attempt("fail: same build error");
        goal.record_attempt("fail: same build error");
        assert!(
            goal.plateaued(),
            "3 identical consecutive attempts is a plateau"
        );
    }

    #[test]
    fn continuation_prompt_injects_attempt_log_and_steer_on_plateau() {
        let mut goal = SessionGoal::new("ship it");
        let prompt = goal.continuation_prompt();
        assert!(!prompt.contains("Recent attempts"));
        assert!(!prompt.contains("Plateau detected"));

        goal.record_attempt("fail: lint error");
        let prompt = goal.continuation_prompt();
        assert!(prompt.contains("Recent attempts"));
        assert!(prompt.contains("fail: lint error"));
        assert!(!prompt.contains("Plateau detected"));

        goal.record_attempt("fail: lint error");
        goal.record_attempt("fail: lint error");
        let prompt = goal.continuation_prompt();
        assert!(prompt.contains("Plateau detected"));
        assert!(prompt.contains("different strategy"));
    }

    #[test]
    fn failed_verification_never_ends_the_goal_only_budget_does() {
        let store = ControlStore::memory().unwrap();
        let mut goal = SessionGoal::new("x");
        goal.max_turns = 2;
        goal.record_attempt("fail: verification failed");
        store.set_goal("s1", Some(&goal)).unwrap();
        // A failed verification is just an attempt-log entry; the goal keeps
        // going until budget exhaustion, completion, or a user cancel.
        let c1 = after_turn(&store, "s1", 0, false, false).unwrap();
        assert!(matches!(c1, Some(Continuation::Goal(_))));
        assert_eq!(
            store.get_goal("s1").unwrap().unwrap().status,
            GoalStatus::Active
        );
        // Budget exhaustion is the actual stop condition, and it keeps the
        // attempt log rather than discarding it.
        let c2 = after_turn(&store, "s1", 0, false, false).unwrap();
        assert!(c2.is_none());
        let paused = store.get_goal("s1").unwrap().unwrap();
        assert_eq!(paused.status, GoalStatus::Paused);
        assert_eq!(paused.attempt_log[0], "fail: verification failed");
    }

    fn run_auto_turn(store: &ControlStore, sid: &str, calls: &[(&str, Value, &str)]) {
        for (name, args, result) in calls {
            observe_tool(sid, name, args, result);
        }
        after_turn(store, sid, 0, false, false).unwrap();
    }

    #[test]
    fn auto_attempt_line_recorded_without_op_progress() {
        let store = ControlStore::memory().unwrap();
        store.set_goal("auto1", Some(&SessionGoal::new("x"))).unwrap();
        run_auto_turn(
            &store,
            "auto1",
            &[("edit", json!({}), "ok"), ("bash", json!({"command":"cargo test"}), "ok")],
        );
        let g = store.get_goal("auto1").unwrap().unwrap();
        assert_eq!(g.attempt_log.len(), 1);
        assert_eq!(g.attempt_log[0], "auto t2 f0 | files=yes | ver=pass | err=");
        assert!(g.attempt_log[0].len() <= MAX_ATTEMPT_LEN);
    }

    #[test]
    fn repeated_normalized_error_is_a_plateau() {
        let store = ControlStore::memory().unwrap();
        store.set_goal("auto2", Some(&SessionGoal::new("x"))).unwrap();
        for i in 0..PLATEAU_TURNS {
            let err = format!("Error: cannot find /tmp/run{i}/a.rs line {i}");
            run_auto_turn(&store, "auto2", &[("edit", json!({}), "ok"), ("bash", json!({}), &err)]);
        }
        let g = store.get_goal("auto2").unwrap().unwrap();
        assert!(g.plateaued(), "{:?}", g.attempt_log);
        assert!(g.continuation_prompt().contains("Plateau detected"));
    }

    #[test]
    fn nonzero_bash_exit_is_a_failed_verification() {
        let sid = "nonzero-exit-test";
        observe_tool(sid, "bash", &serde_json::json!({"command": "cargo test"}), "1 failed\n\nExit code: 101");
        observe_tool(sid, "bash", &serde_json::json!({"command": "npm test"}), "ok\n--- Command finished with exit code: 0 ---");
        let map = turn_obs().lock().unwrap();
        let obs = &map[sid];
        assert_eq!((obs.tools, obs.failed), (2, 1));
        assert!(obs.verified, "the exit-0 test run still counts as a passing verification");
    }

    #[test]
    fn file_change_plus_passing_test_is_not_a_plateau() {
        let store = ControlStore::memory().unwrap();
        store.set_goal("auto3", Some(&SessionGoal::new("x"))).unwrap();
        for _ in 0..PLATEAU_TURNS {
            run_auto_turn(
                &store,
                "auto3",
                &[("edit", json!({}), "ok"), ("bash", json!({"command":"cargo test"}), "ok")],
            );
        }
        assert!(!store.get_goal("auto3").unwrap().unwrap().plateaued());
    }

    #[test]
    fn due_steer_heartbeat_ignores_follow_up_and_fires_rlm_steer() {
        let store = ControlStore::memory().unwrap();
        let mut follow_up = Heartbeat::new("s1", "watch build", 60);
        follow_up.source = "rlm".into();
        follow_up.last_fired_at_ms = now_ms() - 61_000;
        store.upsert_heartbeat(&follow_up).unwrap();
        assert!(due_steer_heartbeat(&store, "s1").unwrap().is_none());

        let mut steer = Heartbeat::new("s1", "check progress", 60);
        steer.source = "rlm".into();
        steer.delivery_mode = "steer".into();
        steer.last_fired_at_ms = now_ms() - 61_000;
        store.upsert_heartbeat(&steer).unwrap();
        let due = due_steer_heartbeat(&store, "s1").unwrap();
        assert!(matches!(due, Some(Continuation::Heartbeat { .. })));
        // Follow-up heartbeat on the same session is untouched.
        assert!(store.user_heartbeat("s1").unwrap().is_none());
        let stored_follow_up = store
            .list_heartbeats("s1")
            .unwrap()
            .into_iter()
            .find(|h| h.id == follow_up.id)
            .unwrap();
        assert_eq!(stored_follow_up.fire_count, 0);
    }

    #[test]
    fn runner_output_becomes_a_score_vector() {
        use crate::goal_ratchet::*;
        assert_eq!(parse_counts("test result: ok. 5 passed; 1 failed; 0 ignored\ntest result: ok. 2 passed; 0 failed"), Some((7, 1)));
        assert_eq!(parse_counts("=== 2 failed, 5 passed in 0.3s ==="), Some((5, 2)));
        assert_eq!(parse_counts("Tests:       1 failed, 5 passed, 6 total"), Some((5, 1)));
        assert_eq!(score_of(true, "tsc ok"), (1, 0));
        assert_eq!(score_of(false, "boom"), (0, 1));
        assert_eq!(score_of(false, "test result: FAILED. 3 passed; 0 failed"), (3, 1));
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(o.status.success(), "{args:?}");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn test_run(store: &ControlStore, cwd: &Path, out: &str, exit: i32) {
        let res = format!("{out}\n\nExit code: {exit}");
        observe_tool("rt", "edit", &json!({}), "ok");
        observe_tool("rt", "bash", &json!({"command": "cargo test"}), &res);
        after_turn_in(store, "rt", 0, false, false, Some(cwd)).unwrap();
    }

    #[test]
    fn ratchet_checkpoints_hidden_ref_and_regression_points_at_it() {
        let dir = std::env::temp_dir().join(format!("ratchet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_ok(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        git_ok(&dir, &["add", "a.txt"]);
        git_ok(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]);
        let head = git_ok(&dir, &["rev-parse", "HEAD"]);
        let store = ControlStore::memory().unwrap();
        store.set_goal("rt", Some(&SessionGoal::new("x"))).unwrap();
        std::fs::write(dir.join("new.txt"), "untracked").unwrap();
        test_run(&store, &dir, "test result: FAILED. 0 passed; 2 failed", 101);
        assert!(store.get_goal("rt").unwrap().unwrap().lineage.is_empty(), "a failing run never commits");
        test_run(&store, &dir, "test result: ok. 3 passed; 0 failed", 0);
        let g = store.get_goal("rt").unwrap().unwrap();
        let r = g.lineage[0].git_ref.clone();
        assert!(r.starts_with("refs/akira/goals/rt/"), "{r}");
        assert!(git_ok(&dir, &["ls-tree", "-r", "--name-only", &r]).contains("new.txt"), "untracked files are snapshotted");
        assert_eq!(git_ok(&dir, &["rev-parse", "HEAD"]), head, "HEAD untouched");
        assert_eq!(git_ok(&dir, &["status", "--porcelain"]), "?? new.txt", "index and worktree untouched");
        test_run(&store, &dir, "test result: FAILED. 2 passed; 1 failed", 101);
        let p = store.get_goal("rt").unwrap().unwrap().continuation_prompt();
        assert!(p.contains("[Regression]") && p.contains(&format!("git diff {r}")) && p.contains(&format!("git checkout {r} -- ")), "{p}");
        // non-git workspaces record no-vcs
        let plain = std::env::temp_dir().join(format!("plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        store.set_goal("rt", Some(&SessionGoal::new("y"))).unwrap();
        test_run(&store, &plain, "test result: ok. 1 passed; 0 failed", 0);
        assert_eq!(store.get_goal("rt").unwrap().unwrap().lineage[0].git_ref, "no-vcs");
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(plain).ok();
    }

    #[test]
    fn supervisor_is_once_per_plateau_and_rate_limited() {
        let store = ControlStore::memory().unwrap();
        let mut g = SessionGoal::new("port the parser");
        for _ in 0..PLATEAU_TURNS {
            g.record_attempt("auto t1 f1 | files=no | ver=none | err=boom");
        }
        g.turns_used = 3;
        assert!(g.supervisor_due());
        assert!(g.supervisor_request().1.contains("port the parser"));
        store.set_goal("s", Some(&g)).unwrap();
        let p = apply_supervisor(&store, "s", Some("1. rewrite\n2. bisect")).unwrap();
        assert!(p.contains("[Supervisor: alternative directions]") && p.contains("bisect"));
        let after = store.get_goal("s").unwrap().unwrap();
        assert!(!after.supervisor_due(), "same plateau episode");
        assert!(!after.continuation_prompt().contains("Supervisor"), "injected once");
        let mut g2 = after.clone();
        g2.sup_episode = false;
        g2.turns_used = 5;
        assert!(!g2.supervisor_due(), "needs 5 turns since the last call");
        g2.turns_used = 8;
        assert!(g2.supervisor_due());
        // A failed call still consumes the episode and falls back to the text steer.
        store.set_goal("s", Some(&g2)).unwrap();
        assert!(apply_supervisor(&store, "s", None).unwrap().contains("Plateau detected"));
    }
}
