# Sovereign engine: quick start

The Hermes desktop, running on this jcode fork instead of the Python backend.
Everything stays on the machine except calls to the model provider.

## Run

```bash
cargo build --release --bin sovereign
scripts/sovereign-desktop.sh
```

The script starts the engine with a private token, then opens the unmodified
Hermes desktop in remote-gateway mode pointed at it. Quitting the app stops the
engine. Engine state lives in `~/.sovereign` (sessions, memories, learned
instructions); the log is `~/.sovereign/engine.log`.

Default model: local Ollama `qwen3.8:27b`. To use ChatGPT/Codex instead:

```bash
JCODE_HOME=~/.sovereign target/release/sovereign login openai
SOVEREIGN_PROVIDER=openai scripts/sovereign-desktop.sh
```

## What is different from Hermes

- **Engine:** jcode's Rust agent loop, one process (about 45 MB with a chat open,
  against 115 MB for Hermes's Python backend before any chat).
- **Prime REPL:** the model has a `repl` tool, a sandboxed Python subset
  (Pydantic Monty) in a memory-capped worker. `load(path)` puts a workspace
  file into a variable instead of the transcript; `llm_query(prompt)` asks a
  focused sub-question.
- **Continual Harness:** `/refine` learns one small, evidence-checked
  improvement from the current session; `/refine rollback` undoes it;
  `/harness` shows what has been learned. Applies to new sessions.
- **Local memory:** recall is local keyword relevance; the remote Jev service
  and jcode's usage telemetry are disabled.

## Not yet supported

Methods the engine does not implement answer `not_supported_by_engine`; the
first use of each is logged to `engine.log` as `sovereign-gateway: unsupported`.
Messaging gateways, cron, profiles, kanban, pet, voice, the skills hub and the
observability views are later modules (see `SOVEREIGN_PLAN.md`).

## Tests

```bash
cargo test --release -p sovereign-gateway -p sovereign-prime
node crates/sovereign-gateway/e2e/live.mjs          # needs Ollama; E2E_MEMORY=1 adds the memory check
```
