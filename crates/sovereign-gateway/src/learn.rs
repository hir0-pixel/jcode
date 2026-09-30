//! Automatic learning (the Prime loop) for chats served by this gateway.
//!
//! Ports Prime Agent's `reviewAutoRefine` / `_maybeAutoRefine` semantics
//! (`refinement.ts`, `agent-session.ts`): a per-session counter of assistant messages (not error or
//! abort, as Prime counts them), no idle wait. Once `turn_interval` of them have passed since the
//! last review (or a context compaction happened) and a cooldown has elapsed, one lightweight model
//! call ("the gate") decides whether anything here is worth a `/refine` pass; a "no" costs exactly
//! that one call and resets the counter, same as a "yes". There is no free keyword pre-filter:
//! Prime has none, so this doesn't either. A "yes" runs the existing `/refine` (prompt, skill and
//! subagent entries; fact-type memories are jcode extraction's job) exactly once - the same path
//! the interactive `/refine` command and the model-callable `refine` tool use, so there is one
//! learning pipeline, not two. Every step emits a `learning.*` span.

use crate::rpc::Conn;
use jcode_base::obs_sink::{Span, emit};
use serde_json::Value;
use sovereign_prime::refine::{GATE_TRANSCRIPT_CHARS, Turn, parse_json_object, transcript};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct Learning {
    /// Assistant messages since the last auto-refine review before the gate is asked again
    /// (Prime default: 25; `settings.autoRefine.turnInterval`).
    pub turn_interval: usize,
    /// Minimum time between two gate calls, regardless of the count
    /// (Prime default: 20 minutes; `settings.autoRefine.cooldownMs`).
    pub cooldown: Duration,
}

/// Prime's `AutoRefineReason`: why a review is running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Trigger {
    TurnInterval,
    /// A context compaction happened: reviewed whatever the count, once the cooldown allows.
    Compact,
    /// The session is closing: a review that is due runs before it goes.
    Dispose,
}

impl Trigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Trigger::TurnInterval => "turn_interval",
            Trigger::Compact => "compact",
            Trigger::Dispose => "dispose",
        }
    }
}

/// A gate call that is due: how many assistant messages it covers and why it runs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GateDue {
    pub assistants: usize,
    pub trigger: Trigger,
}

/// Prime counts assistant messages that are not error/abort (`agent-session.ts:4656-4659`). The
/// engine's history has one `assistant` row per model response, so a tool-heavy turn yields many.
fn assistants_since(raw: &[Value], seen: usize) -> usize {
    raw.iter().skip(seen).filter(|m| m["role"] == "assistant" && m["is_error"].as_bool() != Some(true)).count()
}

/// Whether a review is due for `session`, given its history `raw`: `Ok` with what to review, or
/// `Err` with the `learning.skip` reason and the assistant count. `compact_pending` is a compaction not yet reviewed.
pub(crate) fn due(
    store: &sovereign_prime::entries::EntryStore,
    session: &str,
    learning: &Learning,
    raw: &[Value],
    trigger: Trigger,
    compact_pending: bool,
    now: i64,
) -> Result<GateDue, (&'static str, usize)> {
    // An undo or rewind can leave fewer messages than the watermark.
    let seen = store.watermark(session).min(raw.len());
    let assistants = assistants_since(raw, seen);
    let cooldown_ms = learning.cooldown.as_millis() as i64;
    match store.learn_checkpoint(session, assistants, learning.turn_interval, cooldown_ms, now, compact_pending) {
        Ok(Some(assistants)) => Ok(GateDue { assistants, trigger: if compact_pending { Trigger::Compact } else { trigger } }),
        Ok(None) if compact_pending || assistants >= learning.turn_interval => Err(("cooldown", assistants)),
        Ok(None) => Err(("below_interval", assistants)),
        Err(_) => Err(("store_error", assistants)),
    }
}

/// One learning model call, taking the process-wide aux permit that memory extraction also takes,
/// so the two never hit a (local) model together or in parallel with the user's own turn.
///
/// Both the permit wait and the call are bounded ([`AUX_TIMEOUT`]): a user's `/refine` must not sit
/// behind background work forever, and one hung provider call must not hold the slot indefinitely.
pub(crate) async fn aux_complete(
    complete: &crate::Complete,
    system: String,
    user: String,
) -> anyhow::Result<jcode_provider_core::SimpleCompletion> {
    aux_complete_within(complete, system, user, AUX_TIMEOUT).await
}

