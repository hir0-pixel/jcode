# Sovereign cleanup inventory

Status: baseline complete; batch 1 deletions complete. Inventory remains open for batches 2–6.

## Baseline (2026-09-26)

Both repositories were clean before work. Engine HEAD: `295a74fce` on
`feature/sovereign-observability`; Hermes HEAD: `63a41ed` on the same branch.

| Area / measurement | Before |
| --- | ---: |
| Rust tracked source, workspace crates | 681,352 lines / 1,200 files |
| Rust tracked source, root engine | 32,144 lines / 52 files |
| `jcode-tui` | 214,658 lines |
| `jcode-app-core` | 143,155 lines |
| `jcode-base` | 121,099 lines |
| `sovereign-gateway` | 7,390 lines |
| `sovereign-prime` | 4,061 lines |
| Hermes desktop tracked JS/TS source | 644,761 lines / 2,903 files |
| Hermes shared tracked JS/TS source | 11,150 lines / 40 files |
| Hermes Python agent / gateway / CLI / cron | 125,052 / 93,801 / 235,787 / 16,969 lines |
| Engine repository / shared `target` | 74 GB / 73 GB (rounded by `du`) |
| Hermes repository | 4.5 GB |
| Existing `target/release/sovereign` | 126.9 MB; pre-existing artifact, not rebuilt |
| Tool schema tokens per call | Latest checked-in benchmark: 7,744 (M10 verification; not freshly measured) |
| Engine idle RSS | 35.3 MiB before chat; 45.1 MiB after the plain session |
| Python idle RSS | 163.3 MiB before chat; 256.6 MiB after the plain session |
| Live plain-task tool-schema tokens / call | 7,744 Sovereign; 10,664 stock Hermes |
| Live plain-task first-turn prompt | 8,860 Sovereign; 14,462 stock Hermes |
| Live plain-task runs / correctness | 1 each; all 4 turns passed |
| Live model / task / idle interval | local `sovereign/bench-hermes-64k:latest`, `plain`, 20 seconds |

Baseline gates:

| Gate | Result |
| --- | --- |
| `cargo check --workspace --tests` | Pass, 327 crates, 0 errors, 11 warnings |
| `cargo test -p sovereign-gateway -p sovereign-prime` | Pass, 85 passed, 2 ignored (8 suites) |
| `cargo build --release --bin sovereign` | First attempt exposed stale shared-target artifacts from an earlier bisect. Per user direction, `cargo clean -p jcode-memory-types -p jcode-base -p jcode-protocol -p jcode-app-core -p jcode-tui --release` removed 28,168 files / 21.2 GB; release rebuild passed in 1m 06s. No source variants were re-added. |
| Desktop `npx tsc --noEmit -p .` | Pass, no errors |
| Current tool-schema benchmark | Completed; detailed raw data in `/private/tmp/sov-cleanup-baseline` (outside the repos). 0 unknown-token calls. |
| Batch 1 gates | Workspace check: pass, same 11 warnings; gateway + prime: pass, 85 passed / 2 ignored; release build: pass, 1m 55s after final source changes; desktop TypeScript: pass. Focused app-core/base unit suite: 1,021 passed / 226 failed / 6 ignored; first failure is macOS `system-configuration` panic `Attempted to create a NULL object.` in this sandbox, and many tests rely on that platform store. |
| Batch 2 gates | Workspace check: pass, same 11 warnings after removing one stale `dev-bins` cfg; gateway + prime: pass, 85 passed / 2 ignored; release build: pass in 35.5s; desktop TypeScript unchanged and passed in batch 1. |
| Batch 3 gates | Workspace check: pass, same 11 warnings; gateway + prime: pass, 85 passed / 2 ignored; release build: pass in 10.56s. |
| Route inventory baseline | `python3 scripts/parity.py`: 235 JSON-RPC methods (51 Rust, 19 placeholder, 165 forwarded); 264 Hermes HTTP routes (28 Rust, 8 refused, 228 forwarded). Desktop literal scan finds 706 `/api/` or RPC references in 173 files under `apps/desktop/src`; this includes tests and generated contracts, so it is a scan count, not a unique call count. |
| Batch 5 docs gate | `docs/M6_CODEX_PROMPT.md` records a completed M6 observability task; `docs/MEMORY_DESIGN.md` and `docs/OBSERVABILITY.md` document the landed system. `docs/BENCH_CODEX_PROMPT.md` and the active cleanup prompt are retained. Workspace check, gateway/prime tests, release build and desktop TypeScript gates all passed (no new warnings). |

The first focused Cargo test attempt with temporary `HOME` could not see the dependency cache
and failed while trying GitHub; retry it with existing `CARGO_HOME` and temporary test homes. Other Cargo commands explicitly use
the required shared `CARGO_TARGET_DIR`. The live
benchmark used temporary homes under `/private/tmp` and did not read or write
real `.jcode` or `.hermes` homes. RSS was sampled by the benchmark's `ps -axo`
process-tree measurement while each temporary backend ran. Raw benchmark files
are under `/private/tmp/sov-cleanup-baseline`.

## Candidate register

