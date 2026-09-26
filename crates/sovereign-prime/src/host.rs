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

/// Whole-run ceiling, including recursive model calls made from the code.
const RUN_TIMEOUT: Duration = Duration::from_secs(20);
const IDLE_REAP_AFTER: Duration = Duration::from_secs(10 * 60);
const MAX_KERNEL_RSS_BYTES: u64 = 128 * 1024 * 1024;
const MAX_LOAD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_QUERY_CHARS: usize = 200_000;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// Recursive model call used by `llm_query(prompt)`.
pub type LlmQuery = Arc<dyn Fn(String) -> BoxFuture<Result<String>> + Send + Sync>;
/// `refine(op_json)` where `op_json` is `{"op":"run","instructions":...,"global":...}`
/// or `{"op":"status"}`; mirrors Prime's `refine.run()`/`refine.status()`. Like
/// the model-callable `refine` tool, this only *schedules* a refinement
/// (applied at turn end, never mid-turn) or reports whether one is pending.
pub type Refine = Arc<dyn Fn(String) -> BoxFuture<Result<String>> + Send + Sync>;
/// Generic host callback (`goal`, `heartbeat`, `spawn_subagent`, `agent_message`).
pub type HostFn = Arc<dyn Fn(String) -> BoxFuture<Result<String>> + Send + Sync>;

/// Optional REPL host hooks beyond `llm_query` / `refine`.
#[derive(Clone)]
pub struct ExtraHostFns {
    pub goal: HostFn,
    pub heartbeat: HostFn,
    pub spawn_subagent: HostFn,
    pub agent_message: HostFn,
}