async fn aux_complete_within(
    complete: &crate::Complete,
    system: String,
    user: String,
    limit: Duration,
) -> anyhow::Result<jcode_provider_core::SimpleCompletion> {
    let _permit = jcode_base::memory_extract::aux_call_permit_within(limit)
        .await
        .ok_or_else(|| anyhow::anyhow!("timed out waiting for the model slot (background work is using it)"))?;
    tokio::time::timeout(limit, complete(system, user))
        .await
        .map_err(|_| anyhow::anyhow!("the model call timed out"))?
}

/// How long an aux call may wait for the shared permit, and then run.
pub(crate) const AUX_TIMEOUT: Duration = Duration::from_secs(120);

/// Run synchronous SQLite work without stalling a tokio worker. Only a multi-thread runtime can
/// hand the worker off; on a current-thread one (tests, the fallback thread) it just runs inline.
pub(crate) fn blocking<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// A short class for a rejected proposal. The error text quotes entry titles and contents, so the
/// span records the class only.
fn error_class(err: &anyhow::Error) -> &'static str {
    let text = format!("{err:#}");
    if text.contains("no durable lesson") {
        "no_durable_lesson"
    } else if text.contains("not valid JSON") {
        "invalid_json"
    } else if text.contains("exceeds the limit") {
        "too_many_edits"
    } else if text.contains("rejected:") {
        "gate_rejected"
    } else {
        "store_error"
    }
}

/// A `learning.*` span for one model call: its tokens, duration and error, if any.
fn call_span(
    kind: &'static str,
    session: &str,
    trigger: Trigger,
    reply: &anyhow::Result<jcode_provider_core::SimpleCompletion>,
    started: i64,
) -> Span {
    let mut span = Span::new(kind).session(session).attr("trigger", trigger.as_str());
    span = span.took_ms(crate::observability::now().saturating_sub(started).max(0) as u64);
    match reply {
        Ok(done) => {
            if let Some(u) = done.usage {
                span = span.tokens(u.input, u.output);
            }
            span
        }
        Err(err) => span.error(err.to_string()),
    }
}

/// `config.get learning.enabled`: Prime's auto-refine switch (on by default).
pub(crate) fn learning_enabled(home: &str) -> bool {
    sovereign_prime::entries::EntryStore::open_cached(std::path::Path::new(home)).map(|s| s.learning_enabled()).unwrap_or(true)
}

/// `config.set learning.enabled`: persist the switch (bool or "on"/"off"/"true"/"false").
pub(crate) fn set_learning_enabled(home: &str, value: &serde_json::Value) -> anyhow::Result<bool> {
    let on = value.as_bool().unwrap_or_else(|| {
        !matches!(value.as_str().unwrap_or("true").trim().to_ascii_lowercase().as_str(), "false" | "off" | "0" | "no")
    });
    sovereign_prime::entries::EntryStore::open_cached(std::path::Path::new(home))?
        .set_setting("learning.enabled", if on { "true" } else { "false" })?;
    Ok(on)
}

const MAX_TOOL_LINES: usize = 40;

/// Tool rows carry whole outputs; Prime's transcript shows `[Tool result (name, error)]` per call.
/// Reduce each to one line, `ok: <tool> <first line>` or `fail: ...` (first line <= 120 chars), and
/// keep the newest [`MAX_TOOL_LINES`] so the gate sees outcomes, not file dumps. The engine's
/// `tool_name` / `is_error` are used when present, else a text heuristic. Only user turns feed the apply gate.
fn compact_tools(rows: &[serde_json::Value]) -> Vec<Turn> {
    let mut left = rows.iter().filter(|m| m["role"] == "tool").count().saturating_sub(MAX_TOOL_LINES);
    rows.iter()
        .filter_map(|m| {
            let text = m["content"].as_str().unwrap_or_default();
            if m["role"] != "tool" {
                return Some(Turn { role: m["role"].as_str().unwrap_or_default().to_string(), text: text.to_string() });
            }
            if left > 0 {
                left -= 1;
                return None;
            }
            let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
            let lower = first.to_lowercase();
            let failed = m["is_error"].as_bool().unwrap_or_else(|| {
                ["error", "failed", "traceback", "panicked", "no such file", "permission denied"].iter().any(|k| lower.contains(k))
            });
            let line: String = first.chars().take(120).collect();
            let name = m["tool_name"].as_str().filter(|n| !n.is_empty()).map(|n| format!("{n} ")).unwrap_or_default();
            Some(Turn { role: "tool".into(), text: format!("{} {name}{line}", if failed { "fail:" } else { "ok:" }) })
        })
        .collect()
}

