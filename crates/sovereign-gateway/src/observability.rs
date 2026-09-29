//! Bounded, local run ledger. All SQLite work stays off the agent path.

#[path = "observability/operations.rs"]
mod operations;
mod schema;

use operations::{
    apply_run_end_flags, budget_status, evaluate_alerts, list_alerts, list_approvals, list_filtered, list_memory_deletions, list_sessions, facts,
    monitors, promote_run, turn_index, window_ms, write_approval,
};
use rusqlite::{Connection, params};
use jcode_provider_core::SimpleUsage;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const QUEUE: usize = 1024;
const BATCH: usize = 128;
const CONTENT_LIMIT: usize = 4096;
const REPLAY_EVENTS: usize = 512;
const REPLAY_SESSIONS: usize = 64;
const REPLAY_SESSION_BYTES: usize = 4 * 1024 * 1024;
const REPLAY_TOTAL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct ReplaySession {
    seq: u64,
    evicted_through: u64,
    bytes: usize,
    events: VecDeque<(Value, usize)>,
}

#[derive(Default)]
struct ReplayState {
    sessions: HashMap<String, ReplaySession>,
    bytes: usize,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn capped(text: &str) -> String {
    text.chars().take(CONTENT_LIMIT).collect()
}

#[derive(Debug)]
enum Op {
    RunStart {
        id: String,
        session: String,
        parent: Option<String>,
        root: String,
        kind: &'static str,
        title: Option<String>,
        model: String,
        provider: String,
        status: &'static str,
        at: i64,
    },
    RunActivate {
        id: String,
    },
    RunEnd {
        id: String,
        status: String,
        error: Option<String>,
        at: i64,
        model_calls: u64,
    },
    RunReplayOf {
        id: String,
        replay_of: String,
    },
    /// First streamed token of a run (evestack's `ttft_ms`, measured from the
    /// run's own start).
    RunTtft {
        id: String,
        at: i64,
    },
    Approval {
        run_id: Option<String>,
        session: String,
        tool: String,
        command: String,
        decision: String,
        actor: String,
        at: i64,
    },
    SpanStart {
        id: String,
        run: String,
        root: String,
        kind: &'static str,
        name: String,
        at: i64,
    },
    SpanEnd {
        id: String,
        status: String,
        error: Option<String>,
        at: i64,
    },
    Usage {
        run: String,
        span: String,
        root: String,
        kind: &'static str,
        model: String,
        provider: String,
        session: String,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost: Option<f64>,
        usage_known: bool,
        error: Option<String>,
        started: i64,
        ended: i64,
    },
    Content {
        id: String,
        input: Option<String>,
        output: Option<String>,
    },
}

#[derive(Default)]
struct Active {
    next: u64,
    run: Option<String>,
    queued: VecDeque<String>,
    root: Option<String>,
    model: String,
    provider: String,
    status: Option<String>,
    error: Option<String>,
    tools: HashMap<String, (String, bool)>,
    tool_errors: HashMap<String, String>,
    chat_started: Option<i64>,
    model_calls: u64,
    run_kind: &'static str,
    ttft_sent: bool,
}

pub struct Observer {
    read_db: Mutex<Connection>,
    tx: SyncSender<Op>,
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    capture_content: bool,
    home: std::path::PathBuf,
    sessions: Mutex<HashMap<String, Active>>,
    replay: Mutex<ReplayState>,
    replay_epoch: String,
}

impl Observer {
    pub fn open(
        home: &Path,
        provider: &str,
        model: &str,
        alert_tx: Option<SyncSender<Value>>,
    ) -> rusqlite::Result<Arc<Self>> {
        std::fs::create_dir_all(home).map_err(|_| rusqlite::Error::InvalidPath(home.into()))?;
        let path = home.join("sovereign.db");
        jcode_base::migrate::private(&path);
        let mut db = Connection::open(&path)?;
        schema::open(&mut db)?;
        setup(&mut db)?;
        let read_db = Connection::open(&path)?;
        jcode_base::migrate::private(&path);
        read_db.busy_timeout(Duration::from_secs(5))?;
        let retention_days = retention_days();
        let capture_content = std::fs::read(home.join("observability.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| v["capture_content"].as_bool())
            .unwrap_or(false);
        refresh_children(&mut db, home, capture_content)?;
        db.execute(
            "UPDATE fact_turn SET status='interrupted',ended_at_ms=?1,outcome='wedged' WHERE status='spawned'",
            [now()],
        )?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let pending = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let observer = Arc::new(Self {
            read_db: Mutex::new(read_db),
            tx,
            pending: pending.clone(),
            dropped: dropped.clone(),
            capture_content,
            home: home.to_path_buf(),
            sessions: Mutex::new(HashMap::new()),
            replay: Mutex::new(ReplayState::default()),
            replay_epoch: format!("{}-{}", std::process::id(), now()),
        });
        let weak = Arc::downgrade(&observer);
        jcode_app_core::tool::set_aux_model_observer(Some(Arc::new(move |title, session, provider, model, started, usage, error| {
            if let Some(observer) = weak.upgrade() {
                observer.record_aux(session, "other", Some(title), Some(&provider), Some(&model), started, usage, error);
            }
        })));
        let writer_home = home.to_path_buf();
        let writer_provider = provider.to_string();
        let writer_model = model.to_string();
        let writer_alert_tx = alert_tx.clone();
        let writer_dropped = dropped.clone();
        std::thread::Builder::new()
            .name("sovereign-observability".into())
            .spawn(move || {
                let mut db = db;
                let mut last_prune = 0;
                let mut last_child_check = Instant::now();
                let mut last_alert = 0_i64;
                loop {
                    let first = match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(op) => op,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let at = now();
                            if at - last_prune >= 86_400_000 {
                                if let Err(err) = prune(&db, at, retention_days) {
                                    eprintln!("sovereign-observability: prune failed: {err}");
                                }
                                last_prune = at;
                            }
                            if at - last_alert >= 60_000 {
                                if let Err(err) = evaluate_alerts(
                                    &mut db,
                                    &writer_home,
                                    at,
                                    writer_dropped.load(Ordering::Relaxed),
                                    writer_alert_tx.as_ref(),
                                ) {
                                    eprintln!("sovereign-observability: alerts failed: {err}");
                                }
                                last_alert = at;
                            }
                            if let Err(err) =
                                refresh_children(&mut db, &writer_home, capture_content)
                            {
                                eprintln!("sovereign-observability: child refresh failed: {err}");
                            }
                            last_child_check = Instant::now();
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let mut batch = vec![first];
                    let deadline = Instant::now() + Duration::from_millis(100);
                    while batch.len() < BATCH {
                        let wait = deadline.saturating_duration_since(Instant::now());
                        if wait.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(wait) {
                            Ok(op) => batch.push(op),
                            Err(_) => break,
                        }
                    }
                    let count = batch.len();
                    if let Err(err) = write_batch(&mut db, batch) {
                        dropped.fetch_add(count as u64, Ordering::Relaxed);
                        eprintln!("sovereign-observability: write failed: {err}");
                    }
                    pending.fetch_sub(count, Ordering::Relaxed);
                    if last_child_check.elapsed() >= Duration::from_secs(1) {
                        if let Err(err) = refresh_children(&mut db, &writer_home, capture_content) {
                            eprintln!("sovereign-observability: child refresh failed: {err}");
                        }
                        last_child_check = Instant::now();
                    }
                }
                let _ = db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            })
            .expect("observability writer thread");
        // Keep model/provider defaults out of the chat path and available for
        // sessions whose model_info event arrives after the first prompt.
        observer.sessions.lock().unwrap().insert(
            String::new(),
            Active {
                model: writer_model,
                provider: writer_provider,
                ..Default::default()
            },
        );
        Ok(observer)
    }

    fn send(&self, op: Op, content: bool) {
        if content && (!self.capture_content || self.pending.load(Ordering::Relaxed) >= QUEUE / 2) {
            return;
        }
        self.pending.fetch_add(1, Ordering::Relaxed);
        // ponytail: accounting backpressures only when 1024 queued ops cannot
        // drain; a persistent writer stall needs a durable spool.
        if self.tx.send(op).is_err() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            if !content { self.dropped.fetch_add(1, Ordering::Relaxed); }
        }
    }

    pub fn start_turn(&self, session: &str, prompt: &str, kind: &'static str, title: Option<&str>) -> String {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.len() > 2048 {
            sessions.retain(|key, active| {
                key.is_empty() || active.run.is_some() || !active.queued.is_empty()
            });
        }
        let defaults = sessions
            .get("")
            .map(|s| (s.provider.clone(), s.model.clone()))
            .unwrap_or_default();
        let active = sessions
            .entry(session.to_string())
            .or_insert_with(|| Active {
                provider: defaults.0,
                model: defaults.1,
                ..Default::default()
            });
        active.next += 1;
        let run = format!("{session}:{}:{}", now(), active.next);
        let status = if active.run.is_none() {
            active.run = Some(run.clone());
            active.root = Some(run.clone());
            active.status = None;
            active.error = None;
            active.model_calls = 0;
            active.ttft_sent = false;
            active.run_kind = kind;
            "running"
        } else {
            active.queued.push_back(run.clone());
            "queued"
        };
        self.send(
            Op::RunStart {
                id: run.clone(),
                session: session.into(),
                parent: None,
                root: run.clone(),
                kind,
                title: title.map(str::to_string),
                model: active.model.clone(),
                provider: active.provider.clone(),
                status,
                at: now(),
            },
            false,
        );
        if self.capture_content {
            self.send(
                Op::Content {
                    id: run.clone(),
                    input: Some(capped(prompt)),
                    output: None,
                },
                true,
            );
        }
        run
    }

