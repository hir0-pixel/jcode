//! Session goals, autonomous mode, and heartbeats (Prime long-running agents
//! semantics, Hermes `session.control` wire shape).
//!
//! Persisted in `sovereign.db` so state survives engine restart and desktop
//! disconnect/reattach. The desktop reads `session.control.read`; autonomous
//! mode is projected into the `loop` field (`mode=self_paced`).

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
        }
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
        format!(
            "[Continuing toward your standing goal]\nGoal: {}\n\nContinue working toward this goal. Take the next concrete step. \
             If you believe the goal is complete, call the goal tool with op=complete. \
             If you are blocked and need input from the user, say so clearly and stop.",
            self.title
        )
    }
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
    pub prompt: String,
    pub status: HeartbeatStatus,
    pub interval_seconds: i64,
    pub created_at_ms: i64,
    pub last_fired_at_ms: i64,
    pub fire_count: i64,
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
            "prompt": self.prompt,
            "status": self.status.as_str(),
            "interval_seconds": self.interval_seconds,
            "created_at_ms": self.created_at_ms,
            "last_fired_at_ms": self.last_fired_at_ms,
            "fire_count": self.fire_count,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v["id"].as_str()?.to_string(),
            session_id: v["session_id"].as_str()?.to_string(),
            source: v["source"].as_str().unwrap_or("user").to_string(),
            prompt: v["prompt"].as_str()?.to_string(),
            status: HeartbeatStatus::parse(v["status"].as_str()?)?,
            interval_seconds: v["interval_seconds"].as_i64().unwrap_or(60),
            created_at_ms: v["created_at_ms"].as_i64().unwrap_or(0),
            last_fired_at_ms: v["last_fired_at_ms"].as_i64().unwrap_or(0),
            fire_count: v["fire_count"].as_i64().unwrap_or(0),
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
}
