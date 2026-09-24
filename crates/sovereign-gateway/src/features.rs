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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// Hermes can take a while on a cold start (it measured ~1.6 s warm here).
const START_TIMEOUT: Duration = Duration::from_secs(120);
pub const IDLE_STOP_AFTER: Duration = Duration::from_secs(10 * 60);

struct Running {
    child: Child,
    port: u16,
}

pub struct Features {
    /// Program + leading args (e.g. `["/…/hermes"]`); `serve …` is appended.
    command: Vec<String>,
    pub token: String,
    running: Mutex<Option<Running>>,
    /// Milliseconds since `epoch` of the last forwarded call.
    last_used_ms: AtomicU64,
    epoch: Instant,
}

impl Features {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            token: crate::auth::generate_token(),
            running: Mutex::new(None),
            last_used_ms: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    pub fn touch(&self) {
        self.last_used_ms.store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last = Duration::from_millis(self.last_used_ms.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(last)
    }

    /// Port of the running backend, starting it if needed.
    pub async fn port(&self) -> Result<u16> {
        self.touch();
        let mut running = self.running.lock().await;
        if let Some(r) = running.as_mut() {
            if r.child.try_wait().ok().flatten().is_none() {
                return Ok(r.port);
            }
            *running = None; // exited: start a fresh one
        }
        let (program, args) = self.command.split_first().context("no Hermes backend command configured")?;
        let mut child = Command::new(program)
            .args(args)
            .args(["serve", "--host", "127.0.0.1", "--port", "0", "--skip-build"])
            .env("HERMES_DASHBOARD_SESSION_TOKEN", &self.token)
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
        if self.idle_for() < IDLE_STOP_AFTER {
            return;
        }
        if let Some(mut r) = self.running.lock().await.take() {
            let _ = r.child.kill().await;
        }
    }

    pub async fn stop(&self) {
        if let Some(mut r) = self.running.lock().await.take() {
            let _ = r.child.kill().await;
        }
    }
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
