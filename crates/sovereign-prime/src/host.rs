//! Engine side of the Prime REPL: one worker process per session, spawned on
//! first use and reaped when idle, so an unused REPL costs no memory.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// Worker exit code monty-alloc uses when the hard memory ceiling is hit.
const OOM_EXIT_CODE: i32 = monty_types::OOM_EXIT_CODE as i32;
/// Whole-run ceiling, including recursive model calls made from the code.
const RUN_TIMEOUT: Duration = Duration::from_secs(300);
const IDLE_REAP_AFTER: Duration = Duration::from_secs(10 * 60);
const MAX_LOAD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_QUERY_CHARS: usize = 200_000;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// Recursive model call used by `llm_query(prompt)`.
pub type LlmQuery = Arc<dyn Fn(String) -> BoxFuture<Result<String>> + Send + Sync>;

pub struct RunOutput {
    pub stdout: String,
    pub value: Option<String>,
    pub error: Option<String>,
    /// The worker was (re)started for this run, so earlier variables are gone.
    pub fresh_state: bool,
    pub host_calls: usize,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    last_used: Instant,
}

pub struct ReplHost {
    exe: PathBuf,
    workers: Mutex<HashMap<String, Arc<Mutex<Option<Worker>>>>>,
}

impl ReplHost {
    /// `exe` is a binary that runs the worker for `__repl-worker`.
    pub fn new(exe: PathBuf) -> Arc<Self> {
        let host = Arc::new(Self { exe, workers: Mutex::new(HashMap::new()) });
        let weak = Arc::downgrade(&host);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(60));
                loop {
                    tick.tick().await;
                    let Some(host) = weak.upgrade() else { return };
                    host.reap_idle().await;
                }
            });
        }
        host
    }

    async fn reap_idle(&self) {
        let slots: Vec<_> = self.workers.lock().await.values().cloned().collect();
        for slot in slots {
            if let Ok(mut guard) = slot.try_lock() {
                if guard.as_ref().is_some_and(|w| w.last_used.elapsed() > IDLE_REAP_AFTER) {
                    *guard = None; // kill_on_drop ends the process
                }
            }
        }
    }

    /// Number of live worker processes (for tests and diagnostics).
    pub async fn live_workers(&self) -> usize {
        let slots: Vec<_> = self.workers.lock().await.values().cloned().collect();
        let mut n = 0;
        for slot in slots {
            n += usize::from(slot.lock().await.is_some());
        }
        n
    }

    async fn spawn(&self) -> Result<Worker> {
        let mut child = Command::new(&self.exe)
            .arg("__repl-worker")
            // The sandbox never sees credentials or the user's environment.
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting the REPL worker")?;
        let stdin = child.stdin.take().context("worker stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().context("worker stdout")?).lines();
        let ready = tokio::time::timeout(Duration::from_secs(10), stdout.next_line())
            .await
            .context("REPL worker did not start")??
            .context("REPL worker exited during startup")?;
        if serde_json::from_str::<Value>(&ready).ok().and_then(|v| v["op"].as_str().map(str::to_owned)).as_deref() != Some("ready") {
            bail!("REPL worker sent an unexpected greeting");
        }
        Ok(Worker { child, stdin, stdout, last_used: Instant::now() })
    }

    pub async fn run(&self, session: &str, code: &str, workdir: Option<&Path>, llm_query: LlmQuery) -> Result<RunOutput> {
        let slot = self.workers.lock().await.entry(session.to_string()).or_default().clone();
        let mut guard = slot.lock().await;
        let fresh_state = guard.is_none();
        if guard.is_none() {
            *guard = Some(self.spawn().await?);
        }
        let worker = guard.as_mut().expect("worker present");
        let result = tokio::time::timeout(RUN_TIMEOUT, drive(worker, code, workdir, &llm_query)).await;
        match result {
            Ok(Ok((stdout, value, error, host_calls))) => {
                worker.last_used = Instant::now();
                Ok(RunOutput { stdout, value, error, fresh_state, host_calls })
            }
            Ok(Err(err)) => {
                let status = worker.child.try_wait().ok().flatten();
                *guard = None;
                if status.and_then(|s| s.code()) == Some(OOM_EXIT_CODE) {
                    bail!("the REPL exceeded its memory limit and was reset; variables were lost")
                }
                Err(err.context("the REPL worker stopped and was reset; variables were lost"))
            }
            Err(_) => {
                *guard = None;
                bail!("the REPL run exceeded {}s and was reset; variables were lost", RUN_TIMEOUT.as_secs())
            }
        }
    }
}

async fn send(worker: &mut Worker, value: Value) -> Result<()> {
    worker.stdin.write_all(format!("{value}\n").as_bytes()).await?;
    worker.stdin.flush().await?;
    Ok(())
}

async fn drive(
    worker: &mut Worker,
    code: &str,
    workdir: Option<&Path>,
    llm_query: &LlmQuery,
) -> Result<(String, Option<String>, Option<String>, usize)> {
    send(worker, json!({"op": "run", "code": code})).await?;
    let mut host_calls = 0;
    loop {
        let line = worker.stdout.next_line().await?.ok_or_else(|| anyhow!("REPL worker exited"))?;
        let msg: Value = serde_json::from_str(&line).context("REPL worker protocol error")?;
        match msg["op"].as_str() {
            Some("done") => {
                let text = |k: &str| msg[k].as_str().map(str::to_owned);
                return Ok((text("stdout").unwrap_or_default(), text("value"), text("error"), host_calls));
            }
            Some("call") => {
                host_calls += 1;
                let arg = msg["args"][0].as_str().unwrap_or_default().to_string();
                let reply = match msg["fn"].as_str() {
                    Some("llm_query") => llm_query(truncate(arg, MAX_QUERY_CHARS)).await,
                    Some("load") => load(workdir, &arg).await,
                    _ => Err(anyhow!("unknown host function")),
                };
                let reply = match reply {
                    Ok(value) => json!({"op": "reply", "value": value}),
                    Err(err) => json!({"op": "reply", "error": format!("{err:#}")}),
                };
                send(worker, reply).await?;
            }
            _ => bail!("REPL worker protocol error"),
        }
    }
}

fn truncate(text: String, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}\n[truncated]", &text[..cut]),
        None => text,
    }
}

/// Read a workspace file for `load(path)`. Confined to the session's working
/// directory after resolving symlinks, and size-capped.
pub async fn load(workdir: Option<&Path>, path: &str) -> Result<String> {
    let root = workdir.context("load() needs a session working directory")?;
    let root = tokio::fs::canonicalize(root).await.context("resolving the working directory")?;
    let requested = Path::new(path);
    let joined = if requested.is_absolute() { requested.to_path_buf() } else { root.join(requested) };
    let resolved = tokio::fs::canonicalize(&joined).await.with_context(|| format!("{path}: not found"))?;
    if !resolved.starts_with(&root) {
        bail!("{path}: outside the working directory");
    }
    let meta = tokio::fs::metadata(&resolved).await?;
    if !meta.is_file() {
        bail!("{path}: not a file");
    }
    if meta.len() > MAX_LOAD_BYTES {
        bail!("{path}: larger than {} MB", MAX_LOAD_BYTES / (1024 * 1024));
    }
    let bytes = tokio::fs::read(&resolved).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
