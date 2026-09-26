//! Monitoring, budgets, alert handling, approval audit, and run helpers.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::mpsc::SyncSender;

pub const FLAG_NO_MODEL_CALL: i64 = 1;

pub fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p / 100.0).round() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

pub fn window_ms(window: &str) -> Option<i64> {
    match window {
        "1h" => Some(3_600_000),
        "24h" => Some(86_400_000),
        "7d" => Some(7 * 86_400_000),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct ObsConfig {
    pub budget_daily_usd: Option<f64>,
    pub budget_by_kind: std::collections::HashMap<String, f64>,
    pub alert_p95_run_ms: i64,
    pub alert_error_rate_pct: f64,
    pub alert_webhook_url: String,
}

impl ObsConfig {
    pub fn load(home: &Path) -> Self {
        let file = std::fs::read(home.join("observability.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(json!({}));
        let budget_daily_usd = std::env::var("SOVEREIGN_BUDGET_DAILY_USD")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| file["budget_daily_usd"].as_f64());
        let mut budget_by_kind = std::collections::HashMap::new();
        if let Some(map) = file["budget_by_kind"].as_object() {
            for (k, v) in map {
                if let Some(n) = v.as_f64() {
                    budget_by_kind.insert(k.clone(), n);
                }
            }
        }
        Self {
            budget_daily_usd,
            budget_by_kind,
            alert_p95_run_ms: file["alert_p95_run_ms"].as_i64().unwrap_or(120_000),
            alert_error_rate_pct: file["alert_error_rate_pct"].as_f64().unwrap_or(25.0),
            alert_webhook_url: std::env::var("SOVEREIGN_ALERT_WEBHOOK_URL")
                .unwrap_or_else(|_| file["alert_webhook_url"].as_str().unwrap_or("").into()),
        }
    }
}

pub fn budget_status(db: &Connection, home: &Path, at: i64) -> rusqlite::Result<Value> {
    let cfg = ObsConfig::load(home);
    let day_start = at - (at % 86_400_000);
    let spend: f64 = db.query_row(
        "SELECT COALESCE(SUM(cost_usd),0) FROM obs_runs WHERE started_at_ms>=?1 AND cost_usd IS NOT NULL",
        [day_start],
        |r| r.get(0),
    )?;
    let cap = cfg.budget_daily_usd;
    let over = cap.is_some_and(|c| spend > c);
    Ok(json!({
        "day_start_ms": day_start,
        "spend_usd": spend,
        "budget_daily_usd": cap,
        "over_budget": over,
        "by_kind": kind_spend(db, day_start)?,
    }))
}

fn kind_spend(db: &Connection, since: i64) -> rusqlite::Result<Value> {
    let mut stmt = db.prepare(
        "SELECT kind, COALESCE(SUM(cost_usd),0) FROM obs_runs WHERE started_at_ms>=?1 AND cost_usd IS NOT NULL GROUP BY kind",
    )?;
    let rows = stmt
        .query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!(rows.into_iter().map(|(kind, usd)| json!({"kind": kind, "spend_usd": usd})).collect::<Vec<_>>()))
}

pub fn list_filtered(
    db: &Connection,
    limit: u64,
    status: Option<&str>,
    kind: Option<&str>,
    q: Option<&str>,
) -> rusqlite::Result<Vec<Value>> {
    let mut sql = String::from(
        "SELECT id,session_id,parent_id,root_id,kind,model,provider,status,started_at_ms,ended_at_ms,\
         input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd,error,title,unpriced_calls,flags,replay_of \
         FROM obs_runs WHERE 1=1",
    );
    let mut binds: Vec<String> = Vec::new();
    if let Some(s) = status.filter(|s| !s.is_empty()) {
        sql.push_str(" AND status=?");
        binds.push(s.into());
    }
    if let Some(k) = kind.filter(|k| !k.is_empty()) {
        sql.push_str(" AND kind=?");
        binds.push(k.into());
    }
    if let Some(query) = q.filter(|q| !q.is_empty()) {
        sql.push_str(" AND (title LIKE ? ESCAPE '\\' OR model LIKE ? ESCAPE '\\')");
        let pat = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        binds.push(pat.clone());
        binds.push(pat);
    }
    sql.push_str(" ORDER BY started_at_ms DESC LIMIT ?");
    let lim = limit.min(200);
    let mut stmt = db.prepare(&sql)?;
    let rows = match binds.len() {
        0 => stmt.query_map([lim], run_row)?.collect(),
        1 => stmt.query_map(params![binds[0], lim], run_row)?.collect(),
        2 => stmt.query_map(params![binds[0], binds[1], lim], run_row)?.collect(),
        3 => stmt.query_map(params![binds[0], binds[1], binds[2], lim], run_row)?.collect(),
        4 => stmt.query_map(params![binds[0], binds[1], binds[2], binds[3], lim], run_row)?.collect(),
        _ => unreachable!(),
    };
    rows
}

pub fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let flags: i64 = r.get(18)?;
    Ok(json!({
        "id": r.get::<_, String>(0)?,
        "session_id": r.get::<_, String>(1)?,
        "parent_id": r.get::<_, Option<String>>(2)?,
        "root_id": r.get::<_, String>(3)?,
        "kind": r.get::<_, String>(4)?,
        "model": r.get::<_, String>(5)?,
        "provider": r.get::<_, String>(6)?,
        "status": r.get::<_, String>(7)?,
        "started_at_ms": r.get::<_, i64>(8)?,
        "ended_at_ms": r.get::<_, Option<i64>>(9)?,
        "input_tokens": r.get::<_, i64>(10)?,
        "output_tokens": r.get::<_, i64>(11)?,
        "cache_read_tokens": r.get::<_, i64>(12)?,
        "cache_write_tokens": r.get::<_, i64>(13)?,
        "cost_usd": r.get::<_, Option<f64>>(14)?,
        "error": r.get::<_, Option<String>>(15)?,
        "title": r.get::<_, Option<String>>(16)?,
        "unpriced_calls": r.get::<_, i64>(17)?,
        "flags": flags,
        "no_model_call": flags & FLAG_NO_MODEL_CALL != 0,
        "replay_of": r.get::<_, Option<String>>(19)?,
    }))
}

pub fn monitors(db: &Connection, since: i64, dropped: u64) -> rusqlite::Result<Value> {
    // EXPLAIN QUERY PLAN (tests): obs_runs_recent index on started_at_ms for window filter.
    let mut run_lat = db.prepare(
        "SELECT kind, (ended_at_ms-started_at_ms) FROM obs_runs WHERE started_at_ms>=?1 AND ended_at_ms IS NOT NULL",
    )?;
    let mut by_kind: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
    for row in run_lat.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (kind, ms) = row?;
        by_kind.entry(kind).or_default().push(ms);
    }
    let run_stats: Vec<Value> = by_kind
        .into_iter()
        .map(|(kind, mut v)| {
            v.sort_unstable();
            json!({
                "kind": kind,
                "count": v.len(),
                "p50_ms": percentile(&v, 50.0),
                "p95_ms": percentile(&v, 95.0),
                "p99_ms": percentile(&v, 99.0),
            })
        })
        .collect();

    let mut model_lat = db.prepare(
        "SELECT s.kind, (s.ended_at_ms-s.started_at_ms), s.output_tokens FROM obs_spans s
         JOIN obs_runs r ON r.id=s.run_id WHERE r.started_at_ms>=?1 AND s.model IS NOT NULL AND s.ended_at_ms IS NOT NULL",
    )?;
    let mut model_by_kind: std::collections::HashMap<String, (Vec<i64>, Vec<f64>)> = std::collections::HashMap::new();
    for row in model_lat.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (kind, ms, out) = row?;
        let e = model_by_kind.entry(kind).or_insert((Vec::new(), Vec::new()));
        e.0.push(ms);
        if ms > 0 && out > 0 {
            e.1.push(out as f64 * 1000.0 / ms as f64);
        }
    }
    let model_stats: Vec<Value> = model_by_kind
        .into_iter()
        .map(|(kind, (mut lat, mut tps))| {
            lat.sort_unstable();
            tps.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            json!({
                "kind": kind,
                "count": lat.len(),
                "p50_ms": percentile(&lat, 50.0),
                "p95_ms": percentile(&lat, 95.0),
                "p99_ms": percentile(&lat, 99.0),
                "output_tokens_per_s_p50": percentile_f(&tps, 50.0),
            })
        })
        .collect();

    let (total, errors, silent): (i64, i64, i64) = db.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN status IN ('error','failed') THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN (flags & ?2) != 0 THEN 1 ELSE 0 END), 0)
         FROM obs_runs WHERE started_at_ms>=?1",
        params![since, FLAG_NO_MODEL_CALL],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let error_rate = if total > 0 {
        errors as f64 * 100.0 / total as f64
    } else {
        0.0
    };

    Ok(json!({
        "since_ms": since,
        "run_latency": run_stats,
        "model_call_latency": model_stats,
        "error_rate_pct": error_rate,
        "silent_failures": silent,
        "dropped_events": dropped,
    }))
}