Evidence convention: `rg` results are literal-reference checks, not proof of
reachability by themselves. The graph is indexed as `sovereign-engine` and
`Users-rameelmalik-Documents-Sovereign-AI-hermes-agent`; structural candidates
need targeted caller/callee traces and coverage checks before a delete verdict.
No candidate below has been deleted. “Keep: unsure” means retain until the
listed reachability audit is complete.

| Path / candidate | What it is | Reachability evidence / check | Verdict | Batch |
| --- | --- | --- | --- | ---: |
| `crates/jcode-tui` | Shared presentation crate re-exported by root library | `cargo tree --offline -e normal --target aarch64-apple-darwin -p jcode -i jcode-tui` shows direct `jcode -> jcode-tui`; `src/lib.rs` re-exports it, and `src/cli/startup.rs::run_with_args` registers the TUI session-cache invalidator and keybind warning hook on `sovereign` startup. A full decoupling trial would require replacing that startup composition plus the jcode CLI exports; keeping it is supported by an actual product startup path, not a text-only reference. | keep: needed by current sovereign startup path | 2 |
| `crates/jcode-tui-*` | TUI support crates | Direct dependencies are listed in `crates/jcode-tui/Cargo.toml`; the package is a normal dependency of `jcode`, which is the engine process composition root used by `sovereign`. | keep: needed by jcode-tui | 2 |
| `src/cli/**`, root CLI command definitions and subcommands | Hermes-compatible launch and inherited jcode CLI | `apps/desktop/electron/backend-command.ts::serveBackendArgs` builds `serve --host ... --port ...`, `main.ts::resolveHermesBackend` launches packaged `sovereign` with those args; `src/bin/sovereign.rs` translates serve to gateway. `src/bin/sovereign.rs` itself calls `__pre-tool` and `__repl-worker`; desktop e2e `sovereign-packaged-apikey.mjs` directly calls `login openai-api`. The `dashboard --no-open` translation had no caller in Electron, shared code or product e2e; removed and now remains pass-through. The remaining CLI tree still contains gateway bootstrap and the API-key login exercised by product e2e. | keep gateway/login; delete dashboard alias | 3 |
| self-update/install, stable/canary, self-dev, pairing, browser-extension bridge, daemon/shared-server flows | Inherited install and development paths | No exhaustive symbol/reference and Electron spawn audit yet. | keep: unsure | 3 |
| `crates/jcode-app-core/src/tool/{compile_remote.rs,computer/**,gmail.rs,jcode_docs.rs,panel.rs,side_panel.rs}`; `crates/jcode-base/src/gmail.rs` | Permanently disabled model tools and implementations | Removed from `tool/mod.rs` registry and module declarations; no remaining registry constructor references (`rg -n 'compile_remote|macos_computer_use|"gmail"|"jcode_docs"|"panel"|"side_panel"' crates/jcode-app-core/src/tool/mod.rs src/bin/sovereign.rs`). `cargo check --workspace --tests` and `cargo build --release --bin sovereign` pass after trial removal. Product desktop tool inventory and current gateway schema do not request these IDs. | delete | 1 |
| ScheduleTool implementation/helpers/tests in `crates/jcode-app-core/src/tool/ambient.rs` | Inherited generic scheduled-task model tool | Removed only `ScheduleTool` and its helpers/tests. The registry no longer exposes it; `schedule_ambient` / `ScheduleAmbientTool` remains a separate ambient-session feature with registry reference in `tool/mod.rs`, so that code is retained. Workspace check and gateway/prime tests pass. | delete ScheduleTool; keep ambient scheduler | 1 |
| `jcode_docs` generated documentation corpus/build script and self-development exceptions | Internal model tool and build-time docs embedding | No product call sites; removed its registry, `build.rs` generator, direct validation guards and tests. `cargo check --workspace --tests` and release build pass. | delete | 1 |
| `DEFAULT_DISABLED_TOOLS` startup mutation in `src/bin/sovereign.rs` | Default suppression of removed tools | The disabled IDs are no longer registered; product tool schema baseline measured 7,744 tokens/call. Deleted startup mutation. | delete | 1 |
| Swarm model-visible registry entry and tool-only tests | Standalone swarm tool vs delegate internals | Removed registry entry and tool-name expectations. `delegate` uses the `CommunicateTool` implementation and its internal coordination path (`tool/delegate.rs` references `CommunicateTool`); retained communicator and swarm coordination internals needed by delegate. | delete standalone entry; keep delegate internals | 1 |
| Underlying side-panel state and goal-tool snapshots | UI state infrastructure shared with goal tools | `tool/goal.rs` calls `crate::side_panel::{snapshot_for_session,...}`; not the disabled side-panel model tool. Kept because reachable from goal tools. | keep: needed by goal | 1 |
| Desktop's removed-tool UI and endpoints | Views, controls, or endpoint calls for candidates above | Desktop RPC/REST call inventory is not complete yet; no desktop files changed in batch 1. | keep: unsure | 1 |
| `src/bin/tui_bench.rs`, `src/bin/tui_bench/side_panel.rs`, `src/bin/mermaid_side_panel_probe.rs`; root and TUI `dev-bins` feature | TUI-only benchmark and debug executables | Both bins require `dev-bins`; their references are only in their own Cargo targets, comments/docs and code-size/error budgets. Neither is spawned by `src/bin/sovereign.rs`, Electron `main.ts`, or engine runtime. Removed both targets, feature forwarding and their budget rows. | delete | 2 |
| `src/bin/test_api.rs`, `src/bin/harness.rs`, `jcode-harness` target | Provider smoke tool and harness fixture CLI | Referenced by smoke/test scripts, not Electron or the sovereign launch path; harness protocol crate remains a runtime dependency of sovereign gateway. Need separate check whether these scripts validate shipped product contracts before deletion. | keep: unsure | 2 |
| Other root `Cargo.toml` workspace crates, providers, examples, benches and features | Inherited jcode workspace members | `cargo tree -i` audit and trial removals remain incomplete; the normal dependency closure is large and platform-all offline tree is blocked by an uncached target-only package. | keep: unsure | 2–3 |
| Python server routes and their handlers | Config, provider/model settings, cron, bot/messaging, browser, files, skills, sessions, profiles, MCP, and system features forwarded by Rust | `python3 scripts/parity.py` generated `docs/PARITY.md`: 165 forwarded JSON-RPC methods and 228 forwarded HTTP routes. Desktop route call sites use `apps/desktop/src/api/**`; shared JSON-RPC request surface is in `apps/shared/src/json-rpc-channel.ts`. Keep route implementations until each namespace has an equivalent current handler or is proven uncalled. | keep: needed by forwarded desktop routes | 4 |
| Python `AIAgent` and agent internals | Python chat/agent implementation, also used by cron run fallback and web agent routes | User requires cron's AIAgent fallback when the engine env is absent; the Python `/api/agent/run` path is included in product routes. Search `cron/`, `gateway/`, and `hermes_cli/` shows current AIAgent imports and live route wiring. A file-by-file reachability cut is not yet proven. | keep AIAgent fallback; agent internals keep: unsure | 4 |
| Python duplicate memory, learning, skills and tracing implementations | Legacy chat-side features vs forwarded APIs and Python fallback | The current API parity map forwards learning graph/node and skill-hub routes to Python; Rust has its own learning handlers. Need route-level ownership and import closure before deleting shared modules. | keep: unsure | 4 |
| Hermes desktop code for disabled model tools | UI calls and routes that might be associated with removed engine tool IDs | No matches for the disabled tool identifiers in engine tool registry after batch 1. Desktop scan hits Gmail as an external connector and SMTP/IMAP help text; these are unrelated to the removed Gmail API model tool. No desktop files have been removed. | keep Gmail connector; tool-specific UI: none found | 1 |
| Hermes CLI/TUI | Standalone front ends | `hermes serve` remains a product root; no command/package entrypoint audit completed. | keep: unsure | 3 |
| `docs/M6_CODEX_PROMPT.md` | Completed request prompt for the M6 observability implementation | The M6 objective is already described as implemented in `docs/MEMORY_DESIGN.md` and `docs/OBSERVABILITY.md`; the prompt is historical work instruction, not current system documentation. Deleted. | delete | 5 |
| `docs/CLEANUP_CODEX_PROMPT.md`, `docs/BENCH_CODEX_PROMPT.md` | Active cleanup prompt and expressly retained benchmark prompt | The cleanup instructions remain active through this work; BENCH prompt is an explicit keep. | keep | 5 |
| `docs/plans/**`, other `docs/*_PROMPT.md`, old `docs/benchmark-runs/**` | Plans, completed task prompts and benchmark artifacts | Multiple plans are marked Proposal or not implemented; `MCP_SKILLS_PLAN.md` contains stale state claims, while `MEMORY_GRAPH_PLAN.md` reflects a real current architecture concern. Newest benchmark-run selection still needs timestamp verification. Do not bulk delete. | keep: unsure per-file audit needed | 5 |
| Engine `target/**` | Generated Rust build output | `du` reports 73 GB; `cargo clean` and a fresh release build are explicitly required only after source gates pass. Current release build fails at baseline. | keep: unsure until final clean/build | 6 |
| `hermes-agent/apps/desktop/release/**`, old stage/build output | Packaged/test output | Need identify and preserve the current packaged app; no release-directory inventory completed. | keep: unsure | 6 |
| `/private/tmp/hermes-baseline`, `/private/tmp/hermes-tip`, `/tmp/sov-bench`, `/tmp/prime-agent`, `/tmp/evestack`, other stale clones/worktrees | Temporary clones, outputs and worktrees | `git worktree list` showed only each repository’s primary checkout. No removable stale worktree was found. Files under `/private/tmp` were not removed; exact ownership/freshness audit remains. | keep: unsure | 6 |

## Next proof required

1. Finish candidate-level graph traces and `check_index_coverage` checks; use
   source searches for non-indexed files and literal configuration.
2. Expand this register to concrete paths, commands, and one verdict per
   candidate before each further deletion batch.
3. Audit desktop RPC/REST calls and Python forwarding before deleting either
   area.
