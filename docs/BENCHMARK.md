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

## Efficiency vs stock Hermes (full run, 2026-09-24)

Supersedes the earlier single-prompt comparison (one `pong` prompt, once, through Hermes's
one-shot `-z` mode, totals only).

### Method

- **Same client for both.** `scripts/sovereign-vs-hermes-bench.mjs` starts `hermes serve`
  (stock Hermes, `~/.local/bin/hermes`, defaults: 25 tools, memory on, titles and background
  review as shipped) and `sovereign serve`, each in a throwaway HOME, and drives both over the
  desktop WebSocket protocol exactly like the desktop: `session.create`, `prompt.submit` per
  turn, approvals answered "once".
- **One meter.** Every model call goes through `scripts/sovereign-counting-proxy.mjs`, which
  logs one line per call (`calls.jsonl`): prompt tokens, KV-cache hits, completion tokens,
  tool-schema size, system-prompt hash, and a purpose label read from the request.
  Ollama's `prompt_tokens` is the full prompt (what an API bills);
  `prompt_tokens_details.cached_tokens` is the part served from cache (verified: an identical
  5,196-token request reports 5,196 both times, cached 0 then 5,192).
- **Same model and context** for both: `qwen3.8:27b` as alias `sovereign/bench-hermes-64k`
  (num_ctx 65,536, Hermes's minimum), loaded once before the run.
- **Six tasks, 3 runs each**, a fresh sandbox repo per run, product order alternated per run:
  plain question (2 turns), read a file (2), edit code (3), shell command (2), memory (a
  preference stated in one session, asked in a new one), and an 11-turn session (Hermes
  reviews memory every 10 user turns and skills every 10 tool steps by default, so short
  sessions never show that cost).
- **Background calls counted:** each backend stays up 120 s idle after every session.
- Raw data: `docs/benchmark-runs/2026-09-24T13-53-35/` (`calls.jsonl`, `turns.jsonl`,
  `rss.jsonl`, `sessions.jsonl`). Tables below: `node scripts/sovereign-bench-summarize.mjs <dir>`.

### Results

Model `sovereign/bench-hermes-64k:latest` (num_ctx 65536), 3 runs, idle window 120s after each session. Values are median (min-max) per task across runs.

"Prompt tokens" is the full input sent (what a paid API bills). "Processed" subtracts tokens served from Ollama's KV cache (what the local machine computes).

#### Model calls

| Task | hermes | sovereign | Sovereign vs Hermes |
|---|---:|---:|---:|
| plain | 3 (3-3) | 2 (2-2) | -33% |
| read-file | 4 (4-4) | 3 (3-3) | -25% |
| edit-code | 12 (10-14) | 6 (5-8) | -50% |
| shell | 4 (4-4) | 3 (3-3) | -25% |
| memory | 5 (5-5) | 3 (3-3) | -40% |
| long-session | 17 (16-17) | 15 (14-15) | -12% |
| **all tasks (sum of medians)** | **45** | **32** | **-29%** |

#### Prompt tokens

| Task | hermes | sovereign | Sovereign vs Hermes |
|---|---:|---:|---:|
| plain | 29,222 (29,222-29,232) | 17,277 (17,275-17,279) | -41% |
| read-file | 44,044 (44,040-44,053) | 26,254 (26,228-26,274) | -40% |
| edit-code | 175,769 (138,300-206,956) | 55,519 (45,392-73,940) | -68% |
| shell | 44,091 (44,088-44,408) | 26,444 (26,354-26,520) | -40% |
| memory | 44,140 (44,134-44,155) | 26,057 (26,045-26,067) | -41% |
| long-session | 243,352 (228,691-244,073) | 138,193 (128,680-138,266) | -43% |
| **all tasks (sum of medians)** | **580,618** | **289,744** | **-50%** |

#### Processed prompt tokens

| Task | hermes | sovereign | Sovereign vs Hermes |
|---|---:|---:|---:|
| plain | 14,764 (14,764-14,769) | 1,050 (1,049-8,658) | -93% |
| read-file | 14,967 (14,964-14,968) | 1,254 (1,245-1,263) | -92% |
| edit-code | 17,497 (16,303-19,074) | 2,165 (1,827-2,211) | -88% |
| shell | 14,998 (14,991-15,160) | 1,373 (1,316-1,396) | -91% |
| memory | 16,244 (16,238-16,249) | 2,216 (2,204-2,225) | -86% |
| long-session | 18,260 (18,086-18,350) | 2,118 (2,057-2,158) | -88% |
| **all tasks (sum of medians)** | **96,730** | **10,176** | **-89%** |

#### Completion tokens

| Task | hermes | sovereign | Sovereign vs Hermes |
|---|---:|---:|---:|
| plain | 119 (69-125) | 92 (86-144) | -23% |
| read-file | 217 (165-231) | 230 (192-237) | 6% |
| edit-code | 1,256 (636-1,291) | 466 (403-571) | -63% |
| shell | 263 (183-320) | 240 (229-396) | -9% |
| memory | 259 (257-527) | 196 (196-228) | -24% |
| long-session | 1,253 (1,243-2,102) | 973 (646-1,217) | -22% |
| **all tasks (sum of medians)** | **3,367** | **2,197** | **-35%** |

#### Wall time of turns (ms)

| Task | hermes | sovereign | Sovereign vs Hermes |
|---|---:|---:|---:|
| plain | 129,186 (122,872-129,441) | 16,855 (15,593-74,239) | -87% |
| read-file | 137,969 (135,424-139,643) | 25,425 (24,401-25,661) | -82% |
| edit-code | 213,176 (186,056-285,444) | 54,714 (41,570-67,803) | -74% |
| shell | 144,803 (139,601-155,658) | 28,603 (26,395-42,160) | -80% |
| memory | 150,407 (144,631-181,885) | 35,395 (33,635-35,644) | -76% |
| long-session | 295,663 (264,396-422,203) | 102,411 (71,304-107,973) | -65% |
| **all tasks (sum of medians)** | **1,071,204** | **263,403** | **-75%** |

#### Dummy API cost per task (USD; input $3/M, cached input $0.3/M, output $15/M)

Ollama treated as a paid API. Cached input uses Ollama's KV-cache hits as a stand-in for provider prompt caching, whose rules differ (minimum prefix, time-to-live, explicit breakpoints on some providers).

| Task | hermes | sovereign | Sovereign vs Hermes |
|---|---:|---:|---:|
| plain | $0.0504 | $0.0094 | -81% |
| read-file | $0.0569 | $0.0147 | -74% |
| edit-code | $0.1231 | $0.0295 | -76% |
| shell | $0.0576 | $0.0153 | -73% |
| memory | $0.0610 | $0.0168 | -72% |
| long-session | $0.1416 | $0.0619 | -56% |
| **all tasks** | **$0.4905** | **$0.1476** | **-70%** |

Same prices with no prompt caching at all (every input token at $3/M):

| hermes | sovereign | Sovereign vs Hermes |
|---:|---:|---:|
| $1.7924 | $0.9022 | -50% |

#### Calls by purpose (all runs)

| Purpose | hermes calls | hermes prompt tokens | hermes dummy cost | sovereign calls | sovereign prompt tokens | sovereign dummy cost |
|---|---:|---:|---:|---:|---:|---:|
| main turn | 66 | 985,623 | $1.0936 | 66 | 595,686 | $0.3309 |
| memory/skill review | 6 | 102,812 | $0.0894 | 0 | 0 | $0.0000 |
| title | 21 | 5,685 | $0.0206 | 0 | 0 | $0.0000 |
| tool follow-up | 41 | 627,850 | $0.2555 | 30 | 272,378 | $0.1327 |

#### Fixed prefix per main-turn call

| Product | Tools | Tool schema (est. tokens) | System prompt (bytes) | First-turn prompt tokens |
|---|---:|---:|---:|---:|
| hermes | 25 | 10,664 | 16,505 | 14,466 |
| sovereign | 22 | 7,544 | 1,121 | 8,627 |

#### RAM (MB, median across all sessions)

| Product | Backend idle before chat | Backend after session | Backend peak | of which Python (peak) | Ollama peak |
|---|---:|---:|---:|---:|---:|
| hermes | 173 | 253 | 278 | 278 | 30,244 |
| sovereign | 34 | 43 | 45 | 0 | 30,097 |

#### Correctness

| Product | Failed turns | edit-code file check passed | memory task recalled preference | memory written to disk |
|---|---:|---:|---:|---:|
| hermes | 0 | 3/3 | 3/3 | 3/3 |
| sovereign | 0 | 3/3 | 0/3 | 3/3 |

Calls with unknown token counts: 0 of 230.

### What explains the gap

1. **Smaller fixed prefix.** Hermes sends a 16.5 KB system prompt and 25 tool schemas
   (~10.7k tokens) on every call; Sovereign sends 1.1 KB and 22 tools (~7.5k tokens).
   First-turn prompt: 14,466 vs 8,627 tokens.
2. **A cacheable prefix.** Hermes embeds the session's start time in its system prompt
   (`agent/system_prompt.py`), so every new session has a different prefix: 21 distinct system
   prompts in 21 sessions, 0% cache hit on each session's first call. Sovereign used one
   identical system prompt for all 66 main calls: 88% cache hit even on a new session's first
   call. On an API with prompt caching, Hermes pays the full prefix price at every session
   start; on this local run it is also most of the wall-time gap (-75%).
3. **Fewer calls.** Every Sovereign call was a user turn or a tool follow-up. Hermes added
   21 title calls (5.7k tokens) and 6 memory/skill reviews (103k tokens, 6% of its total), and
   needed more tool round-trips on the edit task (12 vs 6 calls).

### Caveats

- One model (a local 27B) on one machine. The shipped app uses a cloud API model; token and
  call counts carry over, wall time does not (API prefill is fast).
- Sovereign **failed the memory task 0/3** in this run: it saved the preference but never
  recalled it. Root cause and fix: commit 6fcaa26 (recall was gated on the remote Jev
  service, which the sovereign build disables, and ran one turn late).
  **Re-run with the fix** (`docs/benchmark-runs/2026-09-24T16-32-18/`, Sovereign only, same
  task, 3 runs): **recalled 3/3** ("Nim" in a brand-new session), still 3 calls per run and
  ~26k prompt tokens per run, i.e. no extra cost.
- A low-priority `cargo check` ran 15:02-15:06 UTC and overlapped `hermes|edit-code|run2`;
  tokens are unaffected, that run's wall time may be slightly inflated.
- The old single-prompt result (Sovereign 4 calls vs Hermes 2) came from `sovereign run`, a
  different code path; it did not reproduce through `serve` (the desktop path), where
  Sovereign made no calls other than turns and tool follow-ups.

## How to re-measure Ollama RSS

```bash
# unload
curl -s http://127.0.0.1:11434/api/generate -d '{"model":"qwen3.8:27b","keep_alive":0}'
# load at CTX
curl -s http://127.0.0.1:11434/api/generate -d '{"model":"qwen3.8:27b","prompt":".","stream":false,"keep_alive":"10m","options":{"num_ctx":32768}}'
# RSS: sum rss for Ollama / ollama / llama-server
ps -axo rss,comm,args | …
```
