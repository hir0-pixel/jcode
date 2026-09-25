//! Automatic learning (the Prime loop) for chats served by this gateway.
//!
//! After every completed turn a timer starts. When it fires and the chat is
//! still idle (no newer turn, nothing running), one pass looks at the messages
//! no earlier pass has seen: with no learning signal it records the watermark
//! and stops (no model call); with one it makes a single call, stores the
//! evidenced memories and applies an evidenced instruction change through the
//! Continual Harness gates. Long chats that never go idle for long get a short
//! timer once ten turns have gone unexamined, like Hermes's review cadence.

use crate::rpc::Conn;
use serde_json::json;
use sovereign_prime::harness::{Harness, Outcome, Turn};
use sovereign_prime::learning::{self, Memory, SkillOutcome};
use std::sync::Arc;
use std::time::Duration;

/// Store learned memories; `cwd` is the chat's working directory. Returns how
/// many were new (duplicates only reinforce).
pub type Remember = Arc<dyn Fn(Vec<Memory>, Option<String>) -> anyhow::Result<usize> + Send + Sync>;

#[derive(Clone)]
pub struct Learning {
    /// Idle time after a turn before a pass runs.
    pub idle: Duration,
    pub remember: Remember,
    /// Prime's auto-refine review (`learning.review`, default off/`false`):
    /// when on, every pass that has a learning signal (the same free gate
    /// above) also asks the model for a full Continual Harness CRUD proposal
    /// (`sovereign_prime::refine`) and applies it through the same
    /// evidence-gated path `/refine` uses. Off, a pass behaves exactly as
    /// before this existed.
    pub review: bool,
}

/// Unexamined turns after which the short timer applies.
pub const CADENCE_TURNS: usize = 10;
pub const CADENCE_IDLE: Duration = Duration::from_secs(15);

/// One learning pass over `session`'s unexamined messages. Returns a short
/// human summary when something was learned.
pub(crate) async fn pass(conn: &Arc<Conn>, session: &str, learning: &Learning) -> anyhow::Result<Option<String>> {
    let complete = conn.config().complete.clone().ok_or_else(|| anyhow::anyhow!("no model available"))?;
    let harness = Harness::new(std::path::Path::new(&conn.config().home));
    let history = conn.history(session).await?;
    let turns: Vec<Turn> = history["messages"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|m| Turn {
                    role: m["role"].as_str().unwrap_or_default().to_string(),
                    text: m["content"].as_str().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    // The model-callable `refine` tool (and the REPL's `refine` host
    // function) never apply mid-turn: they only schedule a request, run here
    // once the turn has actually ended. At most one pending request survives
    // per session (a later call before turn-end just replaces it).
    let store = sovereign_prime::entries::EntryStore::open_cached(std::path::Path::new(&conn.config().home)).ok();
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
            refine_summary = match reply {
                Ok(done) => match sovereign_prime::refine::apply(store, session, &done.text, &turns, global, "refine-tool") {
                    Ok(outcome) => Some(outcome.summary),
                    Err(err) => Some(format!("no change ({err:#})")),
                },
                Err(err) => Some(format!("no change ({err:#})")),
            };
        }
    }
    // An undo or rewind can leave fewer messages than the watermark.
    let seen = harness.watermark(session).min(turns.len());
    let fresh = &turns[seen..];
    if fresh.is_empty() {
        return Ok(refine_summary.map(|s| format!("Refined: {s}.")));
    }
    let signals = learning::signals(fresh);
    if !signals.any() {
        harness.set_watermark(session, turns.len())?;
        return Ok(refine_summary.map(|s| format!("Refined: {s}.")));
    }
    let (system, user) = learning::request(&harness, fresh, signals.effort);
    let started = crate::observability::now();
    let reply = complete(system, user).await;
    conn.observer.record_aux(session, "learning", Some("Learning pass"), None, None, started, reply.as_ref().ok().and_then(|done| done.usage), reply.as_ref().err().map(|err| err.to_string()).as_deref());
    let learned = learning::apply(&harness, &reply?.text, fresh, signals.effort).map_err(anyhow::Error::msg)?;
    let cwd = conn.session_cwd(session).await;
    let stored = if learned.memories.is_empty() { 0 } else { (learning.remember)(learned.memories, cwd)? };
    harness.set_watermark(session, turns.len())?;
    let rule = match &learned.harness {
        Ok(Outcome::Updated { changes, .. }) => Some(changes.clone()),
        _ => None,
    };
    let skill_name = match &learned.skill {
        Ok(Some(SkillOutcome::Created { name })) => Some(format!("new skill \"{name}\"")),
        Ok(Some(SkillOutcome::Updated { name })) => Some(format!("updated skill \"{name}\"")),
        _ => None,
    };
    // Prime's auto-refine review: same signal gate, one extra model call that
    // may propose a full Continual Harness changeset (any of the 4 entry
    // kinds), applied through the same gate `/refine` uses.
    let mut review_summary = None;
    if learning.review {
        if let Some(store) = &store {
            let (rsystem, ruser) = sovereign_prime::refine::build_request(store, session, fresh, None, false);
            let rstarted = crate::observability::now();
            let rreply = complete(rsystem, ruser).await;
            conn.observer.record_aux(
                session, "learning", Some("Auto-refine review"), None, None, rstarted,
                rreply.as_ref().ok().and_then(|d| d.usage), rreply.as_ref().err().map(|e| e.to_string()).as_deref(),
            );
            if let Ok(done) = rreply {
                match sovereign_prime::refine::apply(&store, session, &done.text, fresh, false, "auto") {
                    Ok(outcome) => {
                        harness.log(json!({"op": "auto_refine", "session": session, "changeset": outcome.changeset_id, "summary": outcome.summary}))?;
                        review_summary = Some(outcome.summary);
                    }
                    Err(_) => {} // no durable edit this pass; not an error worth surfacing
                }
            }
        }
    }
    harness.log(json!({
        "op": "learn",
        "session": session,
        "signals": {"correction": signals.correction, "explicit": signals.explicit, "effort": signals.effort},
        "memories_stored": stored,
        "rejected": learned.rejected,
        "rule": rule,
        "rule_rejected": learned.harness.as_ref().err(),
        "skill": skill_name,
        "skill_rejected": learned.skill.as_ref().err(),
    }))?;
    let mut parts = Vec::new();
    if stored > 0 {
        parts.push(format!("{stored} new memor{}", if stored == 1 { "y" } else { "ies" }));
    }
    if let Some(rule) = &rule {
        parts.push(format!("instructions: {rule}"));
    }
    if let Some(skill) = &skill_name {
        parts.push(skill.clone());
    }
    if let Some(summary) = &review_summary {
        parts.push(format!("auto-refine: {summary}"));
    }
    if let Some(summary) = &refine_summary {
        parts.push(format!("refine: {summary}"));
    }
    Ok((!parts.is_empty()).then(|| format!("Learned {}.", parts.join("; "))))
}
