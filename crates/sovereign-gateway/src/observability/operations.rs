//! Monitoring, budgets, alert handling, approval audit, and run helpers.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::mpsc::SyncSender;

pub const FLAG_NO_MODEL_CALL: i64 = 1;

/// How long a run may sit in `status='running'` before it is presumed dead —
/// evestack's `STUCK_TURN_MS` (lib/fleet.ts, lib/alerts.ts). Generous on
/// purpose: real tool work can legitimately run for minutes.
pub const STUCK_TURN_MS: i64 = 60 * 60 * 1000;

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
        Self {
            budget_daily_usd,
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
        "SELECT COALESCE(SUM(cost_usd),0) FROM fact_turn WHERE started_at_ms>=?1 AND cost_usd IS NOT NULL",
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
        "SELECT kind, COALESCE(SUM(cost_usd),0) FROM fact_turn WHERE started_at_ms>=?1 AND cost_usd IS NOT NULL GROUP BY kind",
    )?;
    let rows = stmt
        .query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!(rows.into_iter().map(|(kind, usd)| json!({"kind": kind, "spend_usd": usd})).collect::<Vec<_>>()))
}

/// The `run_row` projection. `step_count` (model calls) and `tools_called`
/// are evestack's fact_turn columns of the same name, counted from spans
/// through `spans_run` rather than stored twice.
pub const RUN_COLS: &str = "id,session_id,parent_id,root_id,kind,model,provider,status,started_at_ms,ended_at_ms,\
    input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd,error,title,unpriced_calls,flags,replay_of,outcome,span_coverage,ttft_ms,\
    (SELECT COUNT(*) FROM spans s WHERE s.run_id=fact_turn.id AND s.model IS NOT NULL),\
    (SELECT COUNT(*) FROM spans s WHERE s.run_id=fact_turn.id AND s.kind='execute_tool')";

pub fn list_filtered(
    db: &Connection,
    limit: u64,
    status: Option<&str>,
    kind: Option<&str>,
    q: Option<&str>,
    outcome: Option<&str>,
    session: Option<&str>,
    at: i64,
) -> rusqlite::Result<Vec<Value>> {
    let mut sql = format!("SELECT {RUN_COLS} FROM fact_turn WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();
    if let Some(s) = session.filter(|s| !s.is_empty()) {
        sql.push_str(" AND session_id=?");
        binds.push(s.into());
    }
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
    // `wedged` is not a stored outcome (see run_outcome's doc comment): a run
    // reads as wedged only while it is still `status='running'` and old, so
    // filtering for it is the same test wedged_count runs rather than an
    // equality on the column. evestack's `/sessions?outcome=wedged` link is
    // the same idea over its stored, periodically-refreshed column.
    match outcome.filter(|o| !o.is_empty()) {
        Some("wedged") => {
            sql.push_str(" AND status='running' AND started_at_ms<?");
            binds.push((at - STUCK_TURN_MS).to_string());
        }
        Some(o) => {
            sql.push_str(" AND outcome=?");
            binds.push(o.into());
        }
        None => {}
    }
    sql.push_str(" ORDER BY started_at_ms DESC LIMIT CAST(? AS INTEGER)");
    let lim = limit.min(200);
    let mut stmt = db.prepare(&sql)?;
    binds.push(lim.to_string());
    let rows = stmt
        .query_map(rusqlite::params_from_iter(binds.iter().map(|b| b.as_str())), run_row)?
        .collect::<rusqlite::Result<Vec<Value>>>();
    Ok(rows?.into_iter().map(|row| overlay_wedged(row, at)).collect())
}

/// `fact_turn.outcome` never stores `wedged` (see `run_outcome`'s doc comment):
/// it only applies to a run still `status='running'`, which changes without a
/// RunEnd write to react to. Overlaying it at read time is evestack's
/// periodically-`refresh_facts()`-ed `fact_turn.outcome` made exact instead of
/// eventually-consistent, since Sovereign has no separate refresh pass to wait
/// on. Applied to every row this module hands to a caller, list or detail.
pub fn overlay_wedged(mut row: Value, at: i64) -> Value {
    let wedged = row["status"].as_str() == Some("running")
        && row["started_at_ms"].as_i64().is_some_and(|started| started < at - STUCK_TURN_MS);
    if wedged {
        row["outcome"] = json!("wedged");
    }
    row
}

pub fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let flags: i64 = r.get(18)?;
    let outcome: String = r.get(20)?;
    let parent: Option<String> = r.get(2)?;
    let kind: String = r.get(4)?;
    let unpriced: i64 = r.get(17)?;
    let cost: Option<f64> = r.get(14)?;
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
        "outcome": outcome,
        "span_coverage": r.get::<_, String>(21)?,
        "ttft_ms": r.get::<_, Option<i64>>(22)?,
        "step_count": r.get::<_, i64>(23)?,
        "tools_called": r.get::<_, i64>(24)?,
        // evestack's run_type / trigger. Akira's only non-desktop trigger is
        // the scheduler, which runs with kind 'cron'.
        "run_type": if parent.is_some() { "subagent" } else { "turn" },
        "trigger": if kind == "cron" { "schedule" } else { "desktop" },
        // TRUE priced / FALSE unpriced / NULL no model call, like fact_turn.priced.
        "priced": if unpriced > 0 { json!(false) } else if cost.is_some() { json!(true) } else { Value::Null },
    }))
}

