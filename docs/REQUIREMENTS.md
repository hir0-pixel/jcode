# Akira requirements

What the product must do, and the numbers it must stay inside. Functional scope lives in three
places and is not repeated here; the budgets below are the non-functional contract. Each row says how
it is measured and what the last measurement was, so a change that moves one has to say so.

## Functional scope

- Hermes desktop feature surface served by the engine or forwarded to Python: `PARITY.md`,
  `HERMES_FEATURE_CHECK.md`.
- Prime learning, goals, loops, heartbeats, REPL: `PRIME_PARITY.md`, `PRIME_PARITY_REPORT.md`.
- Ownership: learning and goals are Prime-style (`sovereign-prime`), sessions and the agent loop are
  jcode, memory is Rust (`jcode-base` `memory_store`), observability is EveStack-shaped SQLite,
  the look is `apps/desktop/DESIGN.md` in the desktop repo. Everything runs locally.

## Non-functional budgets

| Budget | Limit | How measured | Last measured |
| --- | --- | --- | --- |
| Idle engine memory | physical footprint <= 50 MB | Engine running, no window, 30 s idle: `footprint -p <pid>` (macOS). `ps` RSS counts shared library pages and reads about 4x higher, so it is not the budget | 25 MB footprint (peak 83 MB during start), 103 MB `ps` RSS, release build, 2026-09-29; method: start `sovereign serve` on a release build, wait 30 s with no client, `footprint -p <pid>` (re-measure on a fresh release build). Audit: 29-30 MB. Benchmark median with a chat session: 34 idle, 43 after a session, 45 peak (`BENCHMARK.md`, RAM table) |
| Python backend | off by default | Starts only for a forwarded feature, a due cron job, or an enabled messaging platform (`features.rs`: `port()`, `ensure_bots`); stops after 10 min idle unless bots are enabled or a request is in flight (lease). Check: no child process of the engine after boot with no bots | 0 MB Python in every benchmark run; unit tests `starts_on_demand_reuses_and_stops_when_idle`, `bots_start_the_backend_and_a_leased_one_is_never_idle_stopped` |
| Tool-schema tokens per call | see `BENCHMARK.md` section "Tool schema budget (2026-09-29)" | Counting proxy (`scripts/sovereign-counting-proxy.mjs`) on the first model call; guard test `agent_tests/tool_schema_budget.rs` | Value and history live in that section, which the lazy-tool-loading work owns; this row follows it. Before it: 7,896 tokens with 26 tools |
| Prompt cache hit | >= 80% of prompt tokens on a new session's first call | Proxy `calls.jsonl` cached vs prompt tokens (`BENCHMARK.md`, "What explains the gap") | 88% on the first call of a new session, 66 main calls (Ollama KV cache; hosted providers differ in TTL and minimum prefix) |
| Background model calls per idle turn | 0 | Idle = no user turn. Count spans whose kind is not a user-driven agent turn in `fact_turn`/`spans`; benchmark "Calls by purpose" | 0 memory/skill review, 0 title calls over 66 main calls. The only unattended calls are the learning gate and the goal supervisor, below |
| Learning gate cost | <= 1 call per 25 assistant turns and per 20 min | `learn.rs` constants (`turnInterval` 25, `cooldownMs` 20 min; env `SOVEREIGN_LEARN_TURN_INTERVAL`, `SOVEREIGN_LEARN_COOLDOWN_MS`); the call is recorded as an aux span | By construction; unit tests in `learn.rs` |
| Goal supervisor cost | <= 1 aux call per plateau episode, never within 5 turns of the last | `agent_loop.rs` `supervisor_due`; error turns never count toward plateau | Test `supervisor_is_once_per_plateau_and_rate_limited` |
| Goal error handling | a failing model turn never loops | 401/403 pauses the goal at once; 429/5xx/network retry after 30 s, 2 min, 10 min, then pause; error turns record no attempt and no turn | Test `error_turns_back_off_then_pause_and_never_count` |
| Cold start | engine answers `/api/health` within 1 s; Python backend ready within 120 s of first use; REPL kernel < 500 ms | Time from process start to first successful `curl /api/health`, temp `JCODE_HOME`, release build; `START_TIMEOUT` in `features.rs`; REPL test | 0.22 s engine; REPL cold start 79 ms (`FINISH_REPORT.md`) |
| REPL latency | warm p95 < 5 ms; run timeout 20 s; kernel RSS watchdog 128 MiB; idle reap 10 min | Rust dispatch test and REPL tests (`PRIME_PARITY.md`) | Dispatch p95 88.8 us |
| Cron punctuality | a due job fires within 60 s of its time, including after macOS system sleep; a job a tick cannot advance backs off 30 s, 60 s, 2 min ... 1 h | `cron_tick.rs` sleeps in chunks of <= 60 s and re-reads the wall clock | Tests `sleeps_in_bounded_chunks_and_backs_off_per_stuck_job` |
| `sovereign.db` growth | <= 150 MB at the default 30-day retention for a heavy user (100 turns a day) | See below | Estimate, not yet measured on a populated file |
| Update safety | a failed update leaves the old build bootable | `sovereign-update.sh` restores the old bundle and the `sovereign.db.pre-v*.bak` taken during the update (`docs/RELEASING.md`) | Test `a rollback restores the sovereign.db backup taken during the update and nothing older` |

### `sovereign.db` growth and retention

Observability keeps one `fact_turn` row per turn, one `spans` row per model or tool call, and
`span_content` (input and output text) per span, each side capped at 4,096 characters
(`CONTENT_LIMIT` in `observability.rs`). Rows older than the retention window are deleted once a day
(`prune`, default 30 days, `SOVEREIGN_OBSERVABILITY_RETENTION_DAYS` 1 to 3650). Deleted pages are reused
by SQLite, so the file stops growing at its peak size and does not shrink. Memory, learning entries and
parked approvals are small and not pruned.

Estimate per turn with one model call and four tool calls: about 0.3 KiB for the turn row, 5 x 0.4 KiB
of span rows, and 5 x (typically 2 KiB, at most 8 KiB) of content, so about 12 KiB typical and 42 KiB
worst case. At 100 turns a day for 30 days: 36 MB typical, 126 MB worst case. At 500 turns a day: 180 MB
typical, 630 MB worst case; lower retention or fewer tool calls per turn is the lever. The estimate is
from the caps, because no populated database was available; check it on a real one with

```sql
select count(*), sum(length(coalesce(input,''))+length(coalesce(output,''))) from span_content;
select name, sum(pgsize) from dbstat group by 1 order by 2 desc limit 5;
```

## Availability and recovery

- Engine restart: active goals and loops resume once per process, retrying every 15 s until the
  continuation is accepted; the resume records no attempt and no turn. Unattended approvals parked for a
  late answer survive the restart (`parked_approvals`).
- Bots: with a messaging platform enabled the engine starts the Python backend at boot and restarts it if
  it dies (checked on the idle-stop tick, at most every 60 s).
- Schema: one `PRAGMA user_version` owned by `sovereign-prime/src/migrate.rs`, covering the memory
  tables too; a backup `sovereign.db.pre-v<N>.bak` is written before migrating a file with data; a file
  from a newer engine is refused. Rollback: `docs/RELEASING.md`.
