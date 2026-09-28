//! Sole owner of the observability DDL: the tables, indexes, columns and views
//! that line up with evestack's SQL (`packages/dashboard/sql/{facts,traces,
//! approvals,memory-audit,alerts,query-indexes}.sql`). The tables carry
//! evestack's names (SQLite has no `evestack.` schema) and timestamps are
//! epoch milliseconds. Everything here is idempotent; the table-by-table
//! mapping is in docs/OBSERVABILITY.md.

use rusqlite::Connection;

/// Open-time entry point: the shared schema first (the audit trigger hangs
/// off `memories`), the versioned migrations, then this module's baseline
/// for fresh files (idempotent).
pub fn open(db: &mut Connection) -> rusqlite::Result<()> {
    let to_sql = |err: anyhow::Error| rusqlite::Error::ToSqlConversionFailure(err.into());
    let had_data: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table')", [], |r| r.get(0))?;
    jcode_base::migrate_sovereign_db(db).map_err(to_sql)?;
    // Versioned migrations (the pre-EveStack `obs_*` renames included) run before the baseline below.
    let path = db.path().filter(|p| !p.is_empty()).map(std::path::PathBuf::from);
    sovereign_prime::migrate::run(db, path.as_deref().filter(|_| had_data)).map_err(to_sql)?;
    db.execute_batch(TABLES)?;
    for (table, column, ddl) in [
        ("fact_turn", "flags", "INTEGER NOT NULL DEFAULT 0"),
        ("fact_turn", "replay_of", "TEXT"),
        ("fact_turn", "outcome", "TEXT NOT NULL DEFAULT 'running'"),
        ("fact_turn", "span_coverage", "TEXT NOT NULL DEFAULT 'none'"),
        ("fact_turn", "ttft_ms", "INTEGER"),
        ("approvals", "request_kind", "TEXT"),
        ("approvals", "approver_via", "TEXT NOT NULL DEFAULT 'unidentified'"),
    ] {
        let present = db.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name=?2")?.exists([table, column])?;
        if !present {
            db.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"), [])?;
        }
    }
    db.execute_batch(DDL)
}

