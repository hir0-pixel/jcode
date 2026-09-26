# Sovereign cleanup inventory

Status: source deletion batches 1–5 recorded; batch 6 disk cleanup completed. Phase 3 found two environment/product gate failures recorded below. Remaining uncertain candidates stay retained.

## Baseline (2026-09-26)

Both repositories were clean before work. Engine HEAD: `295a74fce` on
`feature/sovereign-observability`; Hermes HEAD: `63a41ed` on the same branch.

| Area / measurement | Before |
| --- | ---: |
| Rust tracked source, workspace crates | 681,352 lines / 1,200 files |
| Rust tracked source, root engine `src/` | 32,144 lines / 52 files |
| `jcode-tui` | 214,658 lines |
| `jcode-app-core` | 143,155 lines |
| `jcode-base` | 121,099 lines |
| `sovereign-gateway` | 7,390 lines |
| `sovereign-prime` | 4,061 lines |
| Hermes desktop tracked JS/TS source | 644,305 lines / 2,899 files (uniform git-blob count) |
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

After measurements before final disk clean (2026-09-26):

| Area / measurement | After current source batches |
| --- | ---: |
| Rust workspace crates | 672,889 lines / 83 crates (same tracked `.rs` blob method as baseline) |
| Root engine `src/` | 30,174 lines / 49 files |
| Hermes desktop JS/TS | 644,308 lines / 2,899 files |
| Hermes shared JS/TS | 11,150 lines / 40 files |
| Hermes Python agent / gateway / CLI / cron | 125,052 / 93,801 / 235,787 / 16,969 lines (unchanged) |
| `target/release/sovereign` | 110 MB |
| Engine repo / shared `target` | 75 GB / 75 GB before final clean |
| Hermes repo | 4.7 GB before release-output cleanup |
| Live plain-task tool schema | 7,744 Sovereign; 10,664 Hermes tokens/call (unchanged) |
| Engine idle / post-session / peak RSS | 36,912 / 45,680 / 48,016 KiB (36.1 / 44.6 / 46.9 MiB) |
| Python idle / post-session / peak RSS | 161,648 / 252,640 / 279,936 KiB (157.9 / 246.7 / 273.4 MiB) |
| Plain-task correctness | Both products passed 2/2 turns |
| Final clean build / pack time and post-clean disk | Fresh release build passed (about 8 minutes); pack passed (about 25 seconds); engine repo / shared target 4.9 / 4.4 GB, Hermes repo 2.9 GB |

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
| Rust-only desktop REST calls | Desktop also calls `/api/learning/{graph,node}` and `/api/sovereign/observability/{monitors,budget,runs,approvals,run,promote,replay}`. These are absent from `docs/PARITY.md` because the parity generator inventories Hermes route parity; they're served directly by `crates/sovereign-gateway/src/learning_rest.rs` plus dispatch arms in `crates/sovereign-gateway/src/lib.rs` (there is no separate observability module). |
| Batch 5 docs gate | `docs/M6_CODEX_PROMPT.md` records a completed M6 observability task; `docs/MEMORY_DESIGN.md` and `docs/OBSERVABILITY.md` document the landed system. `docs/BENCH_CODEX_PROMPT.md` and the active cleanup prompt are retained. Workspace check, gateway/prime tests, release build and desktop TypeScript gates all passed (no new warnings). |
| Batch 5 benchmark-output gate | Workspace check: pass; gateway + prime: pass, 85 passed / 2 ignored; release build: pass; desktop TypeScript: pass. Six older benchmark directories removed; newest `m10-verify-90d2b213b` retained by timestamp. |

The first focused Cargo test attempt with temporary `HOME` could not see the dependency cache
and failed while trying GitHub; retry it with existing `CARGO_HOME` and temporary test homes. Other Cargo commands explicitly use
the required shared `CARGO_TARGET_DIR`. The live
benchmark used temporary homes under `/private/tmp` and did not read or write
real `.jcode` or `.hermes` homes. RSS was sampled by the benchmark's `ps -axo`
process-tree measurement while each temporary backend ran. Raw benchmark files
are under `/private/tmp/sov-cleanup-baseline`.

