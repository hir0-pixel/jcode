//! The one engine-level owner of goal / autonomous / heartbeat continuations.
//!
//! It is not a desktop window: a single task started with the gateway holds
//! its own engine link (`Conn { driver: true }`), so goals keep going with no
//! window attached, resume after an engine restart, and heartbeats fire from a
//! timer. The link watches only sessions that have an active goal, loop or
//! heartbeat, and yields (renders and observes nothing) for any session a
//! window has open, since that window's own connection does all of it.
//! Continuations are sent through the normal `prompt.submit` path, so tracing
//! and learning see them like any other turn.
//!
//! No idle cost: with no active goal, loop or heartbeat the task just waits
//! for a poke (a finished turn, `/goal`, `/heartbeat`, `session.control`).

use super::*;
use sovereign_prime::agent_loop::{Continuation, ControlStore, ErrorAction, after_error_turn, after_turn_in, apply_supervisor, due_heartbeat, resume_prompt, retry_due};
use std::sync::OnceLock;
use tokio::sync::Notify;

/// Heartbeat resolution while something is active (heartbeats are >= 60 s).
const TICK: Duration = Duration::from_secs(15);

struct Driver {
    conn: Arc<Conn>,
    wake: Notify,
    /// Sessions found active when this process started, until their resume prompt is accepted.
    resume: std::sync::Mutex<Option<std::collections::HashSet<String>>>,
}

static DRIVER: OnceLock<Arc<Driver>> = OnceLock::new();

/// Something may have become active: re-scan now.
pub(crate) fn poke() {
    if let Some(driver) = DRIVER.get() {
        driver.wake.notify_one();
    }
}

/// Start the driver task (once, with the gateway).
pub(crate) fn start(config: Arc<Config>, hub: Arc<Hub>, observer: Arc<Observer>) {
    tokio::spawn(async move {
        let (to_ws, mut ws_out) = mpsc::channel::<Message>(1024);
        tokio::spawn(async move { while ws_out.recv().await.is_some() {} });
        let client = Arc::new(Client {
            id: hub.next_client_id(),
            to_ws: to_ws.clone(),
            sessions: Mutex::new(Default::default()),
        });
        let conn = Arc::new(Conn {
            config,
            to_ws,
            control: Mutex::new(None),
            links: Mutex::new(HashMap::new()),
            link_tasks: Mutex::new(Vec::new()),
            known: Mutex::new(HashMap::new()),
            fresh: Mutex::new(std::collections::HashSet::new()),
            learning_now: Mutex::new(std::collections::HashSet::new()),
            driver: true,
            next_id: AtomicU64::new(1),
            next_server_request: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT)),
            accept_waiters: Mutex::new(HashMap::new()),
            upstream: Mutex::new(None),
            next_forward: AtomicU64::new(1),
            hub,
            client,
            observer,
            run_kind: "invoke_agent",
            run_title: None,
            replay_of: None,
        });
        match conn.open_link().await {
            Ok(link) => *conn.control.lock().await = Some(link),
            Err(err) => return eprintln!("sovereign: goal driver could not reach the engine: {err:#}"),
        }
        let driver = Arc::new(Driver { conn, wake: Notify::new(), resume: Default::default() });
        let _ = DRIVER.set(driver.clone());
        loop {
            let active = driver.tick().await;
            if active {
                tokio::select! {
                    _ = tokio::time::sleep(TICK) => {}
                    _ = driver.wake.notified() => {}
                }
            } else {
                driver.wake.notified().await;
            }
        }
    });
}

/// The user approved a command this goal session was denied while unattended: tell it so it can
/// go on. Only sessions with an active goal, loop or heartbeat are resumed (a cron or bot run has
/// ended; its late approval just lets the next run through).
pub(crate) fn resume(session_id: &str, command: &str) {
    let Some(driver) = DRIVER.get().cloned() else { return };
    let (sid, text) = (
        session_id.to_string(),
        format!("The user has now approved this command: {command}\nYou may run it; carry on with the task."),
    );
    tokio::spawn(async move {
        let active = ControlStore::open_cached(Path::new(&driver.conn.config.home)).and_then(|s| s.active_sessions());
        if active.is_ok_and(|a| a.contains(&sid)) {
            if let Err(err) = driver.conn.dispatch("prompt.submit", &json!({ "session_id": sid, "text": text })).await {
                eprintln!("sovereign: resume {sid} after approval: {}", err.message);
            }
        }
    });
}