pub fn monitors(db: &Connection, since: i64, dropped: u64) -> rusqlite::Result<Value> {
    // EXPLAIN QUERY PLAN (tests): fact_turn_recent index on started_at_ms for window filter.
    let mut run_lat = db.prepare(
        "SELECT kind, (ended_at_ms-started_at_ms) FROM fact_turn WHERE started_at_ms>=?1 AND ended_at_ms IS NOT NULL",
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
        "SELECT s.kind, (s.ended_at_ms-s.started_at_ms), s.output_tokens FROM spans s
         JOIN fact_turn r ON r.id=s.run_id WHERE r.started_at_ms>=?1 AND s.model IS NOT NULL AND s.ended_at_ms IS NOT NULL",
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

    // evestack's turn_failure_rate divides by FINISHED turns, never `total` —
    // a window where every turn is still running must read `unknown`-shaped
    // (0%, called out below), not a confident 0% built from a denominator
    // that includes work nobody has judged yet.
    let (finished, errors, silent, unpriced_turns): (i64, i64, i64, i64) = db.query_row(
        "SELECT COALESCE(SUM(CASE WHEN ended_at_ms IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status IN ('error','failed') THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN (flags & ?2) != 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN unpriced_calls>0 THEN 1 ELSE 0 END), 0)
         FROM fact_turn WHERE started_at_ms>=?1",
        params![since, FLAG_NO_MODEL_CALL],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let error_rate = if finished > 0 {
        errors as f64 * 100.0 / finished as f64
    } else {
        0.0
    };
    let wedged = wedged_count(db, crate::observability::now())?;

    Ok(json!({
        "since_ms": since,
        "run_latency": run_stats,
        "model_call_latency": model_stats,
        "error_rate_pct": error_rate,
        "finished_runs": finished,
        "silent_failures": silent,
        "unpriced_turns": unpriced_turns,
        "wedged_count": wedged,
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

/// evestack.approvals: same column names; `decided_at` is `decided_at_ms`,
/// `turn_id` the run id, `option_id` the decision, `approver` the actor.
pub fn list_approvals(db: &Connection, limit: u64) -> rusqlite::Result<Value> {
    let mut stmt = db.prepare(
        "SELECT id,at_ms,session_id,run_id,request_kind,tool,decision,actor,approver_via,command_preview FROM approvals ORDER BY at_ms DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit.min(500)], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "decided_at_ms": r.get::<_, i64>(1)?,
                "session_id": r.get::<_, String>(2)?,
                "turn_id": r.get::<_, Option<String>>(3)?,
                "request_kind": r.get::<_, Option<String>>(4)?,
                "tool_name": r.get::<_, String>(5)?,
                "option_id": r.get::<_, String>(6)?,
                "approver": r.get::<_, String>(7)?,
                "approver_via": r.get::<_, String>(8)?,
                "command_preview": r.get::<_, String>(9)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "approvals": rows }))
}

/// evestack.memory_deletions, newest first.
pub fn list_memory_deletions(db: &Connection, limit: u64) -> rusqlite::Result<Value> {
    let mut stmt = db.prepare(
        "SELECT id,deleted_at_ms,memory_id,scope,category,length,actor,actor_via FROM memory_deletions ORDER BY deleted_at_ms DESC, id DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit.min(500)], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "deleted_at_ms": r.get::<_, i64>(1)?,
                "memory_id": r.get::<_, String>(2)?,
                "scope": r.get::<_, Option<String>>(3)?,
                "category": r.get::<_, Option<String>>(4)?,
                "length": r.get::<_, i64>(5)?,
                "actor": r.get::<_, Option<String>>(6)?,
                "actor_via": r.get::<_, String>(7)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "deletions": rows }))
}

/// evestack's sessions list: one row per session (its top-level turns), with
/// the rollup evestack's listSessions computes. The LIMIT applies to the
/// session ids first, using `fact_turn_session`, before any per-session work.
pub fn list_sessions(db: &Connection, limit: u64, at: i64) -> rusqlite::Result<Value> {
    let mut stmt = db.prepare(
        "WITH recent AS (SELECT session_id, MAX(started_at_ms) AS last_ms FROM fact_turn
                         WHERE parent_id IS NULL GROUP BY session_id ORDER BY last_ms DESC LIMIT ?1)
         SELECT r.session_id, recent.last_ms, COUNT(*), MIN(r.started_at_ms),
                SUM(r.outcome='failed'), SUM(r.outcome='cancelled'), SUM(r.outcome='no_model_call'),
                SUM(r.outcome='wedged' OR (r.status='running' AND r.started_at_ms<?2)),
                SUM(r.status IN ('running','queued')),
                SUM(r.input_tokens), SUM(r.output_tokens),
                SUM(r.unpriced_calls), SUM(r.cost_usd),
                (SELECT COUNT(*) FROM fact_turn c WHERE c.parent_id IS NOT NULL AND c.root_id IN
                    (SELECT id FROM fact_turn t WHERE t.session_id=r.session_id AND t.parent_id IS NULL)),
                (SELECT model FROM fact_turn l WHERE l.session_id=r.session_id AND l.parent_id IS NULL ORDER BY l.started_at_ms DESC LIMIT 1),
                (SELECT provider FROM fact_turn l WHERE l.session_id=r.session_id AND l.parent_id IS NULL ORDER BY l.started_at_ms DESC LIMIT 1),
                (SELECT kind FROM fact_turn l WHERE l.session_id=r.session_id AND l.parent_id IS NULL ORDER BY l.started_at_ms DESC LIMIT 1),
                (SELECT outcome FROM fact_turn l WHERE l.session_id=r.session_id AND l.parent_id IS NULL ORDER BY l.started_at_ms DESC LIMIT 1)
         FROM recent JOIN fact_turn r ON r.session_id=recent.session_id AND r.parent_id IS NULL
         GROUP BY r.session_id ORDER BY recent.last_ms DESC",
    )?;
    let rows = stmt
        .query_map(params![limit.min(200), at - STUCK_TURN_MS], |r| {
            let unpriced: i64 = r.get(11)?;
            let kind: String = r.get(16)?;
            Ok(json!({
                "session_id": r.get::<_, String>(0)?,
                "last_started_at_ms": r.get::<_, i64>(1)?,
                "turns": r.get::<_, i64>(2)?,
                "first_started_at_ms": r.get::<_, i64>(3)?,
                "failed": r.get::<_, i64>(4)?,
                "cancelled": r.get::<_, i64>(5)?,
                "no_model_call": r.get::<_, i64>(6)?,
                "wedged": r.get::<_, i64>(7)?,
                "running": r.get::<_, i64>(8)?,
                "input_tokens": r.get::<_, i64>(9)?,
                "output_tokens": r.get::<_, i64>(10)?,
                // NULL, never 0, when any turn ran a model with no price.
                "cost_usd": if unpriced > 0 { Value::Null } else { json!(r.get::<_, Option<f64>>(12)?) },
                "subagents": r.get::<_, i64>(13)?,
                "model": r.get::<_, String>(14)?,
                "provider": r.get::<_, String>(15)?,
                "trigger": if kind == "cron" { "schedule" } else { "desktop" },
                "last_outcome": r.get::<_, String>(17)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "sessions": rows }))
}

/// evestack's facts over `fact_turn` / `fact_tool_call`: turn
/// outcomes per model and per-tool call counts, failure rate and latency.
/// Failure rate divides by judged calls only (`ok IS NOT NULL`), like
/// lib/metrics.ts: an unjudged call is neither a win nor a loss.
pub fn facts(db: &Connection, since: i64) -> rusqlite::Result<Value> {
    let mut outcomes = db.prepare(
        "SELECT model, CASE WHEN outcome='running' AND status='running' AND started_at_ms < CAST(strftime('%s','now') AS INTEGER)*1000 - 3600000
                THEN 'wedged' ELSE outcome END AS o, COUNT(*) FROM fact_turn WHERE started_at_ms>=?1 AND parent_id IS NULL GROUP BY model, o ORDER BY model",
    )?;
    let outcomes = outcomes
        .query_map([since], |r| Ok(json!({"model": r.get::<_, String>(0)?, "outcome": r.get::<_, String>(1)?, "turns": r.get::<_, i64>(2)?})))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut calls = db.prepare(
        "SELECT tool_name, ok, duration_ms FROM fact_tool_call WHERE started_at_ms>=?1 ORDER BY tool_name",
    )?;
    let mut by_tool: std::collections::BTreeMap<String, (i64, i64, i64, Vec<i64>)> = Default::default();
    for row in calls.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?)))? {
        let (tool, ok, ms) = row?;
        let entry = by_tool.entry(tool).or_default();
        entry.0 += 1;
        match ok {
            Some(1) => entry.1 += 1,
            Some(_) => entry.2 += 1,
            None => {}
        }
        if let Some(ms) = ms {
            entry.3.push(ms);
        }
    }
    let tools: Vec<Value> = by_tool
        .into_iter()
        .map(|(tool, (n, ok, failed, mut ms))| {
            ms.sort_unstable();
            let judged = ok + failed;
            json!({
                "tool_name": tool, "calls": n, "failed": failed, "unjudged": n - judged,
                "failure_rate_pct": if judged > 0 { json!(failed as f64 * 100.0 / judged as f64) } else { Value::Null },
                "p50_ms": percentile(&ms, 50.0), "p95_ms": percentile(&ms, 95.0),
            })
        })
        .collect();
    Ok(json!({ "since_ms": since, "turn_outcomes": outcomes, "tools": tools }))
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
        "SELECT COUNT(*) FROM fact_turn WHERE session_id=?1 AND parent_id IS NULL AND started_at_ms<?2",
        params![session, started_at_ms],
        |r| r.get(0),
    )
}

