//! Automatic memory extraction (jcode's, restored on the single store).
//!
//! A chat is turned into short fact/preference/correction/entity memories by one call to the
//! active provider. Triggers (all outside the gateway): every 12 fresh user turns, session end,
//! before compaction drops messages. Each trigger extracts only the messages after the session's
//! persisted `extracted_through` index, under a 200-char / 4-message floor, a 60 s per-session
//! cooldown and one process-wide aux-call permit. It runs in a spawned task and never fails a
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
/// Transcript cap, newest messages kept.
const MAX_TRANSCRIPT_CHARS: usize = 24_000;
const MIN_TRANSCRIPT_CHARS: usize = 200;
const MIN_MESSAGES: usize = 4;
const COOLDOWN: Duration = Duration::from_secs(60);
const EXISTING_LIMIT: usize = 80;
const EXISTING_CHARS: usize = 150;
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

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
    last_run: Option<Instant>,
}

static SESSIONS: LazyLock<Mutex<HashMap<String, SessionState>>> = LazyLock::new(Default::default);

/// One extraction/learning aux call at a time per process, so a local model is not hit in
/// parallel with the user's own turn. Prime learning takes the same permit.
static AUX_CALLS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub async fn aux_call_permit() -> tokio::sync::SemaphorePermit<'static> {
    AUX_CALLS.acquire().await.expect("aux semaphore is never closed")
}

/// Count a fresh user turn; true on every 12th (the caller then triggers a periodic run).
pub fn note_user_turn(session_id: &str) -> bool {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    let state = sessions.entry(session_id.to_string()).or_default();
    state.turns += 1;
    state.turns.is_multiple_of(PERIODIC_INTERVAL)
}

/// Drop a closed session's counters (upstream's map grew without bound).
pub fn forget_session(session_id: &str) {
    SESSIONS.lock().unwrap_or_else(|p| p.into_inner()).remove(session_id);
}

fn cooldown_active(session_id: &str) -> bool {
    let sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    sessions.get(session_id).and_then(|s| s.last_run).is_some_and(|at| at.elapsed() < COOLDOWN)
}

fn stamp_run(session_id: &str) {
    let mut sessions = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    sessions.entry(session_id.to_string()).or_default().last_run = Some(Instant::now());
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

/// The transcript of `messages`, newest first up to `max_chars`, in chronological order.
fn build_transcript(messages: &[Message], max_messages: usize, max_chars: usize) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut total = 0usize;
    for msg in messages.iter().rev().take(max_messages) {
        let chunk = message_chunk(msg);
        if chunk.is_empty() {
            continue;
        }
        let len = chunk.chars().count();
        if total + len > max_chars {
            if total == 0 {
                chunks.push(chunk.chars().take(max_chars).collect());
            }
            break;
        }
        total += len;
        chunks.push(chunk);
    }
    chunks.reverse();
    chunks.concat()
}

struct Job {
    trigger: Trigger,
    session_id: String,
    transcript: String,
    /// Messages covered: `extracted_through` becomes this once the run succeeds.
    upto: usize,
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
    if cooldown_active(session_id) {
        return Plan::Skip("cooldown");
    }
    let messages = fetch(through);
    let max_messages = if trigger == Trigger::Periodic { PERIODIC_MAX_MESSAGES } else { usize::MAX };
    let transcript = build_transcript(&messages, max_messages, MAX_TRANSCRIPT_CHARS);
    if messages.len() < MIN_MESSAGES || transcript.chars().count() < MIN_TRANSCRIPT_CHARS {
        return Plan::Skip("under_floor");
    }
    stamp_run(session_id);
    Plan::Run(Job {
        trigger,
        session_id: session_id.to_string(),
        transcript,
        upto: total,
    })
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
        let existing: Vec<String> = manager
            .related_to(&job.transcript, EXISTING_LIMIT)?
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
        let mut ids = Vec::new();
        for memory in extracted {
            let entry = MemoryEntry::new(MemoryCategory::from_extracted(&memory.category), memory.content)
                .with_source(&job.session_id)
                .with_trust(trust_of(&memory.trust));
            let remembered = manager.remember_extracted(entry)?;
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

/// Run one planned job to completion and record its span. Returns the ids written or merged into.
async fn run_job(manager: &MemoryManager, job: Job, complete: Complete<'_>) -> Vec<String> {
    let _permit = aux_call_permit().await;
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
    let ids = match result {
        Ok(ids) => {
            let key = through_key(&job.session_id);
            let current = manager.meta_get(&key).ok().flatten().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            let _ = manager.meta_set(&key, &job.upto.max(current).to_string());
            // A closed session must not get a fresh injected-id entry (the map is pruned on close).
            if job.trigger != Trigger::SessionEnd {
                crate::memory::mark_memories_known(&job.session_id, &ids, "extracted from this session");
            }
            ids
        }
        Err(error) => {
            span = span.error(error.to_string());
            Vec::new()
        }
    };
    obs_sink::emit(span);
    ids
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