/// Prime's `AUTO_REFINE_REVIEW_SYSTEM_PROMPT` / `parseAutoRefineReview`: one
/// cheap call that decides whether a `/refine` pass is worth its cost.
struct GateReview {
    should_refine: bool,
    rationale: Option<String>,
    instructions: Option<String>,
}

/// Prime's `autoRefineInstructions`: what the approving gate saw (its rationale and
/// instructions) is handed to the refine pass, which would otherwise start blind.
fn approved_instructions(trigger: Trigger, review: &GateReview) -> String {
    let mut text = format!(
        "Automatic refine review triggered by {}. Only create/update/delete entries if there is clear evidence \
         that should help this session or future ones; prefer an empty edits array over speculative or one-off \
         entries. Do not promote anything global unless explicitly requested.",
        trigger.as_str()
    );
    if let Some(r) = &review.rationale {
        text.push_str(&format!(" Reviewer rationale: {r}"));
    }
    if let Some(i) = &review.instructions {
        text.push_str(&format!("\nReviewer instructions: {i}"));
    }
    text
}

/// The gate already found a lesson in this exact window, so an empty or unparsable proposal is a
/// miss to correct once, not a result (Prime records nothing either, but its reviewer is a stronger model).
const APPROVED_RETRY: &str = "\n\nThe reviewer found a durable lesson in this conversation, so your previous reply \
    (empty or not valid JSON) was wrong. Propose the edit the reviewer described: a `prompt` or `subagent` \
    entry (or a `skill` where offered) for a reusable convention, procedure or delegation. Reply with the JSON \
    object only.";

/// The refine pass for an approving gate: one call, plus one corrective retry when it comes back
/// without an applicable edit. `record` sees every model call (reply and start time).
/// The outer `Err` is a failed model call (transient: the checkpoint must be retried); the inner
/// result is what applying the proposal came to (final: nothing more to gain from this window).
async fn refine_approved(
    complete: &crate::Complete,
    store: &sovereign_prime::entries::EntryStore,
    session: &str,
    fresh: &[Turn],
    due: &GateDue,
    review: &GateReview,
    record: &mut (dyn FnMut(&anyhow::Result<jcode_provider_core::SimpleCompletion>, i64) + Send),
) -> anyhow::Result<anyhow::Result<sovereign_prime::refine::RefineOutcome>> {
    let mut instructions = approved_instructions(due.trigger, review);
    let mut attempt = 0;
    loop {
        let (system, user) = blocking(|| sovereign_prime::refine::build_request(store, session, fresh, Some(&instructions), false));
        let started = crate::observability::now();
        let reply = aux_complete(complete, system, user).await;
        record(&reply, started);
        emit(call_span("learning.refine", session, due.trigger, &reply, started).attr("attempt", attempt as u64));
        let text = reply?.text;
        let applied = blocking(|| sovereign_prime::refine::apply(store, session, &text, false, "auto"));
        let span = Span::new("learning.apply").session(session).attr("trigger", due.trigger.as_str());
        emit(match &applied {
            Ok(done) => span
                .attr("approved", true)
                .attr("changeset", done.changeset_id.as_str())
                .attr("edits", serde_json::to_value(&done.by_kind).unwrap_or_default())
                .attr("created", done.created.len() as u64)
                .attr("updated", done.updated.len() as u64)
                .attr("deleted", done.deleted.len() as u64),
            Err(err) => span.attr("approved", false).attr("created", 0u64).attr("updated", 0u64).attr("deleted", 0u64).attr("error_class", error_class(err)),
        });
        match applied {
            Err(err) if attempt == 0 && (format!("{err:#}").contains("no durable lesson") || format!("{err:#}").contains("not valid JSON")) => {
                attempt += 1;
                instructions.push_str(APPROVED_RETRY);
            }
            done => return Ok(done),
        }
    }
}

