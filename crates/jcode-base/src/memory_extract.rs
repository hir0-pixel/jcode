//! Automatic memory extraction (jcode's, restored on the single store).
//!
//! A chat is turned into short fact/preference/correction/entity memories by one call to the
//! active provider. Triggers (all outside the gateway): every 12 fresh user turns, session end,
//! before compaction drops messages. Each trigger extracts only the messages after the session's
//! persisted `extracted_through` index, under a 200-char / 4-message floor, a 60 s per-session
//! cooldown (SessionEnd and Compaction are exempt), an in-flight claim taken at plan time, and one process-wide
//! aux-call permit. The window is built oldest-first from the marker and the marker moves only to
//! the last message actually included, so an oversized backlog is consumed across successive calls. It runs in a spawned task and never fails a
//! turn: errors become a `memory.extract` span.

use crate::memory::{MemoryCategory, MemoryEntry, MemoryManager, TrustLevel};
use crate::memory_store::Remembered;
use crate::message::{ContentBlock, Message, Role};
use crate::obs_sink::{self, Span};
use anyhow::{Result, anyhow};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Fresh user turns between periodic extractions (upstream's interval).
const PERIODIC_INTERVAL: usize = 12;
/// Periodic runs look at no more than this many new messages.
const PERIODIC_MAX_MESSAGES: usize = 40;
/// Transcript cap per aux call (oldest messages first; the rest goes to the next window).
const MAX_TRANSCRIPT_CHARS: usize = 24_000;
const MIN_TRANSCRIPT_CHARS: usize = 200;
const MIN_MESSAGES: usize = 4;
const COOLDOWN: Duration = Duration::from_secs(60);
const EXISTING_LIMIT: usize = 80;
const EXISTING_CHARS: usize = 150;
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a job may wait for the shared aux-call permit.
const PERMIT_TIMEOUT: Duration = Duration::from_secs(120);
/// An in-flight claim older than this is treated as abandoned (the task died).
const CLAIM_TTL: Duration = Duration::from_secs(CALL_TIMEOUT.as_secs() + PERMIT_TIMEOUT.as_secs() + 30);
/// Windows one non-periodic job may consume in a row.
const MAX_WINDOWS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Periodic,
    SessionEnd,
    Compaction,
}

impl Trigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Periodic => "periodic",
            Self::SessionEnd => "session_end",
            Self::Compaction => "compaction",
        }
    }
}

#[derive(Default)]
struct SessionState {
    turns: usize,
    /// When the last job finished (cooldown clock).
    last_done: Option<Instant>,
    /// When a planned job claimed the session; cleared when it finishes.
    claimed: Option<Instant>,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, SessionState>>> = LazyLock::new(Default::default);

/// One extraction/learning aux call at a time per process, so a local model is not hit in
/// parallel with the user's own turn. Prime learning takes the same permit.
static AUX_CALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub async fn aux_call_permit() -> tokio::sync::SemaphorePermit<'static> {
    AUX_CALLS.acquire().await.expect("aux semaphore is never closed")
}

/// The permit, or `None` if it did not free up within `wait` (background work must not queue forever).
pub async fn aux_call_permit_within(wait: Duration) -> Option<tokio::sync::SemaphorePermit<'static>> {
    tokio::time::timeout(wait, aux_call_permit()).await.ok()
}

/// Count a fresh user turn; true on every 12th (the caller then triggers a periodic run).
pub fn note_user_turn(session_id: &str) -> bool {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    let state = sessions.entry(session_id.to_string()).or_default();
    state.turns += 1;
    state.turns.is_multiple_of(PERIODIC_INTERVAL)
}

/// A session first seen in this process (attached, resumed or closed without a user turn here) that
/// has no `extracted_through` marker pre-dates extraction: stamp the marker at its current message
/// count, so only messages that arrive from now on are ever extracted. Once per session per process.
pub fn adopt_session(session_id: &str, total: usize) {
    {
        let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
        if sessions.contains_key(session_id) {
            return;
        }
        sessions.insert(session_id.to_string(), SessionState::default());
    }
    let manager = MemoryManager::new();
    if matches!(manager.meta_get(&through_key(session_id)), Ok(None)) {
        let _ = manager.meta_set(&through_key(session_id), &total.to_string());
    }
}

