# Local observability

## Decision

Sovereign keeps its run ledger in `observability.sqlite3` under `JCODE_HOME`
(`~/.sovereign` in the desktop build). The engine and gateway emit facts at the
point where work is accepted or completed. A single bounded channel feeds a
dedicated SQLite writer; the chat path never waits for a disk write. The view
reads this database through authenticated Rust gateway routes. No trace or
content is exported by default.

This is an *observability ledger*, not a durable-execution engine. On startup,
unfinished work is marked `interrupted`; it is never replayed. Replaying a tool
after a crash could repeat a shell command or another side effect.

## Records

Tier 1 is always enabled. `runs` has one row per accepted turn or subagent:
`id`, `session_id`, `parent_id`, `root_id`, `kind`, `model`, `provider`,
`status`, `started_at_ms`, `ended_at_ms`, `input_tokens`, `output_tokens`,
`cache_read_tokens`, `cache_write_tokens`, `cost_usd`, and `error`. `spans`
has one row per model call or tool invocation: `id`, `run_id`, `parent_id`,
`kind`, `name`, `status`, `started_at_ms`, `ended_at_ms`, token counts, and
`error`. The `kind` vocabulary is OpenTelemetry GenAI's `invoke_agent`,
`chat`, and `execute_tool`. IDs come from session/turn sequence and harness
tool call IDs. A root turn owns its child runs and spans.

Tier 2 adds `content(id, input, output)` for prompt, tool arguments,
and result text. It is disabled by default and can be enabled in local
`observability.json` with `{"capture_content":true}`. Content is size capped
before enqueue; the queue discards tier 2 first when under pressure. Tier 1
does not depend on content rows. The UI makes missing content explicit.

Cost is calculated from a bundled, reviewed USD-per-million-token table, with
separate input, output, cache read, and cache write prices. Ollama has no
API price, so its runs are `unpriced` unless `SOVEREIGN_PRICE_TABLE` lists the
model. An unknown model has `NULL` cost and appears as `unpriced`, never `$0`. Prices are versioned with the binary, so historic cost
is the estimate recorded at the time of each call; a later catalog change
does not silently rewrite history.

For local Ollama runs, the desktop separately shows a Grok 4.7 xHigh
equivalent estimate using the user-provided rates of $2 per million input
tokens and $6 per million output tokens. Cache-read tokens are already part of
the input count and are not added twice. This comparison is not an API charge.

## Writer and retention

SQLite uses WAL, `synchronous=FULL`, one writer, 4 KiB pages, and transactions of at most 128
events or 100 ms. `busy_timeout` is confined to the writer. The chat path does
one bounded `try_send`; no `await`, SQLite call, network call, or model call is
added. A separate read connection serves UI queries concurrently with WAL
writes. A measured 1,000-turn run with one model call per turn
used 2,793,176 bytes including indexes and SQLite pages; enqueue cost
was 0.9 microseconds per event in the release benchmark. A 1,024-event
queue is bounded well under the 5 MB idle RAM budget. The writer reports a
dropped tier-1 counter if even the reserved queue
fills, so a disk failure cannot be mistaken for complete history. We do not
promise impossible lossless capture under an indefinitely stalled disk while
also bounding memory and refusing to block the agent.

Tier-2 content is removed after 7 days. Detailed tool and model spans are
removed after 30 days, while per-run tier-1 summaries remain indefinitely.
Pruning occurs in the writer on startup and daily, never on the chat path.
On startup, queued and running runs become `interrupted`. Spawned children are
first reconciled from their persisted session transcripts, then any unfinished
children become `interrupted`. The writer refreshes child transcripts while
running. SQLite auto-checkpoints WAL; the writer also checkpoints on clean
shutdown. A hard kill can lose events still in the queue, so the database is
authoritative only for committed records.

## Source and tradeoffs

