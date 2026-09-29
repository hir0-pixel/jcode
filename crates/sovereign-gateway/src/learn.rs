//! Automatic learning (the Prime loop) for chats served by this gateway.
//!
//! Ports Prime Agent's `reviewAutoRefine` / `_maybeAutoRefine` semantics
//! (`refinement.ts`, `agent-session.ts`): a per-session turn counter, no
//! idle wait. Once `turn_interval` assistant turns have passed since the
//! last review (and a cooldown has elapsed), one lightweight model call
//! ("the gate") decides whether anything here is worth a `/refine` pass; a
//! "no" costs exactly that one call and resets the counter, same as a "yes".
//! There is no free keyword pre-filter: Prime has none, so this doesn't
//! either. A "yes" runs the existing full-CRUD `/refine` (all four entry
//! kinds, including memory and executable skills) exactly once - the same
//! path the interactive `/refine` command and the model-callable `refine`
//! tool use, so there is one learning pipeline, not two.

use crate::rpc::Conn;
use sovereign_prime::refine::{MemorySink, Turn, parse_json_object, transcript};
use std::sync::Arc;
use std::time::Duration;

/// Stores a proposed memory's text in jcode's own memory store and returns
/// its id; `cwd` is the chat's working directory. Never runs in Python.
pub type Remember = Arc<dyn Fn(&str, &str, bool, Option<&str>) -> anyhow::Result<String> + Send + Sync>;
/// Removes a memory stored through [`Remember`] (rollback / refine delete),
/// returning its `(text, category)` so a rolled-back delete can restore it.
pub type Forget = Arc<dyn Fn(&str) -> Option<(String, String)> + Send + Sync>;

