# M10c observability parity — decisions

## Scope

Local EveStack-style **monitors, budgets, alerts, approvals audit, promote/replay**, and Activity UI — no OTLP export, no extra model calls on read paths.

## Storage (schema v4)

- `obs_runs.flags` bit `1` = `no_model_call` (complete, no error, zero model spans).
- `obs_runs.replay_of` links a replay run to its source.
- `obs_approvals` append-only audit (command preview capped at 500 chars).
- `obs_alerts` holds last monitor state for transition dedup.

## Queries

- List filters use `obs_runs_recent` / `obs_runs_kind_recent` with `ORDER BY started_at_ms DESC LIMIT` (see unit test `EXPLAIN QUERY PLAN` on filtered list).
- Monitors aggregate in SQL then compute p50/p95/p99 in Rust over bounded window rows (1h / 24h / 7d).

## Budgets

- `observability.json` → `budget_daily_usd`; override `SOVEREIGN_BUDGET_DAILY_USD`.
- `over_budget` is informational only (soft banner in Activity); never blocks submits.

## Alerts

- Evaluated on observability writer idle tick (~60s), not on the chat path.
- EveStack-style transition rules: notify on enter/leave firing, not on steady-state firing.
- Desktop: `event` / `observability.alert` on connected WS clients; optional `SOVEREIGN_ALERT_WEBHOOK_URL` (curl POST, off by default).

## Promote / replay

- Promote writes `JCODE_HOME/evals/<sanitized-run-id>.json` (see `docs/eval-case-format.md`).
- Replay: `session.branch` + rewind to turn index derived from run order, resubmit captured prompt, `replay_of` link.

## Approvals audit

- Every `Hub::decide` outcome recorded with actor `user` | `headless-deny` | `session-grant`.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
