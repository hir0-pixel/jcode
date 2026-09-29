//! Bot and cron turns run in hidden (archived) engine sessions. Each one is tagged with
//! the surface that made it so the sidebar can list bot and cron transcripts, and
//! one-shot cron sessions are deleted once they are older than the observability retention.

use super::Config;
use serde_json::{Value, json};
use std::collections::HashMap;

const PREFIX: &str = "session_surface:";

/// Mark `session_id` as made by `surface` ("cron" | "bot") at `at_ms`.
pub(crate) fn tag(home: &str, session_id: &str, surface: &str, at_ms: i64) {
    if let Some(store) = crate::rpc::entries_or_log(home) {
        let _ = store.set_setting(&format!("{PREFIX}{session_id}"), &format!("{surface}@{at_ms}"));
    }
}

/// session id -> (surface, last tagged ms).
pub(crate) fn tags(home: &str) -> HashMap<String, (String, i64)> {
    let Some(store) = crate::rpc::entries_or_log(home) else {
        return HashMap::new();
    };
    store
        .settings_with_prefix(PREFIX)
        .into_iter()
        .filter_map(|(id, value)| {
            let (surface, at) = value.split_once('@')?;
            Some((id, (surface.to_string(), at.parse().ok()?)))
        })
        .collect()
}

/// Cron sessions tagged before `now_ms - retention_days`. Bot sessions are reused per chat
/// and never expire here.
fn expired_cron(tags: &HashMap<String, (String, i64)>, now_ms: i64, retention_days: i64) -> Vec<String> {
    let cutoff = now_ms - retention_days * 86_400_000;
    tags.iter()
        .filter(|(_, (surface, at))| surface == "cron" && *at < cutoff)
        .map(|(id, _)| id.clone())
        .collect()
}

/// Delete expired one-shot cron sessions (and their tags).
pub(crate) async fn prune_cron(config: &Config, now_ms: i64) {
    let expired = expired_cron(&tags(&config.home), now_ms, crate::observability::retention_days());
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
    fn bot_and_cron_transcripts_reach_the_sidebar_and_old_cron_ones_expire() {
        let day = 86_400_000;
        let home = std::env::temp_dir().join(format!("surface-{}", std::process::id()));
        let home_str = home.to_string_lossy().to_string();
        tag(&home_str, "chat", "bot", 100 * day);
        tag(&home_str, "old-cron", "cron", 10 * day);
        tag(&home_str, "new-cron", "cron", 99 * day);
        let tags = tags(&home_str);
        assert_eq!(tags["chat"], ("bot".to_string(), 100 * day));
        assert_eq!(expired_cron(&tags, 100 * day, 30), vec!["old-cron".to_string()]);

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
