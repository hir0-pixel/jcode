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

/// Python truthiness of a JSON value (`bool(job.get(k))`); a missing key is falsy.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `datetime.fromisoformat` as Hermes reads it: an offset, or none (then the local zone).
fn parse_when(text: &str) -> Option<DateTime<Utc>> {
    use chrono::TimeZone as _;
    DateTime::parse_from_rfc3339(text).map(|d| d.with_timezone(&Utc)).ok().or_else(|| {
        let naive = chrono::NaiveDateTime::parse_from_str(&text.replace(' ', "T"), "%Y-%m-%dT%H:%M:%S%.f").ok()?;
        chrono::Local.from_local_datetime(&naive).single().map(|d| d.with_timezone(&Utc))
    })
}

/// When Hermes's due scan would next look at `job`, or None when it never fires it. The gate is
/// `cron.jobs._get_due_jobs_locked`, checked against Hermes itself by the contract test below:
/// finished (`completed`, or `error` on a one-shot) and disabled jobs are skipped, so is anything
/// with a pause marker; a recurring job in `error` is still live. A job with no usable `next_run_at`
/// is one Hermes recovers (recurring, or a one-shot inside its 120 s grace): it is due "at once",
/// stamped with the epoch so a job Hermes cannot advance backs off like any other stuck job.
fn job_due(job: &Value, now: DateTime<Utc>) -> Option<SystemTime> {
    let state = match job.get("state") {
        Some(Value::String(s)) => s.as_str(),
        _ => "",
    };
    let schedule = job.get("schedule").filter(|s| s.is_object());
    let kind = schedule.and_then(|s| s.get("kind")).and_then(Value::as_str);
    let recurring = matches!(kind, Some("cron" | "interval"));
    if matches!(state, "completed" | "error") && !(state == "error" && recurring) {
        return None;
    }
    if !job.get("enabled").is_none_or(|e| truthy(Some(e))) || state.trim() == "paused" || truthy(job.get("paused_at")) {
        return None;
    }
    if let Some(due) = job.get("next_run_at").and_then(Value::as_str).and_then(parse_when) {
        return Some(SystemTime::from(due));
    }
    let oneshot_in_grace = kind == Some("once")
        && !truthy(job.get("last_run_at"))
        && schedule.and_then(|s| s.get("run_at")).and_then(Value::as_str).and_then(parse_when).is_some_and(|at| (now - at).num_seconds() <= 120);
    (recurring || oneshot_in_grace).then_some(SystemTime::UNIX_EPOCH)
}

struct Job {
    id: String,
    due: SystemTime,
}