/// Drop a closed session's counters (upstream's map grew without bound).
pub fn forget_session(session_id: &str) {
    SESSIONS.lock().unwrap_or_else(|p| p.into_inner()).remove(session_id);
}

fn in_flight(session_id: &str) -> bool {
    let sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    sessions.get(session_id).and_then(|s| s.claimed).is_some_and(|at| at.elapsed() < CLAIM_TTL)
}

fn cooldown_active(session_id: &str) -> bool {
    let sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    sessions.get(session_id).and_then(|s| s.last_done).is_some_and(|at| at.elapsed() < COOLDOWN)
}

/// Claim the session for a planned job (at plan time, so a second trigger sees it in flight).
fn claim(session_id: &str) {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    sessions.entry(session_id.to_string()).or_default().claimed = Some(Instant::now());
}

/// Release the claim and start the cooldown. Never recreates a session that was forgotten.
fn release(session_id: &str) {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(state) = sessions.get_mut(session_id) {
        state.claimed = None;
        state.last_done = Some(Instant::now());
    }
}

fn sidecar_enabled() -> bool {
    crate::config::config().agents.memory_sidecar_enabled
}

fn through_key(session_id: &str) -> String {
    format!("extracted_through:{session_id}")
}

fn extracted_through(manager: &MemoryManager, session_id: &str, total: usize) -> usize {
    let stored = manager
        .meta_get(&through_key(session_id))
        .ok()
        .flatten()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    // A shorter history than the marker means the session was rewound: start over.
    if stored > total { 0 } else { stored }
}

/// A memory as the model wrote it (`CATEGORY|CONTENT|TRUST`).
#[derive(Debug, Clone, PartialEq)]
pub struct Extracted {
    pub category: String,
    pub content: String,
    pub trust: String,
}

/// The upstream line format: `category|content|trust`, one per line, other lines ignored.
pub fn parse_extracted(response: &str) -> Vec<Extracted> {
    response
        .lines()
        .filter(|line| line.contains('|'))
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('|').collect();
            (parts.len() >= 3).then(|| Extracted {
                category: parts[0].trim().to_lowercase(),
                content: parts[1].trim().to_string(),
                trust: parts[2].trim().to_lowercase(),
            })
        })
        .filter(|m| !m.content.is_empty())
        .collect()
}

fn trust_of(word: &str) -> TrustLevel {
    match word {
        "high" => TrustLevel::High,
        "low" => TrustLevel::Low,
        _ => TrustLevel::Medium,
    }
}

const SYSTEM_PROMPT: &str = r#"You are a memory extraction assistant. Extract important NEW learnings from the conversation that should be remembered for future sessions.

Categories (use EXACTLY one of these):
- fact: Technical facts about the codebase, architecture, patterns, dependencies, tools, environment
- preference: User preferences, workflow habits, UX expectations, coding style, conventions, how they want the assistant to behave
- correction: Mistakes that were corrected, bugs found and fixed, wrong assumptions, things the user corrected
- entity: Named entities worth tracking - people, projects, services, repos, teams

Categorization rules:
- If it describes what the USER WANTS or HOW THEY LIKE THINGS, it is "preference", not "fact"
- If it describes a BUG FIX or MISTAKE, it is "correction", not "fact"
- "fact" is for objective technical information about code/systems, not user behavior

IMPORTANT - Do NOT extract:
- Transient debugging details, compile errors, or intermediate build steps
- Specific commit hashes, git operations, or "changes were committed/pushed" details
- Line-by-line code changes like "X was updated to Y in file Z" - these belong in git history, not memory
- Self-evident project context (e.g., the project name, repo URL, language) that is already in the system prompt
- Redundant variations of information already known (check the "Already known" list carefully)

Quality bar: Only extract information that would ACTUALLY BE USEFUL if recalled in a future session on a different topic. Ask: "Would a developer benefit from knowing this weeks from now?"

For each memory, output in this format (one per line):
CATEGORY|CONTENT|TRUST

Where:
- CATEGORY is one of: fact, preference, correction, entity
- CONTENT is a concise statement (1-2 sentences max, under 200 characters preferred)
- TRUST is one of: high (user stated), medium (observed), low (inferred)