## Current full-gate evidence (Phase 3)

| Gate | Result |
| --- | --- |
| `cargo check --workspace --tests` | Pass after latest source batches; 11 baseline warnings, no new warnings. |
| `cargo test -p sovereign-gateway -p sovereign-prime` | Pass: gateway 39 passed / 1 ignored, contract 5 passed, Prime 33 passed and REPL 8 passed (85 passed / 2 ignored total). |
| `cargo build --release --bin sovereign` | Pass; latest binary 110 MB. |
| Desktop `npx tsc --noEmit -p .` | Pass, no TypeScript errors. |
| Full `cargo test --workspace` | Failed in `jcode` lib: 241 passed / 29 failed. Failures include macOS `system-configuration` panic `Attempted to create a NULL object.` and local socket bind `Operation not permitted`; Cargo stopped at `-p jcode --lib`, so not every workspace crate test ran. Test homes were `/private/tmp/sov-cleanup-test-*`; real homes were untouched. |
| Gateway live e2e | Pass 7/7: `sessions.mjs`, `learning.mjs`, `refine.mjs`, `agent-loop.mjs`, `agent-run.mjs`, `accounting.mjs`, `replay.mjs`; local model `sovereign/bench-hermes-64k:latest`. |
| Hermes Python `serve` | Pass: staged Python 3.12 server started on loopback with isolated `HOME`, `HERMES_HOME`, `JCODE_HOME`; one authenticated forwarded JSON-RPC `agents.list` returned `result.processes=[]`. |
| Forwarded Python REST spot-check | 20 namespaces sampled. 19 returned HTTP 200; `actions` returned the expected 404 for a nonexistent action name, confirming the route handler responds. Requests were local, read-only except browser's deliberately unknown action (no browser action executed). |
| Packaged desktop e2e | `sovereign-install-launch.mjs` pass (one Sovereign, Python starts for Cron and stops after closing); `sovereign-packaged-chat-approval.mjs` pass (authorized local model, approval writes only inside temp sandbox). |
| Packaged cron-due e2e | Fail: the one-shot agent cron executed and completed, but bundled Python remained running beyond the configured 180-second hold cap. The e2e now waits through that configured cap; the failure remains. |
| Python staging | Pass using the already-present managed 3.12.12 runtime and package cache copied to `/private/tmp`; no download. The initial default staging attempt stopped at uv cache permission before installation. |
| Desktop pack | Pass: current `release/mac-arm64/Hermes.app` produced. |
| `python3 scripts/parity.py` | Pass; generated map still reports 235 RPC methods (51 Rust, 19 placeholders, 165 Python-forwarded) and 264 Hermes HTTP routes (28 Rust, 8 refused, 228 forwarded). Rust-only learning/observability desktop paths are separately inventoried above. |
| Phase 4 stale worktrees | `git worktree list` showed only each primary checkout; no stale managed worktrees remained to remove. |
| Phase 4 release/build outputs | Removed old e2e outputs (`sovereign-chat`, `sovereign-install`, `sovereign-orphan`, `sovereign-apikey`, `sovereign-cron-due`), old ZIP/DMG/blockmap and Windows package outputs; retained the newly packed `release/mac-arm64/Hermes.app` (902 MB). Removed generated desktop `build/` and `dist/` after packing. Removed `/private/tmp/evestack` (13 MB), `/private/tmp/prime-agent` (32 MB), temporary copied Python packages (114 MB), and benchmark raw temp directories. |
| Phase 4 target cleanup | `cargo clean` removed 244,390 files / 77.7 GiB from the required shared target. One fresh release build passed; final target is 4.4 GB. Net target reduction from the pre-clean 75 GB measurement is about 70.6 GB; `du` rounds values. |
| Phase 4 final disk | Engine repo 4.9 GB (target 4.4 GB); Hermes repo 2.9 GB. Previous measurements were 75 GB and 4.7 GB respectively. Desktop release folder now contains only the current packaged app. |

