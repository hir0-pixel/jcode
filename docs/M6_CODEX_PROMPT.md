# M6 — Observability on the store (Codex prompt)

You are working in two repos:

- Engine (Rust, jcode fork): `/Users/rameelmalik/Documents/Sovereign AI/sovereign-engine`, branch `feature/sovereign-observability`.
- Hermes desktop + Python: `/Users/rameelmalik/Documents/Sovereign AI/hermes-agent`. It has about 140 uncommitted files that belong to the user. Never stage, stash, revert, reset or reformat them. Stage only files you changed, by explicit path.

Read first:
- `docs/MEMORY_DESIGN.md` (store design, the M1–M5 status and the planned M6).
- `docs/BENCHMARK.md`.
- `crates/sovereign-gateway/src/observability.rs`.
- `crates/sovereign-gateway/src/lib.rs`: the observer wiring, the Activity REST routes and `/api/analytics/usage`.
- `crates/sovereign-gateway/src/learn.rs`.
- `crates/sovereign-gateway/src/rpc.rs`: `agent_run`, title generation and `on_harness_frame`.
- `crates/jcode-base/src/memory_store.rs`, which shows how `sovereign.db` is designed.

## Goal

Every model call the product makes is recorded as a trace with tokens and cost, and is visible in the desktop Activity view. That includes chat turns, tool follow-ups, titles, learning passes, cron `/api/agent/run` runs and anything else that calls a model. Tracing must stay local and cheap: no extra model calls and no noticeable RAM or latency cost.

## Current state

- `Observer` writes `runs`, `spans` and `content` to a separate `observability.sqlite3` in the jcode home.
- Every read (`list`, `analytics`, `insights`, `detail`) opens a new `Connection`.
- Chat turns are traced from harness frames.
- Learning passes (learn.rs), `agent_run` and title calls are probably not traced, or only partly. Verify this; don't assume it.
- Cost uses a per-model price table where one exists. Local Ollama is unpriced.

## Work

1. **Accounting first (write this test before changing anything).**
   - Add `crates/sovereign-gateway/e2e/accounting.mjs`. Run the engine behind `scripts/sovereign-counting-proxy.mjs`, which writes one JSONL line per upstream model call with a purpose label.
   - Drive these flows: a chat with a tool call, a title, a learning pass (`SOVEREIGN_LEARN_IDLE_MS=4000`, same approach as `e2e/learning.mjs`), and one `POST /api/agent/run`.
   - Assert that every proxy call has exactly one matching span in the store. Model name and token counts must equal what the proxy saw (Ollama's `prompt_tokens` is the full prompt; `cached_tokens` is reported separately).
   - Record the starting gap in the commit message.
2. **Close the gaps.** Every model call gets a span with a `kind`: `chat`, `tool_followup`, `title`, `learning`, `cron`, `memory` if any model call exists there, and `other`.
   - Learning spans attach to their own run: kind `learning`, linked by session.
   - Cron runs are recorded as runs with kind `cron` and the job title.
   - Memory recall is local FTS and makes no model call. Record it as a cheap non-model span (duration, hits, bytes injected) only if that costs under about 0.1 ms per turn; measure it.
3. **Move to the store.**
   - Put the tables in `sovereign.db`, the same WAL database as memory, under clear names (`obs_runs`, `obs_spans`, `obs_content`), created by the same migration mechanism (`memory_meta.schema_version` / `once()`).
   - Add a one-time import from an existing `observability.sqlite3`, then remove the old file path from the code.
   - Use one long-lived writer connection plus one reused read connection, not a new connection per request.
   - Batch span writes inside a transaction per turn.
   - Add indexes that match the real queries, and check them with `EXPLAIN QUERY PLAN`. Keep the existing index-driven correlated subquery for analytics; in earlier measurements it was 12–16x faster than a CTE.
   - Add retention: a configurable age, default 30 days, pruned by an idle-time job. Content capture stays opt-in, as today.
   - Measure with a unit bench: write cost per span, p50/p95 latency for list/analytics/detail on 50k spans, and RSS delta. Put the numbers in `docs/MEMORY_DESIGN.md`.
4. **OTel GenAI naming.** Store span attributes using the OpenTelemetry GenAI semantic conventions:
   - `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`;
   - `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, plus the cached-token attribute;
   - `gen_ai.conversation.id` for the session.

   Check the current spec text online before naming anything, and don't guess. Optional OTLP/HTTP export is allowed only behind an env var, off by default, with no new heavy dependency. Skip it if that isn't possible.
5. **Cost.**
   - One price table used everywhere: per model, input, cached input and output per million tokens.
   - Local models are priced by an optional config table. The bench uses dummy prices: $3 input, $0.30 cached, $15 output per million tokens (see `scripts/sovereign-bench-summarize.mjs`). That file and the engine must read the same source, not two copies.
   - Unpriced calls stay counted as `unpriced_calls` and are never shown as $0.
6. **Desktop Activity.**
   - Learning and cron runs appear in the existing Activity / run history (`apps/desktop/src/app/agents/run-history.tsx` and its API client), with kind, tokens and cost.
   - Keep the UI changes minimal and in the existing style. No new visual patterns.

## Constraints

- No overlaps: one owner per job. The engine owns tracing, and Hermes Python must not keep its own parallel trace store for these calls. If one exists, remove it properly (don't just switch it off) and make sure Hermes still launches.
- Nothing may break:
  - `cargo test -p sovereign-gateway -p sovereign-prime -p jcode-base`;
  - the live e2e scripts `e2e/sessions.mjs`, `e2e/learning.mjs`, `e2e/agent-run.mjs` and your new `e2e/accounting.mjs`;
  - in `hermes-agent/apps/desktop`: `npm run stage:sovereign-python`, `npm run pack`, then `node e2e/sovereign-install-launch.mjs`, `node e2e/sovereign-packaged-chat-approval.mjs`, and `SOVEREIGN_CRON_AGENT=1 node e2e/sovereign-packaged-cron-due.mjs`.

  Note that staging packages git HEAD, so commit Python changes before packing. Known pre-existing failures: 10 jcode-tui tests, some `auth::transfer` / `spawn_detached` / session tests. Confirm any other failure against the parent commit before calling it pre-existing.
- Tests use throwaway `HOME` and `JCODE_HOME` directories. Never touch the real `~/.hermes` or `~/.jcode`.
- Local Ollama only (`sovereign/bench-hermes-64k:latest`). No paid APIs. No downloads without asking.
- Don't leave dead code, duplicated helpers or switched-off features behind.
- Match the surrounding style.
- Commit in small steps with the configured git identity, ending each message with the project's usual co-author line.

## Done when

- `accounting.mjs` shows 100% of proxy calls matched to spans with equal tokens.
- Activity shows learning and cron runs.
- The bench numbers are in `docs/MEMORY_DESIGN.md`.
- All the checks above pass.

Report: the starting and final accounting gap, the bench numbers, files changed, and test results.