impl Default for ExtraHostFns {
    fn default() -> Self {
        fn unavailable(name: &'static str) -> HostFn {
            Arc::new(move |_| {
                let name = name.to_string();
                Box::pin(async move { Err(anyhow!("{name} is not available")) })
            })
        }
        Self {
            goal: unavailable("goal"),
            heartbeat: unavailable("heartbeat"),
            spawn_subagent: unavailable("spawn_subagent"),
            agent_message: unavailable("agent_message"),
        }
    }
}

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
    pid: u32,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    last_used: Instant,
    _session_tmp: Option<PathBuf>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        if let Some(path) = &self._session_tmp {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

pub struct ReplHost {
    python: PathBuf,
    workers: Mutex<HashMap<String, Arc<Mutex<Option<Worker>>>>>,
}

impl ReplHost {
    /// `python` is the Hermes-bundled CPython interpreter.
    pub fn new(python: PathBuf) -> Arc<Self> {
        let host = Arc::new(Self {
            python,
            workers: Mutex::new(HashMap::new()),
        });
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
                if guard
                    .as_ref()
                    .is_some_and(|w| w.last_used.elapsed() > IDLE_REAP_AFTER)
                {
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

    /// Stop and remove the persistent kernel when its owning chat is deleted.
    pub async fn stop_session(&self, session: &str) {
        if let Some(slot) = self.workers.lock().await.remove(session) {
            *slot.lock().await = None;
        }
    }

    #[cfg(not(target_os = "macos"))]
    async fn spawn(&self, workdir: Option<&Path>, approved: bool) -> Result<Worker> {
        if !approved {
            bail!("Python REPL cell denied; this platform requires per-cell approval")
        }
        self.spawn_approved(workdir).await
    }

    #[cfg(target_os = "macos")]
    async fn spawn(&self, workdir: Option<&Path>, _approved: bool) -> Result<Worker> {
        self.spawn_approved(workdir).await
    }

    #[cfg(target_os = "macos")]
    async fn spawn_approved(&self, workdir: Option<&Path>) -> Result<Worker> {
        let (mut child, session_tmp) = {
            let cwd;
            let project = match workdir {
                Some(path) => path,
                None => {
                    cwd = std::env::current_dir()?;
                    &cwd
                }
            };
            let project = std::fs::canonicalize(project)
                .context("resolving the session working directory")?;
            let session_tmp =
                std::env::temp_dir().join(format!("sovereign-repl-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&session_tmp)?;
            let skills = jcode_storage::jcode_dir()?.join("skills");
            std::fs::create_dir_all(&skills)?;
            let runtime = self
                .python
                .parent()
                .and_then(Path::parent)
                .context("resolving bundled Python runtime")?;
            let profile = sandbox_profile(&self.python, runtime, &project, &session_tmp, &skills);
            let child = Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &profile])
                .arg(&self.python)
                .args(["-I", "-S", "-u", "-c", crate::worker::PYTHON_WORKER])
                .arg(&project)
                .arg(&skills)
                // The worker receives no model credentials or arbitrary environment.
                .env_clear()
                .env("PYTHONNOUSERSITE", "1")
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .env("TMPDIR", &session_tmp)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context("starting the sandboxed CPython worker")?;
            (child, session_tmp)
        };
        let stdin = child.stdin.take().context("worker stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().context("worker stdout")?).lines();
        let ready = tokio::time::timeout(Duration::from_secs(10), stdout.next_line())
            .await
            .context("REPL worker did not start")??
            .context("REPL worker exited during startup")?;
        let ready: Value =
            serde_json::from_str(&ready).context("REPL worker sent malformed startup data")?;
        if ready["op"] != "ready" {
            bail!("REPL worker sent an unexpected greeting");
        }
        let pid = ready["pid"]
            .as_u64()
            .unwrap_or_else(|| child.id().unwrap_or_default() as u64) as u32;
        Ok(Worker {
            child,
            pid,
            stdin,
            stdout,
            last_used: Instant::now(),
            _session_tmp: Some(session_tmp),
        })
    }

    #[cfg(not(target_os = "macos"))]
    async fn spawn_approved(&self, workdir: Option<&Path>) -> Result<Worker> {
        let cwd;
        let project = match workdir {
            Some(path) => path,
            None => {
                cwd = std::env::current_dir()?;
                &cwd
            }
        };
        let project = std::fs::canonicalize(project)?;
        let session_tmp =
            std::env::temp_dir().join(format!("sovereign-repl-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&session_tmp)?;
        let skills = jcode_storage::jcode_dir()?.join("skills");
        std::fs::create_dir_all(&skills)?;
        let mut child = Command::new(&self.python)
            .args(["-I", "-S", "-u", "-c", crate::worker::PYTHON_WORKER])
            .arg(&project)
            .arg(&skills)
            .env_clear()
            .env("PYTHONNOUSERSITE", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("TMPDIR", &session_tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting the approved CPython worker")?;
        let stdin = child.stdin.take().context("worker stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().context("worker stdout")?).lines();
        let ready = tokio::time::timeout(Duration::from_secs(10), stdout.next_line())
            .await
            .context("REPL worker did not start")??
            .context("REPL worker exited during startup")?;
        let ready: Value = serde_json::from_str(&ready)?;
        if ready["op"] != "ready" {
            bail!("REPL worker sent an unexpected greeting");
        }
        let pid = ready["pid"]
            .as_u64()
            .unwrap_or_else(|| child.id().unwrap_or_default() as u64) as u32;
        Ok(Worker {
            child,
            pid,
            stdin,
            stdout,
            last_used: Instant::now(),
            _session_tmp: Some(session_tmp),
        })
    }

    pub async fn run(
        &self,
        session: &str,
        code: &str,
        workdir: Option<&Path>,
        llm_query: LlmQuery,
        refine: Refine,
        extra: ExtraHostFns,
        approved: bool,
    ) -> Result<RunOutput> {
        #[cfg(not(target_os = "macos"))]
        if !approved {
            bail!("Python REPL cell denied; this platform requires per-cell approval")
        }
        let slot = self
            .workers
            .lock()
            .await
            .entry(session.to_string())
            .or_default()
            .clone();
        let mut guard = slot.lock().await;
        let fresh_state = guard.is_none();
        if guard.is_none() {
            *guard = Some(self.spawn(workdir, approved).await?);
        }
        let worker = guard.as_mut().expect("worker present");
        #[cfg(target_os = "macos")]
        let worker_pid = worker.pid;
        #[cfg(target_os = "macos")]
        let result = tokio::select! {
            result = tokio::time::timeout(RUN_TIMEOUT, drive(worker, code, workdir, &llm_query, &refine, &extra)) => result,
            _ = wait_for_rss_limit(worker_pid) => {
                let _ = worker.child.start_kill();
                *guard = None;
                bail!("kernel exceeded 128 MB memory cap and was restarted; session variables were lost")
            }
        };
        #[cfg(target_os = "macos")]
        if matches!(&result, Ok(Ok(_)))
            && process_rss_bytes(worker_pid).is_some_and(|rss| rss > MAX_KERNEL_RSS_BYTES)
        {
            let _ = worker.child.start_kill();
            *guard = None;
            bail!(
                "kernel exceeded 128 MB memory cap and was restarted; session variables were lost"
            )
        }
        #[cfg(not(target_os = "macos"))]
        let result = tokio::time::timeout(
            RUN_TIMEOUT,
            drive(worker, code, workdir, &llm_query, &refine, &extra),
        )
        .await;
        match result {
            Ok(Ok((stdout, value, error, host_calls))) => {
                worker.last_used = Instant::now();
                Ok(RunOutput {
                    stdout,
                    value,
                    error,
                    fresh_state,
                    host_calls,
                })
            }
            Ok(Err(err)) => {
                *guard = None;
                Err(err.context("the REPL worker stopped and was reset; variables were lost"))
            }
            Err(_) => {
                *guard = None;
                bail!(
                    "the REPL run exceeded {}s and was reset; variables were lost",
                    RUN_TIMEOUT.as_secs()
                )
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcTaskInfo {
    virtual_size: u64,
    resident_size: u64,
    total_user: u64,
    total_system: u64,
    threads_user: u64,
    threads_system: u64,
    policy: i32,
    faults: i32,
    pageins: i32,
    cow_faults: i32,
    messages_sent: i32,
    messages_received: i32,
    syscalls_mach: i32,
    syscalls_unix: i32,
    context_switches: i32,
    thread_count: i32,
    running_count: i32,
    priority: i32,
}

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut ProcTaskInfo, size: i32) -> i32;
}

#[cfg(target_os = "macos")]
async fn wait_for_rss_limit(pid: u32) {
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if process_rss_bytes(pid).is_some_and(|rss| rss > MAX_KERNEL_RSS_BYTES) {
            return;
        }
    }
}

#[cfg(target_os = "macos")]
fn process_rss_bytes(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<ProcTaskInfo>::uninit();
    let size = std::mem::size_of::<ProcTaskInfo>() as i32;
    // PROC_PIDTASKINFO = 4 in Apple's libproc API.
    let read = unsafe { proc_pidinfo(pid as i32, 4, 0, info.as_mut_ptr(), size) };
    (read == size).then(|| unsafe { info.assume_init() }.resident_size)
}

#[cfg(target_os = "macos")]
fn sbpl_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn sandbox_profile(
    python: &Path,
    runtime: &Path,
    project: &Path,
    temp: &Path,
    skills: &Path,
) -> String {
    format!(
        "(version 1)\n(deny default)\n(import \"system.sb\")\n(deny network*)\n(deny file-write*)\n(deny file-read* (subpath \"/Users\"))\n(deny file-read* (subpath \"/etc\"))\n(deny file-read* (subpath \"/private/etc\"))\n(deny file-read* (subpath \"/Volumes\"))\n(allow process-exec (literal \"{}\"))\n(allow file-read* (subpath \"{}\") (subpath \"{}\") (subpath \"{}\") (subpath \"/System/Library\") (subpath \"/usr/lib\") (subpath \"/Library/Developer/CommandLineTools\"))\n(allow file-write* (subpath \"{}\") (subpath \"{}\"))\n",
        sbpl_path(python),
        sbpl_path(runtime),
        sbpl_path(project),
        sbpl_path(skills),
        sbpl_path(project),
        sbpl_path(temp)
    )
}

async fn send(worker: &mut Worker, value: Value) -> Result<()> {
    worker
        .stdin
        .write_all(format!("{value}\n").as_bytes())
        .await?;
    worker.stdin.flush().await?;
    Ok(())
}

async fn drive(
    worker: &mut Worker,
    code: &str,
    workdir: Option<&Path>,
    llm_query: &LlmQuery,
    refine: &Refine,
    extra: &ExtraHostFns,
) -> Result<(String, Option<String>, Option<String>, usize)> {
    send(worker, json!({"op": "run", "code": code})).await?;
    let mut host_calls = 0;
    loop {
        let line = worker
            .stdout
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("REPL worker exited"))?;
        let msg: Value = serde_json::from_str(&line).context("REPL worker protocol error")?;
        match msg["op"].as_str() {
            Some("done") => {
                let text = |k: &str| msg[k].as_str().map(str::to_owned);
                return Ok((
                    text("stdout").unwrap_or_default(),
                    text("value"),
                    text("error"),
                    host_calls,
                ));
            }
            Some("call") => {
                host_calls += 1;
                let arg = msg["args"][0].as_str().unwrap_or_default().to_string();
                let reply = match msg["fn"].as_str() {
                    Some("llm_query") => llm_query(truncate(arg.clone(), MAX_QUERY_CHARS)).await,
                    Some("load") => load(workdir, &arg).await,
                    Some("refine") => refine(truncate(arg, MAX_QUERY_CHARS)).await,
                    Some("goal") => (extra.goal)(truncate(arg, MAX_QUERY_CHARS)).await,
                    Some("heartbeat") => (extra.heartbeat)(truncate(arg, MAX_QUERY_CHARS)).await,
                    Some("spawn_subagent") => {
                        (extra.spawn_subagent)(truncate(arg, MAX_QUERY_CHARS)).await
                    }
                    Some("agent_message") => {
                        (extra.agent_message)(truncate(arg, MAX_QUERY_CHARS)).await
                    }
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
    let root = tokio::fs::canonicalize(root)
        .await
        .context("resolving the working directory")?;
    let requested = Path::new(path);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let resolved = tokio::fs::canonicalize(&joined)
        .await
        .with_context(|| format!("{path}: not found"))?;
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
