# Sovereign cleanup inventory

Status: baseline and inventory in progress. No cleanup deletion has been made.

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
| Engine / Python idle RSS | unavailable: host process listing fails (`sysmond` unavailable) |
| Fresh live model benchmark | unavailable: local Ollama at `127.0.0.1:11434` is not running |

Baseline gates:

| Gate | Result |
| --- | --- |
| `cargo check --workspace --tests` | Pass, 327 crates, 0 errors, 11 warnings |
| `cargo test -p sovereign-gateway -p sovereign-prime` | Pass, 85 passed, 2 ignored (8 suites) |
| `cargo build --release --bin sovereign` | Fail before cleanup: two non-exhaustive matches in `crates/jcode-base/src/memory/activity.rs:276` and `crates/jcode-base/src/memory_log.rs:106` for `jcode_memory_types` sidecar variants |
| Desktop `npx tsc --noEmit -p .` | Pass, no errors |
| Current tool-schema benchmark | Not run: local Ollama/model unavailable; existing documentation value retained as reference only |

The first Cargo attempt with temporary `HOME` could not see the dependency cache
and retried GitHub; it was stopped. Subsequent Cargo commands explicitly used
the existing `CARGO_HOME` and the required shared `CARGO_TARGET_DIR`. The
temporary test home is `/private/tmp/sov-cleanup-home`; real `.jcode` and
`.hermes` homes were not used.

## Candidate register

Evidence convention: `rg` results are literal-reference checks, not proof of
reachability by themselves. The graph is indexed as `sovereign-engine` and
`Users-rameelmalik-Documents-Sovereign-AI-hermes-agent`; structural candidates
need targeted caller/callee traces and coverage checks before a delete verdict.
No candidate below has been deleted. “Keep: unsure” means retain until the
listed reachability audit is complete.

| Path / candidate | What it is | Reachability evidence / check | Verdict | Batch |
| --- | --- | --- | --- | ---: |
| `crates/jcode-tui` | Standalone terminal UI and broad re-export surface | `src/lib.rs` re-exports `jcode_tui::*`; root `Cargo.toml` depends on it; initial `rg -n 'jcode-tui' Cargo.toml src crates/sovereign-gateway crates/sovereign-prime` shows feature forwarding and UI callers. Must trace actual non-TUI modules before unlinking. | keep: unsure | 2 |
| `crates/jcode-tui-*` | TUI support crates | Workspace members are listed in root `Cargo.toml`; no dependency closure audit completed yet. | keep: unsure | 2 |
| `src/cli/**`, root CLI command definitions and subcommands | Hermes-compatible binary pass-through and inherited jcode CLI | `src/bin/sovereign.rs` translates `serve` to `gateway`, pre-tool and REPL-worker are explicit spawn roots; other args pass through. Desktop/electron reachability is not exhaustively traced. | keep: unsure | 3 |
| self-update/install, stable/canary, self-dev, pairing, browser-extension bridge, daemon/shared-server flows | Inherited install and development paths | No exhaustive symbol/reference and Electron spawn audit yet. | keep: unsure | 3 |
| `crates/jcode-app-core/src/tool/compile_remote.rs`, `crates/jcode-app-core/src/tool/side_panel.rs`, `crates/jcode-app-core/src/tool/panel.rs`, `crates/jcode-app-core/src/tool/jcode_docs.rs` | Tool implementations named as disabled candidates | Literal search found these source paths. `src/bin/sovereign.rs` defaults `JCODE_DISABLED_TOOLS` to `gmail,compile_remote,macos_computer_use,panel,side_panel,schedule,jcode_docs,swarm`; this alone does not prove there are no overrides or other callers. | keep: unsure | 1 |
| Integration tools, Gmail, maintainer feedback, macOS computer use, schedule tool definitions/config/tests | Other tool candidates named in the brief | Initial literal search found references in protocol/config/tool modules, tests, and desktop; no per-tool gateway/schema/desktop-path trace completed. `gmail` is in the engine default-disabled list. | keep: unsure | 1 |
| `crates/jcode-swarm-core` and swarm-only code | Swarm support, with `delegate` explicitly retained | `swarm` appears in `DEFAULT_DISABLED_TOOLS`; the requested compact `delegate` path may share internals. No call-graph closure audit yet. | keep: unsure | 1 |
| Other root `Cargo.toml` workspace crates, providers, bins, examples, benches and features | Inherited jcode workspace members | Full member list is in `Cargo.toml`; initial literal scan identified `src/bin/tui_bench.rs` and `src/bin/mermaid_side_panel_probe.rs`, but no complete workspace dependency/target audit exists. | keep: unsure | 2–3 |
| Hermes Python agent, memory/learning/skills/tracing duplicates | Python features potentially replaced by Rust engine | `docs/PARITY.md` says Python is still forwarded for non-chat routes; user brief additionally retains cron fallback, bots, browser and other forwarded routes. No Python import/route closure audit completed. | keep: unsure | 4 |
| Hermes desktop code for disabled features | Desktop routes, views, and controls potentially made unreachable by tool deletion | Need exact desktop RPC/REST call inventory and connection to Rust/Python routes before removing code. `apps/desktop/src` and `apps/shared/src` are the specified roots. | keep: unsure | 1 |
| Hermes CLI/TUI | Standalone front ends | `hermes serve` remains a product root; no command/package entrypoint audit completed. | keep: unsure | 3 |
| `docs/plans/**`, `docs/*_PROMPT.md` except `docs/BENCH_CODEX_PROMPT.md`, old `docs/benchmark-runs/**` | Potentially stale project notes and benchmark outputs | Current docs include `MEMORY_DESIGN.md`, `PARITY.md`, and `BENCHMARK.md`; newest-run selection and all current-system documentation were not audited. | keep: unsure | 5 |
| Engine `target/**` | Generated Rust build output | `du` reports 73 GB; `cargo clean` and a fresh release build are explicitly required only after source gates pass. Current release build fails at baseline. | keep: unsure until final clean/build | 6 |
| `hermes-agent/apps/desktop/release/**`, old stage/build output | Packaged/test output | Need identify and preserve the current packaged app; no release-directory inventory completed. | keep: unsure | 6 |
| `/private/tmp/hermes-baseline`, `/private/tmp/hermes-tip`, `/tmp/sov-bench`, `/tmp/prime-agent`, `/tmp/evestack`, other stale clones/worktrees | Temporary clones, outputs and worktrees | `git worktree list` showed only each repository’s primary checkout. No removable stale worktree was found. Files under `/private/tmp` were not removed; exact ownership/freshness audit remains. | keep: unsure | 6 |

## Next proof required

1. Finish candidate-level graph traces and `check_index_coverage` checks; use
   source searches for non-indexed files and literal configuration.
2. Expand this register to concrete paths, commands, and one verdict per
   candidate before deleting anything.
3. Resolve the pre-existing release-build failure and obtain local Ollama plus
   host process visibility before treating the requested live baseline as
   complete.