/// Every fireable job of the home and its profiles with its `next_run_at`.
fn fireable(home: &Path) -> Vec<Job> {
    let now = Utc::now();
    job_stores(home)
        .iter()
        .filter_map(|store| serde_json::from_str::<Value>(&std::fs::read_to_string(store).ok()?).ok())
        .flat_map(|value| match value {
            Value::Array(list) => list,
            Value::Object(map) => map.get("jobs").and_then(Value::as_array).cloned().unwrap_or_default(),
            _ => Vec::new(),
        })
        .filter_map(|job| Some(Job { id: job["id"].as_str().unwrap_or_default().to_string(), due: job_due(&job, now)? }))
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

    /// Contract: the jobs the timer treats as live are exactly the ones Hermes's own due scan gate
    /// lets through, run through Hermes's `cron.jobs` (its loader, per-store profile scoping,
    /// terminal/pause predicates and next_run recovery) over one fixture with every case.
    #[test]
    fn the_timer_agrees_with_hermes_on_which_jobs_are_eligible() {
        let hermes = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../hermes-agent");
        let python = hermes.join(".venv/bin/python");
        if !python.exists() {
            eprintln!("skipped: no Hermes venv at {}", python.display());
            return;
        }
        let iso = |secs: i64| (Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339();
        let (soon, fresh, stale) = (iso(3600), iso(-30), iso(-3600));
        let cron = serde_json::json!({ "kind": "cron", "expr": "0 9 * * *" });
        let every = serde_json::json!({ "kind": "interval", "minutes": 30 });
        let jobs = |list: Vec<Value>| serde_json::json!({ "jobs": list });
        let main = jobs(vec![
            serde_json::json!({ "id": "plain", "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "enabled-null", "enabled": null, "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "enabled-zero", "enabled": 0, "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "disabled", "enabled": false, "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "paused-state", "state": "paused", "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "paused-padded", "state": " paused ", "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "paused-at", "paused_at": "2026-01-01T00:00:00+00:00", "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "paused-at-empty", "paused_at": "", "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "completed", "state": "completed", "schedule": cron, "next_run_at": soon }),
            serde_json::json!({ "id": "error-recurring", "state": "error", "schedule": every, "next_run_at": soon }),
            serde_json::json!({ "id": "error-once", "state": "error", "schedule": { "kind": "once", "run_at": fresh }, "next_run_at": soon }),
            serde_json::json!({ "id": "missing-next-cron", "schedule": cron }),
            serde_json::json!({ "id": "missing-next-interval", "schedule": every, "next_run_at": null }),
            serde_json::json!({ "id": "garbage-next-interval", "schedule": every, "next_run_at": "soon" }),
            serde_json::json!({ "id": "naive-next", "schedule": every, "next_run_at": "2099-01-01T09:00:00" }),
            serde_json::json!({ "id": "once-fresh-missing-next", "schedule": { "kind": "once", "run_at": fresh } }),
            serde_json::json!({ "id": "once-stale-missing-next", "schedule": { "kind": "once", "run_at": stale } }),
            serde_json::json!({ "id": "once-ran-missing-next", "last_run_at": stale, "schedule": { "kind": "once", "run_at": fresh } }),
            serde_json::json!({ "id": "once-pending", "schedule": { "kind": "once", "run_at": soon }, "next_run_at": soon }),
            serde_json::json!({ "id": "no-schedule-missing-next" }),
        ]);
        let profile = jobs(vec![
            serde_json::json!({ "id": "work-plain", "schedule": every, "next_run_at": soon }),
            serde_json::json!({ "id": "work-disabled", "enabled": false, "schedule": every, "next_run_at": soon }),
            serde_json::json!({ "id": "work-completed", "state": "completed", "schedule": every, "next_run_at": soon }),
        ]);
        let dir = home_with(&main.to_string());
        let work = dir.join("profiles/work/cron");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("jobs.json"), profile.to_string()).unwrap();

        let script = r#"
import json, os
from pathlib import Path
from cron import jobs as J
home = Path(os.environ["HERMES_HOME"])
stores = [home] + sorted(p for p in (home / "profiles").iterdir() if p.is_dir())
eligible = []
for store in stores:
    with J.use_cron_store(store):
        raw = J.load_jobs()
        J._normalize_due_scan_records(raw)
        scan = J._DueScan(raw, J._hermes_now())
        for job in raw:
            if J.is_terminal_job(job) and not J._is_recoverable_error_job(job):
                continue
            if not job.get("enabled", True) or J._has_pause_marker(job):
                continue
            if job.get("next_run_at") or J._recover_missing_next_run(job, scan):
                eligible.append(job["id"])
print(json.dumps(sorted(eligible)))
"#;
        let out = std::process::Command::new(&python)
            .current_dir(&hermes)
            .env("HERMES_HOME", &dir)
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let theirs: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
        let mut ours: Vec<String> = fireable(&dir).into_iter().map(|j| j.id).collect();
        ours.sort();
        assert_eq!(ours, theirs);
        assert!(theirs.contains(&"work-plain".to_string()) && !theirs.contains(&"work-disabled".to_string()), "profiles are covered");
        assert!(theirs.contains(&"error-recurring".to_string()) && theirs.contains(&"once-fresh-missing-next".to_string()));
        let _ = std::fs::remove_dir_all(dir);
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
