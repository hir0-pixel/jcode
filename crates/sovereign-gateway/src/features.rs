//! Hermes's own Python backend as an on-demand feature service.
//!
//! The Rust harness owns the hot path (chat, tools, memory, sessions). The
//! ~200 other Hermes features (cron, profiles, skills hub, vault, messaging,
//! voice, ...) are served by Hermes's real Python backend, started only when
//! one of them is first used and stopped again after an idle period, so a
//! chat-only session never loads Python. The backend binds loopback with its
//! own random token; that token never leaves this process.

use anyhow::{Context, Result, bail};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};

/// Hermes can take a while on a cold start (it measured ~1.6 s warm here).
const START_TIMEOUT: Duration = Duration::from_secs(120);
pub const IDLE_STOP_AFTER: Duration = Duration::from_secs(10 * 60);

pub fn idle_stop_after() -> Duration {
    std::env::var("SOVEREIGN_FEATURE_IDLE_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(IDLE_STOP_AFTER)
}

struct Running {
    child: Child,
    port: u16,
}

/// Drop guard from [`Features::lease`].
pub struct Lease(std::sync::Arc<Features>);

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.leases.fetch_sub(1, Ordering::Relaxed);
        self.0.touch("lease-released", "lease");
    }
}

pub struct Features {
    /// Program + leading args (e.g. `["/…/hermes"]`); `serve …` is appended.
    command: Vec<String>,
    pub token: String,
    running: Mutex<Option<Running>>,
    /// Milliseconds since `epoch` of the last forwarded call.
    last_used_ms: AtomicU64,
    epoch: Instant,
    /// Work in flight that must not be idle-stopped (a cron tick running jobs); see [`Lease`].
    leases: AtomicUsize,
    /// Rung when a forwarded call touched the cron API, so the cron timer re-reads `jobs.json`.
    pub cron_changed: Notify,
    /// This gateway's own REST base URL and auth token, so cron (`run_job`)
    /// can call `/api/agent/run` instead of its own `AIAgent`. `None` until
    /// `set_engine_env` runs, right after the gateway binds its port.
    engine_env: std::sync::Mutex<Option<(String, String)>>,
}

