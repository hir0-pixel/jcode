# Sovereign vs stock Hermes — baseline benchmark

Last updated: 2026-09-24  
Host: macOS arm64, Ollama app, model `qwen3.8:27b` (Q4_K_M, 27.3B).

## Ollama serving context (RSS)

Measured with `ps` RSS summed across Ollama.app + `ollama serve` + `llama-server` (the runner that holds weights + KV). Chat used native `/api/chat` with matching `options.num_ctx` so the runner did not reload between idle and chat.

| State | num_ctx 16 384 | num_ctx 32 768 | Δ (32k − 16k) |
| --- | ---: | ---: | ---: |
| Idle, model **unloaded** | 87 MB | 87 MB | — |
| Idle, model **loaded** | 19 397 MB (~18.9 GB) | 20 483 MB (~20.0 GB) | **+1.1 GB** |
| After one short chat turn | 19 709 MB (~19.2 GB) | 20 794 MB (~20.3 GB) | **+1.1 GB** |
| `/api/ps` `context_length` | 16 384 | 32 768 | — |
| `/api/ps` `size` (weights+KV report) | 18.19 GB | 18.36 GB | +0.17 GB |

**Default chosen: `SOVEREIGN_OLLAMA_NUM_CTX=32768`.**  
Weights dominate RAM (~19 GB). Doubling context from 16k→32k costs ~1.1 GB RSS. Sovereign’s tool-schema prefix alone is ~8k tokens (below), so 16k leaves only ~8k for dialogue/tool results and forces early compaction; 32k leaves ~24k headroom. The extra gigabyte is worth that headroom on this machine.

Override: set `SOVEREIGN_OLLAMA_NUM_CTX=16384` (minimum accepted by warm-up is 8192).

## Warm-up vs chat (no reload)

Root cause: Ollama’s OpenAI-compat `/v1/chat/completions` **ignores** per-request `num_ctx` and reloads the base model at its trained window (262 144 here), wiping a `num_ctx=32768` warm load.

**Fix (engine):** on serve, create local alias `sovereign/<model>:latest` with integer `PARAMETER num_ctx`, load it with `keep_alive: -1`, and use that alias for chat. Verified:

- After warm: `ollama ps` → `context_length=32768`
- After **3** `/v1/chat/completions` turns on the alias: still `32768` (no reload)
- `context_window()` after catalog refresh: **32768** (alias id includes `:latest` so cache hits)

## Keep-alive policy

| Phase | Behavior |
| --- | --- |
| App open / engine up | `keep_alive: -1` (stay loaded until explicit unload) |
| Engine exit (SIGTERM/SIGINT, Drop, parent death) | `keep_alive: 0` via warm-file unload |
| Electron `will-quit` | Best-effort unload of `sovereign-ollama-warm.json` model |

**Not used:** wall-clock TTLs such as `"2h"` (would leave the model resident after quit).

## Per-request token cost — tool-schema prefix

| Line item | Tokens (approx.) | Notes |
| --- | ---: | --- |
| **Tool-schema prefix** | **~8 200** | Dominant fixed cost every turn; target of lazy tool-loading |
| System / session reminder | ~400 | Date, cwd, version |
| Short user prompt (e2e) | ~150 | |
| **First-turn `prompt_tokens` (observed)** | **8 596** | Packaged Ollama chat e2e, tools enabled, before reply |

Source: packaged chat journal token_usage on the first model call (`apps/desktop/release/sovereign-chat/` runs). Tool schemas are ~95% of that prefix. Lazy tool-loading (MEMORY_DESIGN M5) should cut this line item.

## Efficiency vs stock Hermes (counting proxy)

Same prompt (`Reply with exactly one word: pong`), same machine, Ollama behind `scripts/sovereign-counting-proxy.mjs`.

Evidence: `docs/benchmark-runs/2026-09-24T12-45-25/summary.json`.

| Metric | Stock Hermes CLI (`-z`) | Sovereign (`run`) | Notes |
| --- | ---: | ---: | --- |
| Serving `num_ctx` | **65 536** (Hermes minimum) | **32 768** (product default) | Hermes Agent refuses windows &lt; 64k (`MINIMUM_CONTEXT_LENGTH`) |
| `/v1/chat/completions` calls | **2** | **4** | Sovereign oneshot still does internal turns (tools/session); Hermes answered in 2 |
| Prompt tokens (sum) | 12 560 | 13 422 | First-turn tool prefix still dominates; see line item above |
| Completion tokens (sum) | 20 | 153 | |
| Wall time | 100 s | 75 s | Cold-ish GPU; not the primary efficiency claim |
| Idle Python while Cron closed | Always on (desktop `hermes serve`) | Stopped after `SOVEREIGN_FEATURE_IDLE_MS` | Packaged e2e |
| Cron still fires after idle | N/A | Wake → fire → idle again | `sovereign-cron-due/` |
| Ollama after app quit | Depends on Ollama defaults | Unloaded (`keep_alive: 0`) | Engine Drop + Electron `will-quit` |

**Takeaway:** On a simple oneshot, call/token counts are in the same ballpark. The measured wins for Sovereign are (1) **half the KV budget Hermes requires** (32k vs ≥64k), (2) **no resident Python** until Cron/features, (3) **unload Ollama on quit**. Tool-schema prefix (~8.2k) remains the main per-request token cost and the lazy-tool-loading target.

Re-run:

```bash
# needs release sovereign + hermes on PATH + Ollama with qwen3.8:27b
node scripts/sovereign-vs-hermes-bench.mjs
```

## How to re-measure Ollama RSS

```bash
# unload
curl -s http://127.0.0.1:11434/api/generate -d '{"model":"qwen3.8:27b","keep_alive":0}'
# load at CTX
curl -s http://127.0.0.1:11434/api/generate -d '{"model":"qwen3.8:27b","prompt":".","stream":false,"keep_alive":"10m","options":{"num_ctx":32768}}'
# RSS: sum rss for Ollama / ollama / llama-server
ps -axo rss,comm,args | …
```
