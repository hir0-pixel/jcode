You are running public benchmarks overnight, unattended, comparing stock Hermes Agent with Sovereign (a Hermes fork on a Rust engine with Prime-style self-learning, SQLite memory and full tracing). The owner is asleep. Nobody will answer questions, so make sensible decisions, write them down, and keep going. Honesty matters more than a good-looking result: report every run, cherry-pick nothing, and never change a task, a checker or a model setting to favour one arm.

REPOS AND ISOLATION (read first)
- Engine: /Users/rameelmalik/Documents/Sovereign AI/sovereign-engine. Other work may be uncommitted there. Don't build in that folder, and don't edit, stage or reset anything in it.
  - Create a worktree for all engine work: `git -C "<engine>" worktree add /tmp/sov-bench feature/sovereign-observability` (the current tip; record its SHA in the report).
  - Build there: `cargo build --release --bin sovereign`.
  - Run the bench scripts from the worktree.
- Hermes: /Users/rameelmalik/Documents/Sovereign AI/hermes-agent. It has about 146 uncommitted files belonging to the owner. Never stage, stash, reset, revert or edit anything there.
  - Stock Hermes is upstream `origin/main` = 67f7e1d. Create a worktree: `git -C "<hermes>" worktree add /tmp/hermes-stock 67f7e1d`.
  - Use hermes-agent's existing `.venv` Python to run it, with PYTHONPATH pointed at the worktree. If the venv doesn't match that commit's dependencies, report it and stop; don't pip install without cause.
- Aider polyglot exercises: /Users/rameelmalik/Documents/Sovereign AI/benchmarks/polyglot-benchmark (already downloaded).
- Results go under /tmp/sov-bench/bench-results/ and are copied at the end to <engine>/docs/benchmark-runs/<date>-public/. That's a new folder; leave it uncommitted for the owner to review.
- Never touch the real ~/.hermes or ~/.jcode. Every run uses throwaway HOME, HERMES_HOME and JCODE_HOME directories.

MODEL (same for both arms, no exceptions)
- Local Ollama at http://127.0.0.1:11434.
- Model: `sovereign/bench-hermes-64k:latest`. It's qwen3.8:27b with a 64k context. If it's missing, create it from `qwen3.8:27b` with num_ctx 65536 the way scripts/sovereign-vs-hermes-bench.mjs does.
- If Ollama isn't running, start it with `ollama serve` in the background. Don't pull other models.
- Same temperature and context for both arms; record them in the results.
- Prices: the dummy table from scripts/lib/bench-prices.mjs ($3 input, $0.30 cached, $15 output per million tokens).

READ FIRST (in the worktree)
- docs/BENCHMARK.md, especially the "Public benchmarks" section.
- scripts/bench/abeval.py, scripts/bench/polyglot.mjs and scripts/bench/polyglot-exercises.json.
- scripts/sovereign-counting-proxy.mjs.
- hermes-agent's evals/toolperf_abeval/README.md.

Start with `--dry-run` on both drivers. The scripts were written but have never been run against a model, so expect to fix bugs. Fix bugs in the DRIVERS only; never in a checker or a task. Make those fixes in the /tmp/sov-bench worktree, and list every fix in the report.

PHASE 0: smoke test (about 20 minutes)
- 1 abeval task × 1 rep × both arms, and 2 polyglot exercises × both arms.
- Confirm:
  - every model call shows up in the proxy log;
  - success checks run;
  - Sovereign's learning is on (default local-idle mode; the log line reads "sovereign: learning on");
  - tool calls and errors are captured for both arms.
- For Hermes's tool counts, abeval.py uses Hermes's own traces. If the NeMo Relay tracing isn't available, use Hermes's session DB instead and note it.
- Don't start phase 1 until the smoke test is clean.

PHASE 1: Hermes's own toolperf A/B eval (9 trap tasks)
- `python3 scripts/bench/abeval.py run --arm hermes --reps 3`, then the same with `--arm sovereign`. Interleave the arms per task if the driver allows it, so machine drift hits both equally.
- Then run `report`.

PHASE 2: Aider polyglot, 40 fixed exercises (python / rust / cpp)
- `node scripts/bench/polyglot.mjs run --arm hermes`, then `--arm sovereign`. One pass per arm, same order.
- Sovereign keeps one engine home for the whole pass so learning can carry over; Hermes keeps one HERMES_HOME likewise.
- Then run `report`.

TIME BUDGET
- Phases 1 and 2 are the priority. If they can't both finish tonight, finish phase 1 completely first, then as much of phase 2 as possible. Both drivers are resume-safe.
- Record elapsed time per phase.
- Keep the machine usable: one agent run at a time, never parallel model runs.

PHASE 3: GAIA (only after 1 and 2 are complete, and only if you can access it)
- GAIA is gated on Hugging Face. Proceed only if HF_TOKEN is set in the environment AND the dataset `gaia-benchmark/GAIA` downloads with it. Otherwise skip phase 3 and write exactly what the owner must do: accept the terms on the dataset page, then export HF_TOKEN.
- Use the 2023 validation split, level 1 only (it has answers), with the official quasi-exact-match scoring from the GAIA paper/leaderboard.
- Both arms need the SAME tools. GAIA needs web search and browsing plus file reading.
  - Check what each arm can do without paid API keys. Sovereign has a `browser` tool backed by Hermes's browser code (the local Chromium backend); Hermes has its web and browser tools.
  - If one arm lacks a capability the other has, don't run an unfair comparison. Report the gap instead.
  - Don't sign up for anything or use any paid API.
- Build the GAIA driver as scripts/bench/gaia.py, in the same style and with the same metrics as the other drivers.

REPORT (write to docs/benchmark-runs/<date>-public/REPORT.md, and print a summary)
- Setup: hardware (sysctl hw.model / memory), OS, model and settings, commit SHAs of both arms, exact commands, dates and durations.
- For each benchmark, a table with per-arm medians: success/pass rate; model calls; tool calls; tool errors; prompt, cached and completion tokens; dummy cost; wall time. Add per-task rows for abeval.
- A plain-language summary of where Sovereign was better, the same or worse, with no spin. If Sovereign lost on something, say so.
- For polyglot: whether Sovereign's learning produced anything (harness/log.jsonl entries) and whether later exercises did better than earlier ones for each arm (first half vs second half).
- Every driver fix you made, anything skipped and why, and any anomaly (timeouts, crashes, Ollama restarts).
- Raw data (proxy JSONL, meta.jsonl, per-run logs) goes alongside the report.

DON'TS
- Don't commit to either repo. Don't push. Don't open PRs.
- Don't edit hermes-agent or the owner's uncommitted work.
- No paid APIs, no account sign-ups, no downloads beyond what the benchmarks need (the GAIA dataset with the owner's token counts; npm/gradle installs don't).
- Don't delete the worktrees at the end; the owner may want to inspect them.
