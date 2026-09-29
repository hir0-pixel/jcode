//! Bot and cron turns run in hidden (archived) engine sessions. Each one is tagged with
//! the surface that made it so the sidebar can list bot and cron transcripts, and
//! one-shot cron sessions are deleted once they are older than the observability retention.

use super::Config;
use serde_json::{Value, json};
use std::collections::HashMap;

const PREFIX: &str = "session_surface:";

/// Newest cron sessions kept per job, on top of the age limit.
const KEEP_PER_JOB: usize = 20;

/// Mark `session_id` as made by `surface` ("cron" | "bot") at `at_ms`; `job` is the cron job's title.
pub(crate) fn tag(home: &str, session_id: &str, surface: &str, at_ms: i64, job: Option<&str>) {
    if let Some(store) = crate::rpc::entries_or_log(home) {
        let job = job.map(|j| format!("|{j}")).unwrap_or_default();
        let _ = store.set_setting(&format!("{PREFIX}{session_id}"), &format!("{surface}@{at_ms}{job}"));
    }
}

/// session id -> (surface, last tagged ms, job title).
fn tagged(home: &str) -> HashMap<String, (String, i64, Option<String>)> {
    let Some(store) = crate::rpc::entries_or_log(home) else {
        return HashMap::new();
    };
    store
        .settings_with_prefix(PREFIX)
        .into_iter()
        .filter_map(|(id, value)| {
            let (surface, rest) = value.split_once('@')?;
            let (at, job) = match rest.split_once('|') {
                Some((at, job)) => (at, Some(job.to_string())),
                None => (rest, None),
            };
            Some((id, (surface.to_string(), at.parse().ok()?, job)))
        })
        .collect()
}

/// session id -> (surface, last tagged ms).
pub(crate) fn tags(home: &str) -> HashMap<String, (String, i64)> {
    tagged(home).into_iter().map(|(id, (surface, at, _))| (id, (surface, at))).collect()
}

/// Cron sessions tagged before `now_ms - retention_days`, plus each job's runs beyond its newest
/// [`KEEP_PER_JOB`]. Bot sessions are reused per chat and never expire here.
fn expired_cron(tags: &HashMap<String, (String, i64, Option<String>)>, now_ms: i64, retention_days: i64) -> Vec<String> {
    let cutoff = now_ms - retention_days * 86_400_000;
    let mut out: Vec<String> = tags.iter().filter(|(_, (s, at, _))| s == "cron" && *at < cutoff).map(|(id, _)| id.clone()).collect();
    let mut by_job: HashMap<&str, Vec<(i64, &String)>> = HashMap::new();
    for (id, (s, at, job)) in tags {
        if let (true, Some(job)) = (s == "cron" && *at >= cutoff, job) {
            by_job.entry(job).or_default().push((*at, id));
        }
    }
    for mut runs in by_job.into_values() {
        runs.sort_by(|a, b| b.cmp(a));
        out.extend(runs.into_iter().skip(KEEP_PER_JOB).map(|(_, id)| id.clone()));
    }
    out
}

/// Delete expired one-shot cron sessions (and their tags).
pub(crate) async fn prune_cron(config: &Config, now_ms: i64) {
    let expired = expired_cron(&tagged(&config.home), now_ms, crate::observability::retention_days());
    let Some(store) = crate::rpc::entries_or_log(&config.home) else {
        return;
    };
    for id in expired {
        // A session already deleted by hand is as gone as one we delete.
        if crate::sessions_rest::delete_everywhere(config, &id).await.is_ok() {
            let _ = store.delete_setting(&format!("{PREFIX}{id}"));
        }
    }
}

/// Split the engine's session list into the sidebar's three groups: visible chats,
/// hidden cron transcripts, hidden bot transcripts (newest first, each capped).
pub(crate) fn sidebar(
    sessions: &[Value],
    tags: &HashMap<String, (String, i64)>,
    limits: (usize, usize, usize),
    map: impl Fn(&Value) -> Value,
) -> Value {
    let id = |s: &Value| s["session_id"].as_str().unwrap_or_default().to_string();
    let mut recents = Vec::new();
    let (mut cron, mut bots): (Vec<(i64, Value)>, Vec<(i64, Value)>) = (Vec::new(), Vec::new());
    for session in sessions {
        match tags.get(&id(session)) {
            Some((surface, at)) if surface == "cron" => cron.push((*at, session.clone())),
            Some((_, at)) => bots.push((*at, session.clone())),
            None if session["archived"] != true && recents.len() < limits.0 => recents.push(map(session)),
            None => {}
        }
    }
    let group = |mut rows: Vec<(i64, Value)>, cap: usize, source: &str| {
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        let rows: Vec<Value> = rows
            .into_iter()
            .take(cap)
            .map(|(_, s)| {
                let mut row = map(&s);
                row["source"] = json!(source);
                row
            })
            .collect();
        json!({ "sessions": rows })
    };
    json!({ "recents": { "sessions": recents }, "cron": group(cron, limits.1, "cron"), "messaging": group(bots, limits.2, "messaging") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_newest_runs_of_each_cron_job_are_kept() {
        let day = 86_400_000;
        let tags: HashMap<_, _> = (0..30)
            .map(|i| (format!("a{i}"), ("cron".to_string(), 90 * day + i, Some("every-5".to_string()))))
            .chain([("b0".to_string(), ("cron".to_string(), 90 * day, Some("nightly".to_string())))])
            .chain([("bot".to_string(), ("bot".to_string(), 90 * day, None))])
            .collect();
        let mut gone = expired_cron(&tags, 100 * day, 30);
        gone.sort();
        let mut want: Vec<String> = (0..10).map(|i| format!("a{i}")).collect();
        want.sort();
        assert_eq!(gone, want, "the 10 oldest runs of the busy job go; other jobs and bots are untouched");
    }

    #[test]
    fn bot_and_cron_transcripts_reach_the_sidebar_and_old_cron_ones_expire() {
        let day = 86_400_000;
        let home = std::env::temp_dir().join(format!("surface-{}", std::process::id()));
        let home_str = home.to_string_lossy().to_string();
        tag(&home_str, "chat", "bot", 100 * day, None);
        tag(&home_str, "old-cron", "cron", 10 * day, None);
        tag(&home_str, "new-cron", "cron", 99 * day, None);
        let all = tagged(&home_str);
        assert_eq!(expired_cron(&all, 100 * day, 30), vec!["old-cron".to_string()]);
        let tags = tags(&home_str);
        assert_eq!(tags["chat"], ("bot".to_string(), 100 * day));

        let list = vec![
            json!({"session_id": "desk"}),
            json!({"session_id": "chat", "archived": true}),
            json!({"session_id": "new-cron", "archived": true}),
            json!({"session_id": "orphan", "archived": true}),
        ];
        let body = sidebar(&list, &tags, (10, 10, 10), |s| json!({ "id": s["session_id"] }));
        assert_eq!(body["recents"]["sessions"], json!([{ "id": "desk" }]));
        assert_eq!(body["messaging"]["sessions"], json!([{ "id": "chat", "source": "messaging" }]));
        assert_eq!(body["cron"]["sessions"], json!([{ "id": "new-cron", "source": "cron" }]));
        let _ = std::fs::remove_dir_all(home);
    }
}