Output ONLY the formatted lines, no other text. If no NEW memories worth extracting, output nothing."#;

fn system_prompt(existing: &[String]) -> String {
    let mut system = SYSTEM_PROMPT.to_string();
    if !existing.is_empty() {
        system.push_str("\n\nAlready known (do NOT re-extract these or close paraphrases):\n");
        for mem in existing.iter().take(EXISTING_LIMIT) {
            system.push_str("- ");
            system.push_str(crate::util::truncate_str(mem, EXISTING_CHARS));
            system.push('\n');
        }
    }
    system
}

/// Remove `<system-reminder>...</system-reminder>` blocks (an unclosed one runs to the end).
fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn message_chunk(msg: &Message) -> String {
    let role = match msg.role {
        Role::User => "User",
        Role::Assistant => "Assistant",
    };
    let mut body = String::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text, .. } => {
                let text = strip_system_reminders(text);
                let text = text.trim();
                if !text.is_empty() {
                    body.push_str(text);
                    body.push('\n');
                }
            }
            ContentBlock::ToolUse { name, .. } => body.push_str(&format!("[Used tool: {name}]\n")),
            ContentBlock::ToolResult { content, .. } => {
                let preview = if content.len() > 200 {
                    format!("{}...", crate::util::truncate_str(content, 200))
                } else {
                    content.clone()
                };
                body.push_str(&format!("[Result: {preview}]\n"));
            }
            ContentBlock::Image { .. } => body.push_str("[Image]\n"),
            ContentBlock::OpenAICompaction { .. } => body.push_str("[OpenAI native compaction]\n"),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ReasoningTrace { .. }
            | ContentBlock::AnthropicThinking { .. }
            | ContentBlock::OpenAIReasoning { .. } => {}
        }
    }
    if body.is_empty() { String::new() } else { format!("**{role}:**\n{body}\n") }
}

/// The oldest-first window of `messages`: at most `max_messages` messages and `max_chars` chars.
/// Returns the transcript and how many messages it consumed (a message that would overflow the
/// cap is left for the next window, unless it is the first and gets truncated).
fn build_window(messages: &[Message], max_messages: usize, max_chars: usize) -> (String, usize) {
    let mut out = String::new();
    let mut total = 0usize;
    let mut consumed = 0usize;
    for msg in messages.iter().take(max_messages) {
        let chunk = message_chunk(msg);
        if !chunk.is_empty() {
            let len = chunk.chars().count();
            if total + len > max_chars {
                if total == 0 {
                    out.extend(chunk.chars().take(max_chars));
                    consumed += 1;
                }
                break;
            }
            total += len;
            out.push_str(&chunk);
        }
        consumed += 1;
    }
    (out, consumed)
}

struct Job {
    trigger: Trigger,
    session_id: String,
    /// Absolute index of `messages[0]`.
    from: usize,
    /// Every message from `from` to the end of the planned range.
    messages: Vec<Message>,
    transcript: String,
    /// Last message the transcript actually includes: `extracted_through` becomes this on success.
    upto: usize,
}

impl Job {
    fn window(&self) -> (usize, usize) {
        let max_messages = if self.trigger == Trigger::Periodic { PERIODIC_MAX_MESSAGES } else { usize::MAX };
        (max_messages, MAX_TRANSCRIPT_CHARS)
    }

    /// Rebuild the window so it starts at absolute index `start` (>= `from`).
    fn rewindow(&mut self, start: usize) {
        let skip = start.saturating_sub(self.from).min(self.messages.len());
        self.messages.drain(..skip);
        self.from += skip;
        let (max_messages, max_chars) = self.window();
        let (transcript, consumed) = build_window(&self.messages, max_messages, max_chars);
        self.transcript = transcript;
        self.upto = self.from + consumed;
    }

    fn drained(&self) -> bool {
        self.upto >= self.from + self.messages.len()
    }
}

