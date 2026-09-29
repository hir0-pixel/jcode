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
    /// When each parked goal was first seen waiting on sub-agents.
    waiting_since: std::sync::Mutex<HashMap<String, std::time::Instant>>,
}

static DRIVER: OnceLock<Arc<Driver>> = OnceLock::new();

/// Something may have become active: re-scan now.
pub(crate) fn poke() {
    if let Some(driver) = DRIVER.get() {
        driver.wake.notify_one();
    }
}

/// A parked goal's sub-agents finished: send its continuation on the next scan.
pub(super) fn resume_soon(session_id: &str) {
    if let Some(driver) = DRIVER.get() {
        if let Some(resume) = driver.resume.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            resume.insert(session_id.to_string());
        }
    }
    poke();
}

/// The parent of `session_id` when it was the last of that parent's children still running.
async fn parent_left_waiting(conn: &Arc<Conn>, waiting: &[String], session_id: &str) -> Option<String> {
    let reply = conn.call(json!({ "req": "list_sessions" })).await.ok()?;
    let list = reply["sessions"].as_array()?;
    let parent = list.iter().find(|s| s["session_id"].as_str() == Some(session_id))?["parent_session_id"].as_str()?;
    if !waiting.iter().any(|w| w == parent) {
        return None;
    }
    let running = children_running(list, parent, Some(session_id), &*conn.sessions.lock().await);
    (!running).then(|| parent.to_string())
}

/// A parked goal gives up on its sub-agents after this long and carries on.
const WAIT_CAP: Duration = Duration::from_secs(30 * 60);