fn gate_request(store: &sovereign_prime::entries::EntryStore, session: &str, due: &GateDue, fresh: &[Turn]) -> (String, String) {
    let system = "You are this agent's automatic /refine review gate. Decide whether this checkpoint should run \
                  /refine. Auto /refine writes local Continual Harness state by default, so approve when the \
                  trajectory contains evidence useful to this session's future turns. Reject one-off noise, \
                  unsupported hypotheses, and transient tool output. Ask for global refinement only for durable \
                  cross-session lessons or explicitly project-qualified lessons likely to be reused in future \
                  sessions. Return JSON only: {\"shouldRefine\": true|false, \"rationale\": <short reason>, \
                  \"instructions\": <optional concise instructions for /refine if shouldRefine is true>}."
        .to_string();
    let user = format!(
        "<trigger>\n{}; {} assistant turns since the last auto-refine review\n</trigger>\n\n\
         <current_harness_state>\n{}\n</current_harness_state>\n\n\
         <refinement_history>\n{}\n</refinement_history>\n\n\
         <conversation>\n{}\n</conversation>\n\n\
         Return shouldRefine=true when the trajectory contains evidence useful to this session's future turns. \
         Prefer local harness edits for current task progress, temporary blockers, and current-run coordination.",
        due.trigger.as_str(),
        due.assistants,
        sovereign_prime::refine::overview(store, session),
        sovereign_prime::refine::history(store, session),
        transcript(fresh, GATE_TRANSCRIPT_CHARS)
    );
    (system, user)
}

fn parse_gate_review(reply: &str) -> GateReview {
    let Some(value) = parse_json_object(reply) else {
        return GateReview { should_refine: false, rationale: None, instructions: None };
    };
    GateReview {
        should_refine: value["shouldRefine"].as_bool().unwrap_or(false),
        rationale: value["rationale"].as_str().map(str::to_string),
        instructions: value["instructions"].as_str().map(str::to_string),
    }
}

/// One learning checkpoint over `session`'s unexamined messages: the pending
/// scheduled `/refine` request (if any) always runs first, then the gate.
/// `gate` is `Some` when [`due`] said a review is due (the caller in `rpc.rs`
/// enforces the interval, cooldown and busy checks before calling `pass` at all).
/// Returns a short human summary when something ran.
pub(crate) async fn pass(
    conn: &Arc<Conn>,
    session: &str,
    gate: Option<GateDue>,
) -> anyhow::Result<Option<String>> {
    let complete = conn.config().complete.clone().ok_or_else(|| anyhow::anyhow!("no model available"))?;
    let history = conn.history(session).await?;
    let raw = history["messages"].as_array().map(Vec::as_slice).unwrap_or_default();
    let turns = compact_tools(raw);
    // The model-callable `refine` tool (and the REPL's `refine` host
    // function) never apply mid-turn: they only schedule a request, run here
    // once the turn has actually ended. At most one pending request survives
    // per session (a later call before turn-end just replaces it).
    let store = match sovereign_prime::entries::EntryStore::open_cached(std::path::Path::new(&conn.config().home)) {
        Ok(store) => Some(store),
        Err(err) => {
            static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("learning is off: cannot open the entry store: {err:#}");
            }
            return Err(anyhow::anyhow!("learning is off: cannot open the entry store ({err:#})"));
        }
    };
    let mut refine_summary = None;
    if let Some(store) = &store {
        if let Ok(Some((instructions, global))) = store.take_pending_refine(session) {
            let (system, user) = blocking(|| sovereign_prime::refine::build_request(store, session, &turns, instructions.as_deref(), global));
            let started = crate::observability::now();
            let reply = aux_complete(&complete, system, user).await;
            conn.observer.record_aux(
                session, "learning", Some("Scheduled refine"), None, None, started,
                reply.as_ref().ok().and_then(|d| d.usage), reply.as_ref().err().map(|e| e.to_string()).as_deref(),
            );
            refine_summary = match reply {
                Ok(done) => match blocking(|| sovereign_prime::refine::apply(store, session, &done.text, global, "refine-tool")) {
                    Ok(outcome) => Some(outcome.summary),
                    Err(err) => Some(format!("no change ({err:#})")),
                },
                Err(err) => Some(format!("no change ({err:#})")),
            };
        }
    }
    // Not a checkpoint turn: only the scheduled request above was due.
    let (Some(due), Some(store)) = (gate, &store) else {
        return Ok(refine_summary.map(|s| format!("Refined: {s}.")));
    };
    // The watermark counts raw messages: compaction drops old tool rows, so it can't index `turns`.
    let seen = store.watermark(session).min(raw.len());
    let fresh_owned = compact_tools(&raw[seen..]);
    let fresh = &fresh_owned[..];
    if fresh.is_empty() {
        emit(Span::new("learning.skip").session(session).attr("trigger", due.trigger.as_str()).attr("reason", "no_new_messages"));
        return Ok(refine_summary.map(|s| format!("Refined: {s}.")));
    }

    let learned = checkpoint(&complete, store, session, raw.len(), &due, fresh, &mut |title, reply, started| {
        conn.observer.record_aux(
            session, "learning", Some(title), None, None, started,
            reply.as_ref().ok().and_then(|d| d.usage), reply.as_ref().err().map(|e| e.to_string()).as_deref(),
        );
    })
    .await?;
    Ok(match (learned, refine_summary) {
        (Some(summary), Some(refined)) => Some(format!("Learned {summary}; refine: {refined}.")),
        (Some(summary), None) => Some(format!("Learned {summary}.")),
        (None, refined) => refined.map(|s| format!("Refined: {s}.")),
    })
}