impl Features {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            token: crate::auth::generate_token(),
            running: Mutex::new(None),
            last_used_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            leases: AtomicUsize::new(0),
            cron_changed: Notify::new(),
            engine_env: std::sync::Mutex::new(None),
        }
    }

    /// Record the engine's own REST URL and token, passed to the Python
    /// backend on its next (re)spawn as `SOVEREIGN_ENGINE_URL`/`_TOKEN`.
    pub fn set_engine_env(&self, url: String, token: String) {
        *self.engine_env.lock().unwrap_or_else(|e| e.into_inner()) = Some((url, token));
    }

    pub fn touch(&self, route: &str, source: &str) {
        self.last_used_ms.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        if route.starts_with("/api/cron") || route.starts_with("cron.") {
            self.cron_changed.notify_one();
        }
        if std::env::var_os("SOVEREIGN_TRACE_FEATURE_ACTIVITY").is_some() {
            eprintln!("feature_activity at={:?} route={} source={}", SystemTime::now(), route, source);
        }
    }

    /// Keep the backend alive until the returned guard drops.
    pub fn lease(self: &std::sync::Arc<Self>) -> Lease {
        self.leases.fetch_add(1, Ordering::Relaxed);
        Lease(self.clone())
    }

    fn idle_for(&self) -> Duration {
        let last = Duration::from_millis(self.last_used_ms.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(last)
    }

    /// Port of the running backend, starting it if needed.
    pub async fn port(&self) -> Result<u16> {
        self.touch("backend-start-or-reuse", "feature-port");
        let mut running = self.running.lock().await;
        if let Some(r) = running.as_mut() {
            if r.child.try_wait().ok().flatten().is_none() {
                return Ok(r.port);
            }
            *running = None; // exited: start a fresh one
        }
        let (program, args) = self.command.split_first().context("no Hermes backend command configured")?;
        let mut command = Command::new(program);
        #[cfg(unix)]
        command.process_group(0);
        if let Ok(pythonpath) = std::env::var("SOVEREIGN_HERMES_PYTHONPATH") {
            command.env("PYTHONPATH", pythonpath);
        }
        if let Ok(tools_dir) = std::env::var("SOVEREIGN_HERMES_TOOLS_DIR") {
            let mut paths = vec![std::path::PathBuf::from(tools_dir)];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
            command.env("PATH", std::env::join_paths(paths)?);
        }
        if let Some((url, token)) = self.engine_env.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            command.env("SOVEREIGN_ENGINE_URL", url).env("SOVEREIGN_ENGINE_TOKEN", token);
        }
        let mut child = command.args(args)
            .args(["serve", "--host", "127.0.0.1", "--port", "0", "--skip-build"])
            .env("HERMES_DASHBOARD_SESSION_TOKEN", &self.token)
            // Desktop-owned backend. With SOVEREIGN_ENGINE_URL set (below) it runs no cron
            // ticker: the engine's timer fires `POST /api/cron/tick` at the due time.
            .env("HERMES_DESKTOP", "1")
            // Hermes's parent-death watchdog: exit within ~2 s if the engine
            // dies, even by SIGKILL (kill_on_drop only covers clean exits).
            // A start marker without a matching nonce would disarm it, so drop
            // any inherited from the desktop that launched us.
            .env("HERMES_PARENT_PID", std::process::id().to_string())
            .env_remove("HERMES_PARENT_START_MARKER")
            .env_remove("HERMES_PARENT_NONCE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting the Hermes feature backend ({program})"))?;
        let mut lines = BufReader::new(child.stdout.take().context("backend stdout")?).lines();
        let port = tokio::time::timeout(START_TIMEOUT, async {
            while let Some(line) = lines.next_line().await? {
                if let Some(rest) = line.split("HERMES_BACKEND_READY port=").nth(1) {
                    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                    return Ok::<u16, anyhow::Error>(digits.parse()?);
                }
            }
            bail!("the Hermes feature backend exited before it was ready")
        })
        .await
        .context("the Hermes feature backend did not become ready in time")??;
        // Keep draining stdout so a chatty backend never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        *running = Some(Running { child, port });
        Ok(port)
    }

    pub async fn is_running(&self) -> bool {
        let mut running = self.running.lock().await;
        match running.as_mut() {
            Some(r) => r.child.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }

    /// Stop the backend if it has been idle for `IDLE_STOP_AFTER`.
    pub async fn stop_if_idle(&self) {
        if self.leases.load(Ordering::Relaxed) > 0 {
            return;
        }
        if let Ok(home) = std::env::var("HERMES_HOME") {
            if messaging_enabled(std::path::Path::new(&home)) {
                return; // inbound bot traffic never touches the proxy; stopping now would deafen the bots
            }
        }
        if self.idle_for() < idle_stop_after() {
            if std::env::var_os("SOVEREIGN_TRACE_FEATURE_ACTIVITY").is_some() { eprintln!("feature_idle decision=keep idle_ms={} threshold_ms={}", self.idle_for().as_millis(), idle_stop_after().as_millis()); }
            return;
        }
        if std::env::var_os("SOVEREIGN_TRACE_FEATURE_ACTIVITY").is_some() { eprintln!("feature_idle decision=stop idle_ms={}", self.idle_for().as_millis()); }
        if let Some(mut r) = self.running.lock().await.take() {
            kill_backend(&mut r.child).await;
        }
    }

    /// Bots only receive messages while the backend runs and nothing else starts it: when the
    /// config in `home` has a platform enabled, start it if it isn't running (boot, or it died).
    pub async fn ensure_bots(&self, home: &std::path::Path) {
        if messaging_enabled(home) && !self.is_running().await {
            if let Err(err) = self.port().await {
                eprintln!("sovereign: could not start the messaging backend: {err:#}");
            }
        }
    }

    pub async fn stop(&self) {
        if let Some(mut r) = self.running.lock().await.take() {
            kill_backend(&mut r.child).await;
        }
    }
}

