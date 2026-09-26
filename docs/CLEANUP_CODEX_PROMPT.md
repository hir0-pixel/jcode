CONTEXT
"Sovereign" is a desktop AI agent built by merging four projects:
- Hermes Agent (Nous Research): the Electron desktop app plus a Python backend.
- jcode: a Rust coding agent, forked here as the chat engine.
- Prime Agent (Prime Intellect): its self-learning ideas were REIMPLEMENTED in Rust (crates/sovereign-prime). No Prime code was copied in.
- EveStack: its observability ideas were REIMPLEMENTED in Rust (runs/spans in sovereign.db). No EveStack code was copied in.

The shipped product is ONE thing: the Hermes desktop app, driving the Rust engine binary `sovereign`, with Hermes's Python backend started on demand for non-chat features (cron scheduling and delivery, messaging/bots, the browser driver, and the other forwarded routes listed in docs/PARITY.md).

Everything that isn't part of that product is junk: jcode's standalone terminal UI, CLI and installers; switched-off tools; duplicate implementations; dead Python; stale docs; leftover build output. Your job is to remove it, safely and with proof, so there is one lean build.

REPOS
- Engine: /Users/rameelmalik/Documents/Sovereign AI/sovereign-engine (branch feature/sovereign-observability).
- Hermes: /Users/rameelmalik/Documents/Sovereign AI/hermes-agent (branch feature/sovereign-observability).
- Read first: engine docs/MEMORY_DESIGN.md, docs/PARITY.md, docs/BENCHMARK.md, src/bin/sovereign.rs, src/cli/dispatch.rs, crates/sovereign-gateway/src/*.rs; hermes-agent apps/desktop/electron/main.ts (how the desktop starts the engine and Python) and apps/desktop/DESIGN.md.

RULES
- Both working trees must be clean before you start. If `git status` shows uncommitted changes in either repo, STOP and report; don't stash, commit or discard someone else's work.
- Delete, never disable. No feature flags, no commented-out code, no "unused" env switches left behind.
- Never delete anything you cannot PROVE is unreachable from the product (definition below). If unsure, keep it and list it under "kept: unsure" with the reason.
- The desktop's look is owned by apps/desktop/DESIGN.md (squircles, the chosen colors, exactly two themes). Don't change styles, themes or colors.
- For EVERY cargo command, set CARGO_TARGET_DIR="/Users/rameelmalik/Documents/Sovereign AI/sovereign-engine/target". Disk is tight; never create another target dir.
- Tests use temporary HOME/JCODE_HOME/HERMES_HOME. Never touch the real ~/.jcode or ~/.hermes.
- Live tests use local Ollama only (model sovereign/bench-hermes-64k:latest). No paid APIs, no downloads.
- One commit per deletion batch, with the configured git identity, each message ending with "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>". Never push. Command line only.

THE PRODUCT (reachability roots; everything reachable from these is KEPT)
1. The `sovereign` binary: every subcommand and code path the desktop or the engine itself invokes (serve/gateway, the pre-tool approval hook, the REPL worker, anything electron/main.ts or features.rs spawns). Trace it from src/bin/sovereign.rs and electron/main.ts; don't assume.
2. The model-visible tool set on the gateway path (tools not in JCODE_DISABLED_TOOLS), plus everything those tools call.
3. Every JSON-RPC method and REST route the desktop calls (grep apps/desktop/src and apps/shared/src), in both the Rust gateway and the forwarded Python routes.
4. Hermes Python code reachable from `hermes serve` (or however the desktop starts the backend) for the forwarded features, including cron (scheduling, delivery, and the AIAgent fallback when the engine env is absent), messaging/bots, and the browser tool route.
5. Tests, e2e scripts and bench scripts that exercise the above, plus docs that describe the current system.

CANDIDATES TO INVESTIGATE (each needs proof before deletion)
- jcode's standalone terminal UI (crates/jcode-tui) and anything only it uses. The root crate may link it (check src/lib.rs); if so, unlink it and remove it only if the product doesn't need it. Refactoring the root crate to drop the TUI is in scope.
- jcode's own CLI subcommands, self-update/install channels (scripts/install*.sh/.ps1, builds/stable/canary logic), self-dev builds, the desktop-pairing and browser-extension bridge, and daemon/shared-server install flows. Keep only what `sovereign` actually uses.
- Tools permanently disabled for Hermes chats: integration_tools, gmail, maintainer_feedback, compile_remote, macos_computer_use, panel, side_panel, schedule, jcode_docs. Delete their code, config and tests. (swarm: the compact `delegate` tool uses swarm internals; keep what `delegate` needs and delete the rest.)
- Any other jcode crates, providers, examples, bins, benches or features not reachable from the roots. Check every crate in the workspace Cargo.toml.
- Hermes Python features replaced by the engine and no longer reachable: e.g. the Python chat-agent paths the desktop no longer uses, and duplicate learning/memory/skills/tracing code. Keep the AIAgent pieces cron's fallback or bots still use.
- Hermes desktop code for removed features.
- Hermes CLI/TUI parts not used by the desktop. Note that `hermes serve` itself is a root.
- Stale docs describing removed systems (e.g. docs/plans/*), old prompts in docs/*_PROMPT.md once their work is done (keep docs/BENCH_CODEX_PROMPT.md), and old docs/benchmark-runs/* folders (keep the newest).
- Build junk:
  - /private/tmp/hermes-baseline, /private/tmp/hermes-tip, /tmp/sov-bench, /tmp/prime-agent, /tmp/evestack and any other stale git worktrees (`git worktree list` in both repos; remove them with `git worktree remove` and prune);
  - hermes-agent/apps/desktop/release/* test outputs, except the current packaged app;
  - old build/stage folders;
  - finally `cargo clean` of the shared target dir, then ONE fresh release build.

METHOD
Phase 0, baseline (before deleting anything). Record:
- lines of code per crate/package;
- `sovereign` release binary size;
- idle RSS of the engine and of Python;
- disk usage of both repos plus the target dir;
- the per-call tool-schema tokens (from `node scripts/sovereign-vs-hermes-bench.mjs` "plain" task, 1 run);
- the full gate results (below).

Phase 1, inventory. Write docs/CLEANUP_INVENTORY.md with one row per candidate:
path | what it is | reachability evidence (who references it, or "no references" with the grep/cargo commands used) | verdict (delete / keep / keep: unsure) | batch number.
Group the deletions into batches by area (e.g. 1: disabled tools, 2: TUI, 3: CLI/install, 4: Python, 5: docs, 6: build junk). Commit the inventory.

Phase 2, delete in batches. For each batch: delete, fix references, then run the FAST gates:
- `cargo check --workspace --tests` (no errors, no new warnings);
- `cargo test -p sovereign-gateway -p sovereign-prime`;
- `cargo build --release --bin sovereign`;
- python3 -m py_compile on the touched Python files;
- `npx tsc --noEmit -p .` in hermes-agent/apps/desktop.
If a gate fails and the fix isn't obvious, restore that item, mark it "keep: needed by X" in the inventory, and continue. Commit each batch.

Phase 3, full gates (after all batches):
- cargo test for every remaining crate (all green; run any known-flaky suite twice);
- live e2e in crates/sovereign-gateway/e2e: sessions, learning, refine, agent-loop, agent-run, accounting, replay;
- desktop, in hermes-agent/apps/desktop:
  - npm run stage:sovereign-python (it bundles git HEAD, so commit Python changes first)
  - npm run pack
  - node e2e/sovereign-install-launch.mjs
  - node e2e/sovereign-packaged-chat-approval.mjs
  - SOVEREIGN_CRON_AGENT=1 node e2e/sovereign-packaged-cron-due.mjs
- `hermes serve` starts, and every forwarded route in docs/PARITY.md still answers (spot-check one per namespace);
- rerun `python3 scripts/parity.py` and confirm that nothing the desktop calls became unserved.

Phase 4, disk: remove the stale worktrees, clones and test outputs, run `cargo clean`, then one release build and one `npm run pack`. Report the space freed.

REPORT
- A before/after table: lines of code per area, binary size, idle RSS, disk used, tool-schema tokens per call, build time.
- The list of batches with what was removed.
- Everything kept as "unsure", with reasons.
- Gate results with counts, and the final SHAs in both repos.