fn percentile_f(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p / 100.0).round() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

pub fn list_approvals(db: &Connection, limit: u64) -> rusqlite::Result<Value> {
    let mut stmt = db.prepare(
        "SELECT id,run_id,session_id,tool,command_preview,decision,actor,at_ms FROM obs_approvals ORDER BY at_ms DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit.min(500)], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "run_id": r.get::<_, Option<String>>(1)?,
                "session_id": r.get::<_, String>(2)?,
                "tool": r.get::<_, String>(3)?,
                "command_preview": r.get::<_, String>(4)?,
                "decision": r.get::<_, String>(5)?,
                "actor": r.get::<_, String>(6)?,
                "at_ms": r.get::<_, i64>(7)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "approvals": rows }))
}

pub fn promote_run(db: &Connection, home: &Path, run_id: &str, at: i64) -> rusqlite::Result<Value> {
    let detail = super::detail_from_db(db, run_id)?;
    let run = &detail["run"];
    let spans = detail["spans"].as_array().cloned().unwrap_or_default();
    let content = detail.get("content").cloned().unwrap_or(Value::Null);
    let tools: Vec<Value> = spans
        .iter()
        .filter(|s| s["kind"] == "execute_tool")
        .map(|s| {
            json!({
                "name": s["name"],
                "input": s["input"],
                "output": s["output"],
                "status": s["status"],
            })
        })
        .collect();
    let case = json!({
        "version": 1,
        "source_run_id": run_id,
        "session_id": run["session_id"],
        "kind": run["kind"],
        "prompt": content["input"],
        "final_answer": content["output"],
        "tool_calls": tools,
        "created_at_ms": at,
    });
    let evals = home.join("evals");
    std::fs::create_dir_all(&evals).map_err(|_| rusqlite::Error::InvalidPath(evals.clone()))?;
    let slug = run_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();
    let path = evals.join(format!("{slug}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&case).unwrap_or_default())
        .map_err(|_| rusqlite::Error::InvalidPath(path.clone()))?;
    Ok(json!({ "ok": true, "path": path.to_string_lossy(), "id": slug }))
}

pub fn turn_index(db: &Connection, session: &str, started_at_ms: i64) -> rusqlite::Result<i64> {
    db.query_row(
        "SELECT COUNT(*) FROM obs_runs WHERE session_id=?1 AND parent_id IS NULL AND started_at_ms<?2",
        params![session, started_at_ms],
        |r| r.get(0),
    )
}

pub fn apply_run_end_flags(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    status: &str,
    error: &Option<String>,
    model_calls: u64,
) -> rusqlite::Result<()> {
    if status == "complete" && error.is_none() && model_calls == 0 {
        tx.execute(
            "UPDATE obs_runs SET flags = flags | ?2 WHERE id=?1",
            params![id, FLAG_NO_MODEL_CALL],
        )?;
    }
    Ok(())
}

pub fn write_approval(
    tx: &rusqlite::Transaction<'_>,
    run_id: Option<&str>,
    session: &str,
    tool: &str,
    command: &str,
    decision: &str,
    actor: &str,
    at: i64,
) -> rusqlite::Result<()> {
    let preview: String = command.chars().take(500).collect();
    tx.execute(
        "INSERT INTO obs_approvals(run_id,session_id,tool,command_preview,decision,actor,at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![run_id, session, tool, preview, decision, actor, at],
    )?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AlertState {
    Ok,
    Firing,
    NotChecked,
}

impl AlertState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Firing => "firing",
            Self::NotChecked => "not_checked",
        }
    }
}