The data model, span/run semantics and dashboard views come from EveStack's
own code, read from a checkout (`packages/dashboard/sql/*.sql`,
`packages/dashboard/lib/{facts,alerts,fleet,queries}.ts`, `docs/observability.mdx`),
not from its README. Storage stays Akira's single local SQLite file
(`sovereign.db`): no Postgres, no Docker, no extra resident process. The
[OpenTelemetry GenAI conventions](https://opentelemetry.io/docs/specs/semconv/registry/attributes/gen-ai/)
provide span kinds (`invoke_agent`, `chat`, `execute_tool`) and the
`gen_ai.*` attribute names stored in `spans.attributes`. Restate's and
Morling's write-ups informed the single-writer, side-effect-free design;
Jcode's `jcode-harness-api` already emits tool, token-usage and turn-boundary
events, which the gateway consumes.

## EveStack mapping

SQLite has no schemas, so the tables carry EveStack's names without the
`evestack.` prefix (databases from before this used `obs_*` names and are
renamed by schema migration 3, `sovereign-prime/src/migrate.rs`). Timestamps are epoch milliseconds (`*_ms`) instead of `timestamptz`;
JSON is `TEXT`. The DDL beyond the shared tables lives in
`crates/sovereign-gateway/src/observability/schema.rs` and is idempotent.

| EveStack | Akira (`sovereign.db`) | Difference |
| --- | --- | --- |
| `fact_turn` | `fact_turn`, with its own column names (`id`, `parent_id`, `started_at_ms`, ...); `run_id`, `run_type`, `duration_ms`, `priced` are derived when a run is read | EveStack rebuilds facts from `workflow_runs` on a watermark (`refresh_facts`); Akira writes the row at RunEnd, so there is no refresh pass and no watermark. |
| `fact_turn.run_type` (`turn`/`subagent`) | `parent_id IS NULL` | Derived, not stored. |
| `fact_turn.session_id` | `session_id` | Same. A subagent has its own session; `root_id` links it to the turn that spawned it. |
| `fact_turn.trigger` | `kind = 'cron'` is `schedule`, otherwise `desktop` | EveStack's slack/http/webhook triggers do not exist in Akira. |
| `fact_turn.environment` | none | Akira has one local environment. |
| `fact_turn.model`, `provider` | `model`, `provider` | Same. |
| `created_at`, `started_at`, `completed_at`, `duration_ms` | `started_at_ms`, `ended_at_ms`, `duration_ms` derived | No `created_at`: a queued turn is stored with `status='queued'` and its start is the enqueue time, so time-to-first-token for a queued turn includes the wait. |
| `ttft_ms` | `ttft_ms` | Measured at the first `message.delta`. `time_per_output_chunk_ms` is not recorded. |
| `output_tokens_per_second` | not stored | Derivable from `output_tokens` and `duration_ms`. |
| `input_tokens` (total, includes cache), `output_tokens`, `cache_read_tokens`, `cache_write_tokens` | same names | Same meaning. |
| `priced` (true/false/NULL) | derived `priced` | 1 when every model call had a catalog price, 0 when any did not, NULL with no model call. |
| `cost_usd` and the four `cost_*_usd` parts | `cost_usd` | NULL, never 0, when unpriced (`unpriced_calls > 0`). The four component costs are not stored; per-call components are recomputable from the token columns and the price table. |
| `step_count`, `tools_called` | counted from `spans` when a run is read (`step_count`, `tools_called`) | Not stored, so they cannot disagree with the spans. |
| `retry_count`, `tools_offered`, `finish_reason`, `error_code` | none | Not available from jcode's event stream. `error` is kept. |
| `outcome` (`ok`, `failed`, `no_model_call`, `cancelled`, `budget_stopped`, `wedged`, `running`) | `outcome`, same vocabulary | `cancelled` is a turn ended by a stop request. `wedged` is either a run still `running` an hour after it started (evestack's `STUCK_TURN_MS`, applied when read so it is exact) or one the engine died in the middle of (marked at startup). `budget_stopped` is never written: nothing stops a turn for spend, the daily budget is advisory. |
| `span_coverage` (`none`/`partial`/`full`) | `span_coverage` | `partial` once a tool span landed, `full` once a model-call span landed. |
| `fact_tool_call` | view `fact_tool_call` over `spans` where `kind='execute_tool'` | `ok` is tri-state like EveStack's: 1 done, 0 errored, NULL unjudged. `arguments_bytes` and `result_bytes` are present only when content capture is on. |
| `fact_watermark`, `schema_version`, `schema_fingerprint` | none | No external writer to reconcile with, so no watermark. Additive changes are made idempotently at open. |
| `spans` | `spans` | `trace_id` is `run_id` (a run is a trace); `span_id` is `id`; `name`, `kind`, `parent_id`, status (`complete`/`error`/`running`/`interrupted`), token and cost columns, `attributes` as JSON with `gen_ai.*` keys. No OTLP `resource`, `events` or `scope_*` columns: nothing is received over OTLP. |
| `spans.resolved_session_id` / `resolved_turn_id`, `resolve_span_ancestry` | not needed | EveStack must infer ownership from partially attributed OTLP spans. Akira writes `run_id` and `root_id` on every span at emission. |
| `spans` prompt and result content | `span_content(id, input, output)` | Off by default, as before. |
| `approvals` | `approvals` | `decided_at`=`at_ms`, `turn_id`=`run_id`, `tool_name`=`tool`, `option_id`=`decision`, `approver`=`actor`, `approver_via`, `request_kind` (always `tool-approval`), `command_preview` in place of `answer_text`. No `request_id`, `remote_addr` or `user_agent`: the approver is the local desktop session. |
| `memory_deletions` | `memory_deletions`, filled by an `AFTER DELETE` trigger on `memories` | Catches every deletion path with nothing added to the memory code or the chat path. `actor` is NULL and `actor_via` is `unidentified` (the trigger cannot know the caller), which EveStack also records rather than inventing. No `session_id`, `created_at`. |
| `alert_state` | `alert_state` (`monitor_key`=`id`, `message`=`detail`) | `since` is `updated_at_ms` (rows are only rewritten on a transition). No `notified_state` per sink or `delivery_error`: delivery is a fire-and-forget webhook and the desktop event. |
| `alert_deliveries`, `alert_lease` | none | Single process, so no lease; webhook delivery is not recorded. |
| Alert ids `turn_failure_rate`, `turn_latency_p95`, `daily_spend`, `unpriced_spend`, `wedged` | same ids | Thresholds come from Akira's `observability.json`, not EveStack's environment variables. `turn_failure_rate` divides by finished turns; with none it is `not_checked`, not 0%. Akira adds `silent_failures`. Not ported: `no_spans_while_active` (Akira writes its spans in-process, so a missing-span gap cannot happen the way it does over OTLP), `sandbox_networked`, `sandbox_long_lived`, `schedule_failing` (no sandboxes; cron failures already show as failed runs). |
| `query-indexes.sql` (`evestack_runs_type_created_idx`, `_root_idx`, `_parent_idx`) | `fact_turn_kind_recent`, `fact_turn_root`, `fact_turn_parent` (partial, `IS NOT NULL`) | Verified with `EXPLAIN QUERY PLAN` in `hot_reads_plan_on_their_indexes`, together with the recent-list, session, outcome, wedged-scan, span-by-run, span-by-name, approvals and memory-audit reads. |
| `traces.sql` indexes (`spans_trace_idx`, `spans_parent_idx`, `spans_name_idx`) | `spans_run`, `spans_name` | `spans_session_idx` is not needed: a span reaches its session through `run_id`. |
| Dashboard: sessions list | `GET /api/sovereign/observability/sessions` | One row per session with turns, subagents, failed/cancelled/wedged counts, tokens, cost, last outcome and trigger. The LIMIT is applied to session ids first. Subagent tokens are shown on their own runs, not rolled into the parent session. |
| Dashboard: session tree, trace/span view | `GET .../runs?session=`, `GET .../run?id=` | Runs list (with parent/child grouping) and the run's span timeline. |
| Dashboard: monitors / fleet banner | `GET .../monitors`, `GET .../alerts` | Adds `wedged_count`, `unpriced_turns`, `finished_runs`. |
| Dashboard: facts (turn outcomes by model, tool failure rate and latency) | `GET .../facts?days=` | Failure rate divides by judged calls only (`ok IS NOT NULL`). |
| Dashboard: costs | `GET .../budget`, `GET /api/analytics/usage` | Unchanged; unpriced stays `unpriced`. |
| Dashboard: approvals, memory audit | `GET .../approvals`, `GET .../memory-audit` | |
| Ingest contract (`contract/`, OTLP `POST /v1/traces`) | none | EveStack ingests spans from a separate agent process over OTLP. Akira emits in-process, so there is no wire to validate and no partial-success handling. OTLP export remains an explicit opt-in that is not built. |
| Not ported | Evals, skills, sandboxes, schedules, channel pages and the chat view of the EveStack dashboard | Not observability; Hermes has its own pages for these. |

The desktop Activity pane renders these views with its existing components:
Runs (outcome, trigger, model, provider, model and tool call counts, first
token, span timeline, a banner for wedged runs), Sessions, Approvals, Alerts,
Facts and Memory (the audit). It polls only the tab that is open.

SQLite gives local durability and indexed timeline queries without a server,
but it has one writer. Batching and WAL absorb this workload; a genuinely
multi-writer or remote deployment would need a different store. OTLP export is
deferred: adding network egress by default would violate Sovereign's local
privacy rule. If offered later, it must be an explicit opt-in.

The system-design diagnostic scores this local design 6/10 (five of eight
checks). Requirements, measured storage and latency, a single-writer scaling
ceiling, queued writes, and failure counters are covered. Replicas, a read
cache, and a rolling deployment plan are absent because this is one local
desktop process with a small, indexed read view. A remote multi-user version
would need replicated storage, measured read caching, and a staged rollout.