    pub fn has_active_run(&self, session: &str) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(session)
            .is_some_and(|active| active.run.is_some())
    }

    pub fn record_aux(&self, session: &str, kind: &'static str, title: Option<&str>, provider_override: Option<&str>, model_override: Option<&str>, started: i64, usage: Option<SimpleUsage>, error: Option<&str>) {
        let mut sessions = self.sessions.lock().unwrap();
        let defaults = sessions.get("").map(|s| (s.provider.clone(), s.model.clone())).unwrap_or_default();
        let active = sessions.entry(session.to_string()).or_insert_with(|| Active {
            provider: defaults.0, model: defaults.1, ..Default::default()
        });
        active.next += 1;
        let run = format!("{session}:aux:{started}:{}", active.next);
        let model = model_override.unwrap_or(&active.model).to_string();
        let provider = provider_override.unwrap_or(&active.provider).to_string();
        drop(sessions);
        self.send(Op::RunStart { id: run.clone(), session: session.into(), parent: None, root: run.clone(), kind,
            title: title.map(str::to_string), model: model.clone(), provider: provider.clone(), status: "running", at: started }, false);
        let cost = usage.and_then(|usage| cost_usd(&provider, &model, usage.input, usage.output, usage.cache_read, usage.cache_write));
        let usage_known = usage.is_some();
        let usage = usage.unwrap_or(SimpleUsage { input: 0, output: 0, cache_read: 0, cache_write: 0 });
        self.send(Op::Usage { run: run.clone(), span: format!("{run}:model"), root: run.clone(), kind,
            model, provider, session: session.into(), input: usage.input, output: usage.output,
            cache_read: usage.cache_read, cache_write: usage.cache_write, cost, usage_known,
            error: error.map(capped), started, ended: now() }, false);
        self.send(
            Op::RunEnd {
                id: run,
                status: if error.is_some() { "error" } else { "complete" }.into(),
                error: error.map(capped),
                at: now(),
                model_calls: 1,
            },
            false,
        );
    }

    pub fn event(&self, session: &str, ty: &str, payload: &Value) {
        let mut sessions = self.sessions.lock().unwrap();
        let defaults = sessions
            .get("")
            .map(|s| (s.provider.clone(), s.model.clone()))
            .unwrap_or_default();
        let active = sessions
            .entry(session.to_string())
            .or_insert_with(|| Active {
                provider: defaults.0,
                model: defaults.1,
                ..Default::default()
            });
        let Some(run) = active.run.clone() else {
            return;
        };
        match ty {
            "tool.start" => {
                let call = payload["tool_id"].as_str().unwrap_or_default();
                if call.is_empty() {
                    return;
                }
                let name = payload["name"].as_str().unwrap_or("tool").to_string();
                let span = format!("{run}:tool:{call}");
                let spawn = name == "swarm" && payload["args"]["action"] == "spawn";
                active.tools.insert(call.into(), (span.clone(), spawn));
                self.send(
                    Op::SpanStart {
                        id: span.clone(),
                        run,
                        root: active.root.clone().unwrap_or_default(),
                        kind: "execute_tool",
                        name,
                        at: now(),
                    },
                    false,
                );
                if self.capture_content && !payload["args"].is_null() {
                    self.send(
                        Op::Content {
                            id: span,
                            input: Some(capped(&payload["args"].to_string())),
                            output: None,
                        },
                        true,
                    );
                }
            }
            "tool.complete" => {
                let call = payload["tool_id"].as_str().unwrap_or_default();
                if let Some((span, spawn)) = active.tools.remove(call) {
                    let result = payload["result_text"].as_str().unwrap_or_default();
                    let error = active.tool_errors.remove(call);
                    let status = if error.is_some() { "error" } else { "complete" };
                    self.send(
                        Op::SpanEnd {
                            id: span.clone(),
                            status: status.into(),
                            error: error.clone(),
                            at: now(),
                        },
                        false,
                    );
                    if self.capture_content {
                        self.send(
                            Op::Content {
                                id: span,
                                input: None,
                                output: Some(capped(result)),
                            },
                            true,
                        );
                    }
                    if spawn && error.is_none() {
                        if let Some(session_id) = result
                            .strip_prefix("Spawned new agent: ")
                            .map(str::trim)
                            .filter(|id| !id.is_empty())
                        {
                            self.send(
                                Op::RunStart {
                                    id: format!("{run}:agent:{session_id}"),
                                    session: session_id.into(),
                                    parent: Some(run.clone()),
                                    root: active.root.clone().unwrap_or(run.clone()),
                                    kind: "invoke_agent",
                                    title: None,
                                    model: active.model.clone(),
                                    provider: active.provider.clone(),
                                    status: "spawned",
                                    at: now(),
                                },
                                false,
                            );
                        }
                    }
                }
            }
            "message.complete" => {
                let status = payload["status"].as_str().unwrap_or("complete");
                let text = payload["text"].as_str().unwrap_or_default();
                if let Some(started) = active.chat_started.take() {
                    active.next += 1;
                    self.send(Op::Usage {
                        run: run.clone(),
                        span: format!("{run}:chat:{}", active.next),
                        root: active.root.clone().unwrap_or_else(|| run.clone()),
                        kind: if active.run_kind == "cron" { "cron" } else if active.model_calls == 0 { "chat" } else { "tool_followup" },
                        model: active.model.clone(),
                        provider: active.provider.clone(),
                        session: session.into(),
                        input: 0,
                        output: 0,
                        cache_read: 0,
                        cache_write: 0,
                        cost: None,
                        usage_known: false,
                        error: Some(active.error.clone().unwrap_or_else(|| "token usage unavailable".into())),
                        started,
                        ended: now(),
                    }, false);
                    active.model_calls += 1;
                }
                let model_calls = active.model_calls;
                self.send(
                    Op::RunEnd {
                        id: run.clone(),
                        status: status.into(),
                        error: active.error.take(),
                        at: now(),
                        model_calls,
                    },
                    false,
                );
                if self.capture_content {
                    self.send(
                        Op::Content {
                            id: run,
                            input: None,
                            output: Some(capped(text)),
                        },
                        true,
                    );
                }
                active.run = active.queued.pop_front();
                active.root = active.run.clone();
                if let Some(id) = active.run.clone() {
                    self.send(Op::RunActivate { id }, false);
                }
                active.tools.clear();
                active.tool_errors.clear();
                active.chat_started = None;
                active.model_calls = 0;
                active.ttft_sent = false;
            }
            "message.delta" if !active.ttft_sent => {
                active.ttft_sent = true;
                self.send(Op::RunTtft { id: run, at: now() }, false);
            }
            "error" => active.error = payload["message"].as_str().map(capped),
            _ => {}
        }
    }

