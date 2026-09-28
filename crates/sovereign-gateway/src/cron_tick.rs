//! The engine owns cron *timing*. Hermes still owns the job store, the run ledger and delivery, so
//! this timer reads the `next_run_at` Hermes wrote to `jobs.json`, sleeps until the earliest one
//! (no polling, no idle CPU), and only then wakes the on-demand Python backend for one
//! `POST /api/cron/tick`. Hermes runs its normal tick there: the model turn goes to the engine
//! (`/api/agent/run`), the outcome and next run are written back, and results are delivered.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::features::Features;

/// Longest sleep with the backend down: catches `jobs.json` edits made outside the desktop.
const RESCAN: Duration = Duration::from_secs(10 * 60);
/// While the backend runs its agent tools may add jobs, so look again sooner.
const RESCAN_LIVE: Duration = Duration::from_secs(30);
/// Never fire twice within this, so a job a tick cannot advance cannot spin the timer.
const MIN_GAP: Duration = Duration::from_secs(30);
/// A tick runs every due job to completion.
const TICK_TIMEOUT: Duration = Duration::from_secs(3 * 3600);

fn job_stores(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![home.join("cron").join("jobs.json")];
    if let Ok(entries) = std::fs::read_dir(home.join("profiles")) {
        out.extend(entries.flatten().map(|e| e.path().join("cron").join("jobs.json")).filter(|p| p.is_file()));
    }
    out
}

fn job_is_fireable(job: &Value) -> bool {
    // Mirrors cron.jobs "runnable": enabled (default true), not paused/terminal, has next_run_at.
    if job.get("enabled") == Some(&Value::Bool(false)) {
        return false;
    }
    if matches!(job.get("state").and_then(Value::as_str), Some("paused" | "completed" | "error")) {
        return false;
    }
    if job.get("paused_at").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
        return false;
    }
    job.get("next_run_at").and_then(Value::as_str).is_some()
}

/// Earliest `next_run_at` among fireable jobs of the home and its profiles.
pub fn next_due(home: &Path) -> Option<SystemTime> {
    job_stores(home)
        .iter()
        .filter_map(|store| serde_json::from_str::<Value>(&std::fs::read_to_string(store).ok()?).ok())
        .flat_map(|value| match value {
            Value::Array(list) => list,
            Value::Object(map) => map.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default(),
            _ => Vec::new(),
        })
        .filter(job_is_fireable)
        .filter_map(|job| DateTime::parse_from_rfc3339(job["next_run_at"].as_str()?).ok())
        .map(|dt| SystemTime::from(dt.with_timezone(&Utc)))
        .min()
}

/// How long to sleep given the next due time.
fn wait_for(due: Option<SystemTime>, backend_up: bool) -> Duration {
    let cap = if backend_up { RESCAN_LIVE } else { RESCAN };
    due.map_or(cap, |d| d.duration_since(SystemTime::now()).unwrap_or_default().min(cap))
}

async fn fire(features: &Arc<Features>) -> anyhow::Result<()> {
    let _lease = features.lease();
    let port = features.port().await?;
    let reply = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/api/cron/tick"))
        .header("X-Hermes-Session-Token", &features.token)
        .timeout(TICK_TIMEOUT)
        .send()
        .await?;
    anyhow::ensure!(reply.status().is_success(), "cron tick returned {}", reply.status());
    Ok(())
}

pub async fn run(features: Arc<Features>) {
    let mut last_fire: Option<std::time::Instant> = None;
    loop {
        let home = std::env::var_os("HERMES_HOME").map(PathBuf::from);
        let due = home.as_deref().and_then(next_due);
        let wait = wait_for(due, features.is_running().await);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = features.cron_changed.notified() => continue,
        }
        let overdue = due.is_some_and(|d| d <= SystemTime::now());
        let settled = last_fire.is_none_or(|t| t.elapsed() >= MIN_GAP);
        if overdue && settled {
            last_fire = Some(std::time::Instant::now());
            if let Err(err) = fire(&features).await {
                eprintln!("sovereign-gateway: cron tick failed: {err}");
            }
        } else if overdue {
            tokio::time::sleep(MIN_GAP).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with(jobs: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("cron-tick-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir_all(dir.join("cron")).unwrap();
        std::fs::write(dir.join("cron/jobs.json"), jobs).unwrap();
        dir
    }

    fn at(iso: &str) -> SystemTime {
        SystemTime::from(DateTime::parse_from_rfc3339(iso).unwrap().with_timezone(&Utc))
    }

    #[test]
    fn next_due_is_the_earliest_runnable_job_and_the_timer_sleeps_until_it() {
        let dir = home_with(
            r#"{"jobs":[
              {"id":"later","state":"scheduled","next_run_at":"2099-01-02T00:00:00+00:00"},
              {"id":"soon","enabled":true,"state":"scheduled","next_run_at":"2099-01-01T05:00:00+05:00"},
              {"id":"paused","state":"paused","next_run_at":"2098-01-01T00:00:00+00:00"},
              {"id":"off","enabled":false,"next_run_at":"2097-01-01T00:00:00+00:00"},
              {"id":"done","state":"completed","next_run_at":"2097-01-01T00:00:00+00:00"}]}"#,
        );
        assert_eq!(next_due(&dir), Some(at("2099-01-01T00:00:00+00:00")));
        // Far away: the rescan cap wins; overdue: fire now.
        assert_eq!(wait_for(next_due(&dir), false), RESCAN);
        assert_eq!(wait_for(next_due(&dir), true), RESCAN_LIVE);
        assert_eq!(wait_for(Some(at("2000-01-01T00:00:00+00:00")), false), Duration::ZERO);
        assert_eq!(wait_for(None, false), RESCAN);
        std::fs::write(dir.join("cron/jobs.json"), r#"[{"id":"x","state":"error","next_run_at":"2099-01-01T00:00:00+00:00"}]"#).unwrap();
        assert_eq!(next_due(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