fn prompt_of(continuation: Continuation) -> String {
    match continuation {
        Continuation::Goal(p) | Continuation::Autonomous(p) => p,
        Continuation::Heartbeat { prompt, .. } => prompt,
    }
}

/// Prompts to send now: for each idle active session a due heartbeat, and for the sessions in
/// `resume` (active at process start, so nothing is running) the goal or loop continuation, so
/// work resumes where it stopped. A session stays in `resume` until the caller has sent its prompt.
fn due_work(store: &ControlStore, active: &[String], resume: &mut std::collections::HashSet<String>, busy: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    resume.retain(|sid| active.contains(sid) && !busy(sid));
    active
        .iter()
        .filter(|sid| !busy(sid))
        .filter_map(|sid| {
            let resumed = if resume.contains(sid) {
                let cont = resume_prompt(store, sid).ok().flatten();
                if cont.is_none() {
                    resume.remove(sid);
                }
                cont
            } else {
                None
            };
            let cont = resumed.or_else(|| due_heartbeat(store, sid).ok().flatten())?;
            Some((sid.clone(), prompt_of(cont)))
        })
        .collect()
}

impl Driver {
    /// One scan: watch the active sessions, send due work. `false` when
    /// nothing is active (the task then sleeps until poked).
    async fn tick(&self) -> bool {
        let conn = &self.conn;
        let Ok(store) = ControlStore::open_cached(Path::new(&conn.config.home)) else {
            return false;
        };
        let active = store.active_sessions().unwrap_or_default();
        let first = {
            let mut resume = self.resume.lock().unwrap();
            let first = resume.is_none();
            resume.get_or_insert_with(|| active.iter().cloned().collect());
            first
        };
        for sid in &active {
            if let Err(err) = conn.ensure_attached(sid).await {
                if first {
                    eprintln!("sovereign: goal driver cannot attach {sid}: {err:#}");
                }
            }
        }
        let stale: Vec<String> = conn.links.lock().await.keys().filter(|k| !active.contains(k)).cloned().collect();
        for sid in stale {
            conn.links.lock().await.remove(&sid);
            conn.sessions.lock().await.remove(&sid);
            conn.known.lock().await.remove(&sid);
        }
        conn.link_tasks.lock().await.retain(|task| !task.is_finished());
        let busy: std::collections::HashSet<String> = {
            let sessions = conn.sessions.lock().await;
            active
                .iter()
                .filter(|sid| sessions.get(*sid).is_some_and(SessionState::turn_active) || conn.observer.has_active_run(sid))
                .cloned()
                .collect()
        };
        let work = due_work(&store, &active, self.resume.lock().unwrap().as_mut().unwrap(), |sid| busy.contains(sid));
        for (sid, text) in work {
            match conn.dispatch("prompt.submit", &json!({ "session_id": sid, "text": text })).await {
                Ok(_) => {
                    self.resume.lock().unwrap().as_mut().unwrap().remove(&sid);
                }
                Err(err) => eprintln!("sovereign: goal driver submit for {sid}: {}", err.message),
            }
        }
        !active.is_empty()
    }
}

/// AVO self-supervisor: at most one cheap aux call per plateau episode (and
/// never within 5 turns of the last), asking for 2-3 alternative strategies
/// injected once into this continuation. Any failure keeps the text steer.
async fn supervise(conn: &Arc<Conn>, store: &ControlStore, sid: &str, fallback: String) -> String {
    let Ok(Some(goal)) = store.get_goal(sid) else { return fallback };
    if !goal.supervisor_due() {
        return fallback;
    }
    let (system, user) = goal.supervisor_request();
    let started = crate::observability::now();
    let reply = match conn.config.complete.clone() {
        Some(complete) => complete(system, user).await,
        None => Err(anyhow::anyhow!("no model available")),
    };
    conn.observer.record_aux(
        sid, "other", Some("Goal supervisor"), None, None, started,
        reply.as_ref().ok().and_then(|d| d.usage), reply.as_ref().err().map(|e| e.to_string()).as_deref(),
    );
    apply_supervisor(store, sid, reply.ok().as_ref().map(|d| d.text.as_str())).unwrap_or(fallback)
}