/// evestack's `fact_turn.outcome` vocabulary, computed at RunEnd (docs'
/// mapping table has the full correspondence). `wedged` is deliberately never
/// written here: it only ever applies to a run stuck in `status='running'`,
/// which is exactly the state that never reaches this function — see
/// `wedged_count`/`overlay_wedged` for how it is derived live instead.
/// `budget_stopped` has no Akira source: nothing stops a turn for spend.
pub fn run_outcome(status: &str, error: &Option<String>, model_calls: u64) -> &'static str {
    if error.is_some() || status == "error" || status == "failed" {
        "failed"
    } else if status == "interrupted" {
        // A stop request that ended the turn (message.complete carries
        // status=interrupted). Work the process died in the middle of is
        // marked `wedged` by setup() instead.
        "cancelled"
    } else if model_calls == 0 {
        "no_model_call"
    } else {
        "ok"
    }
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
            "UPDATE fact_turn SET flags = flags | ?2 WHERE id=?1",
            params![id, FLAG_NO_MODEL_CALL],
        )?;
    }
    tx.execute(
        "UPDATE fact_turn SET outcome=?2 WHERE id=?1",
        params![id, run_outcome(status, error, model_calls)],
    )?;
    Ok(())
}

/// Runs open past `STUCK_TURN_MS` with nothing to retry them — evestack's
/// `wedged()` (lib/alerts.ts) and the fleet banner's "wedged" health, adapted
/// to Akira's single-process model: there is no separate agent to probe, so
/// `status='running'` plus age is definitive rather than a liveness guess.
pub fn wedged_count(db: &Connection, at: i64) -> rusqlite::Result<i64> {
    db.query_row(
        "SELECT COUNT(*) FROM fact_turn WHERE status='running' AND started_at_ms<?1",
        [at - STUCK_TURN_MS],
        |r| r.get(0),
    )
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
        "INSERT INTO approvals(run_id,session_id,tool,command_preview,decision,actor,at_ms,request_kind,approver_via) VALUES(?1,?2,?3,?4,?5,?6,?7,'tool-approval','session')",
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

    let wedged = mon["wedged_count"].as_i64().unwrap_or(0);
    let unpriced_turns = mon["unpriced_turns"].as_i64().unwrap_or(0);

    let checks: Vec<(&str, &str, AlertState, String)> = vec![
        (
            // evestack's `turn_failure_rate` (lib/alerts.ts): named and
            // thresholded the same way, over finished turns only (see
            // monitors()'s denominator fix).
            "turn_failure_rate",
            "page",
            if mon["finished_runs"].as_i64().unwrap_or(0) == 0 {
                AlertState::NotChecked
            } else if mon["error_rate_pct"].as_f64().unwrap_or(0.0) >= cfg.alert_error_rate_pct {
                AlertState::Firing
            } else {
                AlertState::Ok
            },
            if mon["finished_runs"].as_i64().unwrap_or(0) == 0 {
                "No turns have finished in the window, so there is no rate to judge.".into()
            } else {
                format!(
                    "{:.1}% of {} finished turns failed (threshold {:.0}%)",
                    mon["error_rate_pct"].as_f64().unwrap_or(0.0),
                    mon["finished_runs"].as_i64().unwrap_or(0),
                    cfg.alert_error_rate_pct
                )
            },
        ),
        (
            // evestack's `wedged` (lib/alerts.ts): a turn open past
            // STUCK_TURN_MS with nothing in Akira to retry it either.
            "wedged",
            "page",
            if wedged > 0 { AlertState::Firing } else { AlertState::Ok },
            if wedged > 0 {
                format!(
                    "{wedged} run{} started over {}h ago and never finished. Nothing retries these.",
                    if wedged == 1 { "" } else { "s" },
                    STUCK_TURN_MS / 3_600_000
                )
            } else {
                format!("No run has been open longer than {}h.", STUCK_TURN_MS / 3_600_000)
            },
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
            // evestack's `unpriced_spend` (lib/alerts.ts): any turn today that
            // ran a model with no catalog price means the real bill is
            // unknown, never zero.
            "unpriced_spend",
            "info",
            if unpriced_turns > 0 { AlertState::Firing } else { AlertState::Ok },
            if unpriced_turns > 0 {
                format!("{unpriced_turns} turn(s) today ran a model with no catalog price; their real cost is unknown, not zero.")
            } else {
                "Every model that ran today has a catalog price.".into()
            },
        ),
        (
            "daily_spend",
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
            // evestack's `turn_latency_p95` (lib/alerts.ts).
            "turn_latency_p95",
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
                "SELECT state FROM alert_state WHERE monitor_key=?1",
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
            "INSERT INTO alert_state(monitor_key,state,severity,message,updated_at_ms,last_notified_ms)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(monitor_key) DO UPDATE SET state=excluded.state, message=excluded.message,
             updated_at_ms=excluded.updated_at_ms, last_notified_ms=COALESCE(excluded.last_notified_ms, alert_state.last_notified_ms)",
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

/// evestack.alert_state.
pub fn list_alerts(db: &Connection) -> rusqlite::Result<Value> {
    let mut stmt = db.prepare("SELECT monitor_key,state,severity,monitor_key,message,updated_at_ms,last_notified_ms FROM alert_state ORDER BY monitor_key")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "state": r.get::<_, String>(1)?,
                "severity": r.get::<_, String>(2)?,
                "title": r.get::<_, String>(3)?,
                "detail": r.get::<_, Option<String>>(4)?,
                "since_ms": r.get::<_, i64>(5)?,
                "notified_at_ms": r.get::<_, Option<i64>>(6)?,
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
        crate::observability::schema::open(&mut db).unwrap();
        db
    }

    #[test]
    fn list_filtered_uses_started_at_index() {
        let db = mem_db();
        db.execute(
            "INSERT INTO fact_turn(id,session_id,root_id,kind,model,provider,status,started_at_ms,title)
             VALUES('a','s','a','invoke_agent','m','p','complete',1,'hello')",
            [],
        )
        .unwrap();
        let plan: String = db
            .prepare("EXPLAIN QUERY PLAN SELECT id FROM fact_turn WHERE status='complete' ORDER BY started_at_ms DESC LIMIT 10")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(plan.contains("fact_turn") || plan.contains("INDEX"), "{plan}");
        let rows = list_filtered(&db, 10, Some("complete"), None, Some("hel"), None, None, 0).unwrap();
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
            "INSERT INTO fact_turn(id,session_id,root_id,kind,model,provider,status,started_at_ms,ended_at_ms,cost_usd,flags)
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
            "INSERT INTO fact_turn(id,session_id,root_id,kind,model,provider,status,started_at_ms) VALUES('x','s','x','invoke_agent','m','p','running',1)",
            [],
        )
        .unwrap();
        apply_run_end_flags(&tx, "x", "complete", &None, 0).unwrap();
        tx.commit().unwrap();
        let flags: i64 = db.query_row("SELECT flags FROM fact_turn WHERE id='x'", [], |r| r.get(0)).unwrap();
        assert_eq!(flags & FLAG_NO_MODEL_CALL, FLAG_NO_MODEL_CALL);
    }

    /// The indexes ported from evestack's query-indexes.sql / traces.sql are
    /// the ones each hot read actually plans on (EXPLAIN QUERY PLAN).
    #[test]
    fn hot_reads_plan_on_their_indexes() {
        let db = mem_db();
        let plan = |sql: &str| -> String {
            db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(3))
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .join(" | ")
        };
        for (sql, index) in [
            ("SELECT id FROM fact_turn ORDER BY started_at_ms DESC LIMIT 50", "fact_turn_recent"),
            ("SELECT id FROM fact_turn WHERE session_id='s' ORDER BY started_at_ms DESC LIMIT 50", "fact_turn_session"),
            ("SELECT id FROM fact_turn WHERE kind='cron' ORDER BY started_at_ms DESC LIMIT 50", "fact_turn_kind_recent"),
            ("SELECT id FROM fact_turn WHERE outcome='failed' ORDER BY started_at_ms DESC LIMIT 50", "fact_turn_outcome_recent"),
            ("SELECT COUNT(*) FROM fact_turn WHERE status='running' AND started_at_ms<5", "fact_turn_status_started"),
            ("SELECT id FROM fact_turn WHERE root_id='r' AND parent_id IS NOT NULL", "fact_turn_root"),
            ("SELECT id FROM fact_turn WHERE parent_id='r'", "fact_turn_parent"),
            ("SELECT id FROM spans WHERE run_id='r' ORDER BY started_at_ms", "spans_run"),
            ("SELECT id FROM spans WHERE name='bash' ORDER BY started_at_ms DESC LIMIT 20", "spans_name"),
            ("SELECT id FROM approvals WHERE session_id='s' ORDER BY at_ms DESC", "approvals_session"),
            ("SELECT id FROM approvals ORDER BY at_ms DESC LIMIT 20", "approvals_recent"),
            ("SELECT id FROM memory_deletions ORDER BY deleted_at_ms DESC LIMIT 20", "memory_deletions_recent"),
            // The sessions list picks its LIMIT session ids from the covering
            // session index before joining anything.
            ("SELECT session_id, MAX(started_at_ms) FROM fact_turn WHERE parent_id IS NULL GROUP BY session_id", "fact_turn_session"),
        ] {
            let p = plan(sql);
            assert!(p.contains(index), "{sql}\n  plans as: {p}");
            assert!(!p.contains("SCAN fact_turn |") && !p.ends_with("SCAN fact_turn"), "{sql}\n  scans the table: {p}");
        }
    }
}