/// One auto-refine checkpoint over `fresh` (the messages after the watermark): the gate call,
/// then, when it approves, the refine pass. The watermark and cooldown only move once the window
/// has been judged - a failed model call (429, timeout) leaves both so the next checkpoint
/// re-reviews the same evidence instead of skipping it for good. Returns what was learned.
async fn checkpoint(
    complete: &crate::Complete,
    store: &sovereign_prime::entries::EntryStore,
    session: &str,
    raw_len: usize,
    due: &GateDue,
    fresh: &[Turn],
    record: &mut (dyn FnMut(&str, &anyhow::Result<jcode_provider_core::SimpleCompletion>, i64) + Send),
) -> anyhow::Result<Option<String>> {
    // The gate: one cheap call, always asked once the interval and cooldown allow it (the
    // caller in `rpc.rs` enforces both). No keyword pre-filter: Prime has none either.
    let (gsystem, guser) = blocking(|| gate_request(store, session, due, fresh));
    let started = crate::observability::now();
    let greply = aux_complete(complete, gsystem, guser).await;
    record("Auto-refine gate", &greply, started);
    let gate_span = call_span("learning.gate", session, due.trigger, &greply, started).attr("assistants", due.assistants as u64);
    let review = match &greply {
        Ok(done) => parse_gate_review(&done.text),
        Err(_) => GateReview { should_refine: false, rationale: None, instructions: None },
    };
    emit(gate_span.attr("approved", review.should_refine));
    greply?;
    let judged = || -> anyhow::Result<()> {
        blocking(|| {
            store.set_watermark(session, raw_len)?;
            store.learn_reviewed(session, crate::observability::now())
        })
    };
    if !review.should_refine {
        judged()?;
        return Ok(None);
    }
    let outcome = refine_approved(complete, store, session, fresh, due, &review, &mut |reply, started| {
        record("Auto-refine", reply, started)
    })
    .await?;
    judged()?;
    Ok(Some(outcome.map_err(|e| anyhow::anyhow!("{e:#}"))?.summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> crate::Complete {
        std::sync::Arc::new(|_s, _u| Box::pin(async { std::future::pending().await }))
    }

    #[tokio::test]
    async fn a_busy_slot_or_a_hung_model_fails_the_aux_call_instead_of_queueing_forever() {
        let hung = aux_complete_within(&never(), "s".into(), "u".into(), Duration::from_millis(50)).await.err().expect("fails");
        assert!(hung.to_string().contains("timed out"), "{hung}");
        let held = jcode_base::memory_extract::aux_call_permit().await;
        let busy = aux_complete_within(&never(), "s".into(), "u".into(), Duration::from_millis(50)).await.err().expect("fails");
        assert!(busy.to_string().contains("model slot"), "{busy}");
        drop(held);
    }

    #[test]
    fn the_apply_span_error_is_a_class_never_the_text_with_titles() {
        let err = anyhow::anyhow!("rejected: edit for \"Secret client plan\" appears to contain a secret");
        assert_eq!(error_class(&err), "gate_rejected");
        assert_eq!(error_class(&anyhow::anyhow!("no durable lesson in this session")), "no_durable_lesson");
        assert_eq!(error_class(&anyhow::anyhow!("the refine reply was not valid JSON")), "invalid_json");
    }

    #[test]
    fn learning_is_on_by_default_and_config_set_persists_it() {
        let home = std::env::temp_dir().join(format!("learning-setting-{}", std::process::id()));
        let home = home.to_str().unwrap();
        assert!(learning_enabled(home));
        assert!(!set_learning_enabled(home, &serde_json::json!("off")).unwrap());
        assert!(!learning_enabled(home));
        assert!(set_learning_enabled(home, &serde_json::json!(true)).unwrap());
        assert!(learning_enabled(home));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn tool_rows_become_one_bounded_outcome_line_and_only_the_newest_are_kept() {
        use serde_json::json;
        let row = |role: &str, text: String| json!({ "role": role, "content": text });
        let mut rows = vec![row("user", "fix it".into())];
        rows.push(row("tool", format!("\nerror: cannot find x {}\nmore", "y".repeat(300))));
        for i in 0..MAX_TOOL_LINES {
            rows.push(row("tool", format!("done {i}\n{}", "z".repeat(5000))));
        }
        let out = compact_tools(&rows);
        let tools: Vec<_> = out.iter().filter(|t| t.role == "tool").collect();
        assert_eq!(tools.len(), MAX_TOOL_LINES, "the oldest (the failure) fell off the cap");
        assert_eq!(tools[0].text, "ok: done 0");
        let one = compact_tools(&[row("tool", format!("Error: boom {}", "q".repeat(300)))]);
        assert!(one[0].text.starts_with("fail: Error: boom") && one[0].text.len() <= 126);
        assert_eq!(out[0].text, "fix it");
        // The engine's tool name and error flag win over the text heuristic.
        let named = compact_tools(&[
            json!({ "role": "tool", "content": "Error: not really\n", "tool_name": "bash", "is_error": false }),
            json!({ "role": "tool", "content": "exit 2", "tool_name": "grep", "is_error": true }),
        ]);
        assert_eq!((named[0].text.as_str(), named[1].text.as_str()), ("ok: bash Error: not really", "fail: grep exit 2"));
    }

    #[test]
    fn watermark_counts_raw_messages_so_compaction_never_skips_unseen_turns() {
        use serde_json::json;
        let mut rows = vec![json!({ "role": "user", "content": "one" })];
        for i in 0..MAX_TOOL_LINES + 10 {
            rows.push(json!({ "role": "tool", "content": format!("done {i}") }));
        }
        let seen = rows.len(); // pass 1 stores the raw count
        rows.push(json!({ "role": "user", "content": "second" }));
        let fresh = compact_tools(&rows[seen..]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].text, "second");
    }

    #[test]
    fn gate_reply_parsing_defaults_to_no() {
        let yes = parse_gate_review("```json\n{\"shouldRefine\": true, \"instructions\": \"save the pnpm rule\"}\n```");
        assert!(yes.should_refine && yes.instructions.as_deref() == Some("save the pnpm rule"));
        assert!(!parse_gate_review("not json at all").should_refine);
        assert!(!parse_gate_review("{\"shouldRefine\": false}").should_refine);
    }

    /// An entry store in its own temp home, removed on drop.
    struct TempStore(sovereign_prime::entries::EntryStore, std::path::PathBuf);

    impl TempStore {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let home = std::env::temp_dir().join(format!("learn-store-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            Self(sovereign_prime::entries::EntryStore::open(&home).unwrap(), home)
        }
    }

    impl std::ops::Deref for TempStore {
        type Target = sovereign_prime::entries::EntryStore;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.1).ok();
        }
    }

    fn scripted(replies: Vec<&str>) -> (crate::Complete, Arc<std::sync::Mutex<Vec<String>>>) {
        let replies = Arc::new(std::sync::Mutex::new(replies.into_iter().map(String::from).collect::<std::collections::VecDeque<_>>()));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = prompts.clone();
        let complete: crate::Complete = Arc::new(move |_system, user| {
            seen.lock().unwrap().push(user);
            let text = replies.lock().unwrap().pop_front().unwrap_or_default();
            Box::pin(async move {
                if text == "ERR" {
                    anyhow::bail!("429 rate limited");
                }
                Ok(jcode_provider_core::SimpleCompletion { text, usage: None })
            })
        });
        (complete, prompts)
    }

    const LESSON: &str = r#"{"summary":"s","rationale":"r","expectedOutcome":"e","edits":[{"action":"create","kind":"prompt","title":"Nim","content":"Write quick scripts in Nim"}]}"#;

    fn gate_due() -> GateDue {
        GateDue { assistants: 25, trigger: Trigger::TurnInterval }
    }

    fn msgs(assistants: usize) -> Vec<Value> {
        let mut rows = vec![serde_json::json!({ "role": "user", "content": "do the thing" })];
        for i in 0..assistants {
            rows.push(serde_json::json!({ "role": "assistant", "content": format!("step {i}") }));
            rows.push(serde_json::json!({ "role": "tool", "content": "ok", "tool_name": "bash" }));
        }
        rows
    }

    fn learning(interval: usize, cooldown: Duration) -> Learning {
        Learning { turn_interval: interval, cooldown }
    }

    #[test]
    fn one_prompt_with_25_assistant_messages_triggers_a_review_and_24_do_not() {
        let store = TempStore::new();
        let l = learning(25, Duration::from_secs(1200));
        assert_eq!(due(&store, "s1", &l, &msgs(24), Trigger::TurnInterval, false, 1_000).unwrap_err().0, "below_interval");
        // Tool rows and errored assistant rows do not count.
        let mut errored = msgs(24);
        errored.push(serde_json::json!({ "role": "assistant", "content": "", "is_error": true }));
        assert_eq!(due(&store, "s1", &l, &errored, Trigger::TurnInterval, false, 1_001).unwrap_err().0, "below_interval");
        let d = due(&store, "s1", &l, &msgs(25), Trigger::TurnInterval, false, 1_002).unwrap();
        assert_eq!((d.assistants, d.trigger), (25, Trigger::TurnInterval));
    }

    #[test]
    fn only_messages_after_the_last_review_are_counted() {
        let store = TempStore::new();
        let l = learning(25, Duration::ZERO);
        store.set_watermark("s1", msgs(10).len()).unwrap();
        assert_eq!(due(&store, "s1", &l, &msgs(30), Trigger::TurnInterval, false, 1_000).unwrap_err(), ("below_interval", 20));
        assert_eq!(due(&store, "s1", &l, &msgs(35), Trigger::TurnInterval, false, 1_001).unwrap().assistants, 25);
    }

    #[test]
    fn compaction_and_dispose_triggers_and_the_cooldown() {
        let store = TempStore::new();
        let l = learning(25, Duration::from_millis(1_000));
        let few = msgs(3);
        // Compaction reviews whatever the count; dispose does not.
        assert_eq!(due(&store, "s1", &l, &few, Trigger::Dispose, false, 5_000).unwrap_err().0, "below_interval");
        assert_eq!(due(&store, "s1", &l, &few, Trigger::TurnInterval, true, 5_000).unwrap().trigger, Trigger::Compact);
        assert_eq!(due(&store, "s1", &l, &msgs(25), Trigger::Dispose, false, 5_000).unwrap().trigger, Trigger::Dispose);
        // Once a review ran, both are deferred while cooling, then due again.
        store.learn_reviewed("s1", 5_000).unwrap();
        assert_eq!(due(&store, "s1", &l, &few, Trigger::TurnInterval, true, 5_500).unwrap_err().0, "cooldown");
        assert_eq!(due(&store, "s1", &l, &msgs(60), Trigger::TurnInterval, false, 5_500).unwrap_err().0, "cooldown");
        assert_eq!(due(&store, "s1", &l, &few, Trigger::TurnInterval, true, 6_001).unwrap().trigger, Trigger::Compact);
    }

    #[tokio::test]
    async fn an_approved_gate_hands_its_lesson_to_refine_and_a_blank_reply_is_retried_once() {
        let home = std::env::temp_dir().join(format!("learn-handoff-{}", std::process::id()));
        let fresh = vec![Turn { role: "user".into(), text: "From now on write quick scripts in Nim. Remember that.".into() }];
        let review = parse_gate_review(r#"{"shouldRefine": true, "rationale": "stated preference", "instructions": "save: quick scripts in Nim"}"#);
        // First reply: the model says nothing durable; the retry produces the edit.
        let (complete, prompts) = scripted(vec![r#"{"summary":"none","edits":[]}"#, LESSON]);
        let store = sovereign_prime::entries::EntryStore::open(&home).unwrap();
        let outcome = refine_approved(&complete, &store, "s1", &fresh, &gate_due(), &review, &mut |_, _| {}).await.unwrap().unwrap();
        assert_eq!(outcome.created.len(), 1);
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2, "exactly one retry");
        assert!(prompts[0].contains("save: quick scripts in Nim") && prompts[0].contains("stated preference"), "gate handoff: {}", prompts[0]);
        assert!(prompts[0].contains("write quick scripts in Nim") && !prompts[0].contains("previous reply"));
        assert!(prompts[1].contains("previous reply"), "the retry says why");

        // Two blanks in a row is a genuine miss, surfaced (not looped).
        let (complete, prompts) = scripted(vec!["{\"edits\":[]}", "{\"edits\":[]}", LESSON]);
        let store = sovereign_prime::entries::EntryStore::open(&home).unwrap();
        let err = refine_approved(&complete, &store, "s1", &fresh, &gate_due(), &review, &mut |_, _| {}).await.unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("no durable lesson") && prompts.lock().unwrap().len() == 2);
        std::fs::remove_dir_all(home).ok();
    }

    #[tokio::test]
    async fn a_memory_proposal_is_rejected_and_reported_in_the_apply_span() {
        let home = std::env::temp_dir().join(format!("learn-memory-{}", std::process::id()));
        let store = sovereign_prime::entries::EntryStore::open(&home).unwrap();
        let fresh = vec![Turn { role: "user".into(), text: "I prefer Nim.".into() }];
        let review = parse_gate_review(r#"{"shouldRefine": true}"#);
        let memory = r#"{"summary":"s","edits":[{"action":"create","kind":"memory","title":"Nim","category":"preference","content":"Prefers Nim"}]}"#;
        let spans = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = spans.clone();
        jcode_base::obs_sink::install(move |span| sink.lock().unwrap().push(span));
        let (complete, _) = scripted(vec![memory]);
        let err = refine_approved(&complete, &store, "memory-span", &fresh, &gate_due(), &review, &mut |_, _| {}).await.unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("memory extraction"), "{err:#}");
        assert!(store.list_visible("memory-span", None).unwrap().is_empty());
        let spans = spans.lock().unwrap();
        let apply = spans.iter().find(|s| s.kind == "learning.apply" && s.session_id.as_deref() == Some("memory-span")).expect("apply span");
        assert_eq!(apply.attributes["approved"], false);
        assert_eq!(apply.attributes["error_class"], "gate_rejected");
        assert!(apply.attributes.get("rejected").is_none(), "the span carries a class, not the error text");
        assert_eq!(apply.attributes["created"], 0);
        std::fs::remove_dir_all(home).ok();
    }

    #[tokio::test]
    async fn a_failed_gate_call_keeps_the_window_and_cooldown_so_the_next_checkpoint_retries() {
        let home = std::env::temp_dir().join(format!("learn-retry-{}", std::process::id()));
        let store = sovereign_prime::entries::EntryStore::open(&home).unwrap();
        let fresh = vec![Turn { role: "user".into(), text: "From now on write quick scripts in Nim.".into() }];
        let gate_yes = r#"{"shouldRefine": true, "rationale": "preference", "instructions": "save Nim"}"#;
        let (complete, _) = scripted(vec!["ERR", gate_yes, "ERR", LESSON]);
        let run = |complete: &crate::Complete| {
            let (complete, store, fresh) = (complete.clone(), &store, &fresh);
            async move { checkpoint(&complete, store, "s1", 7, &gate_due(), fresh, &mut |_, _, _| {}).await }
        };
        // Gate 429: nothing is judged, the checkpoint is still due (cooldown untouched).
        assert!(run(&complete).await.is_err());
        assert_eq!(store.watermark("s1"), 0);
        assert!(store.learn_checkpoint("s1", 25, 25, 3_600_000, crate::observability::now(), false).unwrap().is_some());
        // Gate approves but the refine call 429s: still not judged.
        assert!(run(&complete).await.is_err());
        assert_eq!(store.watermark("s1"), 0);
        // Next checkpoint succeeds end to end and only now closes the window and starts the cooldown.
        let (complete, _) = scripted(vec![gate_yes, LESSON]);
        assert!(run(&complete).await.unwrap().is_some());
        assert_eq!(store.watermark("s1"), 7);
        assert!(store.learn_checkpoint("s1", 25, 25, 3_600_000, crate::observability::now(), false).unwrap().is_none());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn the_gate_sees_the_harness_overview_and_history_and_the_transcript_is_capped_at_40k() {
        let store = TempStore::new();
        sovereign_prime::refine::apply(&store, "s1", LESSON, false, "refine").unwrap();
        let turns = vec![Turn { role: "user".into(), text: "hello".into() }];
        let (_, user) = gate_request(&store, "s1", &GateDue { assistants: 25, trigger: Trigger::Compact }, &turns);
        assert!(user.contains("compact; 25 assistant turns") && user.contains("hello"));
        assert!(user.contains("prompt: 1") && user.contains("Nim") && user.contains("Expected outcome: e"), "{user}");
        let long: Vec<Turn> = (0..3000).map(|i| Turn { role: "user".into(), text: format!("m{i} {}", "x".repeat(50)) }).collect();
        let (_, user) = gate_request(&store, "s1", &gate_due(), &long);
        let conversation = user.split("<conversation>").nth(1).unwrap().split("</conversation>").next().unwrap();
        assert!(conversation.len() <= GATE_TRANSCRIPT_CHARS + 2 && conversation.contains("m2999"));
    }

    #[test]
    fn the_approved_instructions_keep_primes_no_global_promotion_rule() {
        let review = GateReview { should_refine: true, rationale: Some("why".into()), instructions: None };
        let text = approved_instructions(Trigger::Dispose, &review);
        assert!(text.contains("triggered by dispose") && text.contains("Do not promote anything global unless explicitly requested."));
    }
}
