//! Bounded, local run ledger. All SQLite work stays off the agent path.

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const QUEUE: usize = 1024;
const BATCH: usize = 128;
const CONTENT_LIMIT: usize = 4096;

fn now() -> i64 {
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
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        cost: Option<f64>,
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
}

pub struct Observer {
    path: PathBuf,
    tx: SyncSender<Op>,
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    capture_content: bool,
    sessions: Mutex<HashMap<String, Active>>,
}

impl Observer {
    pub fn open(home: &Path, provider: &str, model: &str) -> rusqlite::Result<Arc<Self>> {
        std::fs::create_dir_all(home).map_err(|_| rusqlite::Error::InvalidPath(home.into()))?;
        let path = home.join("observability.sqlite3");
        let mut db = Connection::open(&path)?;
        setup(&mut db)?;
        let capture_content = std::fs::read(home.join("observability.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| v["capture_content"].as_bool())
            .unwrap_or(false);
        refresh_children(&mut db, home, capture_content)?;
        db.execute(
            "UPDATE runs SET status='interrupted',ended_at_ms=?1 WHERE status='spawned'",
            [now()],
        )?;
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let pending = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let observer = Arc::new(Self {
            path,
            tx,
            pending: pending.clone(),
            dropped: dropped.clone(),
            capture_content,
            sessions: Mutex::new(HashMap::new()),
        });
        let writer_home = home.to_path_buf();
        let writer_provider = provider.to_string();
        let writer_model = model.to_string();
        std::thread::Builder::new()
            .name("sovereign-observability".into())
            .spawn(move || {
                let mut db = db;
                let mut last_prune = now();
                let mut last_child_check = Instant::now();
                loop {
                    let first = match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(op) => op,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
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
                    if now() - last_prune >= 86_400_000 {
                        if let Err(err) = prune(&db, now()) {
                            eprintln!("sovereign-observability: prune failed: {err}");
                        }
                        last_prune = now();
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
        match self.tx.try_send(op) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.pending.fetch_sub(1, Ordering::Relaxed);
                if !content {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.pending.fetch_sub(1, Ordering::Relaxed);
                if !content {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    pub fn start_turn(&self, session: &str, prompt: &str) -> String {
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
                kind: "invoke_agent",
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
                self.send(
                    Op::RunEnd {
                        id: run.clone(),
                        status: status.into(),
                        error: active.error.take(),
                        at: now(),
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
            }
            "error" => active.error = payload["message"].as_str().map(capped),
            _ => {}
        }
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
                self.send(
                    Op::Usage {
                        run,
                        span,
                        root: active.root.clone().unwrap_or_default(),
                        input,
                        output,
                        cache_read,
                        cache_write,
                        cost,
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
                },
                false,
            );
        }
    }

    pub fn list(&self, limit: u64) -> rusqlite::Result<Value> {
        let db = Connection::open(&self.path)?;
        let mut stmt = db.prepare("SELECT id,session_id,parent_id,root_id,kind,model,provider,status,started_at_ms,ended_at_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd,error FROM runs ORDER BY started_at_ms DESC LIMIT ?1")?;
        let rows = stmt
            .query_map([limit.min(200)], run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({ "runs": rows, "dropped_events": self.dropped.load(Ordering::Relaxed), "capture_content": self.capture_content }),
        )
    }

    pub fn detail(&self, id: &str) -> rusqlite::Result<Value> {
        let db = Connection::open(&self.path)?;
        let mut stmt = db.prepare("SELECT id,session_id,parent_id,root_id,kind,model,provider,status,started_at_ms,ended_at_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd,error FROM runs WHERE id=?1")?;
        let run = stmt.query_row([id], run_row)?;
        let mut spans = db.prepare("SELECT s.id,s.run_id,s.parent_id,s.root_id,s.kind,s.name,s.status,s.started_at_ms,s.ended_at_ms,s.input_tokens,s.output_tokens,s.cache_read_tokens,s.cache_write_tokens,s.cost_usd,s.error,c.input,c.output FROM spans s LEFT JOIN content c ON c.id=s.id WHERE s.run_id=?1 ORDER BY s.started_at_ms")?;
        let spans = spans.query_map([id], |r| Ok(json!({"id":r.get::<_,String>(0)?,"run_id":r.get::<_,String>(1)?,"parent_id":r.get::<_,String>(2)?,"root_id":r.get::<_,String>(3)?,"kind":r.get::<_,String>(4)?,"name":r.get::<_,String>(5)?,"status":r.get::<_,String>(6)?,"started_at_ms":r.get::<_,i64>(7)?,"ended_at_ms":r.get::<_,Option<i64>>(8)?,"input_tokens":r.get::<_,i64>(9)?,"output_tokens":r.get::<_,i64>(10)?,"cache_read_tokens":r.get::<_,i64>(11)?,"cache_write_tokens":r.get::<_,i64>(12)?,"cost_usd":r.get::<_,Option<f64>>(13)?,"error":r.get::<_,Option<String>>(14)?,"input":r.get::<_,Option<String>>(15)?,"output":r.get::<_,Option<String>>(16)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let content: Option<(Option<String>, Option<String>)> = db
            .query_row("SELECT input,output FROM content WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .ok();
        Ok(
            json!({"run":run,"spans":spans,"content":content.map(|(input,output)| json!({"input":input,"output":output}))}),
        )
    }
}

fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"session_id":r.get::<_,String>(1)?,"parent_id":r.get::<_,Option<String>>(2)?,"root_id":r.get::<_,String>(3)?,"kind":r.get::<_,String>(4)?,"model":r.get::<_,String>(5)?,"provider":r.get::<_,String>(6)?,"status":r.get::<_,String>(7)?,"started_at_ms":r.get::<_,i64>(8)?,"ended_at_ms":r.get::<_,Option<i64>>(9)?,"input_tokens":r.get::<_,i64>(10)?,"output_tokens":r.get::<_,i64>(11)?,"cache_read_tokens":r.get::<_,i64>(12)?,"cache_write_tokens":r.get::<_,i64>(13)?,"cost_usd":r.get::<_,Option<f64>>(14)?,"error":r.get::<_,Option<String>>(15)?}),
    )
}

fn setup(db: &mut Connection) -> rusqlite::Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=1000;
        CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,parent_id TEXT,root_id TEXT NOT NULL,kind TEXT NOT NULL,model TEXT NOT NULL,provider TEXT NOT NULL,status TEXT NOT NULL,started_at_ms INTEGER NOT NULL,ended_at_ms INTEGER,input_tokens INTEGER NOT NULL DEFAULT 0,output_tokens INTEGER NOT NULL DEFAULT 0,cache_read_tokens INTEGER NOT NULL DEFAULT 0,cache_write_tokens INTEGER NOT NULL DEFAULT 0,cost_usd REAL,error TEXT,unpriced_calls INTEGER NOT NULL DEFAULT 0);
        CREATE INDEX IF NOT EXISTS runs_recent ON runs(started_at_ms DESC);
        CREATE INDEX IF NOT EXISTS runs_session ON runs(session_id,started_at_ms DESC);
        CREATE TABLE IF NOT EXISTS spans(id TEXT PRIMARY KEY,run_id TEXT NOT NULL,parent_id TEXT NOT NULL,root_id TEXT NOT NULL,kind TEXT NOT NULL,name TEXT NOT NULL,status TEXT NOT NULL,started_at_ms INTEGER NOT NULL,ended_at_ms INTEGER,input_tokens INTEGER NOT NULL DEFAULT 0,output_tokens INTEGER NOT NULL DEFAULT 0,cache_read_tokens INTEGER NOT NULL DEFAULT 0,cache_write_tokens INTEGER NOT NULL DEFAULT 0,cost_usd REAL,error TEXT);
        CREATE INDEX IF NOT EXISTS spans_run ON spans(run_id,started_at_ms);
        CREATE TABLE IF NOT EXISTS content(id TEXT PRIMARY KEY,input TEXT,output TEXT);")?;
    db.execute(
        "UPDATE runs SET status='interrupted',ended_at_ms=?1 WHERE status IN ('running','queued')",
        [now()],
    )?;
    db.execute(
        "UPDATE spans SET status='interrupted',ended_at_ms=?1 WHERE status='running'",
        [now()],
    )?;
    prune(db, now())?;
    Ok(())
}

fn prune(db: &Connection, at: i64) -> rusqlite::Result<()> {
    let day = 86_400_000_i64;
    db.execute("DELETE FROM content WHERE id IN (SELECT id FROM runs WHERE started_at_ms<?1 UNION SELECT id FROM spans WHERE started_at_ms<?1)", [at - 7 * day])?;
    db.execute(
        "DELETE FROM spans WHERE ended_at_ms IS NOT NULL AND ended_at_ms<?1",
        [at - 30 * day],
    )?;
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
                model,
                provider,
                status,
                at,
            } => {
                let initial_cost = provider.eq_ignore_ascii_case("ollama").then_some(0.0);
                tx.execute("INSERT OR IGNORE INTO runs(id,session_id,parent_id,root_id,kind,model,provider,status,started_at_ms,cost_usd) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![id,session,parent,root,kind,model,provider,status,at,initial_cost])?;
            }
            Op::RunActivate { id } => {
                tx.execute("UPDATE runs SET status='running' WHERE id=?1", [id])?;
            }
            Op::RunEnd {
                id,
                status,
                error,
                at,
            } => {
                tx.execute(
                    "UPDATE runs SET status=?2,error=?3,ended_at_ms=?4 WHERE id=?1",
                    params![id, status, error, at],
                )?;
            }
            Op::SpanStart {
                id,
                run,
                root,
                kind,
                name,
                at,
            } => {
                tx.execute("INSERT OR IGNORE INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms) VALUES(?1,?2,?2,?3,?4,?5,'running',?6)", params![id,run,root,kind,name,at])?;
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
                input,
                output,
                cache_read,
                cache_write,
                cost,
                started,
                ended,
            } => {
                tx.execute("INSERT OR IGNORE INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms,ended_at_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd) VALUES(?1,?2,?2,?3,'chat','Model call','complete',?4,?5,?6,?7,?8,?9,?10)", params![span,run,root,started,ended,input,output,cache_read,cache_write,cost])?;
                tx.execute("UPDATE runs SET input_tokens=input_tokens+?2,output_tokens=output_tokens+?3,cache_read_tokens=cache_read_tokens+?4,cache_write_tokens=cache_write_tokens+?5,unpriced_calls=unpriced_calls+CASE WHEN ?6 IS NULL THEN 1 ELSE 0 END,cost_usd=CASE WHEN ?6 IS NULL OR unpriced_calls>0 THEN NULL ELSE COALESCE(cost_usd,0)+?6 END WHERE id=?1", params![run,input,output,cache_read,cache_write,cost])?;
            }
            Op::Content { id, input, output } => {
                tx.execute("INSERT INTO content(id,input,output) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET input=COALESCE(excluded.input,input),output=COALESCE(excluded.output,output)", params![id,input,output])?;
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
    let mut stmt = db.prepare("SELECT c.id,c.session_id,c.root_id,p.session_id FROM runs c JOIN runs p ON p.id=c.parent_id WHERE c.status='spawned'")?;
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
                total_cost = total_cost
                    .and_then(|sum| cost_usd(provider, model, i, o, r, w).map(|price| sum + price));
                calls.push((
                    message_time(message),
                    i,
                    o,
                    r,
                    w,
                    cost_usd(provider, model, i, o, r, w),
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
        let tx = db.transaction()?;
        tx.execute("UPDATE runs SET status='complete',ended_at_ms=?2,provider=?3,model=?4,input_tokens=?5,output_tokens=?6,cache_read_tokens=?7,cache_write_tokens=?8,cost_usd=?9,unpriced_calls=?10 WHERE id=?1", params![run,ended,provider,model,input,output,read,write,total_cost,if total_cost.is_none() { 1 } else { 0 }])?;
        for (n, (at, i, o, r, w, cost)) in calls.into_iter().enumerate() {
            tx.execute("INSERT OR IGNORE INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms,ended_at_ms,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd) VALUES(?1,?2,?2,?3,'chat','Model call','complete',?4,?4,?5,?6,?7,?8,?9)", params![format!("{run}:chat:{n}"),run,root,at,i,o,r,w,cost])?;
        }
        for (id, at, result) in results {
            if let Some((name, started, args)) = tools.remove(&id) {
                let span = format!("{run}:tool:{id}");
                tx.execute("INSERT OR IGNORE INTO spans(id,run_id,parent_id,root_id,kind,name,status,started_at_ms,ended_at_ms) VALUES(?1,?2,?2,?3,'execute_tool',?4,'complete',?5,?6)", params![span,run,root,name,started,at])?;
                if capture_content {
                    tx.execute(
                        "INSERT OR IGNORE INTO content(id,input,output) VALUES(?1,?2,?3)",
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
                "INSERT OR IGNORE INTO content(id,input,output) VALUES(?1,?2,?3)",
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
    let provider = provider.to_ascii_lowercase();
    if provider == "ollama" {
        return Some(0.0);
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
    fn writer_cost_recovery_and_retention() {
        let dir = std::env::temp_dir().join(format!("sovereign-observability-test-{}", now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("observability.sqlite3");
        let mut db = Connection::open(&path).unwrap();
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
                    model: "gpt-5.4".into(),
                    provider: "openai".into(),
                    status: "running",
                    at: now(),
                },
                Op::Usage {
                    run: "r".into(),
                    span: "c".into(),
                    root: "r".into(),
                    input: 100,
                    output: 50,
                    cache_read: 20,
                    cache_write: 0,
                    cost: cost_usd("openai", "gpt-5.4", 100, 50, 20, 0),
                    started: now(),
                    ended: now(),
                },
            ],
        )
        .unwrap();
        assert!(cost_usd("openai", "gpt-5.4", 100, 50, 20, 0).unwrap() > 0.0);
        assert_eq!(cost_usd("unknown", "x", 100, 50, 0, 0), None);
        setup(&mut db).unwrap();
        let status: String = db
            .query_row("SELECT status FROM runs WHERE id='r'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "interrupted");
        db.execute(
            "UPDATE spans SET ended_at_ms=?1",
            [now() - 31 * 86_400_000_i64],
        )
        .unwrap();
        prune(&db, now()).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM spans", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))
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
        let observer = Observer::open(&dir, "Ollama", "local").unwrap();
        let root = observer.start_turn("parent", "hello");
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
            if observer.list(10).unwrap()["runs"].as_array().unwrap().len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let rows = observer.list(10).unwrap();
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
            if observer.list(10).unwrap()["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["session_id"] == "child-session" && row["status"] == "complete")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let complete = observer.list(10).unwrap();
        let child = complete["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["session_id"] == "child-session")
            .unwrap();
        assert_eq!(child["status"], "complete");
        assert_eq!(child["input_tokens"], 9);
        assert_eq!(child["cost_usd"], 0.0);
        drop(observer);
        std::fs::remove_dir_all(dir).unwrap();
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
        let observer = Observer::open(&dir, "Ollama", "local").unwrap();
        let idle_delta_kb = rss_kb().saturating_sub(before);
        let mut nanos = 0_u128;
        for i in 0..1000 {
            let session = format!("bench-{i}");
            let at = Instant::now();
            observer.start_turn(&session, "prompt");
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
            let db = Connection::open(dir.join("observability.sqlite3")).unwrap();
            let count: i64 = db
                .query_row("SELECT count(*) FROM runs", [], |row| row.get(0))
                .unwrap();
            if count == 1000 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut bytes = 0;
        for suffix in ["", "-wal", "-shm"] {
            if let Ok(metadata) =
                std::fs::metadata(dir.join(format!("observability.sqlite3{suffix}")))
            {
                bytes += metadata.len();
            }
        }
        println!(
            "observability: {:.1} us/event, {:.2} MB idle RSS delta, {} bytes/1000 turns, {} dropped",
            nanos as f64 / 3000.0 / 1000.0,
            idle_delta_kb as f64 / 1024.0,
            bytes,
            observer.dropped.load(Ordering::Relaxed)
        );
        drop(observer);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