#[derive(Clone)]
pub struct Learning {
    /// Assistant turns since the last auto-refine review before the gate is
    /// asked again (Prime default: 25; `settings.autoRefine.turnInterval`).
    pub turn_interval: usize,
    /// Minimum time between two gate calls, regardless of turn count
    /// (Prime default: 20 minutes; `settings.autoRefine.cooldownMs`).
    pub cooldown: Duration,
    pub remember: Remember,
    pub forget: Forget,
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

/// Runs `f` with the bridge to jcode's memory store (`None` when learning is
/// off, in which case memory entries just carry their text).
pub(crate) fn with_sink<R>(
    learning: Option<&Learning>,
    cwd: Option<String>,
    f: impl FnOnce(Option<&MemorySink<'_>>) -> R,
) -> R {
    let Some(l) = learning else { return f(None) };
    let remember = |text: &str, category: &str, user_stated: bool| {
        (l.remember)(text, category, user_stated, cwd.as_deref())
    };
    let forget = |id: &str| (l.forget)(id);
    f(Some(&MemorySink { remember: &remember, forget: &forget }))
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
fn approved_instructions(review: &GateReview) -> String {
    let mut text = String::from(
        "Automatic refine review approved this checkpoint. Only create/update/delete entries if there is clear \
         evidence that should help this session or future ones; prefer an empty edits array over speculative or \
         one-off memories.",
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
    (empty or not valid JSON) was wrong. Propose the edit the reviewer described: a `memory` entry for a stated \
    fact, preference or correction. Reply with the JSON object only.";

/// The refine pass for an approving gate: one call, plus one corrective retry when it comes back
/// without an applicable edit. `record` sees every model call (reply and start time).
/// The outer `Err` is a failed model call (transient: the checkpoint must be retried); the inner
/// result is what applying the proposal came to (final: nothing more to gain from this window).
async fn refine_approved(
    complete: &crate::Complete,
    store: &sovereign_prime::entries::EntryStore,
    session: &str,
    fresh: &[Turn],
    review: &GateReview,
    learning: &Learning,
    cwd: Option<String>,
    record: &mut (dyn FnMut(&anyhow::Result<jcode_provider_core::SimpleCompletion>, i64) + Send),
) -> anyhow::Result<anyhow::Result<sovereign_prime::refine::RefineOutcome>> {
    let mut instructions = approved_instructions(review);
    let mut attempt = 0;
    loop {
        let (system, user) = sovereign_prime::refine::build_request(store, session, fresh, Some(&instructions), false);
        let started = crate::observability::now();
        let reply = complete(system, user).await;
        record(&reply, started);
        let text = reply?.text;
        match with_sink(Some(learning), cwd.clone(), |sink| {
            sovereign_prime::refine::apply(store, session, &text, fresh, false, "auto", sink)
        }) {
            Err(err) if attempt == 0 && (format!("{err:#}").contains("no durable lesson") || format!("{err:#}").contains("not valid JSON")) => {
                attempt += 1;
                instructions.push_str(APPROVED_RETRY);
            }
            done => return Ok(done),
        }
    }
}

fn gate_request(turns_since_last_review: usize, fresh: &[Turn]) -> (String, String) {
    let system = "You are this agent's automatic /refine review gate. Decide whether this checkpoint should run \
                  /refine. Auto /refine writes local Continual Harness state by default, so approve when the \
                  trajectory contains evidence useful to this session's future turns. Reject one-off noise, \
                  unsupported hypotheses, and transient tool output. Return JSON only: {\"shouldRefine\": true|false, \
                  \"rationale\": <short reason>, \"instructions\": <optional concise instructions for /refine if \
                  shouldRefine is true>}."
        .to_string();
    let user = format!(
        "Trigger: turn_interval; {turns_since_last_review} assistant turns since the last auto-refine review.\n\n\
         Conversation:\n{}",
        transcript(fresh)
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
/// scheduled `/refine` request (if any) always runs first, then the
/// turn-interval gate. `turns_since_review` is the caller's count of
/// assistant turns since the last gate call (kept in `rpc.rs`, which also
/// enforces the turn-interval and cooldown before calling `pass` at all).
/// Returns a short human summary when something ran.
pub(crate) async fn pass(
    conn: &Arc<Conn>,
    session: &str,
    learning: &Learning,
    gate_due: Option<usize>,
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
            let (system, user) = sovereign_prime::refine::build_request(store, session, &turns, instructions.as_deref(), global);
            let started = crate::observability::now();
            let reply = complete(system, user).await;
            conn.observer.record_aux(
                session, "learning", Some("Scheduled refine"), None, None, started,
                reply.as_ref().ok().and_then(|d| d.usage), reply.as_ref().err().map(|e| e.to_string()).as_deref(),
            );
            let cwd = conn.session_cwd(session).await;
            refine_summary = match reply {
                Ok(done) => match with_sink(Some(learning), cwd, |sink| sovereign_prime::refine::apply(store, session, &done.text, &turns, global, "refine-tool", sink)) {
                    Ok(outcome) => Some(outcome.summary),
                    Err(err) => Some(format!("no change ({err:#})")),
                },
                Err(err) => Some(format!("no change ({err:#})")),
            };
        }
    }
    // Not a checkpoint turn: only the scheduled request above was due.
    let (Some(turns_since_review), Some(store)) = (gate_due, &store) else {
        return Ok(refine_summary.map(|s| format!("Refined: {s}.")));
    };
    // An undo or rewind can leave fewer messages than the watermark.
    // The watermark counts raw messages: compaction drops old tool rows, so it can't index `turns`.
    let seen = store.watermark(session).min(raw.len());
    let fresh_owned = compact_tools(&raw[seen..]);
    let fresh = &fresh_owned[..];
    if fresh.is_empty() {
        return Ok(refine_summary.map(|s| format!("Refined: {s}.")));
    }

    let cwd = conn.session_cwd(session).await;
    let learned = checkpoint(&complete, store, session, raw.len(), turns_since_review, fresh, learning, cwd, &mut |title, reply, started| {
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
    turns_since_review: usize,
    fresh: &[Turn],
    learning: &Learning,
    cwd: Option<String>,
    record: &mut (dyn FnMut(&str, &anyhow::Result<jcode_provider_core::SimpleCompletion>, i64) + Send),
) -> anyhow::Result<Option<String>> {
    // The gate: one cheap call, always asked once the turn-interval and cooldown allow it (the
    // caller in `rpc.rs` enforces both). No keyword pre-filter: Prime has none either.
    let (gsystem, guser) = gate_request(turns_since_review, fresh);
    let started = crate::observability::now();
    let greply = complete(gsystem, guser).await;
    record("Auto-refine gate", &greply, started);
    let review = parse_gate_review(&greply?.text);
    let judged = || -> anyhow::Result<()> {
        store.set_watermark(session, raw_len)?;
        store.learn_reviewed(session, crate::observability::now())
    };
    if !review.should_refine {
        judged()?;
        return Ok(None);
    }
    let outcome = refine_approved(complete, store, session, fresh, &review, learning, cwd, &mut |reply, started| {
        record("Auto-refine", reply, started)
    })
    .await?;
    judged()?;
    Ok(Some(outcome.map_err(|e| anyhow::anyhow!("{e:#}"))?.summary))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn an_approved_gate_hands_its_lesson_to_refine_and_a_blank_reply_is_retried_once() {
        let home = std::env::temp_dir().join(format!("learn-handoff-{}", std::process::id()));
        let fresh = vec![Turn { role: "user".into(), text: "From now on write quick scripts in Nim. Remember that.".into() }];
        let review = parse_gate_review(r#"{"shouldRefine": true, "rationale": "stated preference", "instructions": "save: quick scripts in Nim"}"#);
        let lesson = r#"{"summary":"s","rationale":"r","expectedOutcome":"e","edits":[{"action":"create","kind":"memory","title":"Nim","category":"preference","content":"Write quick scripts in Nim"}]}"#;
        let learning = Learning {
            turn_interval: 1,
            cooldown: Duration::ZERO,
            remember: Arc::new(|_, _, _, _| Ok("mem-1".into())),
            forget: Arc::new(|_| None),
        };
        // First reply: the model says nothing durable; the retry produces the edit.
        let (complete, prompts) = scripted(vec![r#"{"summary":"none","edits":[]}"#, lesson]);
        let store = sovereign_prime::entries::EntryStore::open(&home).unwrap();
        let outcome = refine_approved(&complete, &store, "s1", &fresh, &review, &learning, None, &mut |_, _| {}).await.unwrap().unwrap();
        assert_eq!(outcome.created.len(), 1);
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2, "exactly one retry");
        assert!(prompts[0].contains("save: quick scripts in Nim") && prompts[0].contains("stated preference"), "gate handoff: {}", prompts[0]);
        assert!(prompts[0].contains("write quick scripts in Nim") && !prompts[0].contains("previous reply"));
        assert!(prompts[1].contains("previous reply"), "the retry says why");

        // Two blanks in a row is a genuine miss, surfaced (not looped).
        let (complete, prompts) = scripted(vec!["{\"edits\":[]}", "{\"edits\":[]}", lesson]);
        let store = sovereign_prime::entries::EntryStore::open(&home).unwrap();
        let err = refine_approved(&complete, &store, "s1", &fresh, &review, &learning, None, &mut |_, _| {}).await.unwrap().unwrap_err();
        assert!(format!("{err:#}").contains("no durable lesson") && prompts.lock().unwrap().len() == 2);
        std::fs::remove_dir_all(home).ok();
    }

    #[tokio::test]
    async fn a_failed_gate_call_keeps_the_window_and_cooldown_so_the_next_checkpoint_retries() {
        let home = std::env::temp_dir().join(format!("learn-retry-{}", std::process::id()));
        let store = sovereign_prime::entries::EntryStore::open(&home).unwrap();
        let fresh = vec![Turn { role: "user".into(), text: "From now on write quick scripts in Nim.".into() }];
        let learning = Learning {
            turn_interval: 1,
            cooldown: Duration::from_secs(3600),
            remember: Arc::new(|_, _, _, _| Ok("mem-1".into())),
            forget: Arc::new(|_| None),
        };
        let gate_yes = r#"{"shouldRefine": true, "rationale": "preference", "instructions": "save Nim"}"#;
        let lesson = r#"{"summary":"s","rationale":"r","expectedOutcome":"e","edits":[{"action":"create","kind":"memory","title":"Nim","category":"preference","content":"Write quick scripts in Nim"}]}"#;
        let (complete, _) = scripted(vec!["ERR", gate_yes, "ERR", lesson]);
        let run = |complete: &crate::Complete| {
            let (complete, store, fresh, learning) = (complete.clone(), &store, &fresh, &learning);
            async move { checkpoint(&complete, store, "s1", 7, 1, fresh, learning, None, &mut |_, _, _| {}).await }
        };
        // Gate 429: nothing is judged, the checkpoint is still due (cooldown untouched).
        assert!(run(&complete).await.is_err());
        assert_eq!(store.watermark("s1"), 0);
        assert!(store.learn_checkpoint("s1", 1, 3_600_000, crate::observability::now()).unwrap().is_some());
        // Gate approves but the refine call 429s: still not judged.
        assert!(run(&complete).await.is_err());
        assert_eq!(store.watermark("s1"), 0);
        // Next checkpoint succeeds end to end and only now closes the window and starts the cooldown.
        let (complete, _) = scripted(vec![gate_yes, lesson]);
        assert!(run(&complete).await.unwrap().is_some());
        assert_eq!(store.watermark("s1"), 7);
        assert!(store.learn_checkpoint("s1", 1, 3_600_000, crate::observability::now()).unwrap().is_none());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn gate_request_is_one_bounded_call_with_the_turn_count() {
        let turns = vec![Turn { role: "user".into(), text: "hello".into() }];
        let (_, user) = gate_request(25, &turns);
        assert!(user.contains("25 assistant turns") && user.contains("hello"));
    }
}