/// After a completed turn on `conn`: maybe inject the goal / loop / heartbeat
/// continuation through that connection, then poke the driver so it watches
/// (or stops watching) the session.
pub(super) fn turn_done(conn: Arc<Conn>, session_id: String, payload: Value) {
    tokio::spawn(async move {
        let Ok(store) = ControlStore::open_cached(Path::new(&conn.config.home)) else {
            return;
        };
        let usage = &payload["usage"];
        let tokens = usage["total"]
            .as_u64()
            .unwrap_or_else(|| usage["input"].as_u64().unwrap_or(0) + usage["output"].as_u64().unwrap_or(0)) as i64;
        let interrupted = payload["status"].as_str() == Some("interrupted");
        let busy = || async { conn.sessions.lock().await.get(&session_id).is_some_and(SessionState::turn_active) };
        if payload["status"].as_str() == Some("error") {
            let error = payload["error"].as_str().unwrap_or("model error");
            match after_error_turn(&store, &session_id, error) {
                Ok(Some(ErrorAction::Paused(text))) => conn.emit("status.update", Some(&session_id), json!({ "kind": "status", "text": format!("Goal paused: {text}") })).await,
                Ok(Some(ErrorAction::Retry { after, stamp, prompt, note })) => {
                    conn.emit("status.update", Some(&session_id), json!({ "kind": "status", "text": note })).await;
                    tokio::time::sleep(after).await;
                    if retry_due(&store, &session_id, stamp) && !busy().await {
                        let text = prompt_of(prompt);
                        if let Err(err) = conn.dispatch("prompt.submit", &json!({ "session_id": session_id, "text": text })).await {
                            eprintln!("sovereign: error retry for {session_id}: {}", err.message);
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => eprintln!("sovereign: after_error_turn for {session_id}: {err:#}"),
            }
            return poke();
        }
        let subagents_running = conn.child_sessions_running(&session_id).await;
        let cwd = conn.session_cwd(&session_id).await;
        let continuation = match after_turn_in(&store, &session_id, tokens, subagents_running, interrupted, cwd.as_deref().map(Path::new)) {
            Ok(None) if !interrupted && !busy().await => due_heartbeat(&store, &session_id).ok().flatten(),
            Ok(next) => next,
            Err(err) => return eprintln!("sovereign: after_turn for {session_id}: {err:#}"),
        };
        let continuation = match continuation {
            Some(Continuation::Goal(p)) => Some(Continuation::Goal(supervise(&conn, &store, &session_id, p).await)),
            other => other,
        };
        if let Some(cont) = continuation {
            tokio::time::sleep(Duration::from_millis(400)).await;
            if !busy().await {
                let text = prompt_of(cont);
                if let Err(err) = conn.dispatch("prompt.submit", &json!({ "session_id": session_id, "text": text })).await {
                    eprintln!("sovereign: agent loop submit for {session_id}: {}", err.message);
                }
            }
        }
        poke();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use sovereign_prime::agent_loop::{Heartbeat, SessionGoal};

    #[test]
    fn driver_resumes_goals_once_and_fires_due_heartbeats_only_when_idle() {
        let home = std::env::temp_dir().join(format!("driver-{}", std::process::id()));
        let store = ControlStore::open(&home).unwrap();
        store.set_goal("goal", Some(&SessionGoal::new("ship the parser"))).unwrap();
        let mut beat = Heartbeat::new("beat", "check the build", 60);
        beat.last_fired_at_ms = 0;
        store.upsert_heartbeat(&beat).unwrap();
        let active = store.active_sessions().unwrap();
        assert_eq!(active, ["beat", "goal"]);

        let mut resume: std::collections::HashSet<String> = active.iter().cloned().collect();

        // A busy session is left alone (and its heartbeat stays due).
        assert!(due_work(&store, &active, &mut resume, |_| true).is_empty());
        let mut resume: std::collections::HashSet<String> = active.iter().cloned().collect();
        // After a restart: the goal resumes and the due heartbeat fires.
        let work = due_work(&store, &active, &mut resume, |_| false);
        assert_eq!(work.len(), 2);
        assert!(work.iter().any(|(sid, p)| sid == "goal" && p.contains("ship the parser")));
        assert!(work.iter().any(|(sid, p)| sid == "beat" && p.contains("check the build")));
        // A failed submit is retried: the goal stays queued until the caller drops it, and resuming
        // recorded no turn and no attempt.
        let again = due_work(&store, &active, &mut resume, |_| false);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].0, "goal");
        let goal = store.get_goal("goal").unwrap().unwrap();
        assert_eq!((goal.turns_used, goal.attempt_log.len()), (0, 0));
        // Once submitted (caller drops it) goals are not re-sent by later ticks.
        resume.remove("goal");
        assert!(due_work(&store, &active, &mut resume, |_| false).is_empty());
        std::fs::remove_dir_all(home).ok();
    }
}
