//! Wake Hermes's on-demand Python backend before a cron job is due.
//!
//! On stock Hermes Desktop the cron scheduler ticks inside the `hermes serve`
//! process (`HERMES_DESKTOP=1`). Sovereign idle-stops that process, so without
//! this module enabled jobs would silently never fire. We read `jobs.json`
//! ourselves, start Python shortly before the next due time, hold it until the
//! job's `next_run_at` advances (or a cap elapses), then let idle-stop reclaim it.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::features::Features;

/// How early to start Python before `next_run_at`.
pub fn wake_lead() -> Duration {
    std::env::var("SOVEREIGN_CRON_WAKE_LEAD_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(45))
}

/// Cap how long we keep Python up after a due time while waiting for the job to finish.
pub fn hold_cap() -> Duration {
    std::env::var("SOVEREIGN_CRON_HOLD_CAP_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(30 * 60))
}

fn hermes_home() -> Option<PathBuf> {
    std::env::var_os("HERMES_HOME").map(PathBuf::from)
}

fn job_stores(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![home.join("cron").join("jobs.json")];
    let profiles = home.join("profiles");
    if let Ok(entries) = std::fs::read_dir(profiles) {
        for entry in entries.flatten() {
            let path = entry.path().join("cron").join("jobs.json");
            if path.is_file() {
                out.push(path);
            }
        }
    }
    out
}

fn parse_due(raw: &str) -> Option<SystemTime> {
    let dt = DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| raw.parse::<DateTime<Utc>>().ok())?;
    let secs = dt.timestamp();
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64) + Duration::from_nanos(dt.timestamp_subsec_nanos() as u64))
}

fn job_is_fireable(job: &Value) -> bool {
    // Mirrors cron.jobs effective "runnable": enabled (default true), not paused, has next_run_at.
    if job.get("enabled") == Some(&Value::Bool(false)) {
        return false;
    }
    let state = job.get("state").and_then(|s| s.as_str()).unwrap_or("");
    if matches!(state, "paused" | "completed" | "error") {
        return false;
    }
    if job.get("paused_at").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).is_some() {
        return false;
    }
    job.get("next_run_at").and_then(|v| v.as_str()).is_some()
}

/// Earliest `next_run_at` among fireable jobs, if any.
pub fn earliest_due(home: &Path) -> Option<(SystemTime, String, String)> {
    let mut best: Option<(SystemTime, String, String)> = None;
    for store in job_stores(home) {
        let Ok(text) = std::fs::read_to_string(&store) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        let jobs = match value {
            Value::Array(list) => list,
            Value::Object(map) => map.get("jobs").and_then(|j| j.as_array()).cloned().unwrap_or_default(),
            _ => continue,
        };
        for job in jobs {
            if !job_is_fireable(&job) {
                continue;
            }
            let Some(raw) = job.get("next_run_at").and_then(|v| v.as_str()) else { continue };
            let Some(due) = parse_due(raw) else { continue };
            let id = job.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            match &best {
                Some((best_due, _, _)) if *best_due <= due => {}
                _ => best = Some((due, id, raw.to_string())),
            }
        }
    }
    best
}

/// Whether idle-stop must stand down because a job is due within the wake lead (or overdue).
pub fn due_within_wake_lead(home: &Path) -> bool {
    let Some((due, _, _)) = earliest_due(home) else { return false };
    match due.duration_since(SystemTime::now()) {
        Ok(until) => until <= wake_lead(),
        Err(_) => true, // overdue
    }
}

pub async fn run(features: Arc<Features>) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    // Watched due stamp: hold until jobs.json advances past this next_run_at (or hold_cap).
    let mut watching: Option<(String /*job id*/, String /*next_run_at*/, SystemTime /*due*/, SystemTime /*hold_deadline*/)> = None;
    loop {
        tick.tick().await;
        let Some(home) = hermes_home() else {
            features.clear_cron_hold();
            watching = None;
            continue;
        };

        if let Some((id, stamped, due, deadline)) = watching.clone() {
            let still = earliest_due(&home).filter(|(d, jid, raw)| jid == &id && raw == &stamped && *d == due);
            if still.is_none() || SystemTime::now() >= deadline {
                watching = None;
                features.clear_cron_hold();
            } else {
                features.touch();
                features.set_cron_hold_until(deadline);
                if let Err(err) = features.port().await {
                    eprintln!("sovereign-gateway: cron hold wake failed: {err}");
                }
                continue;
            }
        }

        let Some((due, id, raw)) = earliest_due(&home) else {
            features.clear_cron_hold();
            continue;
        };

        let now = SystemTime::now();
        let until = due.duration_since(now).unwrap_or(Duration::ZERO);
        if until > wake_lead() {
            features.clear_cron_hold();
            continue;
        }

        let deadline = due.checked_add(hold_cap()).unwrap_or(due);
        features.set_cron_hold_until(deadline);
        if let Err(err) = features.port().await {
            eprintln!("sovereign-gateway: cron wake failed: {err}");
            continue;
        }
        watching = Some((id, raw, due, deadline));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn picks_earliest_enabled_job() {
        let dir = std::env::temp_dir().join(format!("cron-wake-{}", std::process::id()));
        let cron = dir.join("cron");
        fs::create_dir_all(&cron).unwrap();
        fs::write(
            cron.join("jobs.json"),
            r#"[
              {"id":"later","enabled":true,"state":"scheduled","next_run_at":"2099-01-02T00:00:00+00:00"},
              {"id":"soon","enabled":true,"state":"scheduled","next_run_at":"2099-01-01T00:00:00+00:00"},
              {"id":"paused","enabled":true,"state":"paused","next_run_at":"2098-01-01T00:00:00+00:00"}
            ]"#,
        )
        .unwrap();
        let (due, id, _) = earliest_due(&dir).unwrap();
        assert_eq!(id, "soon");
        assert!(due < parse_due("2099-01-02T00:00:00+00:00").unwrap());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ignores_disabled_and_completed() {
        let dir = std::env::temp_dir().join(format!("cron-wake-empty-{}", std::process::id()));
        let cron = dir.join("cron");
        fs::create_dir_all(&cron).unwrap();
        fs::write(
            cron.join("jobs.json"),
            r#"[{"id":"x","enabled":false,"state":"scheduled","next_run_at":"2099-01-01T00:00:00+00:00"},
                {"id":"y","enabled":true,"state":"completed","next_run_at":"2099-01-01T00:00:00+00:00"}]"#,
        )
        .unwrap();
        assert!(earliest_due(&dir).is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