## Candidate register

Evidence convention: `rg` results are literal-reference checks, not proof of
reachability by themselves. The graph is indexed as `sovereign-engine` and
`Users-rameelmalik-Documents-Sovereign-AI-hermes-agent`; structural candidates
need targeted caller/callee traces and coverage checks before a delete verdict.
Committed delete verdicts record completed batches. “Keep: unsure” means retain until the
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
| Rust-only learning and observability REST handlers | Learning graph/node and Sovereign Activity history routes called by desktop | `apps/desktop/src/api/skills.ts` calls `/api/learning/graph` and `/api/learning/node`; run-history modules call the seven `/api/sovereign/observability/*` endpoints. `sovereign-gateway/src/lib.rs` dispatches learning paths to `learning_rest`; observability REST routes are matched directly in `lib.rs`. The generated parity document omits these Rust-only routes, so they are tracked here explicitly. | keep: needed by desktop | 4 |
| Python `AIAgent` and agent internals | Python chat/agent implementation, also used by cron run fallback and web agent routes | User requires cron's AIAgent fallback when the engine env is absent; the Python `/api/agent/run` path is included in product routes. Search `cron/`, `gateway/`, and `hermes_cli/` shows current AIAgent imports and live route wiring. A file-by-file reachability cut is not yet proven. | keep AIAgent fallback; agent internals keep: unsure | 4 |
| Python duplicate memory, learning, skills and tracing implementations | Legacy chat-side features vs forwarded APIs and Python fallback | The current API parity map forwards learning graph/node and skill-hub routes to Python; Rust has its own learning handlers. Need route-level ownership and import closure before deleting shared modules. | keep: unsure | 4 |
| Hermes desktop code for disabled model tools | UI calls and routes that might be associated with removed engine tool IDs | No matches for the disabled tool identifiers in engine tool registry after batch 1. Desktop scan hits Gmail as an external connector and SMTP/IMAP help text; these are unrelated to the removed Gmail API model tool. No desktop files have been removed. | keep Gmail connector; tool-specific UI: none found | 1 |
| Hermes CLI/TUI | Standalone front ends | `hermes serve` remains a product root; no command/package entrypoint audit completed. | keep: unsure | 3 |
| `docs/M6_CODEX_PROMPT.md` | Completed request prompt for the M6 observability implementation | The M6 objective is already described as implemented in `docs/MEMORY_DESIGN.md` and `docs/OBSERVABILITY.md`; the prompt is historical work instruction, not current system documentation. Deleted. | delete | 5 |
| `docs/CLEANUP_CODEX_PROMPT.md`, `docs/BENCH_CODEX_PROMPT.md` | Active cleanup prompt and expressly retained benchmark prompt | The cleanup instructions remain active through this work; BENCH prompt is an explicit keep. | keep | 5 |
| `docs/benchmark-runs/m10-verify-90d2b213b` | Newest archived benchmark run | Directory mtime `2026-09-26 03:10:55`, later than every other direct child; retained as the newest archived run. | keep | 5 |
| `docs/benchmark-runs/m10-verify-276139934` | Older archived benchmark run | Directory mtime `2026-09-26 03:01:17`, 9m38s earlier than retained newest run. Deleted as requested. | delete | 5 |
| `docs/benchmark-runs/2026-09-24-memory-fix` | Older archived benchmark run | Directory mtime `2026-09-24 21:36:45`, predates retained newest run. Deleted as requested. | delete | 5 |
| `docs/benchmark-runs/2026-09-24T16-32-18` | Older archived benchmark run | Directory mtime `2026-09-24 21:43:55`, predates retained newest run. Deleted as requested. | delete | 5 |
| `docs/benchmark-runs/2026-09-24T13-53-35` | Older archived benchmark run | Directory mtime `2026-09-24 21:26:42`, predates retained newest run. Deleted as requested. | delete | 5 |
| `docs/benchmark-runs/2026-09-24T12-45-25` | Older archived benchmark run | Directory mtime `2026-09-24 17:49:13`, predates retained newest run. Deleted as requested. | delete | 5 |
| `docs/benchmark-runs/2026-09-24T12-41-44` | Older archived benchmark run | Directory mtime `2026-09-24 17:43:31`, predates retained newest run. Deleted as requested. | delete | 5 |
| `docs/plans/MCP_SKILLS_PLAN.md` | Completed dynamic skills/MCP implementation proposal with stale “No MCP support” status | Current `crates/jcode-app-core/src/mcp/` and `mcp_tools` implementation plus `server.rs` MCP pool lifecycle implement MCP support; the plan states the opposite and is not current system documentation. Delete the stale plan. | delete | 5 |
| Other `docs/plans/**`, other `docs/*_PROMPT.md` | Plans and remaining task prompts | Multiple plans are active proposals or preserved implementation history; `MEMORY_GRAPH_PLAN.md` reflects an architecture concern. Keep pending per-file status/evidence audit. `CLEANUP_CODEX_PROMPT.md` is active; `BENCH_CODEX_PROMPT.md` is explicit keep. | keep: unsure per-file audit needed | 5 |
| Engine `target/**` | Generated Rust build output | Required shared target was cleaned after source gates; 244,390 files / 77.7 GiB removed, then one fresh release binary built. | retain current build output; cleanup complete | 6 |
| `hermes-agent/apps/desktop/release/**`, old stage/build output | Current packaged app plus generated/test outputs | Enumerated direct release children. Kept only current `release/mac-arm64/Hermes.app`; removed stale e2e folders, archives, installer outputs and regenerated `build/` and `dist/` after pack. | delete stale outputs; keep current app | 6 |
| `/private/tmp/hermes-baseline`, `/private/tmp/hermes-tip`, `/tmp/sov-bench`, `/tmp/prime-agent`, `/tmp/evestack`, other stale clones/worktrees | Temporary clones, outputs and worktrees | `git worktree list` showed only each repository’s primary checkout. `/private/tmp/prime-agent` (32 MB), `/private/tmp/evestack` (13 MB), benchmark raw temp directories and copied package cache (114 MB) were removed. No `hermes-baseline`, `hermes-tip`, or `sov-bench` existed at those paths. | delete identified stale outputs; no stale worktrees found | 6 |