    pub fn replay_event(&self, mut params: Value) -> Value {
        let sid = params["session_id"].as_str().unwrap_or_default().to_string();
        if sid.is_empty() { return params; }
        let size = params.to_string().len();
        let mut replay = self.replay.lock().unwrap();
        let seq = {
            let event = replay.sessions.entry(sid.clone()).or_default();
            event.seq += 1;
            event.seq
        };
        while replay.sessions.len() > REPLAY_SESSIONS {
            let oldest = replay.sessions.keys().find(|id| **id != sid).cloned();
            let Some(oldest) = oldest else { break };
            replay.bytes -= replay.sessions.remove(&oldest).map_or(0, |entry| entry.bytes);
        }
        let event = replay.sessions.get_mut(&sid).unwrap();
        params["seq"] = json!(seq);
        if size <= REPLAY_SESSION_BYTES && size <= REPLAY_TOTAL_BYTES {
            event.bytes += size;
            replay.bytes += size;
            replay.sessions.get_mut(&sid).unwrap().events.push_back((params.clone(), size));
            while replay.sessions[&sid].events.len() > REPLAY_EVENTS || replay.sessions[&sid].bytes > REPLAY_SESSION_BYTES {
                let (removed, bytes) = replay.sessions.get_mut(&sid).unwrap().events.pop_front().unwrap();
                replay.sessions.get_mut(&sid).unwrap().bytes -= bytes;
                replay.bytes -= bytes;
                replay.sessions.get_mut(&sid).unwrap().evicted_through = removed["seq"].as_u64().unwrap_or(0);
            }
            while replay.bytes > REPLAY_TOTAL_BYTES {
                let oldest = replay.sessions.iter().find(|(_, entry)| !entry.events.is_empty()).map(|(id, _)| id.clone());
                let Some(oldest) = oldest else { break };
                let entry = replay.sessions.get_mut(&oldest).unwrap();
                let (removed, bytes) = entry.events.pop_front().unwrap();
                entry.bytes -= bytes;
                entry.evicted_through = removed["seq"].as_u64().unwrap_or(0);
                replay.bytes -= bytes;
            }
        } else {
            replay.sessions.get_mut(&sid).unwrap().evicted_through = seq;
        }
        params["epoch"] = json!(self.replay_epoch);
        params
    }

    pub fn replay_since(&self, sid: &str, last_seen: u64) -> (Vec<Value>, u64, bool, String) {
        let replay = self.replay.lock().unwrap();
        let entry = replay.sessions.get(sid);
        let Some(entry) = entry else { return (Vec::new(), 0, false, self.replay_epoch.clone()); };
        let events = entry.events.iter().filter(|(event, _)| event["seq"].as_u64().unwrap_or(0) > last_seen).map(|(event, _)| event.clone()).collect();
        (events, entry.seq, last_seen < entry.evicted_through, self.replay_epoch.clone())
    }

    pub fn replay_stats(&self) -> Value {
        let replay = self.replay.lock().unwrap();
        json!({
            "sessions": replay.sessions.len(),
            "events": replay.sessions.values().map(|entry| entry.events.len()).sum::<usize>(),
            "bytes": replay.bytes,
            "max_per_session": REPLAY_EVENTS,
            "max_bytes_per_session": REPLAY_SESSION_BYTES,
            "max_bytes_process": REPLAY_TOTAL_BYTES,
        })
    }

    pub fn replay_epoch(&self) -> &str { &self.replay_epoch }

    pub fn metered_usage(&self) -> rusqlite::Result<(f64, u64)> {
        let db = self.read_db.lock().unwrap();
        db.query_row("SELECT COALESCE(SUM(cost_usd), 0), COUNT(cost_usd) FROM fact_turn", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
    }

    pub fn harness_event(&self, frame: &Value) {
        let Some(session) = frame["session_id"].as_str() else {
            return;
        };
        let mut sessions = self.sessions.lock().unwrap();
        let defaults = sessions
            .get("")
            .map(|s| (s.provider.clone(), s.model.clone()))
            .unwrap_or_default();
        let active = sessions
            .entry(session.to_string())
            .or_insert_with(|| Active {
                provider: defaults.0,
                model: defaults.1,
                ..Default::default()
            });
        match frame["ev"].as_str().unwrap_or_default() {
            "model_info" | "runtime_info" => {
                if let Some(model) = frame["model"].as_str() {
                    active.model = model.into();
                }
                if let Some(provider) = frame["provider"].as_str() {
                    active.provider = provider.into();
                }
            }
            "token_usage" => {
                let Some(run) = active.run.clone() else {
                    return;
                };
                active.next += 1;
                let span = format!("{run}:chat:{}", active.next);
                let input = frame["input"].as_u64().unwrap_or(0);
                let output = frame["output"].as_u64().unwrap_or(0);
                let cache_read = frame["cache_read_input"].as_u64().unwrap_or(0);
                let cache_write = frame["cache_creation_input"].as_u64().unwrap_or(0);
                let cost = cost_usd(
                    &active.provider,
                    &active.model,
                    input,
                    output,
                    cache_read,
                    cache_write,
                );
                let kind = if active.run_kind == "cron" { "cron" } else if active.model_calls == 0 { "chat" } else { "tool_followup" };
                active.model_calls += 1;
                self.send(
                    Op::Usage {
                        run,
                        span,
                        root: active.root.clone().unwrap_or_default(),
                        kind,
                        model: active.model.clone(),
                        provider: active.provider.clone(),
                        session: session.into(),
                        input,
                        output,
                        cache_read,
                        cache_write,
                        cost,
                        usage_known: true,
                        error: None,
                        started: active.chat_started.take().unwrap_or_else(now),
                        ended: now(),
                    },
                    false,
                );
            }
            "connection_phase" => {
                if active.run.is_some() && active.chat_started.is_none() {
                    active.chat_started = Some(now());
                }
            }
            "turn_stopped" => {
                active.error = frame["message"].as_str().map(capped);
            }
            "tool_done" => {
                if let (Some(call), Some(error)) =
                    (frame["call_id"].as_str(), frame["error"].as_str())
                {
                    active.tool_errors.insert(call.into(), capped(error));
                }
            }
            _ => {}
        }
    }

    pub fn failed_submit(&self, session: &str, run: &str, error: &str) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(active) = sessions.get_mut(session) {
            if active.run.as_deref() == Some(run) {
                active.run = active.queued.pop_front();
                if let Some(id) = active.run.clone() {
                    self.send(Op::RunActivate { id }, false);
                }
            } else {
                active.queued.retain(|id| id != run);
            }
            self.send(
                Op::RunEnd {
                    id: run.into(),
                    status: "error".into(),
                    error: Some(capped(error)),
                    at: now(),
                    model_calls: active.model_calls,
                },
                false,
            );
        }
    }

    pub fn list(
        &self,
        limit: u64,
        status: Option<&str>,
        kind: Option<&str>,
        q: Option<&str>,
        outcome: Option<&str>,
        session: Option<&str>,
    ) -> rusqlite::Result<Value> {
        let db = self.read_db.lock().unwrap();
        let rows = list_filtered(&db, limit, status, kind, q, outcome, session, now())?;
        let budget = budget_status(&db, &self.home, now())?;
        Ok(json!({
            "runs": rows,
            "dropped_events": self.dropped.load(Ordering::Relaxed),
            "capture_content": self.capture_content,
            "budget": budget,
        }))
    }