/// Waiting sessions whose sub-agents are done (per the engine's `list`) or have overrun `cap`:
/// `(session, timed_out)`. `since` tracks when each was first seen waiting.
fn released_waits(waiting: &[String], since: &mut HashMap<String, std::time::Instant>, list: &[Value], live: &HashMap<String, SessionState>, cap: Duration) -> Vec<(String, bool)> {
    since.retain(|sid, _| waiting.contains(sid));
    waiting
        .iter()
        .filter_map(|sid| {
            let timed_out = since.entry(sid.clone()).or_insert_with(std::time::Instant::now).elapsed() >= cap;
            (timed_out || !children_running(list, sid, None, live)).then(|| (sid.clone(), timed_out))
        })
        .collect()
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
        let conn = Conn::new(config, to_ws, hub, client, observer, true, "invoke_agent", None, None);
        let driver = Arc::new(Driver { conn, wake: Notify::new(), resume: Default::default(), waiting_since: Default::default() });
        let _ = DRIVER.set(driver.clone());
        let (mut failures, mut last_error) = (0u32, String::new());
        loop {
            // The engine may be down at start or restart later: keep trying, forever, with backoff.
            if let Err(err) = driver.conn.ensure_control().await {
                let err = format!("{err:#}");
                if err != last_error {
                    eprintln!("sovereign: goal driver could not reach the engine (retrying): {err}");
                    last_error = err;
                }
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = driver.wake.notified() => {}
                }
                continue;
            }
            (failures, last_error) = (0, String::new());
            report_backup_error(&driver.conn.hub, jcode_base::migrate::backup_error()).await;
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

/// Seconds to wait after `failures` failed engine connects: 1, 2, 4 ... capped at 30.
fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs((1u64 << failures.min(5)).min(30))
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
        let Some(store) = conn.control_store().await else { return false };
        let mut active = store.active_sessions().unwrap_or_default();
        let first = {
            let mut resume = self.resume.lock().unwrap_or_else(|e| e.into_inner());
            let first = resume.is_none();
            resume.get_or_insert_with(|| active.iter().cloned().collect());
            first
        };
        if first {
            // Nothing is running after a restart: whatever a goal was waiting on is over.
            for sid in store.waiting_sessions().unwrap_or_default() {
                let _ = store.clear_wait(&sid);
            }
        }
        // A parked goal's sub-agents may finish on a connection nobody watches: ask the engine.
        let mut notes = HashMap::new();
        let waiting = store.waiting_sessions().unwrap_or_default();
        if !waiting.is_empty() {
            match conn.call(json!({ "req": "list_sessions" })).await {
                Ok(reply) => {
                    let list = reply["sessions"].as_array().map_or(&[][..], Vec::as_slice);
                    let live = conn.sessions.lock().await;
                    let released = released_waits(&waiting, &mut self.waiting_since.lock().unwrap_or_else(|e| e.into_inner()), list, &live, WAIT_CAP);
                    drop(live);
                    for (sid, timed_out) in released {
                        if store.clear_wait(&sid).unwrap_or(false) {
                            if timed_out {
                                notes.insert(sid.clone(), "Note: your sub-agents did not finish within 30 minutes and timed out; continue without their results.\n\n".to_string());
                            }
                            resume_soon(&sid);
                        }
                    }
                }
                Err(err) => eprintln!("sovereign: goal driver could not list sub-agents: {err:#}"),
            }
        }
        let mut gone = Vec::new();
        for sid in &active {
            if let Err(err) = conn.ensure_attached(sid).await {
                // A chat deleted behind our back (its file is gone) has no work left.
                if !jcode_base::session::session_exists(sid) {
                    crate::sessions_rest::forget_rows(&conn.config.home, sid);
                    gone.push(sid.clone());
                } else if first {
                    eprintln!("sovereign: goal driver cannot attach {sid}: {err:#}");
                }
            }
        }
        active.retain(|sid| !gone.contains(sid));
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
        let work = due_work(&store, &active, self.resume.lock().unwrap_or_else(|e| e.into_inner()).as_mut().unwrap(), |sid| busy.contains(sid));
        for (sid, text) in work {
            let text = format!("{}{text}", notes.remove(&sid).unwrap_or_default());
            match conn.dispatch("prompt.submit", &json!({ "session_id": sid, "text": text })).await {
                Ok(_) => {
                    self.resume.lock().unwrap_or_else(|e| e.into_inner()).as_mut().unwrap().remove(&sid);
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
        let Some(store) = conn.control_store().await else { return };
        // A sub-agent finishing may be the last thing its parent's goal was waiting on.
        if let Some(waiting) = store.waiting_sessions().ok().filter(|w| !w.is_empty()) {
            if let Some(parent) = parent_left_waiting(&conn, &waiting, &session_id).await {
                if store.clear_wait(&parent).unwrap_or(false) {
                    resume_soon(&parent);
                }
            }
        }
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
        // Ratchet checkpoints shell out to git: keep that off the async workers.
        let turn = {
            let (store, sid, cwd) = (store.clone(), session_id.clone(), cwd.clone());
            tokio::task::spawn_blocking(move || after_turn_in(&store, &sid, tokens, subagents_running, interrupted, cwd.as_deref().map(Path::new)))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!(e)))
        };
        let continuation = match turn {
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

    #[tokio::test]
    async fn a_goal_turn_lost_with_its_bridge_is_resumed() {
        let conn = crate::rpc::tests::test_conn_as("link-lost", true);
        let store = ControlStore::open_cached(Path::new(&conn.config.home)).unwrap();
        store.set_goal("s", Some(&SessionGoal::new("ship the parser"))).unwrap();
        let driver = Arc::new(Driver { conn: conn.clone(), wake: Notify::new(), resume: std::sync::Mutex::new(Some(Default::default())), waiting_since: Default::default() });
        let _ = DRIVER.set(driver.clone());
        // Mid-turn on a session link: the turn is running and the observer has a run open.
        conn.ensure_control().await.unwrap();
        let link = conn.control.lock().await.clone().unwrap();
        conn.links.lock().await.insert("s".into(), link);
        conn.sessions.lock().await.entry("s".into()).or_default().mark_running();
        conn.observer.start_turn("s", "work", "invoke_agent", None);
        assert!(conn.sessions.lock().await.get("s").is_some_and(SessionState::turn_active) && conn.observer.has_active_run("s"));
        conn.link_tasks.lock().await[0].abort(); // the bridge drops; message.complete is lost
        tokio::time::timeout(Duration::from_secs(5), async {
            while conn.control.lock().await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the dead link is forgotten");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let busy = conn.sessions.lock().await.get("s").is_some_and(SessionState::turn_active) || conn.observer.has_active_run("s");
        assert!(!busy, "the lost turn no longer wedges the session");
        let mut resume = driver.resume.lock().unwrap().take().unwrap();
        assert!(resume.contains("s"), "queued for resume");
        let work = due_work(&store, &["s".to_string()], &mut resume, |_| busy);
        assert_eq!(work.len(), 1, "the driver resumes the goal");
        assert!(work[0].1.contains("ship the parser"));
    }

    #[test]
    fn engine_reconnects_back_off_from_one_to_thirty_seconds() {
        let secs: Vec<u64> = (0..8).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(retry_delay(u32::MAX).as_secs(), 30);
    }

    #[test]
    fn a_parked_goal_is_released_by_the_engines_view_of_its_children_or_the_cap() {
        let list = |status: &str| vec![json!({ "session_id": "child", "parent_session_id": "parent", "status": status })];
        let waiting = ["parent".to_string()];
        let (mut since, live) = (HashMap::new(), HashMap::new());
        let day = Duration::from_secs(86400);
        // A child the engine reports running (on any connection) keeps the parent parked.
        assert!(released_waits(&waiting, &mut since, &list("running"), &live, day).is_empty());
        // Once it is no longer running, the parent is released without a timeout.
        assert_eq!(released_waits(&waiting, &mut since, &list("idle"), &live, day), [("parent".to_string(), false)]);
        // A child that never finishes is given up on at the cap.
        assert_eq!(released_waits(&waiting, &mut since, &list("running"), &live, Duration::ZERO), [("parent".to_string(), true)]);
        // Sessions no longer waiting are forgotten.
        released_waits(&[], &mut since, &[], &live, day);
        assert!(since.is_empty());
    }

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

    #[test]
    fn a_goal_parked_for_sub_agents_is_sent_on_once_they_finish() {
        let home = std::env::temp_dir().join(format!("driver-wait-{}", std::process::id()));
        let store = ControlStore::open(&home).unwrap();
        let mut goal = SessionGoal::new("ship the parser");
        goal.waiting_on_subagents = true;
        store.set_goal("parent", Some(&goal)).unwrap();
        assert_eq!(store.waiting_sessions().unwrap(), ["parent"]);
        assert!(store.clear_wait("parent").unwrap());
        let mut resume: std::collections::HashSet<String> = ["parent".to_string()].into();
        let work = due_work(&store, &["parent".to_string()], &mut resume, |_| false);
        assert_eq!(work.len(), 1);
        assert!(!store.get_goal("parent").unwrap().unwrap().waiting_on_subagents);
        std::fs::remove_dir_all(home).ok();
    }
}