## Next proof required

1. Remaining self-update/install/self-dev/pairing/daemon flows and workspace
   crate closures still need candidate-level proof; retain them until that
   audit and compiler trial are complete.
2. The full Hermes desktop RPC/REST inventory and Python import closure are
   incomplete; retain route code and duplicate Python modules until those are
   mapped to product callers.
3. Full workspace tests and packaged cron-idle e2e remain failed as recorded in
   Phase 3; investigate those failures before calling the cleanup fully green.

## Tracked Rust source lines by workspace crate

Counts use every tracked `.rs` blob in the baseline commit and current HEAD, grouped by `crates/<package>/`; the `root package` row contains non-`crates/` Rust. This is the full workspace comparison.

| Crate/package | Before | After |
| --- | ---: | ---: |
| jcode-agent-runtime | 283 | 283 |
| jcode-ambient-types | 32 | 32 |
| jcode-app-core | 143155 | 136033 |
| jcode-auth-types | 180 | 180 |
| jcode-azure-auth | 8 | 8 |
| jcode-background-types | 201 | 201 |
| jcode-base | 121099 | 119763 |
| jcode-batch-types | 37 | 37 |
| jcode-build-meta | 457 | 457 |
| jcode-build-support | 3371 | 3371 |
| jcode-command-risk | 2542 | 2542 |
| jcode-compaction-core | 1042 | 1042 |
| jcode-config-types | 2608 | 2608 |
| jcode-core | 2216 | 2216 |
| jcode-embedding | 702 | 702 |
| jcode-fuzzy | 833 | 833 |
| jcode-gateway-types | 19 | 19 |
| jcode-harness-api | 3166 | 3166 |
| jcode-harness-api-server | 8573 | 8573 |
| jcode-import-core | 2833 | 2833 |
| jcode-logging | 1265 | 1265 |
| jcode-memory-types | 1975 | 1975 |
| jcode-message-types | 1010 | 1010 |
| jcode-notify-email | 529 | 529 |
| jcode-overnight-core | 1471 | 1471 |
| jcode-pdf | 51 | 51 |
| jcode-plan | 5806 | 5806 |
| jcode-productivity-core | 1736 | 1736 |
| jcode-protocol | 5624 | 5624 |
| jcode-provider-anthropic | 1649 | 1649 |
| jcode-provider-anthropic-runtime | 5452 | 5452 |
| jcode-provider-antigravity | 580 | 580 |
| jcode-provider-antigravity-runtime | 1622 | 1622 |
| jcode-provider-bedrock | 1937 | 1937 |
| jcode-provider-copilot | 313 | 313 |
| jcode-provider-copilot-runtime | 1957 | 1957 |
| jcode-provider-core | 8245 | 8245 |
| jcode-provider-cursor-runtime | 1303 | 1303 |
| jcode-provider-doctor | 6785 | 6785 |
| jcode-provider-env | 408 | 408 |
| jcode-provider-gemini | 804 | 804 |
| jcode-provider-gemini-runtime | 2997 | 2997 |
| jcode-provider-grok-build-runtime | 369 | 369 |
| jcode-provider-metadata | 2107 | 2107 |
| jcode-provider-openai | 3018 | 3018 |
| jcode-provider-openai-runtime | 10488 | 10488 |
| jcode-provider-openrouter | 2872 | 2872 |
| jcode-provider-openrouter-runtime | 9272 | 9272 |
| jcode-render-core | 4532 | 4532 |
| jcode-schema-dialect | 2657 | 2657 |
| jcode-sdk | 9258 | 9258 |
| jcode-selfdev-types | 281 | 281 |
| jcode-session-types | 1117 | 1117 |
| jcode-setup-hints | 11132 | 11132 |
| jcode-side-panel-types | 102 | 102 |
| jcode-storage | 1340 | 1340 |
| jcode-swarm-core | 846 | 846 |
| jcode-task-types | 853 | 853 |
| jcode-terminal-image | 759 | 759 |
| jcode-terminal-launch | 1730 | 1730 |
| jcode-tool-core | 330 | 330 |
| jcode-tool-types | 151 | 151 |
| jcode-transport | 589 | 589 |
| jcode-tui | 214658 | 214653 |
| jcode-tui-account-picker | 1607 | 1607 |
| jcode-tui-anim | 1131 | 1131 |
| jcode-tui-core | 3241 | 3241 |
| jcode-tui-markdown | 9847 | 9847 |
| jcode-tui-mermaid | 11452 | 11452 |
| jcode-tui-messages | 1139 | 1139 |
| jcode-tui-permissions | 866 | 866 |
| jcode-tui-render | 5590 | 5590 |
| jcode-tui-session-picker | 295 | 295 |
| jcode-tui-style | 5283 | 5283 |
| jcode-tui-tool-display | 255 | 255 |
| jcode-tui-usage-overlay | 928 | 928 |
| jcode-tui-visual-debug | 857 | 857 |
| jcode-tui-workspace | 1228 | 1228 |
| jcode-update-core | 622 | 622 |
| jcode-usage-types | 223 | 223 |
| root package | 40947 | 38977 |
| sovereign-gateway | 7390 | 7390 |
| sovereign-prime | 4061 | 4061 |