    pub fn monitors(&self, window: &str) -> Result<Value, &'static str> {
        let Some(ms) = window_ms(window) else {
            return Err("window must be 1h, 24h, or 7d");
        };
        let db = self.read_db.lock().unwrap();
        let mut body = monitors(&db, now() - ms, self.dropped.load(Ordering::Relaxed)).map_err(|_| "query failed")?;
        if let Ok(alerts) = list_alerts(&db) {
            body["alerts"] = alerts["monitors"].clone();
        }
        Ok(body)
    }

    /// One of evestack's dashboard views, by route name.
    pub fn view(&self, name: &str, limit: u64, days: u64) -> rusqlite::Result<Value> {
        let db = self.read_db.lock().unwrap();
        match name {
            "sessions" => list_sessions(&db, limit, now()),
            "facts" => facts(&db, now() - days as i64 * 86_400_000),
            "alerts" => list_alerts(&db),
            "memory-audit" => list_memory_deletions(&db, limit),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }

    pub fn budget(&self) -> rusqlite::Result<Value> {
        let db = self.read_db.lock().unwrap();
        budget_status(&db, &self.home, now())
    }

    pub fn approvals(&self, limit: u64) -> rusqlite::Result<Value> {
        let db = self.read_db.lock().unwrap();
        list_approvals(&db, limit)
    }

    pub fn promote(&self, run_id: &str) -> rusqlite::Result<Value> {
        let db = self.read_db.lock().unwrap();
        promote_run(&db, &self.home, run_id, now())
    }

    pub fn replay_turn_index(&self, run_id: &str) -> rusqlite::Result<(String, i64, Option<String>)> {
        let db = self.read_db.lock().unwrap();
        let run = detail_from_db(&db, run_id)?;
        let session = run["run"]["session_id"].as_str().unwrap_or_default().to_string();
        let started = run["run"]["started_at_ms"].as_i64().unwrap_or(0);
        let prompt = run
            .get("content")
            .and_then(|c| c.get("input"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let nth = turn_index(&db, &session, started)?;
        Ok((session, nth, prompt))
    }

    pub fn link_replay(&self, new_run: &str, original: &str) {
        self.send(
            Op::RunReplayOf {
                id: new_run.into(),
                replay_of: original.into(),
            },
            false,
        );
    }

    pub fn active_run_id(&self, session: &str) -> Option<String> {
        self.sessions.lock().unwrap().get(session).and_then(|a| a.run.clone())
    }

    pub fn record_approval(
        &self,
        session: &str,
        tool: &str,
        command: &str,
        decision: &str,
        actor: &str,
    ) {
        let run_id = self.active_run_id(session);
        self.send(
            Op::Approval {
                run_id,
                session: session.into(),
                tool: tool.into(),
                command: command.into(),
                decision: decision.into(),
                actor: actor.into(),
                at: now(),
            },
            false,
        );
    }

    /// Hermes's `/api/analytics/usage` shape, from the ledger: runs are turns,
    /// `chat` spans are model calls, `execute_tool` spans are tool calls.
    pub fn analytics(&self, days: u64) -> rusqlite::Result<Value> {
        let db = self.read_db.lock().unwrap();
        let since = now() - days as i64 * 86_400_000;
        let mut daily = db.prepare(
            // Driven from runs_recent, counting each run's model calls through
            // spans_run: cost follows the requested window, not total history
            // (a CTE over spans scanned the whole table; measured 12-16x slower).
            "SELECT date(r.started_at_ms/1000,'unixepoch','localtime') AS day, COUNT(DISTINCT r.session_id),
                    SUM(r.input_tokens), SUM(r.output_tokens), SUM(r.cache_read_tokens),
                    CASE WHEN SUM(r.unpriced_calls)>0 THEN NULL ELSE SUM(r.cost_usd) END,
                    SUM((SELECT COUNT(*) FROM spans s WHERE s.run_id=r.id AND s.model IS NOT NULL)), SUM(r.unpriced_calls)
             FROM fact_turn r WHERE r.started_at_ms>=?1 GROUP BY day ORDER BY day",
        )?;
        let daily = daily
            .query_map([since], |r| {
                let cost: Option<f64> = r.get(5)?;
                Ok(json!({"day": r.get::<_, String>(0)?, "sessions": r.get::<_, i64>(1)?, "input_tokens": r.get::<_, i64>(2)?,
                    "output_tokens": r.get::<_, i64>(3)?, "cache_read_tokens": r.get::<_, i64>(4)?, "reasoning_tokens": 0,
                    "estimated_cost": cost, "actual_cost": cost, "api_calls": r.get::<_, i64>(6)?, "unpriced_calls": r.get::<_,i64>(7)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut by_model = db.prepare(
            "SELECT r.model, COUNT(DISTINCT r.session_id), SUM(r.input_tokens), SUM(r.output_tokens),
                    CASE WHEN SUM(r.unpriced_calls)>0 THEN NULL ELSE SUM(r.cost_usd) END,
                    SUM((SELECT COUNT(*) FROM spans s WHERE s.run_id=r.id AND s.model IS NOT NULL)), SUM(r.unpriced_calls)
             FROM fact_turn r WHERE r.started_at_ms>=?1 GROUP BY r.model ORDER BY SUM(r.input_tokens) DESC",
        )?;
        let by_model = by_model
            .query_map([since], |r| {
                Ok(json!({"model": r.get::<_, String>(0)?, "sessions": r.get::<_, i64>(1)?, "input_tokens": r.get::<_, i64>(2)?,
                    "output_tokens": r.get::<_, i64>(3)?, "estimated_cost": r.get::<_, Option<f64>>(4)?, "api_calls": r.get::<_, i64>(5)?, "unpriced_calls": r.get::<_,i64>(6)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tools = db.prepare(
            "SELECT s.name, COUNT(*) FROM fact_turn r JOIN spans s ON s.run_id=r.id
             WHERE r.started_at_ms>=?1 AND s.kind='execute_tool' GROUP BY s.name ORDER BY COUNT(*) DESC",
        )?;
        let tools: Vec<(String, i64)> = tools.query_map([since], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let tool_total = tools.iter().map(|(_, n)| n).sum::<i64>().max(1) as f64;
        let tools: Vec<Value> = tools
            .into_iter()
            .map(|(tool, count)| json!({"tool": tool, "count": count, "percentage": count as f64 * 100.0 / tool_total}))
            .collect();
        let sum = |key: &str| daily.iter().map(|d| d[key].as_i64().unwrap_or(0)).sum::<i64>();
        let unpriced = sum("unpriced_calls");
        let cost: Option<f64> = (unpriced == 0).then(|| daily.iter().map(|d| d["estimated_cost"].as_f64().unwrap_or(0.0)).sum());
        let sessions: i64 = db.query_row("SELECT COUNT(DISTINCT session_id) FROM fact_turn WHERE started_at_ms>=?1", [since], |r| r.get(0))?;
        Ok(json!({
            "period_days": days,
            "daily": daily,
            "by_model": by_model,
            "tools": tools,
            "skills": {"summary": {"distinct_skills_used": 0, "total_skill_actions": 0}, "top_skills": []},
            "totals": {"total_sessions": sessions, "total_input": sum("input_tokens"), "total_output": sum("output_tokens"),
                "total_cache_read": sum("cache_read_tokens"), "total_reasoning": 0, "total_api_calls": sum("api_calls"),
                "total_estimated_cost": cost, "total_actual_cost": cost, "unpriced_calls": unpriced},
        }))
    }

    /// Hermes's `insights.get`: sessions and messages over the last `days`.
    /// A top-level run is one user turn and its reply, i.e. two messages.
    pub fn insights(&self, days: u64) -> rusqlite::Result<Value> {
        let db = self.read_db.lock().unwrap();
        let since = now() - days as i64 * 86_400_000;
        let (sessions, turns): (i64, i64) = db.query_row(
            "SELECT COUNT(DISTINCT session_id), COUNT(*) FROM fact_turn WHERE parent_id IS NULL AND started_at_ms>=?1",
            [since],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(json!({"days": days, "sessions": sessions, "messages": turns * 2}))
    }

    pub fn detail(&self, id: &str) -> rusqlite::Result<Value> {
        let db = self.read_db.lock().unwrap();
        detail_from_db(&db, id)
    }
}

pub(crate) fn detail_from_db(db: &Connection, id: &str) -> rusqlite::Result<Value> {
        let mut stmt = db.prepare(&format!("SELECT {} FROM fact_turn WHERE id=?1", operations::RUN_COLS))?;
        let run = operations::overlay_wedged(stmt.query_row([id], operations::run_row)?, now());
        let mut spans = db.prepare("SELECT s.id,s.run_id,s.parent_id,s.root_id,s.kind,s.name,s.status,s.started_at_ms,s.ended_at_ms,s.input_tokens,s.output_tokens,s.cache_read_tokens,s.cache_write_tokens,s.cost_usd,s.error,c.input,c.output,s.model,s.provider,s.attributes FROM spans s LEFT JOIN span_content c ON c.id=s.id WHERE s.run_id=?1 ORDER BY s.started_at_ms")?;
        let spans = spans.query_map([id], |r| Ok(json!({"id":r.get::<_,String>(0)?,"run_id":r.get::<_,String>(1)?,"parent_id":r.get::<_,String>(2)?,"root_id":r.get::<_,String>(3)?,"kind":r.get::<_,String>(4)?,"name":r.get::<_,String>(5)?,"status":r.get::<_,String>(6)?,"started_at_ms":r.get::<_,i64>(7)?,"ended_at_ms":r.get::<_,Option<i64>>(8)?,"input_tokens":r.get::<_,i64>(9)?,"output_tokens":r.get::<_,i64>(10)?,"cache_read_tokens":r.get::<_,i64>(11)?,"cache_write_tokens":r.get::<_,i64>(12)?,"cost_usd":r.get::<_,Option<f64>>(13)?,"error":r.get::<_,Option<String>>(14)?,"input":r.get::<_,Option<String>>(15)?,"output":r.get::<_,Option<String>>(16)?,"model":r.get::<_,Option<String>>(17)?,"provider":r.get::<_,Option<String>>(18)?,"attributes":serde_json::from_str::<Value>(&r.get::<_,String>(19)?).unwrap_or_else(|_| json!({}))})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let content: Option<(Option<String>, Option<String>)> = db
            .query_row("SELECT input,output FROM span_content WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .ok();
        Ok(
            json!({"run":run,"spans":spans,"content":content.map(|(input,output)| json!({"input":input,"output":output}))}),
        )
}

fn setup(db: &mut Connection) -> rusqlite::Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;")?;
    db.execute(
        // Work the process died in the middle of: evestack's `wedged`.
        "UPDATE fact_turn SET status='interrupted',ended_at_ms=?1,outcome='wedged' WHERE status IN ('running','queued')",
        [now()],
    )?;
    db.execute(
        "UPDATE spans SET status='interrupted',ended_at_ms=?1 WHERE status='running'",
        [now()],
    )?;
    Ok(())
}

/// How long observability rows (and hidden one-shot cron sessions) are kept.
pub(crate) fn retention_days() -> i64 {
    std::env::var("SOVEREIGN_OBSERVABILITY_RETENTION_DAYS")
        .ok().and_then(|raw| raw.parse::<i64>().ok()).filter(|days| (1..=3650).contains(days)).unwrap_or(30)
}

/// Delete every trace row of a deleted session: its runs, their spans and captured content, and
/// its approval records.
pub(crate) fn forget_session(home: &Path, session: &str) -> rusqlite::Result<()> {
    let mut db = Connection::open(home.join("sovereign.db"))?;
    db.busy_timeout(Duration::from_secs(5))?;
    let tx = db.transaction()?;
    let runs = "SELECT id FROM fact_turn WHERE session_id=?1";
    tx.execute(&format!("DELETE FROM span_content WHERE id IN ({runs}) OR id IN (SELECT id FROM spans WHERE run_id IN ({runs}))"), [session])?;
    tx.execute(&format!("DELETE FROM spans WHERE run_id IN ({runs})"), [session])?;
    tx.execute("DELETE FROM fact_turn WHERE session_id=?1", [session])?;
    tx.execute("DELETE FROM approvals WHERE session_id=?1", [session])?;
    tx.commit()
}

fn prune(db: &Connection, at: i64, retention_days: i64) -> rusqlite::Result<()> {
    let day = 86_400_000_i64;
    db.execute("DELETE FROM span_content WHERE id IN (SELECT id FROM fact_turn WHERE started_at_ms<?1 UNION SELECT id FROM spans WHERE started_at_ms<?1)", [at - retention_days * day])?;
    db.execute(
        "DELETE FROM spans WHERE ended_at_ms IS NOT NULL AND ended_at_ms<?1",
        [at - retention_days * day],
    )?;
    db.execute("DELETE FROM fact_turn WHERE ended_at_ms IS NOT NULL AND ended_at_ms<?1", [at - retention_days * day])?;
    // Command previews and the memory audit expire with everything else.
    db.execute("DELETE FROM approvals WHERE at_ms<?1", [at - retention_days * day])?;
    db.execute("DELETE FROM memory_deletions WHERE deleted_at_ms<?1", [at - retention_days * day])?;
    Ok(())
}

fn write_batch(db: &mut Connection, batch: Vec<Op>) -> rusqlite::Result<()> {
    let tx = db.transaction()?;
    for op in batch {
        match op {
            Op::RunStart {
                id,
                session,
                parent,
                root,
                kind,
                title,
                model,
                provider,
                status,
                at,
            } => {
                tx.execute("INSERT OR IGNORE INTO fact_turn(id,session_id,parent_id,root_id,kind,title,model,provider,status,started_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![id,session,parent,root,kind,title,model,provider,status,at])?;
            }
            Op::RunActivate { id } => {
                tx.execute("UPDATE fact_turn SET status='running' WHERE id=?1", [id])?;
            }
            Op::RunEnd {
                id,
                status,
                error,
                at,
                model_calls,
            } => {
                tx.execute(
                    "UPDATE fact_turn SET status=?2,error=?3,ended_at_ms=?4 WHERE id=?1",
                    params![id, status, error, at],
                )?;
                apply_run_end_flags(&tx, &id, &status, &error, model_calls)?;
            }
            Op::RunTtft { id, at } => {
                tx.execute("UPDATE fact_turn SET ttft_ms=?2-started_at_ms WHERE id=?1 AND ttft_ms IS NULL", params![id, at])?;
            }
            Op::RunReplayOf { id, replay_of } => {
                tx.execute("UPDATE fact_turn SET replay_of=?2 WHERE id=?1", params![id, replay_of])?;
            }
            Op::Approval {
                run_id,
                session,
                tool,
                command,
                decision,
                actor,
                at,
            } => write_approval(&tx, run_id.as_deref(), &session, &tool, &command, &decision, &actor, at)?,
            Op::SpanStart {
                id,
                run,
                root,
                kind,
                name,
                at,
            } => {
                tx.execute("INSERT OR IGNORE INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms) VALUES(?1,?2,?2,?3,?4,?5,'running',?6)", params![id,run,root,kind,name,at])?;
                if kind == "execute_tool" {
                    // evestack's span_coverage: at least something landed for
                    // this run, but not yet a model call — see the 'full'
                    // upgrade in the Usage arm below.
                    tx.execute("UPDATE fact_turn SET span_coverage='partial' WHERE id=?1 AND span_coverage='none'", [run])?;
                }
            }
            Op::SpanEnd {
                id,
                status,
                error,
                at,
            } => {
                tx.execute(
                    "UPDATE spans SET status=?2,error=?3,ended_at_ms=?4 WHERE id=?1",
                    params![id, status, error, at],
                )?;
            }
            Op::Usage {
                run,
                span,
                root,
                kind,
                model,
                provider,
                session,
                input,
                output,
                cache_read,
                cache_write,
                cost,
                usage_known,
                error,
                started,
                ended,
            } => {
                let mut attributes = json!({
                    "gen_ai.operation.name": "chat",
                    "gen_ai.provider.name": provider,
                    "gen_ai.request.model": model,
                    "gen_ai.response.model": model,
                    "gen_ai.conversation.id": session,
                });
                if usage_known {
                    attributes["gen_ai.usage.input_tokens"] = json!(input);
                    attributes["gen_ai.usage.output_tokens"] = json!(output);
                    attributes["gen_ai.usage.cache_read.input_tokens"] = json!(cache_read);
                    attributes["gen_ai.usage.cache_creation.input_tokens"] = json!(cache_write);
                }
                let status = if error.is_some() { "error" } else { "complete" };
                let inserted = tx.execute("INSERT OR IGNORE INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms,ended_at_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd,error,model,provider,attributes) VALUES(?1,?2,?2,?3,?4,'Model call',?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)", params![span,run,root,kind,status,started,ended,input,output,cache_read,cache_write,cost,error,model,provider,attributes.to_string()])?;
                if inserted != 0 {
                    tx.execute("UPDATE fact_turn SET input_tokens=input_tokens+?2,output_tokens=output_tokens+?3,cache_read_tokens=cache_read_tokens+?4,cache_write_tokens=cache_write_tokens+?5,unpriced_calls=unpriced_calls+CASE WHEN ?6 IS NULL THEN 1 ELSE 0 END,cost_usd=CASE WHEN ?6 IS NULL OR unpriced_calls>0 THEN NULL ELSE COALESCE(cost_usd,0)+?6 END,span_coverage='full' WHERE id=?1", params![run,input,output,cache_read,cache_write,cost])?;
                }
            }
            Op::Content { id, input, output } => {
                tx.execute("INSERT INTO span_content(id,input,output) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET input=COALESCE(excluded.input,input),output=COALESCE(excluded.output,output)", params![id,input,output])?;
            }
        }
    }
    tx.commit()
}

fn message_time(message: &Value) -> i64 {
    message["timestamp"]
        .as_str()
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|date| date.timestamp_millis())
        .unwrap_or_else(now)
}

/// Swarm workers persist their own transcript outside the parent connection.
/// Reconcile that transcript on the writer thread, never in prompt submission.
fn refresh_children(
    db: &mut Connection,
    home: &Path,
    capture_content: bool,
) -> rusqlite::Result<()> {
    let mut stmt = db.prepare("SELECT c.id,c.session_id,c.root_id,p.session_id FROM fact_turn c JOIN fact_turn p ON p.id=c.parent_id WHERE c.status='spawned'")?;
    let children = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (run, session, root, parent_session) in children {
        if !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(home.join("sessions").join(format!("{session}.json"))) else {
            continue;
        };
        let Ok(saved) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if saved["parent_id"].as_str() != Some(parent_session.as_str()) {
            continue;
        }
        let Some(messages) = saved["messages"].as_array() else {
            continue;
        };
        let Some(last) = messages.last() else {
            continue;
        };
        let finished = last["role"] == "assistant"
            && last["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "text"));
        if !finished {
            continue;
        }
        let provider = saved["provider_key"].as_str().unwrap_or("unknown");
        let model = saved["model"].as_str().unwrap_or("unknown");
        let mut input = 0;
        let mut output = 0;
        let mut read = 0;
        let mut write = 0;
        let mut total_cost = Some(0.0);
        let mut unpriced_calls = 0;
        let mut calls = Vec::new();
        let mut tools = HashMap::<String, (String, i64, String)>::new();
        let mut results = Vec::new();
        for message in messages {
            if message["role"] == "assistant" && message["token_usage"].is_object() {
                let usage = &message["token_usage"];
                let i = usage["input_tokens"].as_u64().unwrap_or(0);
                let o = usage["output_tokens"].as_u64().unwrap_or(0);
                let r = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                let w = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                input += i;
                output += o;
                read += r;
                write += w;
                let cost = cost_usd(provider, model, i, o, r, w);
                if cost.is_none() { unpriced_calls += 1; }
                total_cost = total_cost.and_then(|sum| cost.map(|price| sum + price));
                calls.push((
                    message_time(message),
                    i,
                    o,
                    r,
                    w,
                    cost,
                ));
            }
            if let Some(parts) = message["content"].as_array() {
                for part in parts {
                    if part["type"] == "tool_use" {
                        if let Some(id) = part["id"].as_str() {
                            tools.insert(
                                id.into(),
                                (
                                    part["name"].as_str().unwrap_or("tool").into(),
                                    message_time(message),
                                    part["input"].to_string(),
                                ),
                            );
                        }
                    } else if part["type"] == "tool_result" {
                        if let Some(id) = part["tool_use_id"].as_str() {
                            results.push((
                                id.to_string(),
                                message_time(message),
                                part["content"].to_string(),
                            ));
                        }
                    }
                }
            }
        }
        let ended = message_time(last);
        // Same outcome/coverage vocabulary as apply_run_end_flags / the
        // execute_tool and Usage arms above — this reconciliation path writes
        // fact_turn directly rather than through Op::RunEnd, so it computes
        // both here instead of falling through to run_outcome.
        let outcome = if calls.is_empty() { "no_model_call" } else { "ok" };
        let span_coverage = if !calls.is_empty() { "full" } else if !tools.is_empty() || !results.is_empty() { "partial" } else { "none" };
        let tx = db.transaction()?;
        tx.execute("UPDATE fact_turn SET status='complete',ended_at_ms=?2,provider=?3,model=?4,input_tokens=?5,output_tokens=?6,cache_read_tokens=?7,cache_write_tokens=?8,cost_usd=?9,unpriced_calls=?10,outcome=?11,span_coverage=?12 WHERE id=?1", params![run,ended,provider,model,input,output,read,write,total_cost,unpriced_calls,outcome,span_coverage])?;
        for (n, (at, i, o, r, w, cost)) in calls.into_iter().enumerate() {
            let kind = if n == 0 { "chat" } else { "tool_followup" };
            let attributes = json!({"gen_ai.operation.name":"chat","gen_ai.provider.name":provider,"gen_ai.request.model":model,
                "gen_ai.response.model":model,"gen_ai.usage.input_tokens":i,"gen_ai.usage.output_tokens":o,
                "gen_ai.usage.cache_read.input_tokens":r,"gen_ai.usage.cache_creation.input_tokens":w,
                "gen_ai.conversation.id":session}).to_string();
            tx.execute("INSERT OR IGNORE INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms,ended_at_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd,model,provider,attributes) VALUES(?1,?2,?2,?3,?4,'Model call','complete',?5,?5,?6,?7,?8,?9,?10,?11,?12,?13)", params![format!("{run}:chat:{n}"),run,root,kind,at,i,o,r,w,cost,model,provider,attributes])?;
        }
        for (id, at, result) in results {
            if let Some((name, started, args)) = tools.remove(&id) {
                let span = format!("{run}:tool:{id}");
                tx.execute("INSERT OR IGNORE INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms,ended_at_ms) VALUES(?1,?2,?2,?3,'execute_tool',?4,'complete',?5,?6)", params![span,run,root,name,started,at])?;
                if capture_content {
                    tx.execute(
                        "INSERT OR IGNORE INTO span_content(id,input,output) VALUES(?1,?2,?3)",
                        params![span, capped(&args), capped(&result)],
                    )?;
                }
            }
        }
        if capture_content {
            let prompt = messages
                .iter()
                .filter(|message| message["role"] == "user")
                .flat_map(|message| message["content"].as_array().into_iter().flatten())
                .find_map(|part| {
                    part["text"]
                        .as_str()
                        .filter(|text| !text.starts_with("<system-reminder>"))
                });
            let reply = last["content"]
                .as_array()
                .and_then(|parts| parts.iter().find_map(|part| part["text"].as_str()));
            tx.execute(
                "INSERT OR IGNORE INTO span_content(id,input,output) VALUES(?1,?2,?3)",
                params![run, prompt.map(capped), reply.map(capped)],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn cost_usd(
    provider: &str,
    model: &str,
    input: u64,
    output: u64,
    read: u64,
    write: u64,
) -> Option<f64> {
    #[derive(Deserialize)]
    struct Price { input: f64, cached: f64, output: f64 }
    static PRICES: OnceLock<HashMap<String, Price>> = OnceLock::new();
    let prices = PRICES.get_or_init(|| {
        std::env::var_os("SOVEREIGN_PRICE_TABLE")
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    });
    if let Some(price) = prices.get(model) {
        let cached = read.min(input);
        let fresh = input.saturating_sub(cached).saturating_sub(write);
        return Some((fresh as f64 * price.input + cached as f64 * price.cached
            + write as f64 * price.input + output as f64 * price.output) / 1_000_000.0);
    }
    let provider = provider.to_ascii_lowercase();
    if provider == "ollama" {
        return None;
    }
    let price = match provider.as_str() {
        "anthropic" | "claude" | "anthropic-api" => {
            jcode_provider_core::pricing::anthropic_api_pricing(model)
        }
        "openai" | "openai-api" => jcode_provider_core::pricing::openai_api_pricing(model),
        _ => None,
    }?;
    let input_price = price.input_price_per_mtok_micros? as f64;
    let output_price = price.output_price_per_mtok_micros? as f64;
    let read_price = price
        .cache_read_price_per_mtok_micros
        .unwrap_or(price.input_price_per_mtok_micros?) as f64;
    let fresh = if provider.starts_with("anthropic") || provider == "claude" {
        input
    } else {
        input.saturating_sub(read).saturating_sub(write)
    };
    let write_multiplier = if provider.starts_with("anthropic") || provider == "claude" {
        1.25
    } else {
        1.0
    };
    Some(
        (fresh as f64 * input_price
            + output as f64 * output_price
            + read as f64 * read_price
            + write as f64 * input_price * write_multiplier)
            / 1_000_000_000_000.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_sequenced_events_and_reports_eviction() {
        let dir = std::env::temp_dir().join(format!("sovereign-replay-{}", now()));
        let observer = Observer::open(&dir, "ollama", "local", None).unwrap();
        let mut latest = Value::Null;
        for i in 0..=REPLAY_EVENTS {
            latest = observer.replay_event(json!({"session_id":"s","type":"message.delta","payload":{"i":i}}));
        }
        assert_eq!(latest["seq"], REPLAY_EVENTS as u64 + 1);
        let (events, seq, truncated, epoch) = observer.replay_since("s", 0);
        assert_eq!(seq, REPLAY_EVENTS as u64 + 1);
        assert!(truncated);
        assert_eq!(events.len(), REPLAY_EVENTS);
        assert_eq!(events[0]["seq"], 2);
        assert!(!epoch.is_empty());
        assert_eq!(observer.replay_stats()["events"], REPLAY_EVENTS);
        drop(observer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn evestack_views_and_memory_audit() {
        let dir = std::env::temp_dir().join(format!("sovereign-observability-views-{}", now()));
        std::fs::create_dir_all(&dir).unwrap();
        let observer = Observer::open(&dir, "ollama", "local", None).unwrap();
        observer.record_approval("s1", "bash", "ls", "approve", "user");
        let run = observer.start_turn("s1", "hello", "invoke_agent", None);
        observer.event("s1", "message.delta", &json!({"text":"hi"}));
        observer.event("s1", "tool.start", &json!({"tool_id":"t1","name":"read"}));
        observer.event("s1", "tool.complete", &json!({"tool_id":"t1","result_text":"ok"}));
        observer.event("s1", "message.complete", &json!({"status":"interrupted"}));
        let mut listed = Value::Null;
        for _ in 0..100 {
            listed = observer.list(10, None, None, None, None, Some("s1")).unwrap();
            if listed["runs"][0]["outcome"] == "cancelled" && listed["runs"][0]["tools_called"] == 1 { break; }
            std::thread::sleep(Duration::from_millis(20));
        }
        let row = &listed["runs"][0];
        assert_eq!(row["id"], run);
        assert_eq!(row["outcome"], "cancelled", "a stop is evestack's cancelled");
        assert_eq!(row["run_type"], "turn");
        assert_eq!(row["trigger"], "desktop");
        assert_eq!(row["tools_called"], 1);
        assert!(row["ttft_ms"].as_i64().is_some(), "first delta is recorded: {row}");
        let sessions = observer.view("sessions", 10, 1).unwrap();
        assert_eq!(sessions["sessions"][0]["session_id"], "s1");
        assert_eq!(sessions["sessions"][0]["cancelled"], 1);
        let facts = observer.view("facts", 10, 1).unwrap();
        assert_eq!(facts["tools"][0]["tool_name"], "read");
        assert_eq!(facts["tools"][0]["failure_rate_pct"], 0.0);
        let approvals = observer.approvals(10).unwrap();
        assert_eq!(approvals["approvals"][0]["tool_name"], "bash");
        assert_eq!(approvals["approvals"][0]["approver"], "user");
        // The memory audit trigger records a deletion made by any path.
        let db = Connection::open(dir.join("sovereign.db")).unwrap();
        db.execute("INSERT INTO memories(id,scope,active,content,tags) VALUES('m1','global',1,'remember this','a b')", []).unwrap();
        db.execute("DELETE FROM memories WHERE id='m1'", []).unwrap();
        let audit = observer.view("memory-audit", 10, 1).unwrap();
        assert_eq!(audit["deletions"][0]["memory_id"], "m1");
        assert_eq!(audit["deletions"][0]["length"], 13);
        assert!(audit["deletions"][0].get("content").is_none(), "forgetting keeps no text");
        drop((observer, db));
        // Re-opening is idempotent and keeps the rows.
        let observer = Observer::open(&dir, "ollama", "local", None).unwrap();
        assert_eq!(observer.list(10, None, None, None, None, None).unwrap()["runs"].as_array().unwrap().len(), 1);
        drop(observer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writer_cost_recovery_and_retention() {
        let dir = std::env::temp_dir().join(format!("sovereign-observability-test-{}", now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sovereign.db");
        let mut db = Connection::open(&path).unwrap();
        schema::open(&mut db).unwrap();
        setup(&mut db).unwrap();
        write_batch(
            &mut db,
            vec![
                Op::RunStart {
                    id: "r".into(),
                    session: "s".into(),
                    parent: None,
                    root: "r".into(),
                    kind: "invoke_agent",
                    title: None,
                    model: "gpt-5.4".into(),
                    provider: "openai".into(),
                    status: "running",
                    at: now(),
                },
                Op::Usage {
                    run: "r".into(),
                    span: "c".into(),
                    root: "r".into(),
                    kind: "chat",
                    model: "gpt-5.4".into(),
                    provider: "openai".into(),
                    session: "s".into(),
                    input: 100,
                    output: 50,
                    cache_read: 20,
                    cache_write: 0,
                    cost: cost_usd("openai", "gpt-5.4", 100, 50, 20, 0),
                    usage_known: true,
                    error: None,
                    started: now(),
                    ended: now(),
                },
            ],
        )
        .unwrap();
        write_batch(&mut db, vec![Op::Usage {
            run: "r".into(), span: "c".into(), root: "r".into(), kind: "chat",
            model: "gpt-5.4".into(), provider: "openai".into(), session: "s".into(),
            input: 100, output: 50, cache_read: 20, cache_write: 0,
            cost: cost_usd("openai", "gpt-5.4", 100, 50, 20, 0),
            usage_known: true, error: None,
            started: now(), ended: now(),
        }]).unwrap();
        assert_eq!(db.query_row("SELECT input_tokens FROM fact_turn WHERE id='r'", [], |r| r.get::<_, i64>(0)).unwrap(), 100);
        write_batch(&mut db, vec![Op::Usage {
            run: "r".into(), span: "canceled".into(), root: "r".into(), kind: "other",
            model: "gpt-5.4".into(), provider: "openai".into(), session: "s".into(),
            input: 0, output: 0, cache_read: 0, cache_write: 0, cost: None,
            usage_known: false, error: Some("interrupted".into()), started: now(), ended: now(),
        }]).unwrap();
        let (cost, unpriced): (Option<f64>, i64) = db.query_row(
            "SELECT cost_usd,unpriced_calls FROM fact_turn WHERE id='r'", [], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!((cost, unpriced), (None, 1));
        let (status, attributes): (String, String) = db.query_row(
            "SELECT status,attributes FROM spans WHERE id='canceled'", [], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(status, "error");
        assert!(serde_json::from_str::<Value>(&attributes).unwrap().get("gen_ai.usage.input_tokens").is_none());
        assert!(cost_usd("openai", "gpt-5.4", 100, 50, 20, 0).unwrap() > 0.0);
        assert_eq!(cost_usd("unknown", "x", 100, 50, 0, 0), None);
        setup(&mut db).unwrap();
        let status: String = db
            .query_row("SELECT status FROM fact_turn WHERE id='r'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "interrupted");
        db.execute(
            "UPDATE spans SET ended_at_ms=?1",
            [now() - 31 * 86_400_000_i64],
        )
        .unwrap();
        for ago in [31, 1] {
            let at = now() - ago * 86_400_000_i64;
            db.execute("INSERT INTO approvals(session_id,tool,command_preview,decision,actor,at_ms) VALUES('s','bash','rm x','deny','u',?1)", [at]).unwrap();
            db.execute("INSERT INTO memory_deletions(deleted_at_ms,memory_id,length) VALUES(?1,'m',5)", [at]).unwrap();
        }
        prune(&db, now(), 30).unwrap();
        for table in ["approvals", "memory_deletions"] {
            assert_eq!(db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap(), 1, "{table} keeps only the recent row");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM spans", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM fact_turn", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn child_identity_and_content_toggle() {
        let dir = std::env::temp_dir().join(format!("sovereign-observability-child-{}", now()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("observability.json"),
            r#"{"capture_content":true}"#,
        )
        .unwrap();
        let observer = Observer::open(&dir, "Ollama", "local", None).unwrap();
        let root = observer.start_turn("parent", "hello", "invoke_agent", None);
        observer.event(
            "parent",
            "tool.start",
            &json!({"tool_id":"call","name":"swarm","args":{"action":"spawn"}}),
        );
        observer.event(
            "parent",
            "tool.complete",
            &json!({"tool_id":"call","result_text":"Spawned new agent: child-session"}),
        );
        observer.event(
            "parent",
            "message.complete",
            &json!({"status":"complete","text":"done"}),
        );
        for _ in 0..100 {
            if observer.list(10, None, None, None, None, None).unwrap()["runs"].as_array().unwrap().len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let rows = observer.list(10, None, None, None, None, None).unwrap();
        let child = rows["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["parent_id"] == root)
            .unwrap();
        assert_eq!(child["session_id"], "child-session");
        assert_eq!(child["root_id"], root);
        assert_eq!(child["status"], "spawned");
        assert_eq!(observer.detail(&root).unwrap()["content"]["output"], "done");
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        let at = chrono::Utc::now().to_rfc3339();
        std::fs::write(dir.join("sessions/child-session.json"), json!({
            "parent_id":"parent","provider_key":"ollama","model":"local","messages":[
                {"role":"user","timestamp":at,"content":[{"type":"text","text":"Reply READY"}]},
                {"role":"assistant","timestamp":at,"token_usage":{"input_tokens":9,"output_tokens":2},"content":[{"type":"text","text":"READY"}]}
            ]
        }).to_string()).unwrap();
        for _ in 0..200 {
            if observer.list(10, None, None, None, None, None).unwrap()["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["session_id"] == "child-session" && row["status"] == "complete")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let complete = observer.list(10, None, None, None, None, None).unwrap();
        let child = complete["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["session_id"] == "child-session")
            .unwrap();
        assert_eq!(child["status"], "complete");
        assert_eq!(child["input_tokens"], 9);
        assert!(child["cost_usd"].is_null());
        drop(observer);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn records_unknown_usage_only_for_an_unreported_inflight_call() {
        let mut db = Connection::open_in_memory().unwrap();
        schema::open(&mut db).unwrap();
        setup(&mut db).unwrap();
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let observer = Observer {
            read_db: Mutex::new(Connection::open_in_memory().unwrap()),
            tx,
            pending: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            capture_content: false,
            home: std::env::temp_dir(),
            sessions: Mutex::new(HashMap::new()),
            replay: Mutex::new(ReplayState::default()),
            replay_epoch: "test".into(),
        };

        observer.harness_event(&json!({"ev":"model_info","session_id":"s","provider":"openai","model":"gpt-5.4"}));
        let completed = observer.start_turn("s", "prompt", "invoke_agent", None);
        observer.harness_event(&json!({"ev":"connection_phase","session_id":"s"}));
        observer.harness_event(&json!({"ev":"token_usage","session_id":"s","input":10,"output":2}));
        observer.event("s", "message.complete", &json!({"status":"complete"}));

        let interrupted = observer.start_turn("s", "prompt", "cron", None);
        observer.harness_event(&json!({"ev":"connection_phase","session_id":"s"}));
        observer.harness_event(&json!({"ev":"turn_stopped","session_id":"s","message":"cancelled"}));
        observer.event("s", "message.complete", &json!({"status":"interrupted"}));

        let ops: Vec<Op> = rx.try_iter().collect();
        assert_eq!(ops.iter().filter(|op| matches!(op, Op::Usage { run, usage_known: true, .. } if run == &completed)).count(), 1);
        assert_eq!(ops.iter().filter(|op| matches!(op, Op::Usage { run, usage_known: false, kind: "cron", model, provider, session, cost: None, error: Some(_), .. } if run == &interrupted && model == "gpt-5.4" && provider == "openai" && session == "s")).count(), 1);
        write_batch(&mut db, ops).unwrap();
        let (calls, unpriced, cost): (i64, i64, Option<f64>) = db.query_row(
            "SELECT COUNT(*), MAX(r.unpriced_calls), MAX(r.cost_usd) FROM fact_turn r JOIN spans s ON s.run_id=r.id WHERE r.id=?1 AND s.model IS NOT NULL",
            [&interrupted], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!((calls, unpriced, cost), (1, 1, None));
    }

    #[test]
    #[ignore = "manual release measurement"]
    fn measure_overhead() {
        fn rss_kb() -> u64 {
            std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0)
        }
        let dir = std::env::temp_dir().join(format!("sovereign-observability-bench-{}", now()));
        let before = rss_kb();
        let observer = Observer::open(&dir, "Ollama", "local", None).unwrap();
        let idle_delta_kb = rss_kb().saturating_sub(before);
        let mut nanos = 0_u128;
        for i in 0..1000 {
            let session = format!("bench-{i}");
            let at = Instant::now();
            observer.start_turn(&session, "prompt", "invoke_agent", None);
            nanos += at.elapsed().as_nanos();
            let at = Instant::now();
            observer.harness_event(
                &json!({"ev":"token_usage","session_id":session,"input":100,"output":20}),
            );
            nanos += at.elapsed().as_nanos();
            let at = Instant::now();
            observer.event(&session, "message.complete", &json!({"status":"complete"}));
            nanos += at.elapsed().as_nanos();
            if i % 100 == 99 {
                std::thread::sleep(Duration::from_millis(120));
            }
        }
        for _ in 0..100 {
            let db = Connection::open(dir.join("sovereign.db")).unwrap();
            let count: i64 = db
                .query_row("SELECT count(*) FROM fact_turn", [], |row| row.get(0))
                .unwrap();
            if count == 1000 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut db = Connection::open(dir.join("sovereign.db")).unwrap();
        let runs: Vec<String> = db.prepare("SELECT id FROM fact_turn ORDER BY id").unwrap()
            .query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(runs.len(), 1000);
        let started = Instant::now();
        let tx = db.transaction().unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms,ended_at_ms,input_tokens,output_tokens,model,provider) VALUES(?1,?2,?2,?2,'tool_followup','Model call','complete',?3,?3,100,20,'local','ollama')").unwrap();
            for (i, run) in runs.iter().enumerate() {
                for n in 0..49 {
                    insert.execute(params![format!("{run}:extra:{n}"), run, now() + i as i64]).unwrap();
                }
            }
        }
        tx.commit().unwrap();
        let write_us = started.elapsed().as_secs_f64() * 1e6 / 49_000.0;
        assert_eq!(db.query_row("SELECT COUNT(*) FROM spans", [], |r| r.get::<_,i64>(0)).unwrap(), 50_000);
        let plan = |sql: &str| -> String {
            db.prepare(sql).unwrap().query_map([], |r| r.get::<_,String>(3)).unwrap()
                .map(Result::unwrap).collect::<Vec<_>>().join("; ")
        };
        println!("list plan: {}", plan("EXPLAIN QUERY PLAN SELECT id FROM fact_turn ORDER BY started_at_ms DESC LIMIT 200"));
        println!("analytics plan: {}", plan("EXPLAIN QUERY PLAN SELECT SUM((SELECT COUNT(*) FROM spans s WHERE s.run_id=r.id AND s.model IS NOT NULL)) FROM fact_turn r WHERE r.started_at_ms>=0"));
        println!("detail plan: {}", plan("EXPLAIN QUERY PLAN SELECT id FROM spans WHERE run_id='bench' ORDER BY started_at_ms"));
        fn measure(mut f: impl FnMut()) -> (f64, f64) {
            let mut samples = Vec::with_capacity(100);
            for _ in 0..100 {
                let at = Instant::now();
                f();
                samples.push(at.elapsed().as_secs_f64() * 1e3);
            }
            samples.sort_by(f64::total_cmp);
            (samples[50], samples[95])
        }
        let list = measure(|| { observer.list(200, None, None, None, None, None).unwrap(); });
        let analytics = measure(|| { observer.analytics(30).unwrap(); });
        let detail = measure(|| { observer.detail(&runs[0]).unwrap(); });
        let mut bytes = 0;
        for suffix in ["", "-wal", "-shm"] {
            if let Ok(metadata) =
                std::fs::metadata(dir.join(format!("sovereign.db{suffix}")))
            {
                bytes += metadata.len();
            }
        }
        println!(
            "observability: {:.1} us/event, {:.1} us/span write, list p50/p95 {:.2}/{:.2} ms, analytics {:.2}/{:.2} ms, detail {:.2}/{:.2} ms, {:.2} MB idle RSS delta, {} bytes at 50k spans, {} dropped",
            nanos as f64 / 3000.0 / 1000.0,
            write_us, list.0, list.1, analytics.0, analytics.1, detail.0, detail.1,
            idle_delta_kb as f64 / 1024.0,
            bytes,
            observer.dropped.load(Ordering::Relaxed)
        );
        drop(observer);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