/// True when the config the desktop's messaging page writes (`platforms.<id>.enabled`
/// in `config.yaml`, for the home or any profile) has a platform switched on.
pub fn messaging_enabled(home: &std::path::Path) -> bool {
    let mut files = vec![home.join("config.yaml")];
    if let Ok(entries) = std::fs::read_dir(home.join("profiles")) {
        files.extend(entries.flatten().map(|e| e.path().join("config.yaml")));
    }
    files.iter().any(|f| {
        std::fs::read_to_string(f)
            .ok()
            .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(&raw).ok())
            .and_then(|cfg| cfg["platforms"].as_mapping().cloned())
            .is_some_and(|m| m.values().any(|p| p["enabled"].as_bool() == Some(true)))
    })
}

async fn kill_backend(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // The Python backend can leave worker subprocesses behind; own a process
        // group so idle-stop reclaims the entire feature service tree.
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        let _ = child.wait().await;
        return;
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in backend: announces a port, then stays alive.
    fn fake_backend(dir: &std::path::Path, announce: &str) -> Vec<String> {
        let script = dir.join("fake-hermes");
        std::fs::write(&script, format!("#!/bin/sh\necho 'starting'\necho '{announce}'\nexec sleep 30\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        vec![script.to_string_lossy().into_owned()]
    }

    #[tokio::test]
    async fn starts_on_demand_reuses_and_stops_when_idle() {
        let dir = std::env::temp_dir().join(format!("features-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = Features::new(fake_backend(&dir, "HERMES_BACKEND_READY port=4242"));
        assert!(!f.is_running().await, "nothing runs until a feature is used");
        assert_eq!(f.port().await.unwrap(), 4242);
        assert!(f.is_running().await);
        assert_eq!(f.port().await.unwrap(), 4242, "reused, not respawned");
        f.stop_if_idle().await;
        assert!(f.is_running().await, "recently used: kept");
        f.last_used_ms.store(0, Ordering::Relaxed);
        let f2 = Features { epoch: Instant::now() - IDLE_STOP_AFTER - Duration::from_secs(1), ..f };
        f2.stop_if_idle().await;
        assert!(!f2.is_running().await, "idle: stopped");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bots_start_the_backend_and_a_leased_one_is_never_idle_stopped() {
        let dir = std::env::temp_dir().join(format!("features-bots-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = std::sync::Arc::new(Features::new(fake_backend(&dir, "HERMES_BACKEND_READY port=4243")));
        f.ensure_bots(&dir).await;
        assert!(!f.is_running().await, "no bot enabled: stays off");
        std::fs::write(dir.join("config.yaml"), "platforms:\n  telegram:\n    enabled: true\n").unwrap();
        f.ensure_bots(&dir).await;
        assert!(f.is_running().await, "bot enabled: started without any feature call");
        // A request in flight holds a lease: idle time alone must not stop the backend.
        let f = std::sync::Arc::new(Features { epoch: Instant::now() - IDLE_STOP_AFTER - Duration::from_secs(9), ..std::sync::Arc::try_unwrap(f).ok().unwrap() });
        std::fs::remove_file(dir.join("config.yaml")).unwrap();
        let lease = f.lease();
        f.stop_if_idle().await;
        assert!(f.is_running().await, "leased: kept");
        drop(lease);
        f.last_used_ms.store(0, Ordering::Relaxed);
        f.stop_if_idle().await;
        assert!(!f.is_running().await, "lease released and idle: stopped");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn messaging_enabled_reads_platform_flags() {
        let dir = std::env::temp_dir().join(format!("features-msg-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("profiles/work")).unwrap();
        assert!(!messaging_enabled(&dir));
        std::fs::write(dir.join("config.yaml"), "platforms:\n  telegram:\n    enabled: false\n").unwrap();
        assert!(!messaging_enabled(&dir));
        std::fs::write(dir.join("profiles/work/config.yaml"), "platforms:\n  whatsapp:\n    enabled: true\n").unwrap();
        assert!(messaging_enabled(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_backend_that_never_announces_is_an_error() {
        let dir = std::env::temp_dir().join(format!("features-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("dies");
        std::fs::write(&script, "#!/bin/sh\necho nope\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let f = Features::new(vec![script.to_string_lossy().into_owned()]);
        assert!(f.port().await.unwrap_err().to_string().contains("exited before"));
        assert!(Features::new(vec![]).port().await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

}
