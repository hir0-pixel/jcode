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
/// Removes a memory stored through [`Remember`] (rollback / refine delete).
pub type Forget = Arc<dyn Fn(&str) + Send + Sync>;

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

/// Prime's `AUTO_REFINE_REVIEW_SYSTEM_PROMPT` / `parseAutoRefineReview`: one
/// cheap call that decides whether a `/refine` pass is worth its cost.
struct GateReview {
    should_refine: bool,
    instructions: Option<String>,
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
        return GateReview { should_refine: false, instructions: None };
    };
    GateReview {
        should_refine: value["shouldRefine"].as_bool().unwrap_or(false),
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
    let seen = store.watermark(session).min(turns.len());
    let fresh = &turns[seen..];
    if fresh.is_empty() {
        return Ok(refine_summary.map(|s| format!("Refined: {s}.")));
    }

    // The gate: one cheap call, always asked once the turn-interval and
    // cooldown allow it (the caller in `rpc.rs` enforces both before calling
    // `pass` at all). No keyword pre-filter: Prime has none either.
    let (gsystem, guser) = gate_request(turns_since_review, fresh);
    let gate_started = crate::observability::now();
    let greply = complete(gsystem, guser).await;
    conn.observer.record_aux(
        session, "learning", Some("Auto-refine gate"), None, None, gate_started,
        greply.as_ref().ok().and_then(|d| d.usage), greply.as_ref().err().map(|e| e.to_string()).as_deref(),
    );
    store.set_watermark(session, turns.len())?;
    let review = match greply {
        Ok(done) => parse_gate_review(&done.text),
        Err(_) => GateReview { should_refine: false, instructions: None },
    };
    if !review.should_refine {
        return Ok(refine_summary.map(|s| format!("Refined: {s}.")));
    }

    let (system, user) = sovereign_prime::refine::build_request(store, session, fresh, review.instructions.as_deref(), false);
    let started = crate::observability::now();
    let reply = complete(system, user).await?;
    conn.observer.record_aux(
        session, "learning", Some("Auto-refine"), None, None, started, reply.usage, None,
    );
    let cwd = conn.session_cwd(session).await;
    let outcome = with_sink(Some(learning), cwd, |sink| {
        sovereign_prime::refine::apply(store, session, &reply.text, fresh, false, "auto", sink)
    })
    .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let mut parts = vec![outcome.summary];
    if let Some(summary) = &refine_summary {
        parts.push(format!("refine: {summary}"));
    }
    Ok(Some(format!("Learned {}.", parts.join("; "))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_reply_parsing_defaults_to_no() {
        let yes = parse_gate_review("```json\n{\"shouldRefine\": true, \"instructions\": \"save the pnpm rule\"}\n```");
        assert!(yes.should_refine && yes.instructions.as_deref() == Some("save the pnpm rule"));
        assert!(!parse_gate_review("not json at all").should_refine);
        assert!(!parse_gate_review("{\"shouldRefine\": false}").should_refine);
    }

    #[test]
    fn gate_request_is_one_bounded_call_with_the_turn_count() {
        let turns = vec![Turn { role: "user".into(), text: "hello".into() }];
        let (_, user) = gate_request(25, &turns);
        assert!(user.contains("25 assistant turns") && user.contains("hello"));
    }
}
