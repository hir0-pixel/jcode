# Sovereign packaging handoff

Last updated: 2026-09-24

## Objective

ONE installable app for macOS and Windows that a non-developer can install on a fresh machine with nothing else installed, then prove with numbers that it is more efficient than stock Hermes.

## Step 0 — commits (done)

### hermes-agent `feature/sovereign-observability`

- Packaging, e2e, Windows NSIS workflow, Ollama unload-on-quit, path helpers for win-unpacked.

Left unstaged (user theme/marketplace work): do **not** commit `vscode-marketplace*` or `themes/install*` deletions.

### sovereign-engine `feature/sovereign-observability`

- Bundled Python env + idle stop, engine-owned cron timer, Ollama alias warm (`num_ctx` pin, `keep_alive -1` / unload `0`), gateway startup stubs so Python is not woken by boot probes.

## Cron scheduler (verified)

The engine owns cron timing (`crates/sovereign-gateway/src/cron_tick.rs`): it reads `next_run_at` from Hermes's `jobs.json`, sleeps until the earliest one (no polling; re-read on any cron API call, at most every 10 min), then starts Python for one `POST /api/cron/tick` (Hermes's own `cron.scheduler.tick`, which runs the model turn through `/api/agent/run`, records the run and delivers). The backend runs no cron ticker of its own when `SOVEREIGN_ENGINE_URL` is set and idle-stop reclaims it afterwards. The packaged cron-due proof (`apps/desktop/release/sovereign-cron-due/`) predates this change and has not been re-run.

## Step 1 — macOS arm64 (verified)

Unsigned `Hermes-0.17.6-mac-arm64.{dmg,zip}`; Cron idle; due-wake; orphan cleanup; Ollama chat + approval; API-key mode 600.

## Step 2 — Windows x64 + macOS x64

- NSIS workflow builds unsigned installer; smoke steps for launch/orphan/apikey (+ optional chat if Ollama present).
- Fix (2026-09-24): gateway no longer wakes Python for unknown `/api/*` probes without `feature=1` (Windows smoke had failed with “Python started before a feature opened”).
- macOS x64: `.github/workflows/sovereign-macos-x64.yml` (Intel runner).

## Step 3 — Benchmark (done)

See `docs/BENCHMARK.md`:

- Ollama RSS idle/chat at 16k vs 32k → default **32768**
- Warm-up alias keeps CONTEXT **32768** across 3 `/v1` turns (no reload)
- Keep-alive **-1** while open / **0** on quit (not `"2h"`)
- Tool-schema prefix **~8 200 tokens** as its own line item
- Counting proxy vs stock Hermes: `docs/benchmark-runs/2026-09-24T12-45-25/` (`scripts/sovereign-vs-hermes-bench.mjs`)

## Step 4 — Docs

- [INSTALL.md](../../hermes-agent/INSTALL.md) (end-user install)
- This HANDOFF

## Re-verify warm-up quickly

```bash
ALIAS=sovereign/qwen3.8-27b:latest
curl -s http://127.0.0.1:11434/api/generate -d "{\"model\":\"$ALIAS\",\"prompt\":\".\",\"stream\":false,\"keep_alive\":-1}" >/dev/null
for i in 1 2 3; do
  curl -s http://127.0.0.1:11434/v1/chat/completions -H 'Content-Type: application/json' \
    -d "{\"model\":\"$ALIAS\",\"messages\":[{\"role\":\"user\",\"content\":\"t$i\"}],\"max_tokens\":4}" >/dev/null
  ollama ps   # CONTEXT must stay 32768
done
```