enum Plan {
    Run(Job),
    Skip(&'static str),
}

/// Decide whether to run and build the transcript. `fetch(from)` returns messages `from..total`.
fn plan(
    manager: &MemoryManager,
    trigger: Trigger,
    session_id: &str,
    total: usize,
    enabled: bool,
    fetch: impl FnOnce(usize) -> Vec<Message>,
) -> Plan {
    if !enabled {
        return Plan::Skip("sidecar_off");
    }
    let through = extracted_through(manager, session_id, total);
    if total <= through {
        return Plan::Skip("no_new_messages");
    }
    // SessionEnd and Compaction are exempt: the tail (or the messages about to be cut) must never be
    // dropped. They queue behind any in-flight job on the aux permit and re-read the marker there,
    // so they neither repeat nor skip messages.
    if trigger == Trigger::Periodic {
        if in_flight(session_id) {
            return Plan::Skip("in_flight");
        }
        if cooldown_active(session_id) {
            return Plan::Skip("cooldown");
        }
    }
    let messages = fetch(through);
    let mut job = Job {
        trigger,
        session_id: session_id.to_string(),
        from: through,
        messages,
        transcript: String::new(),
        upto: through,
    };
    job.rewindow(through);
    if job.messages.len() < MIN_MESSAGES || job.transcript.chars().count() < MIN_TRANSCRIPT_CHARS {
        return Plan::Skip("under_floor");
    }
    claim(session_id);
    Plan::Run(job)
}

struct Completion {
    text: String,
    input_tokens: u64,
    output_tokens: u64,
    model: String,
}

type Complete<'a> = &'a (dyn Fn(String, String) -> BoxFuture<'static, Result<Completion>> + Send + Sync);

#[derive(Default)]
struct Outcome {
    existing_shown: usize,
    extracted: usize,
    written: usize,
    merged: usize,
    input_tokens: u64,
    output_tokens: u64,
    model: String,
}

async fn execute(manager: &MemoryManager, job: &Job, complete: Complete<'_>) -> (Outcome, Result<Vec<String>>) {
    let mut outcome = Outcome::default();
    let result = async {
        // The SQLite parts run on the blocking pool, not on a tokio worker.
        let (m, transcript) = (manager.clone(), job.transcript.clone());
        let existing: Vec<String> = tokio::task::spawn_blocking(move || m.related_to(&transcript, EXISTING_LIMIT))
            .await
            .map_err(|e| anyhow!("memory lookup task failed: {e}"))??
            .into_iter()
            .map(|e| e.content)
            .collect();
        outcome.existing_shown = existing.len();
        let completion = tokio::time::timeout(CALL_TIMEOUT, complete(system_prompt(&existing), job.transcript.clone()))
            .await
            .map_err(|_| anyhow!("extraction call timed out"))??;
        outcome.input_tokens = completion.input_tokens;
        outcome.output_tokens = completion.output_tokens;
        outcome.model = completion.model;
        let extracted = parse_extracted(&completion.text);
        outcome.extracted = extracted.len();
        let (m, session) = (manager.clone(), job.session_id.clone());
        let stored = tokio::task::spawn_blocking(move || -> Result<Vec<Remembered>> {
            extracted
                .into_iter()
                .map(|memory| {
                    let entry = MemoryEntry::new(MemoryCategory::from_extracted(&memory.category), memory.content)
                        .with_source(&session)
                        .with_trust(trust_of(&memory.trust));
                    m.remember_extracted(entry)
                })
                .collect()
        })
        .await
        .map_err(|e| anyhow!("memory write task failed: {e}"))??;
        let mut ids = Vec::new();
        for remembered in stored {
            match remembered {
                Remembered::Inserted(_) => outcome.written += 1,
                Remembered::Reinforced(_) | Remembered::Merged { .. } => outcome.merged += 1,
            }
            ids.push(remembered.id().to_string());
        }
        Ok(ids)
    }
    .await;
    (outcome, result)
}

fn read_marker(manager: &MemoryManager, session_id: &str) -> usize {
    manager.meta_get(&through_key(session_id)).ok().flatten().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0)
}

/// Run one planned job to completion and record its spans. Returns the ids written or merged into.
///
/// The job waits for the aux permit (bounded), then re-reads the marker: a job that ran while it
/// waited may have consumed part of its range. A non-periodic job keeps going window by window until
/// its range is consumed, so an oversized backlog is never marked extracted without being read.
async fn run_job(manager: &MemoryManager, mut job: Job, complete: Complete<'_>) -> Vec<String> {
    let mut all_ids = Vec::new();
    let Some(_permit) = aux_call_permit_within(PERMIT_TIMEOUT).await else {
        obs_sink::emit(
            Span::new("memory.extract")
                .session(&job.session_id)
                .attr("trigger", job.trigger.as_str())
                .error("aux permit wait timed out"),
        );
        release(&job.session_id);
        return all_ids;
    };
    let end = job.from + job.messages.len();
    let marker = read_marker(manager, &job.session_id);
    if marker > job.from && marker <= end {
        job.rewindow(marker);
    }
    for window in 0..MAX_WINDOWS {
        if job.messages.is_empty() || job.transcript.chars().count() < MIN_TRANSCRIPT_CHARS {
            if window == 0 {
                skip(job.trigger, &job.session_id, "already_extracted");
            }
            break;
        }
        let started = Instant::now();
        let (outcome, result) = execute(manager, &job, complete).await;
        let mut span = Span::new("memory.extract")
            .session(&job.session_id)
            .attr("trigger", job.trigger.as_str())
            .attr("transcript_chars", job.transcript.chars().count())
            .attr("existing_shown", outcome.existing_shown)
            .attr("extracted", outcome.extracted)
            .attr("written", outcome.written)
            .attr("merged", outcome.merged)
            .attr("tokens", outcome.input_tokens + outcome.output_tokens)
            .attr("model", outcome.model.as_str())
            .tokens(outcome.input_tokens, outcome.output_tokens)
            .took_ms(started.elapsed().as_millis() as u64);
        let ok = match result {
            Ok(ids) => {
                let _ = manager.meta_set(&through_key(&job.session_id), &job.upto.to_string());
                // A closed session must not get a fresh injected-id entry (the map is pruned on close).
                if job.trigger != Trigger::SessionEnd {
                    crate::memory::mark_memories_known(&job.session_id, &ids, "extracted from this session");
                }
                all_ids.extend(ids);
                true
            }
            Err(error) => {
                span = span.error(error.to_string());
                false
            }
        };
        obs_sink::emit(span);
        if !ok || job.trigger == Trigger::Periodic || job.drained() {
            break;
        }
        job.rewindow(job.upto);
    }
    release(&job.session_id);
    all_ids
}

async fn complete_with_active_provider(system: String, prompt: String) -> Result<Completion> {
    let provider = crate::provider::active_provider_fork().ok_or_else(|| anyhow!("no active provider"))?;
    if let Some(model) = crate::config::config().agents.memory_model.as_deref() {
        provider.set_model(model)?;
    }
    let model = provider.model();
    let done = provider.complete_simple_with_usage(&prompt, &system).await?;
    let (input_tokens, output_tokens) = done.usage.map_or((0, 0), |u| (u.input, u.output));
    Ok(Completion { text: done.text, input_tokens, output_tokens, model })
}

fn skip(trigger: Trigger, session_id: &str, reason: &'static str) {
    obs_sink::emit(Span::new("memory.skip").session(session_id).attr("trigger", trigger.as_str()).attr("reason", reason));
}

/// Plan an extraction of the session's messages `..total` and, if it should run, spawn it.
/// `fetch(from)` returns messages `from..total`. Never blocks and never fails the caller.
pub fn spawn(
    trigger: Trigger,
    session_id: &str,
    working_dir: Option<&str>,
    total: usize,
    fetch: impl FnOnce(usize) -> Vec<Message>,
) {
    let manager = match working_dir {
        Some(dir) if !dir.trim().is_empty() => MemoryManager::new().with_project_dir(dir),
        _ => MemoryManager::new(),
    };
    match plan(&manager, trigger, session_id, total, sidecar_enabled(), fetch) {
        Plan::Skip(reason) => skip(trigger, session_id, reason),
        Plan::Run(job) => {
            let work = async move {
                let complete = |system, prompt| -> BoxFuture<'static, Result<Completion>> {
                    Box::pin(complete_with_active_provider(system, prompt))
                };
                run_job(&manager, job, &complete).await;
            };
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(work);
            } else {
                std::thread::spawn(move || {
                    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                        runtime.block_on(work);
                    }
                });
            }
        }
    }
}

#[cfg(test)]
#[path = "memory_extract_tests.rs"]
mod tests;