const TABLES: &str = "
    CREATE TABLE IF NOT EXISTS fact_turn(
        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, parent_id TEXT, root_id TEXT NOT NULL,
        kind TEXT NOT NULL, title TEXT, model TEXT NOT NULL, provider TEXT NOT NULL,
        status TEXT NOT NULL, started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER,
        input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        cost_usd REAL, error TEXT, unpriced_calls INTEGER NOT NULL DEFAULT 0,
        flags INTEGER NOT NULL DEFAULT 0, replay_of TEXT
    );
    CREATE INDEX IF NOT EXISTS fact_turn_recent ON fact_turn(started_at_ms DESC);
    CREATE INDEX IF NOT EXISTS fact_turn_session ON fact_turn(session_id, started_at_ms DESC);
    CREATE INDEX IF NOT EXISTS fact_turn_kind_recent ON fact_turn(kind, started_at_ms DESC);
    CREATE TABLE IF NOT EXISTS spans(
        id TEXT PRIMARY KEY, run_id TEXT NOT NULL, parent_id TEXT NOT NULL, root_id TEXT NOT NULL,
        kind TEXT NOT NULL, name TEXT NOT NULL, status TEXT NOT NULL,
        started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER,
        input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        cost_usd REAL, error TEXT, model TEXT, provider TEXT,
        attributes TEXT NOT NULL DEFAULT '{}'
    );
    CREATE INDEX IF NOT EXISTS spans_run ON spans(run_id, started_at_ms);
    CREATE TABLE IF NOT EXISTS span_content(id TEXT PRIMARY KEY, input TEXT, output TEXT);
    CREATE TABLE IF NOT EXISTS approvals(
        id INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT, session_id TEXT NOT NULL,
        tool TEXT NOT NULL, command_preview TEXT NOT NULL, decision TEXT NOT NULL,
        actor TEXT NOT NULL, at_ms INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS approvals_recent ON approvals(at_ms DESC);
    CREATE TABLE IF NOT EXISTS alert_state(
        monitor_key TEXT PRIMARY KEY, state TEXT NOT NULL, severity TEXT NOT NULL,
        message TEXT, updated_at_ms INTEGER NOT NULL, last_notified_ms INTEGER
    );
";

const DDL: &str = "
    -- query-indexes.sql: the root/parent lookups behind the session tree, as
    -- partial IS NOT NULL indexes so the planner can prove them from an
    -- equality; fact_turn_kind_recent / _session / _status_started already
    -- cover the type-keyset list, session filter and wedged scan.
    CREATE INDEX IF NOT EXISTS fact_turn_outcome_recent ON fact_turn(outcome, started_at_ms DESC);
    CREATE INDEX IF NOT EXISTS fact_turn_status_started ON fact_turn(status, started_at_ms);
    CREATE INDEX IF NOT EXISTS fact_turn_root ON fact_turn(root_id) WHERE root_id IS NOT NULL;
    CREATE INDEX IF NOT EXISTS fact_turn_parent ON fact_turn(parent_id) WHERE parent_id IS NOT NULL;
    -- traces.sql spans_name_idx: newest / slowest calls of one tool.
    CREATE INDEX IF NOT EXISTS spans_name ON spans(name, started_at_ms DESC);
    CREATE INDEX IF NOT EXISTS approvals_session ON approvals(session_id, at_ms DESC);

    -- evestack.fact_tool_call: a tool call is a span here as well. ok is
    -- tri-state like evestack's: 1 done, 0 errored, NULL unjudged (still
    -- running, or the process died mid-call).
    DROP VIEW IF EXISTS fact_tool_call;
    CREATE VIEW fact_tool_call AS
        SELECT s.id AS span_id, s.run_id, r.session_id, s.name AS tool_name, s.started_at_ms,
               s.ended_at_ms - s.started_at_ms AS duration_ms,
               CASE s.status WHEN 'complete' THEN 1 WHEN 'error' THEN 0 END AS ok,
               s.error AS error_message,
               length(c.input) AS arguments_bytes, length(c.output) AS result_bytes
        FROM spans s JOIN fact_turn r ON r.id = s.run_id
        LEFT JOIN span_content c ON c.id = s.id
        WHERE s.kind = 'execute_tool';

    -- evestack.memory_deletions, filled by a trigger so every deletion path
    -- (tool, consolidation, desktop) is caught with no hook in the memory
    -- code and nothing added to the chat path.
    CREATE TABLE IF NOT EXISTS memory_deletions(
        id INTEGER PRIMARY KEY AUTOINCREMENT, deleted_at_ms INTEGER NOT NULL,
        memory_id TEXT NOT NULL, scope TEXT, content TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '',
        actor TEXT, actor_via TEXT NOT NULL DEFAULT 'unidentified'
    );
    CREATE INDEX IF NOT EXISTS memory_deletions_recent ON memory_deletions(deleted_at_ms DESC);
    CREATE TRIGGER IF NOT EXISTS memory_audit AFTER DELETE ON memories BEGIN
        INSERT INTO memory_deletions(deleted_at_ms, memory_id, scope, content, tags)
        VALUES (CAST(strftime('%s','now') AS INTEGER) * 1000, old.id, old.scope, old.content, old.tags);
    END;
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_obs_tables_are_renamed_once() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE obs_runs(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, parent_id TEXT, root_id TEXT NOT NULL,
                kind TEXT NOT NULL, title TEXT, model TEXT NOT NULL, provider TEXT NOT NULL, status TEXT NOT NULL,
                started_at_ms INTEGER NOT NULL, ended_at_ms INTEGER, input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0, cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0, cost_usd REAL, error TEXT, unpriced_calls INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX obs_runs_recent ON obs_runs(started_at_ms DESC);
             INSERT INTO obs_runs VALUES('r','s',NULL,'r','chat',NULL,'m','p','complete',1,2,0,0,0,0,NULL,NULL,0);",
        )
        .unwrap();
        open(&mut db).unwrap();
        open(&mut db).unwrap();
        let leftovers: i64 = db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'obs\\_%' ESCAPE '\\'", [], |r| r.get(0)).unwrap();
        assert_eq!(leftovers, 0);
        assert_eq!(db.query_row("SELECT id FROM fact_turn", [], |r| r.get::<_, String>(0)).unwrap(), "r");
    }
}
