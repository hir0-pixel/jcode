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
/// First retry delay for a job a tick could not advance; it doubles per failure up to `MAX_GAP`.
const MIN_GAP: Duration = Duration::from_secs(30);
const MAX_GAP: Duration = Duration::from_secs(3600);
/// Longest single sleep: tokio's clock stops during macOS system sleep, so re-read the wall clock.
const CHUNK: Duration = Duration::from_secs(60);
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

struct Job {
    id: String,
    due: SystemTime,
}

/// Every fireable job of the home and its profiles with its `next_run_at`.
fn fireable(home: &Path) -> Vec<Job> {
    job_stores(home)
        .iter()
        .filter_map(|store| serde_json::from_str::<Value>(&std::fs::read_to_string(store).ok()?).ok())
        .flat_map(|value| match value {
            Value::Array(list) => list,
            Value::Object(map) => map.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default(),
            _ => Vec::new(),
        })
        .filter(job_is_fireable)
        .filter_map(|job| {
            let due = DateTime::parse_from_rfc3339(job["next_run_at"].as_str()?).ok()?;
            Some(Job { id: job["id"].as_str().unwrap_or_default().to_string(), due: SystemTime::from(due.with_timezone(&Utc)) })
        })
        .collect()
}

/// Per job: a run that left `next_run_at` unchanged waits 30 s, 60 s, 2 min ... (max 1 h) before the
/// next attempt, so one job a tick cannot advance does not wake Python every 30 s.
#[derive(Default)]
struct Backoff(std::collections::HashMap<String, (SystemTime, u32, SystemTime)>); // id -> (stuck due, failures, retry at)

impl Backoff {
    fn effective(&self, job: &Job) -> SystemTime {
        match self.0.get(&job.id) {
            Some((due, _, retry)) if *due == job.due => job.due.max(*retry),
            _ => job.due,
        }
    }

    /// After a tick that ran the jobs in `before` that were due at `now`.
    fn record(&mut self, before: &[Job], after: &[Job], now: SystemTime) {
        for job in before.iter().filter(|j| j.due <= now) {
            if after.iter().any(|a| a.id == job.id && a.due == job.due) {
                let fails = self.0.get(&job.id).filter(|e| e.0 == job.due).map_or(0, |e| e.1) + 1;
                let gap = MIN_GAP.saturating_mul(1 << (fails - 1).min(7)).min(MAX_GAP);
                self.0.insert(job.id.clone(), (job.due, fails, now + gap));
            }
        }
        self.0.retain(|id, e| after.iter().any(|a| a.id == *id && a.due == e.0));
    }
}

/// Earliest time any job should run, backoff included.
fn next_due(jobs: &[Job], backoff: &Backoff) -> Option<SystemTime> {
    jobs.iter().map(|j| backoff.effective(j)).min()
}

/// How long to sleep given the next due time.
fn wait_for(due: Option<SystemTime>, backend_up: bool) -> Duration {
    let cap = if backend_up { RESCAN_LIVE } else { RESCAN };
    due.map_or(cap, |d| d.duration_since(SystemTime::now()).unwrap_or_default().min(cap))
}

/// The next bounded sleep toward `deadline`, or None once the wall clock has reached it.
fn next_chunk(deadline: SystemTime, now: SystemTime) -> Option<Duration> {
    deadline.duration_since(now).ok().filter(|d| !d.is_zero()).map(|d| d.min(CHUNK))
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
    let mut backoff = Backoff::default();
    loop {
        let home = std::env::var_os("HERMES_HOME").map(PathBuf::from);
        let jobs = home.as_deref().map(fireable).unwrap_or_default();
        let due = next_due(&jobs, &backoff);
        let deadline = SystemTime::now() + wait_for(due, features.is_running().await);
        let mut changed = false;
        while let Some(chunk) = next_chunk(deadline, SystemTime::now()) {
            tokio::select! {
                _ = tokio::time::sleep(chunk) => {}
                _ = features.cron_changed.notified() => { changed = true; break }
            }
        }
        if changed || !due.is_some_and(|d| d <= SystemTime::now()) {
            continue;
        }
        if let Err(err) = fire(&features).await {
            eprintln!("sovereign-gateway: cron tick failed: {err}");
        }
        let after = home.as_deref().map(fireable).unwrap_or_default();
        backoff.record(&jobs, &after, SystemTime::now());
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
        let due = |dir: &Path| next_due(&fireable(dir), &Backoff::default());
        assert_eq!(due(&dir), Some(at("2099-01-01T00:00:00+00:00")));
        // Far away: the rescan cap wins; overdue: fire now.
        assert_eq!(wait_for(due(&dir), false), RESCAN);
        assert_eq!(wait_for(due(&dir), true), RESCAN_LIVE);
        assert_eq!(wait_for(Some(at("2000-01-01T00:00:00+00:00")), false), Duration::ZERO);
        assert_eq!(wait_for(None, false), RESCAN);
        std::fs::write(dir.join("cron/jobs.json"), r#"[{"id":"x","state":"error","next_run_at":"2099-01-01T00:00:00+00:00"}]"#).unwrap();
        assert_eq!(due(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sleeps_in_bounded_chunks_and_backs_off_per_stuck_job() {
        // A long wait is cut into <= 60 s naps; a passed deadline (wall clock jumped in system sleep) ends it.
        let now = at("2099-01-01T00:00:00+00:00");
        assert_eq!(next_chunk(now + Duration::from_secs(600), now), Some(CHUNK));
        assert_eq!(next_chunk(now + Duration::from_secs(5), now), Some(Duration::from_secs(5)));
        assert_eq!(next_chunk(now, now), None);
        assert_eq!(next_chunk(now - Duration::from_secs(1), now), None);

        let job = |id: &str, due: SystemTime| Job { id: id.into(), due };
        let (stuck, fine) = (now - Duration::from_secs(100), now - Duration::from_secs(50));
        let mut backoff = Backoff::default();
        let mut waits = Vec::new();
        for _ in 0..3 {
            let before = [job("stuck", stuck), job("fine", fine)];
            // "fine" advanced, "stuck" did not.
            backoff.record(&before, &[job("stuck", stuck), job("fine", now + Duration::from_secs(3600))], now);
            waits.push(backoff.effective(&job("stuck", stuck)).duration_since(now).unwrap());
        }
        assert_eq!(waits, [MIN_GAP, MIN_GAP * 2, MIN_GAP * 4]);
        assert_eq!(backoff.effective(&job("fine", fine)), fine, "a healthy job is not delayed");
        // Once the job advances its backoff is forgotten.
        backoff.record(&[job("stuck", stuck)], &[job("stuck", now + Duration::from_secs(60))], now);
        assert!(backoff.0.is_empty());
    }
}
