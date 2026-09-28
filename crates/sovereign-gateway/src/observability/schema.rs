//! What the observability ledger adds on top of the tables that
//! `jcode_base::migrate_sovereign_db` creates (`obs_runs`, `obs_spans`,
//! `obs_content`, `obs_approvals`, `obs_alerts`), to line them up with
//! evestack's SQL (`packages/dashboard/sql/{facts,traces,approvals,
//! memory-audit,alerts,query-indexes}.sql`). SQLite has no schemas, so
//! evestack's `evestack.` namespace is the `obs_` prefix, and timestamps are
//! epoch milliseconds. Everything here is idempotent; the table-by-table
//! mapping is in docs/OBSERVABILITY.md.

use rusqlite::Connection;

/// Open-time entry point: the shared schema first (the audit trigger hangs
/// off `memories`), then this module's additions.
pub fn open(db: &mut Connection) -> rusqlite::Result<()> {
    jcode_base::migrate_sovereign_db(db).map_err(|err| rusqlite::Error::ToSqlConversionFailure(err.into()))?;
    for (table, column, ddl) in [
        ("obs_runs", "outcome", "TEXT NOT NULL DEFAULT 'running'"),
        ("obs_runs", "span_coverage", "TEXT NOT NULL DEFAULT 'none'"),
        ("obs_runs", "ttft_ms", "INTEGER"),
        ("obs_approvals", "request_kind", "TEXT"),
        ("obs_approvals", "approver_via", "TEXT NOT NULL DEFAULT 'unidentified'"),
    ] {
        let present = db.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name=?2")?.exists([table, column])?;
        if !present {
            db.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"), [])?;
        }
    }
    db.execute_batch(DDL)
}

const DDL: &str = "
    -- query-indexes.sql: the root/parent lookups behind the session tree, as
    -- partial IS NOT NULL indexes so the planner can prove them from an
    -- equality; obs_runs_kind_recent / _session / _status_started already
    -- cover the type-keyset list, session filter and wedged scan.
    CREATE INDEX IF NOT EXISTS obs_runs_outcome_recent ON obs_runs(outcome, started_at_ms DESC);
    CREATE INDEX IF NOT EXISTS obs_runs_status_started ON obs_runs(status, started_at_ms);
    CREATE INDEX IF NOT EXISTS obs_runs_root ON obs_runs(root_id) WHERE root_id IS NOT NULL;
    CREATE INDEX IF NOT EXISTS obs_runs_parent ON obs_runs(parent_id) WHERE parent_id IS NOT NULL;
    -- traces.sql spans_name_idx: newest / slowest calls of one tool.
    CREATE INDEX IF NOT EXISTS obs_spans_name ON obs_spans(name, started_at_ms DESC);
    CREATE INDEX IF NOT EXISTS obs_approvals_session ON obs_approvals(session_id, at_ms DESC);

    -- evestack.fact_turn, under evestack's column names. obs_runs is the
    -- stored table (its ids are what spans and replay link to); this view is
    -- the contract other SQL reads.
    DROP VIEW IF EXISTS obs_fact_turn;
    CREATE VIEW obs_fact_turn AS
        SELECT id AS run_id,
               CASE WHEN parent_id IS NULL THEN 'turn' ELSE 'subagent' END AS run_type,
               session_id, kind, model, provider,
               started_at_ms, ended_at_ms, ended_at_ms - started_at_ms AS duration_ms, ttft_ms,
               input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
               CASE WHEN unpriced_calls > 0 THEN 0 WHEN cost_usd IS NOT NULL THEN 1 END AS priced,
               cost_usd, error, span_coverage,
               CASE WHEN outcome='running' AND status='running'
                         AND started_at_ms < CAST(strftime('%s','now') AS INTEGER)*1000 - 3600000
                    THEN 'wedged' ELSE outcome END AS outcome
        FROM obs_runs;

    -- evestack.fact_tool_call: a tool call is a span here as well. ok is
    -- tri-state like evestack's: 1 done, 0 errored, NULL unjudged (still
    -- running, or the process died mid-call).
    DROP VIEW IF EXISTS obs_fact_tool_call;
    CREATE VIEW obs_fact_tool_call AS
        SELECT s.id AS span_id, s.run_id, r.session_id, s.name AS tool_name, s.started_at_ms,
               s.ended_at_ms - s.started_at_ms AS duration_ms,
               CASE s.status WHEN 'complete' THEN 1 WHEN 'error' THEN 0 END AS ok,
               s.error AS error_message,
               length(c.input) AS arguments_bytes, length(c.output) AS result_bytes
        FROM obs_spans s JOIN obs_runs r ON r.id = s.run_id
        LEFT JOIN obs_content c ON c.id = s.id
        WHERE s.kind = 'execute_tool';

    -- evestack.alert_state: monitor_key is the id, message the detail, and
    -- updated_at_ms is when the monitor entered its state (rows are only
    -- rewritten on a transition), i.e. evestack's `since`.
    DROP VIEW IF EXISTS obs_alert_state;
    CREATE VIEW obs_alert_state AS
        SELECT monitor_key AS id, state, severity, monitor_key AS title, message AS detail,
               updated_at_ms AS since_ms, last_notified_ms AS notified_at_ms
        FROM obs_alerts;

    -- evestack.memory_deletions, filled by a trigger so every deletion path
    -- (tool, consolidation, desktop) is caught with no hook in the memory
    -- code and nothing added to the chat path.
    CREATE TABLE IF NOT EXISTS obs_memory_deletions(
        id INTEGER PRIMARY KEY AUTOINCREMENT, deleted_at_ms INTEGER NOT NULL,
        memory_id TEXT NOT NULL, scope TEXT, content TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '',
        actor TEXT, actor_via TEXT NOT NULL DEFAULT 'unidentified'
    );
    CREATE INDEX IF NOT EXISTS obs_memory_deletions_recent ON obs_memory_deletions(deleted_at_ms DESC);
    CREATE TRIGGER IF NOT EXISTS obs_memory_audit AFTER DELETE ON memories BEGIN
        INSERT INTO obs_memory_deletions(deleted_at_ms, memory_id, scope, content, tags)
        VALUES (CAST(strftime('%s','now') AS INTEGER) * 1000, old.id, old.scope, old.content, old.tags);
    END;
";