pub fn evaluate_alerts(
    db: &mut Connection,
    home: &Path,
    at: i64,
    dropped: u64,
    alert_tx: Option<&SyncSender<Value>>,
) -> rusqlite::Result<()> {
    let cfg = ObsConfig::load(home);
    let since = at - 86_400_000;
    let budget = budget_status(db, home, at)?;
    let mon = monitors(db, since, dropped)?;

    let checks: Vec<(&str, &str, AlertState, String)> = vec![
        (
            "error_rate",
            "warn",
            if mon["error_rate_pct"].as_f64().unwrap_or(0.0) >= cfg.alert_error_rate_pct {
                AlertState::Firing
            } else {
                AlertState::Ok
            },
            format!(
                "Error rate {:.1}% (threshold {:.0}%)",
                mon["error_rate_pct"].as_f64().unwrap_or(0.0),
                cfg.alert_error_rate_pct
            ),
        ),
        (
            "silent_failures",
            "warn",
            if mon["silent_failures"].as_i64().unwrap_or(0) > 0 {
                AlertState::Firing
            } else {
                AlertState::Ok
            },
            format!("{} silent failures (complete with no model calls)", mon["silent_failures"].as_i64().unwrap_or(0)),
        ),
        (
            "budget",
            "warn",
            if budget["over_budget"].as_bool().unwrap_or(false) {
                AlertState::Firing
            } else if budget["budget_daily_usd"].is_null() {
                AlertState::NotChecked
            } else {
                AlertState::Ok
            },
            match budget["budget_daily_usd"].as_f64() {
                Some(cap) => format!(
                    "Daily spend ${:.4} / ${:.2}",
                    budget["spend_usd"].as_f64().unwrap_or(0.0),
                    cap
                ),
                None => "No daily budget configured".into(),
            },
        ),
        (
            "run_p95_latency",
            "warn",
            {
                let bad = mon["run_latency"]
                    .as_array()
                    .map(|rows| {
                        rows.iter().any(|r| {
                            r["p95_ms"].as_i64().unwrap_or(0) > cfg.alert_p95_run_ms
                        })
                    })
                    .unwrap_or(false);
                if bad {
                    AlertState::Firing
                } else {
                    AlertState::Ok
                }
            },
            format!("Run p95 latency over {} ms", cfg.alert_p95_run_ms),
        ),
        (
            "dropped_events",
            "page",
            if dropped > 0 {
                AlertState::Firing
            } else {
                AlertState::Ok
            },
            format!("{dropped} tier-1 observability events dropped"),
        ),
    ];

    for (key, severity, state, message) in checks {
        let prev: Option<String> = db
            .query_row(
                "SELECT state FROM obs_alerts WHERE monitor_key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        let prev_state = prev.as_deref().map(parse_state).unwrap_or(AlertState::NotChecked);
        let new_state = state;
        if prev_state == new_state && prev.is_some() {
            continue;
        }
        let should_notify = transition_notify(prev_state, new_state);
        db.execute(
            "INSERT INTO obs_alerts(monitor_key,state,severity,message,updated_at_ms,last_notified_ms)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(monitor_key) DO UPDATE SET state=excluded.state, message=excluded.message,
             updated_at_ms=excluded.updated_at_ms, last_notified_ms=COALESCE(excluded.last_notified_ms, obs_alerts.last_notified_ms)",
            params![
                key,
                new_state.as_str(),
                severity,
                message,
                at,
                if should_notify { Some(at) } else { None::<i64> }
            ],
        )?;
        if should_notify {
            let payload = json!({
                "monitor": key,
                "state": new_state.as_str(),
                "severity": severity,
                "message": message,
                "at_ms": at,
            });
            if let Some(tx) = alert_tx {
                let _ = tx.try_send(payload.clone());
            }
            if !cfg.alert_webhook_url.is_empty() {
                fire_webhook(&cfg.alert_webhook_url, &payload);
            }
        }
    }
    Ok(())
}

fn parse_state(s: &str) -> AlertState {
    match s {
        "firing" => AlertState::Firing,
        "not_checked" => AlertState::NotChecked,
        _ => AlertState::Ok,
    }
}

fn transition_notify(from: AlertState, to: AlertState) -> bool {
    match (from, to) {
        (AlertState::NotChecked, AlertState::Ok) => false,
        (AlertState::NotChecked, AlertState::NotChecked) => false,
        (AlertState::Firing, AlertState::NotChecked) => true,
        (_, AlertState::Firing) => true,
        (AlertState::Firing, AlertState::Ok) => true,
        _ => false,
    }
}

fn fire_webhook(url: &str, payload: &Value) {
    let url = url.to_string();
    let payload = payload.clone();
    std::thread::spawn(move || {
        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
        else {
            return;
        };
        let _ = client.post(url).json(&payload).send();
    });
}

pub fn list_alerts(db: &Connection) -> rusqlite::Result<Value> {
    let mut stmt = db.prepare(
        "SELECT monitor_key,state,severity,message,updated_at_ms FROM obs_alerts ORDER BY monitor_key",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "monitor": r.get::<_, String>(0)?,
                "state": r.get::<_, String>(1)?,
                "severity": r.get::<_, String>(2)?,
                "message": r.get::<_, Option<String>>(3)?,
                "updated_at_ms": r.get::<_, i64>(4)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "monitors": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem_db() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        jcode_base::migrate_sovereign_db(&mut db).unwrap();
        db
    }

    #[test]
    fn list_filtered_uses_started_at_index() {
        let db = mem_db();
        db.execute(
            "INSERT INTO obs_runs(id,session_id,root_id,kind,model,provider,status,started_at_ms,title)
             VALUES('a','s','a','invoke_agent','m','p','complete',1,'hello')",
            [],
        )
        .unwrap();
        let plan: String = db
            .prepare("EXPLAIN QUERY PLAN SELECT id FROM obs_runs WHERE status='complete' ORDER BY started_at_ms DESC LIMIT 10")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(plan.contains("obs_runs") || plan.contains("INDEX"), "{plan}");
        let rows = list_filtered(&db, 10, Some("complete"), None, Some("hel")).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn alert_dedup_skips_ok_to_ok() {
        assert!(!transition_notify(AlertState::Ok, AlertState::Ok));
        assert!(transition_notify(AlertState::Ok, AlertState::Firing));
        assert!(transition_notify(AlertState::Firing, AlertState::Ok));
        assert!(!transition_notify(AlertState::NotChecked, AlertState::Ok));
    }

    #[test]
    fn monitors_and_budget_queries() {
        let db = mem_db();
        let at = 86_400_000_i64 * 10;
        let day_start = at - (at % 86_400_000);
        db.execute(
            "INSERT INTO obs_runs(id,session_id,root_id,kind,model,provider,status,started_at_ms,ended_at_ms,cost_usd,flags)
             VALUES('r','s','r','invoke_agent','m','p','complete',?1,?2,0.5,0)",
            params![day_start + 1000, day_start + 2000],
        )
        .unwrap();
        let mon = monitors(&db, at - 86_400_000, 0).unwrap();
        assert_eq!(mon["silent_failures"], 0);
        let home = std::env::temp_dir().join(format!("sovereign-budget-test-{}", at));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("observability.json"), r#"{"budget_daily_usd":0.01}"#).unwrap();
        let b = budget_status(&db, &home, at).unwrap();
        assert!(b["over_budget"].as_bool().unwrap());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn no_model_call_flag_on_end() {
        let mut db = mem_db();
        let tx = db.transaction().unwrap();
        tx.execute(
            "INSERT INTO obs_runs(id,session_id,root_id,kind,model,provider,status,started_at_ms) VALUES('x','s','x','invoke_agent','m','p','running',1)",
            [],
        )
        .unwrap();
        apply_run_end_flags(&tx, "x", "complete", &None, 0).unwrap();
        tx.commit().unwrap();
        let flags: i64 = db.query_row("SELECT flags FROM obs_runs WHERE id='x'", [], |r| r.get(0)).unwrap();
        assert_eq!(flags & FLAG_NO_MODEL_CALL, FLAG_NO_MODEL_CALL);
    }
}
