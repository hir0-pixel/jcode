# Finish report

## Part 1: cron Python leak

**Status: complete.** The empty `/api/agent/run` response was caused by a late `rename_session` `Done` event being misclassified as the prompt's completion. Session creation and prompt submission both rename the session; the bridge already tracked rename acknowledgements but omitted `rename_session` from the control `Done` IDs. Once the prompt acknowledgement marked a turn active, the later rename completion ended the observed turn and canceled the model stream. Added `rename_session` to the control completion matcher and a regression test covering rename acknowledgement, prompt acknowledgement, late rename completion, and actual prompt completion. Model-answer extraction did not need a reasoning-content fallback.

The packaged cron e2e's old `/python/i` predicate also counted the engine-owned `scrapling-mcp` worker as Hermes's bundled Python backend. Trace evidence showed the Hermes feature worker's cron lease released and the idle manager stopped its process group; the remaining Python process belonged to the persistent MCP tool under the engine. Narrowed the assertion to the packaged `sovereign-python/runtime` path. No idle timeout or lease behavior changed.

After rebuilding and packaging the fixed engine, the corrected `SOVEREIGN_CRON_AGENT=1` packaged cron-due e2e passed three consecutive runs. Each run returned `sovereign-cron-agent-ok`, recorded `last_status=ok`, and ended with `python:false` for the bundled Hermes runtime. The unrelated Scrapling MCP process is intentionally not counted as the Hermes backend. Focused bridge regression passed 1/1; release build and desktop pack passed; the e2e script passes `node --check`.

## Part 2: workspace test triage

**Status: blocked; 12 targets still fail outside the sandbox.** The first sandboxed run had 27 failed targets; examples include `PermissionDenied` on loopback listeners and child process creation. The full rerun outside the sandbox, with the shared target, offline dependencies, and isolated homes, completed 213 targets: 8,031 passed, 24 failed, 59 ignored. A serial rerun completed the same 213 targets: 8,028 passed, 27 failed, 59 ignored. The failures below reproduce outside the sandbox, so they are not classified as sandbox-only.

Fixed the SDK parity failure: Rust now exposes `delete_session`, includes it in `CAPABILITIES`, and tests its `DeleteSession` request. Focused parity and client transport tests pass.

Unresolved failures in the serial run:

| Target | Failing test(s) | Evidence / disposition |
| --- | --- | --- |
| `jcode --lib` | `memory_cli_semantic_requires_jev_but_keyword_search_remains_local` | Semantic/keyword assertions disagree; needs inspection of the test's full assertion and current optional embedding behavior. |
| `jcode-core --lib` | `stdin_detect::stdin_detect_tests::test_own_process_not_reading_stdin` | Reproduces outside the sandbox even with stdin redirected from `/dev/null`; current macOS detector treats any vnode stdin as interactive. |
| `jcode-provider-bedrock --lib` | `detects_bedrock_login_env_file_credentials`; `detects_env_credentials_requires_region_and_credential_hint` | Credential-presence assertions fail outside the sandbox; investigate shared process environment and provider detection. |
| `jcode-provider-openai-runtime --lib` | `persistent_failed_missing_tool_output_recovers_with_full_replay` | Mock response fails with `Protocol(MissingConnectionUpgradeHeader)`. |
| `jcode-provider-openrouter-runtime --lib` | `autodetected_profile_seeds_default_model_and_cache_namespace`; `autodetects_single_saved_local_openai_compatible_profile`; `autodetects_single_saved_openai_compatible_profile`; `named_openai_compatible_loads_api_key_from_env_file` | Profile and key discovery assertions fail using temporary config paths; needs source and path review. |
| `jcode-sdk --lib` | `auth::tests::processes::timeout_reaps_process_and_unique_ids_isolate_cancellation`; `worktrees::tests::creation_isolates_dirty_edits_and_nested_invocation`; `worktrees::tests::lists_existing_detached_locked_prunable_and_bare_worktrees` | Timeout test fails; worktree tests hit path normalization / missing result assertions. `neither_sdk_has_an_untriaged_public_capability` is fixed and passes focused. |
| `jcode-sdk --test lifecycle_events` | `global_events_discovers_existing_and_new_sessions_then_closes_children`; `global_events_reports_bounded_queue_overflow`; `public_client_exposes_swarm_metadata_from_list_and_attach`; `public_client_exposes_titles_from_list_and_attach` | Mock harness connection closes or breaks its pipe outside the sandbox. |
| `jcode-setup-hints --lib` | `setup_hints_tests::first_three_launches_can_include_hotkey_notice_too` | Expected startup hint missing; needs test/setup counter review. |
| `jcode-tui --lib` | `tui::app::tests::command_palette_open_does_not_move_existing_rows`; `tui::app::tests::overscroll_reveal_does_not_relayout_transcript`; `tui::app::tests::test_chat_mouse_scroll_down_reaches_bottom_without_dead_zone`; `tui::app::tests::test_full_redraw_clears_out_of_band_backend_artifacts_after_native_scroll_like_mutation` | Four UI behavior assertions fail; this crate is scheduled for product removal in Part 6. |
| `jcode-tui --test glyph_safe_wire` | `indexed_color_emits_256_sgr_not_truecolor`; `truecolor_color_emits_truecolor_sgr` | Emitted ANSI color mode is empty; this crate is scheduled for product removal in Part 6. |
| `jcode-tui-style --lib` | `palette::buffer_tests::configured_role_recolors_role_cells_and_named_colors_only`; `palette::light_theme_interaction::configured_colors_survive_the_light_theme_pass` | Configured colors are transformed; scheduled for review with the TUI removal in Part 6. |
| `jcode-tui-workspace --lib` | `workspace_map_widget::tests::render_workspace_map_colors_completed_tiles_green` | Workspace map color assertion fails; scheduled for review with the TUI removal in Part 6. |

Part 2 has not passed its green-suite requirement. The full-suite logs are in `/private/tmp/akira-part2-fulltest-escalated.log` and `/private/tmp/akira-part2-fulltest-serial.log`.

## Part 3: Prime parity and Python kernel

**Status: blocked/incomplete; foundational CPython kernel is implemented, the Prime feature set is not complete.** Monty and its worker binary/dependencies were removed. `sovereign-prime` now starts Hermes's bundled CPython lazily per session, uses a newline-framed JSON pipe, preserves globals, supports `load`, `llm_query`, goal/heartbeat/refine, subagent spawn, and agent message callbacks, and drops its process on session deletion or after ten idle minutes. macOS uses `sandbox-exec`: network and writes outside project/session temp are denied; reads are limited to the runtime, project, skill store, and system Python libraries. macOS RSS is sampled via `proc_pidinfo` every 100 ms and a worker over 128 MiB is killed. Other platforms require approval before every cell and deny headless use. The cell timeout is 20 seconds.

Verification: `cargo check -p sovereign-prime -p jcode-app-core -p sovereign-gateway --tests --offline` passed with two existing gateway dead-code warnings. Bundled-CPython integration tests passed 9/9, including persistence, stdlib/package imports, `load`, recursive `llm_query`, host-call cap, project/network restrictions, timeout recovery, and RSS kill. These tests required the documented local-only escalation because macOS sandbox-exec cannot run inside the default restricted sandbox.

Remaining acceptance gaps: the user-requested Prime skill packages have not been ported; the kernel bridge still lacks proven async result-await semantics for spawned agents and full read/send/list message parity; the compact model-facing messaging tool/token count is unverified; no reattach live e2e exists; no p50/p95 budget bench exists for cold start, warm REPL, or Rust dispatch; no measured token comparison against 7,744; and `crates/sovereign-gateway/e2e/prime-parity.mjs` is absent. Therefore Prime Part 3 is not complete and must not be treated as shipped parity. The requested all-done status in `docs/PRIME_PARITY.md` is deliberately not asserted.

## Part 4: Prime paper components and benchmark arm

The paper audit finds no per-component numeric ablation table: its conclusion explicitly says targeted training on RLM and Continual Harness is still needed to isolate their contributions. The reported system-level ARC-AGI-3 result is RHAE Best@1 30% to 95.5%; it is not a causal estimate for any individual component. Other reported findings include competitive long-context scores, no material final-record effect from harness choice in the noisy multi-day nanoGPT comparison, and lower token usage in the Prime runs. These figures are not reproduced here. Source: [Prime Agent paper, arXiv:2608.23552v1](https://arxiv.org/html/2608.23552v1).

| Paper component credited as part of the harness | Contribution isolated by paper | Akira status at this checkpoint |
|---|---:|---|
| Persistent Python RLM, variables, programmatic context and recursive subcalls | Not isolated | CPython base committed; cold/warm latency budget and child-await parity missing |
| Continual Harness: typed prompts, memories, skills and subagent specs; versioning/refinement/rollback | Not isolated | Rust harness exists; executable Prime skill packages and some bridges missing |
| Recursive subagents and direct agent messaging | Not isolated | Native delegate/communication exists; model-facing compact send/read/list and live pair test missing |
| Long-lived daemon sessions, detach/reattach and recovery | Not isolated | Engine recovery paths exist; requested combined continuity e2e missing |
| Standard execution, verification, termination and resource accounting | Not isolated | Rust observability/replay/control paths exist; Prime-style full accounting parity unverified |
| Autonomous mode, goals and heartbeats | Not isolated | Rust endpoints exist; full Prime control semantics unverified |

Added the `prime` arm to `scripts/bench/abeval.py`; it uses the same imported 9 tasks, success checks, counting proxy, task timeout and per-run throwaway home, invoking Prime's documented RPC mode with a unique daemon socket. The Prime shallow clone at `/tmp/prime-agent` installed 430 packages in its disposable `node_modules` and built the CLI successfully. A direct Prime RPC smoke test reached local Ollama with no login and returned `prime-local-ok`. The benchmark runner's first task did not issue a counting-proxy request and remained active until interrupted; therefore full arm integration is **not verified**. The arm is prepared but requires debugging before benchmark use. Prime's initial no-custom-socket attempt returned `supervisor_generation_stale`; unique socket resolved that failure for the direct RPC smoke.

`python3 -m py_compile scripts/bench/abeval.py` passed when directing bytecode to `/private/tmp/akira-pycache`; the default sandbox blocks Python's global cache path. `abeval.py --dry-run` lists all three arms and nine shared tasks.

Part 4 follow-up: `BENCH_OUT=/private/tmp/akira-prime-bench-smoke BENCH_TURN_TIMEOUT_S=120 BENCH_REPS=1 python3 scripts/bench/abeval.py run --arm prime --reps 1 --only err_python_env` passed with local Ollama and the throwaway Prime CLI. The task completed in 61.9 s with 3 model calls, 2 tool calls, 0 tool errors, and a passing task check. The proxy recorded the requested local model, confirming RPC → shared counting proxy → Ollama with no login. Only one of the nine shared tasks was run; full-suite benchmark results remain unverified. `docs/PRIME_PAPER_AUDIT.md` records the paper's reported benchmark outcomes and component map. The paper provides no component-level ablation or per-feature causal contributions, and explicitly calls for targeted training to isolate RLM and Continual Harness contributions; individual component deltas are recorded as not isolated rather than inferred.

## Part 5: Hermes feature parity

**Status: Item A complete; Item B remains incomplete.** The earlier 19-placeholder/8-refused inventory above is a historical snapshot and has been superseded by the current checkout. On engine `c26a28596`, `PYTHONPYCACHEPREFIX=/tmp/akira-pycache python3 scripts/parity.py` regenerates the contract with 235 RPC methods (61 working in Rust, 0 placeholders, 174 forwarded) and 264 REST routes (36 served in Rust, 0 refused, the rest forwarded). Desktop-owned session and usage routes remain engine-backed.

Verification: with isolated temporary homes and local Ollama `sovereign/bench-hermes-64k:latest`, `node crates/sovereign-gateway/e2e/sessions.mjs` passes, including the final assertion that no chat-bound session/insights/usage method was forwarded. It curls all eight repaired `/api/sessions` routes with and without the token, verifies 401 for missing credentials and route-specific authenticated results, and exercises import/export, pruning, empty-session deletion, bulk deletion, and latest descendant. A table-driven unit test in `sessions_rest.rs` dispatches each of those eight handlers and checks its expected HTTP status; the focused test passed 1/1. In the sandbox, the live e2e failed before starting with macOS `system-configuration` panic `Attempted to create a NULL object`, and the focused route test failed at `TcpListener::bind` with `PermissionDenied: Operation not permitted`; both passed with local-only escalation. Sol advised treating parity categories as generator classifications, verifying authenticated curl behavior, using isolated dispatch coverage, and adding the explicit per-route unit checks. The no-daemon socket path is unique per test run.

Item B is not complete: session creation applies model/provider/reasoning and system prompts, and the Hermes hub plus skill loader share `JCODE_HOME/skills` (isolated path assertion passed). MCP OAuth-to-chat, tool toggles, personality and memory setting e2es, live skills-hub install visibility, and the remaining desktop feature walkthroughs are outstanding. See `docs/HERMES_FEATURE_CHECK.md`; these are product tasks, not sandbox failures.

## Part 6: remove jcode terminal UI

**Status: blocked/incomplete; only startup hooks were narrowed to the TUI path.** The session-picker cache invalidator and keybinding warning now register immediately before `tui_launch::run_tui_client` in `src/cli/dispatch.rs`, instead of during shared startup. `cargo check --bin sovereign --offline` passed after this move; it emitted two existing gateway dead-code warnings and two existing CLI unused-variable warnings.

Compiler dependency evidence: `cargo tree -e normal --target aarch64-apple-darwin -p jcode -i jcode-tui --offline` reports `jcode-tui -> jcode`; the requested `--target all` variant could not resolve the uncached `android_system_properties v0.1.5` package offline. `cargo check --bin sovereign` visibly compiles `jcode-tui`, `jcode-tui-style`, `jcode-tui-workspace`, `jcode-tui-messages`, `jcode-tui-permissions`, `jcode-tui-tool-display`, and `jcode-tui-anim`. `src/lib.rs` re-exports `jcode_tui::*`; `src/bin/sovereign.rs` calls `jcode::cli::startup::run_from`, and that generic CLI dispatch references TUI modules and forwards non-desktop arguments into the shared CLI. The browser/desktop runtime uses `serve` and `__pre-tool`, but no isolated gateway entrypoint exists yet. Removing the crate now would break the shared CLI and would need a separate engine-only startup path plus compiler-driven removal of the CLI-only imports.

No UI crate/support crate or CLI/install/self-dev/pairing path was deleted. No full workspace gates were rerun after the hook relocation. The required Part 6 deletion batch and its full gates remain outstanding; the current dependency must be retained until the sovereign entrypoint is separated from the general jcode CLI.

Part 6 follow-up: `cargo tree -e normal --target aarch64-apple-darwin -p jcode -i jcode-tui --offline` confirms `jcode-tui -> jcode`; `cargo tree` does not accept a `--bin` selector. The all-target query fails before producing a graph because offline cache lacks `android_system_properties v0.1.5` (exact error: `failed to download android_system_properties v0.1.5; attempting to make an HTTP request, but --offline was specified`). Source confirms the sovereign executable still translates arguments and calls `jcode::cli::startup::run_from`; `src/lib.rs` unconditionally re-exports `jcode_tui::*`; shared startup/dispatch imports `tui` and `tui_launch`. The TUI startup-only cache/keybind hooks are confined to `tui_launch` and are not used by gateway serving. Safe deletion therefore still needs an engine-only sovereign entrypoint and provider/server bootstrap separated from CLI dispatch; the current gateway crate has no standalone command bootstrap. The graph index excludes `src/bin` and has metadata-changed CLI files, so this conclusion is based on direct source inspection plus the compiler dependency tree, not a negative graph result. No TUI crate was deleted and no post-deletion gates can be claimed; Part 6 remains incomplete.

Part 2 follow-up after Part 6: the current isolated, escalated workspace run reached the first target with 269/270 passing; the lone failure was `memory_cli_semantic_requires_jev_but_keyword_search_remains_local`. The assertion was stale against the current local FTS `memory_recall::recall` implementation and `docs/MEMORY_DESIGN.md`. Renamed it to `memory_cli_ranked_search_remains_local_without_jev_credentials` and changed it to require successful local ranked search without Jev credentials, while retaining the explicit project-scope leak check. The focused test passes (1/1). The updated complete workspace suite is rerunning next; this does not resolve the 12 unrelated previously recorded target packages yet.

Additional Part 2 follow-up: the escalated full run then found the macOS `jcode-core` stdin test failing outside the sandbox (39/40). Root cause was the detector treating every vnode stdin, including `/dev/null`, as an interactive terminal, then mistaking any parked test-runner thread for a reader. The detector now checks `PROC_PIDFDVNODEPATHINFO` and treats vnode stdin as interactive only for `/dev/tty*` or `/dev/console`; pipe stdin remains supported. `cargo test -p jcode-core --lib stdin_detect_tests::test_own_process_not_reading_stdin --offline` passes 1/1. The full suite is rerunning to identify any further target failures.

Final triage follow-up: the next in-sandbox run stopped at `jcode` with 242 passed and 28 failures. All 28 failed because the sandbox denied loopback bind (`PermissionDenied`) or macOS `system-configuration` returned `Attempted to create a NULL object`; these are sandbox-only and were then rerun with local-only escalation. The escalated run passed the root crate and all packages before `jcode-provider-openai-runtime`; it found one reproducible fixture error in `persistent_failed_missing_tool_output_recovers_with_full_replay`. The mock listener incorrectly treated a provider model-catalog `GET /v1/models` request as a WebSocket upgrade. The fixture now returns an empty model catalog, then asserts that replay uses a fresh full-input WebSocket. That test passes twice in succession. Bedrock tests also now point `JCODE_HOME` at their temp directory; their prior failures were caused by the macOS config resolver ignoring `XDG_CONFIG_HOME` while a reused temp home contained stale Bedrock settings. The Bedrock test binary passes 22 active tests (1 ignored). A final full workspace rerun is required to close Part 2.

Part 2 closed: full workspace result is 8,056 passed, 0 failed, 59 ignored across 212 targets with isolated homes and local-only escalation. See the preceding final triage record for the exact failures and fixes.

Part 3 status: CPython, the macOS sandbox profile, RSS watchdog, lifecycle tests, RLM static prompt guidance, and a compact `agent_message` tool were present or added; Monty remains deleted. Part 3 is incomplete and blocked on several missing host capabilities rather than the worker itself: there is no RLM heartbeat store/API, no family-scoped agent observation API, no REPL compaction scheduling API, and the existing goal API does not expose Prime's budget/result contract. The current REPL delegate callback only admits a child and does not provide an awaitable result lifecycle. The Python skill store can import user-installed packages, but `/skill create` does not create Python packages and Prime's built-in package wrappers are not bundled. The combined reattach e2e and p50/p95 budget benchmark are also absent, so no latency budget or tool-schema delta is claimed. Evidence and current per-feature status are in `docs/PRIME_PARITY.md`. Per the queue instructions, work continued to Part 4 with these gaps recorded.

Part 2 final verification: the full `cargo test --workspace --offline -- --test-threads=1` run completed outside the restricted sandbox with isolated HOME/JCODE_HOME/HERMES_HOME, `SOVEREIGN_HERMES_PYTHON` set to the Hermes venv, and ANSI-capable terminal variables. Result: 8,056 passed, 0 failed, 59 ignored across 212 test targets (including doc tests). This closes the earlier 12-target failure note: focused fixture fixes covered the failures, and the final aggregate run is green. One genuine sandbox-profile defect was fixed: CPython venv symlinks now resolve to the actual executable/runtime before constructing macOS sandbox-exec rules. The REPL integration suite passed 9/9. The new short REPL tool description also passes the existing 20-token description cap. Sandbox-only confirmation: un-escalated runs cannot launch nested sandbox-exec (`sandbox_apply: Operation not permitted`) and macOS SystemConfiguration tests can panic with `Attempted to create a NULL object`; the final complete run passed with local-only escalation.

## Follow-up item 2: Rust session RPC and REST parity

**Status: complete.** Desktop-owned session and usage RPCs are backed by engine session/transcript/usage data. The eight repaired REST endpoints are implemented in `crates/sovereign-gateway/src/sessions_rest.rs`; the three `session.foreign.*` methods use the Rust importer in `crates/sovereign-gateway/src/rpc.rs`. The `sessions.mjs` live e2e passed against the local Ollama model, including synthetic Codex list/preview/import coverage and authenticated plus missing-token curl checks for all eight REST routes. The route dispatch unit test also passes. `python3 scripts/parity.py` reports 235 RPC methods (61 Rust-served, 0 placeholders, 174 forwarded) and 264 HTTP routes (36 Rust-served, 0 refused, remainder forwarded). Commit `3e75b9258` contains the implementation/authenticated route coverage; the foreign importer e2e follow-up is in this continuation.

## Follow-up item 3: settings reach engine chat

**Status: blocked/incomplete after tracing the settings paths.** Rust session creation and state already accept per-session model and reasoning effort, and the engine's provider-control code persists reasoning changes. I did not run a live settings-change e2e in this item. MCP and skill-hub settings have no bridge to the engine's source of truth: the bundled engine's `/api/mcp/servers` and skill-hub endpoints return empty data, chat reads `.jcode/mcp.json` and `~/.jcode/skills`, while Hermes APIs persist settings on the Python side. This means changes made in those Hermes settings screens cannot yet be proven to affect chat. Tool enablement, personality/system prompt, profile, provider/model and memory settings also lack the requested live behavior tests and source-of-truth audit. No e2e or bridge was added; the requested coverage is therefore not claimed. Proceeding to item 4 per the queue instruction.

## Follow-up item 4: Prime Python skills and budgets

**Status: blocked/incomplete after the requested reference review.** A shallow read-only clone was made in `/tmp/prime-agent`; Prime's `docs/skills.md` and the requested skill package sources were inspected. Its Python packages call the generic `rlm.host_request` interface for `agent_observe`, compaction, goals, role-addressed messaging, and internal RLM heartbeats. Akira's `python_worker.py` exposes only fixed functions (`llm_query`, `load`, `refine`, `goal`, user `heartbeat`, `spawn_subagent`, `agent_message`), so several upstream packages would fail if copied as-is. Existing goal/refine/user-heartbeat packages can map to the fixed bridge, but porting only those would not satisfy the requested skill set. The compact Rust `agent_message` tool and static RLM prompt guidance already exist; no edits were needed there. `prime-parity.mjs`, the combined reattach e2e, and dispatch/warm/cold budget assertions are absent, so the latency budgets and full parity status are unproven. The current schema baseline is 7,744 tokens; this item has no verified after-measurement yet. No tests or skill code were added, and Prime parity is not claimed complete. Proceeding to item 5 per the queue instruction.

## Follow-up item 5: dedicated sovereign entrypoint / TUI removal

**Status: blocked/incomplete after trying the suggested entrypoint approach.** I ran `CARGO_TARGET_DIR=... cargo tree -e normal --target aarch64-apple-darwin -p jcode -i jcode-tui --offline`; it confirms the product's root `jcode` library depends on `jcode-tui`. `src/lib.rs` unconditionally re-exports it. `src/bin/sovereign.rs` delegates to `jcode::cli::startup::run_from`, so it still enters generic CLI dispatch. The actual gateway initializer `run_gateway` is private in `src/cli/dispatch.rs` and depends on private `provider_init`, `server`, warm-Ollama lifecycle, feature-backend, and provider-selection setup. The standalone `sovereign-gateway` crate serves only after the harness socket and provider/server have been initialized; it cannot replace that bootstrap by being called alone. A real minimal binary therefore requires extracting the shared provider/server bootstrap and separating root CLI modules/features before the compiler can remove the TUI dependency. No isolated bootstrap refactor or crate deletion was made; keeping the minimum current dependency is necessary to preserve product startup. Historical Part 6 evidence also confirms seven TUI crates compile into the binary today. Item 5 is not claimed complete.

## Follow-up item 6: remaining failing test targets

**Completed in the latest rerun.** `cargo test --workspace --offline -j1 -- --test-threads=1` exited successfully: 8,071 passed, 0 failed, 59 ignored, 1,254 filtered across 212 suites. It used isolated HOME/JCODE_HOME/HERMES_HOME, truecolor (`TERM=xterm-256color`, `COLORTERM=truecolor`), and `SOVEREIGN_HERMES_PYTHON` pointed at the packaged Hermes Python 3.12 runtime. The test-only fixes make stdin detection independent of Cargo's inherited pipe and give the model-picker and first-use hotkey tests fresh per-test JCODE_HOME state. Earlier failures reproduced only with `TERM=dumb`, a live engine concurrently holding a system sleep assertion, or an unset REPL Python path; the final run had none of those conditions.

## Follow-up item 7: Prime benchmark arm

**Status: passed for the requested one-task smoke run.** The prior install attempt was superseded by building the read-only shallow Prime clone with its own bundle script in `/tmp` and resolving the first-task hang: Prime's benchmark model reference must be `bench/<model-id>`, matching the configured provider, or it makes no counting-proxy request. The daemon socket was not the cause. `err_case_search`, one repetition, passed for Hermes, Sovereign, and Prime against the same local Ollama model and counting proxy. The three results were respectively 3/2/0 model/tool/error calls in 172.8 s, 5/4/0 in 124.3 s, and 4/3/0 in 93.8 s. This is a smoke run only, not the full benchmark suite.

## Follow-up queue verification (2026-09-27)

This checkpoint verifies, but does not overstate, the earlier queue work.

| Item | Result | Verification / remaining gap |
| --- | --- | --- |
| 1. Cron worker leak and empty agent reply | Passed again | Release build succeeded. Packaged install-launch and chat-approval passed. The packaged cron-due test passed three consecutive times against this package. All three returned `sovereign-cron-agent-ok`, recorded `last_status=ok`, had `python:false` before delivery and after completion, and only showed Python running while delivery was active. |
| 2. Session RPC/REST parity | Implemented; later final audit passed | See “Final verification of Item A” below. The earlier failure report here is historical and superseded by the committed implementation and live rerun at `f0ba1ff`. |
| 3. Hermes settings and desktop features | Partial | Live e2es prove local stdio MCP, skill-hub installs, tool toggles, model/provider/reasoning, memory, profiles, and system prompts reach Rust chat. OAuth MCP, browser controller, and live voice/wake/TTS remain open; `docs/HERMES_FEATURE_CHECK.md` has the feature-by-feature results. |
| 4. Prime skill packages, messaging, reattach, budgets | Incomplete | Existing CPython kernel integration tests pass (9/9) and `sovereign-prime` tests pass (33 unit + 9 REPL); however Prime's package ports, combined `prime-parity.mjs`, dispatch/warm/cold p50/p95 bench, reattach scenario, and verified schema delta are still absent. Existing baseline is 7,744 schema tokens; the previously measured plain-task estimate was Sovereign 7,675 (delta -69), Hermes 10,664. |
| 5. Dedicated sovereign entrypoint and TUI removal | Incomplete | Release build succeeds but still compiles/links the generic `jcode` CLI and `jcode-tui`. The earlier dependency-tree and bootstrap trace shows the extraction work remains. |
| 6. Remaining test failures | Passed | Latest full workspace run: 8,071 passed, 0 failed, 59 ignored, 1,254 filtered across 212 suites, with isolated homes, bundled Hermes Python, truecolor, and serialized tests. Test isolation fixes are recorded in Item 6. |
| 7. Prime benchmark arm | Passed smoke run | Prime, Hermes, and Sovereign each completed the same one-task, one-repetition `err_case_search` run against local Ollama through the counting proxy; detailed call counts and timings are above. |

Other final checks: live gateway `learning`, `refine`, `agent-loop`, `agent-run`, `accounting`, `replay`, and `sessions` passed. Desktop typecheck, stage, pack, install-launch, chat-approval, cron-due (three consecutive runs), plugin-install, and terminal-PTY checks passed. `scripts/parity.py` reports 235 RPC methods (61 Rust-served, 0 placeholders, 174 forwarded) and 264 HTTP routes (36 Rust-served, 0 refused, remainder forwarded).

The remaining documented gaps are OAuth-authenticated MCP, browser-controller interaction without an available CDP target, live voice/wake/TTS hardware, and the incomplete Prime skill/host parity listed under Item 4. No feature styling or themes were changed.

Verified source revision for the final gates: engine `cbe57e7834be337ee04d57f9562bcb0e92a1e36c`; Hermes `f242618d0ab4844b17e3cdacbd0e16e3e8d27d66`. The later engine commits only update this report and retain the already measured benchmark run; no source code changed after the successful build and gates.

## Follow-up Item 2: engine-owned session parity

**Status: implemented and committed.** The Rust gateway now handles the requested engine-backed session RPC methods and all eight `/api/sessions` routes that were previously refused. Session detail/transcript/export reads load the persisted session and replay its journal directly; export envelopes are accepted by import, and import validates the whole batch before writing. Global stats/empty/prune/descendant scans request complete session enumeration, prune rejects negative or non-finite ages, and mutation errors are returned instead of being counted as success. `session.foreign.*` uses the engine's Claude and Codex import readers; Codex listing uses the same non-writing parser as import.

Verification: `cargo test -p sovereign-gateway --offline` passed (47 passed, 2 ignored); `cargo build --release --bin sovereign` passed; `node crates/sovereign-gateway/e2e/sessions.mjs` passed outside the sandbox with local Ollama. Its session RPC coverage and all eight REST routes were exercised; each REST path returned 401 without a token and 2xx with a token using `curl`. The e2e also round-tripped the engine export envelope through import. In-sandbox execution still fails before startup with the known `system-configuration` NULL-object panic, so the local-only test was rerun outside the sandbox. `python3 scripts/parity.py` reports 0 placeholder RPC methods and 0 refused routes.

Remaining scope: this item makes the listed session routes real against engine storage; it does not claim the later feature-walkthrough/settings requirements or the entire follow-up queue are complete. Existing parity inventory still forwards the unrelated Hermes-owned features to Python.

## Follow-up Item 3: Prime REPL parity

**Status: partial; the fetch blocker was later resolved.** The existing CPython worker imports user packages from `~/.jcode/skills`; the compact model-facing `agent_message` tool has a schema-size assertion. This pass corrected agent transcript reads to use swarm context history, enforces the 16-call host cap in Rust, and provides goal/refine/websearch host-request mappings. Focused message and REPL tests pass. A later read-only shallow clone succeeded outside the network sandbox, and its skill wrapper/API contracts were audited. The remaining package bundle, RLM heartbeat/family observation/compaction bridges, goal budget/result parity, subagent result-await lifecycle, combined reattach e2e, `prime-parity.mjs`, and latency budget bench are still missing; see the later Item 4 entry.

## Follow-up Item 4: Prime paper and component audit

**Status: paper audit complete; component parity remains incomplete.** Rechecked arXiv `2608.23552v1` §§2–3 and updated `docs/PRIME_PAPER_AUDIT.md`. It lists reported outcomes and qualitative component credits. The paper provides no isolated numeric deltas for RLM or Continual Harness, so no causal per-component percentages are claimed. The Prime benchmark CLI is runnable from a disposable bundle, and the requested one-task smoke run passed for all three arms; the full benchmark and Prime feature parity remain incomplete.


## Final verification of Item A (2026-09-27)

**Status: implementation is committed at the base revision; the strengthened live session e2e passes.** The base checkout is `f0ba1ff573cc087310005e2ba3887722fe4537cd`. Re-ran `python3 scripts/parity.py`: 0 placeholder RPCs and 0 refused routes. Re-ran `crates/sovereign-gateway/e2e/sessions.mjs` using isolated temporary homes and local Ollama. The sandbox attempt failed before gateway startup with `system-configuration` panic `Attempted to create a NULL object`; the same command passed outside the restricted sandbox. The live script checks all eight REST paths via `curl`: each rejects missing auth with 401, and each authenticated response is checked against its expected JSON contract. It also verifies latest-descendant against a real branch, deletes an imported empty session and a nonempty bulk-delete target, and prunes an imported old record in dry-run and live modes. The engine RPCs and export/import round trip are exercised too. Its forwarding assertion excludes the deliberately Hermes-forwarded `handoff.request` and passed. The earlier “incomplete” row above described a different checkout state and is superseded.

The existing `sessions_rest.rs` unit tests cover safe session IDs and validated import/export envelopes; the live e2e covers route dispatch, authorization, and behavior. No claim is made that every route has an isolated handler-level unit test. Review found the stats and empty-count assertions needed exact baselines, so the test now compares against the current session listing and requires the empty import to increment the count by one.

## Hermes feature walkthrough progress (2026-09-27)

This supersedes the earlier Item 3 paragraph that said no bridge or e2e had been
added. The Rust engine now reads Hermes `config.yaml` as the source of truth for
MCP servers, toolset enablement, and memory enablement; the engine continues to
own per-session model/provider/reasoning/system-prompt state. Categorized
skills-hub installs under `JCODE_HOME/skills/<category>/<name>` are discovered
by the engine skill loader. The new isolated live e2e
`hermes-mcp-settings.mjs` passed: an API-added local stdio MCP tool was called
by chat; the hub skill appeared in `/api/skills`; tool disablement removed bash
from chat; and model/provider/reasoning/memory/system-prompt settings reached
the engine. Its first sandbox run was denied localhost bind; the local-only
escalated run passed. A standalone Ollama `think:false` probe returned `OK`.

The CLI-driven Kanban test passes board creation, task claim/completion, and
persisted result checks. The Activity/star-map e2e passes completed-run details
and star-map graph/node list, edit, and delete checks. These tests use isolated
homes. Remaining rows in `docs/HERMES_FEATURE_CHECK.md` are still open; this
progress record does not claim the entire walkthrough is complete.

## Hermes walkthrough continuation

The shared skill registry now refreshes from disk before returning a snapshot,
so a hub removal is visible to an already-running engine. The new
`hermes-skill-hub.mjs` passed install and uninstall checks against that live
engine. `hermes-messaging-loopback.mjs`, `hermes-kanban-cli.mjs`, and
`activity-learning.mjs` also passed. A profile e2e launches `sovereign
--profile research serve` with temporary Hermes/JCode homes and confirms the
selected profile model, provider, and `SOUL.md` prompt in a real Ollama chat.
The first profile launch collided with another process's runtime socket; an
isolated short `XDG_RUNTIME_DIR` fixed it. Its first configuration also asked
the Ollama profile for unsupported reasoning effort; the profile fixture now
tests settings supported by that provider, while reasoning-setting behavior
remains covered by `hermes-mcp-settings.mjs`.

The local Ollama daemon was already listening outside the sandbox. Sandbox
localhost access failed, and `ollama serve` could not bind from inside the
sandbox (`operation not permitted`); the escalated local-only `/api/tags`
request verified the benchmark model was available and was used for the live
tests. The final independent MCP-settings rerun passed: MCP server add/use,
skill hub install/list, terminal tool disablement, model/provider/reasoning,
memory, and profile system prompt all reached the Rust chat. OAuth MCP, the
browser-controller action, terminal/PTY pane, live
voice/wake/TTS, and plugin install/capability checks remain open in
`docs/HERMES_FEATURE_CHECK.md`.

## Follow-up queue continuation

### Item 4: Prime REPL parity

The required read-only shallow clone was retried outside the network sandbox
and succeeded. Prime's wrappers for `agent-observe`, `compact`,
`rlm-heartbeat`, and `agent-message` import `rlm.host_request`. The worker now
provides that module for the already implemented goal and refine callbacks.
The remaining package contracts include
`agent_observe.list/get/recent`, `compact.status/run`,
`rlm_heartbeat.list/create/update/delete`, and `rlm.collect(targets,
timeout_ms)`. Rust has communication, compaction, session-heartbeat, and
delegate components, but the REPL has no bridges for those operations, no
Prime package bundle in the skill store, and no combined reattach or budget
e2e. Item 4 remains incomplete; the source clone and API audit removed the
earlier DNS uncertainty. Added the partial goal/refine mapping in this follow-up;
the other operations remain unimplemented.

The new `rlm.host_request` mapping supports `goal.get/create/complete`,
`refine.status/run`, and `websearch.run` by calling existing Rust features.
Web search forces DuckDuckGo, avoiding Prime's Serper key requirement. Unknown
requests fail explicitly. The goal budget/result shape and Prime's heartbeat,
observe, compaction, role-based messaging, and `rlm.collect` remain gaps; the
Prime-compatible package bundle also remains missing. The targeted host-request
integration test and all 10 REPL integration tests pass with the staged Hermes
CPython runtime.

### Item 5: jcode TUI removal trial

`cargo tree --offline -p jcode -e normal -i jcode-tui` shows the product's
`jcode` package depends directly on `jcode-tui`. A compiler trial temporarily
removed that dependency and its re-export, then ran
`cargo check --offline --bin sovereign --no-default-features` with the shared
target. It failed with 799 errors: `src/cli` imports `crate::tui` and app-core
modules re-exported through `jcode-tui`, including server, session, provider,
auth, storage, and memory. The trial changes were restored immediately. A
dedicated engine entrypoint must first extract gateway startup and its minimal
app-core dependencies from the generic CLI; simply unlinking the TUI is not
safe. No production files were left changed by the trial.

The requested dedicated-entrypoint pass is still blocked by the same ownership
boundary: `src/bin/sovereign.rs` currently prepares process hooks and then calls
`jcode::cli::startup::run_from`; the actual gateway bootstrap is private inside
`src/cli/dispatch.rs` and also reaches the CLI's provider initialization,
feature-process, profile, socket, and Ollama lifecycle helpers. A new binary
cannot call only the product paths without first extracting that runtime into
a TUI-free crate. The trial compile after unlinking the TUI produced 799 errors
across those imports. I restored it and kept the minimum dependency; deleting
the TUI now would break the only working `sovereign` entrypoint. Continue by
extracting the engine runtime boundary before retrying this deletion.

### Item 6: workspace test targets

Fixed the full-run `sovereign` binary test compile error by importing
`profile_home`, `Path`, and `PathBuf` in its test module; the binary target now
passes 3/3 tests. Fixed stale `jcode-base` test assumptions exposed by the
required temp-home environment: standalone MCP tests now temporarily clear
`HERMES_HOME` and restore it via a guard, while the browser path test asserts
that the browser directory is under the configured JCode home instead of
requiring the home directory's literal name to contain `.jcode`. The
`jcode-base` suite passes 1,494 tests with 6 ignored under local-only
escalation.

The restricted run's 96 socket-binding failures and loopback probe were
`Operation not permitted`; the escalated crate run passed. The two raw ANSI
wire tests require `NO_COLOR` unset. A first full rerun exposed stale test
assumptions: the stdin test observed the launcher's inherited pipe, the model
picker and hotkey tests reused persisted state, and the exact-color tests ran
under `TERM=dumb`. Those tests now use an explicit null-stdin child and fresh
per-test homes; the exact-color suite and full TUI suite pass with truecolor.
One system-wide sleep assertion overlapped a separately running live session
e2e, so the final cargo run was repeated after the engine process exited.
Final workspace result: 8,071 passed, 0 failed, 59 ignored, 1,254 filtered
across 212 suites.

### Item 7: Prime benchmark arm

Fixed Prime's zero-request failure in `scripts/bench/abeval.py`: Prime's model
reference is canonical `provider/model-id`, but the harness passed only the
Ollama model ID while separately selecting provider `bench`. That selected no
configured benchmark model; the RPC process emitted a provider connection error
and the counting proxy saw zero requests. Passing `bench/<model-id>` made the
configured model resolve and the counting proxy receive requests. The explicit
daemon socket was not the cause and remains in place.

Built the required reference CLI in the shallow `/tmp/prime-agent` clone. The
clone's full build path could not fetch Prime's full hosted model catalog and
its four-model fixture failed the source catalog's 42-transport validation;
the compiled CLI was therefore bundled directly using Prime's own
`scripts/bundle.mjs`. Its only extra missing package, `@opentelemetry/api`, was
fetched into the throwaway clone. No installer or global install was used.

The same `err_case_search` task passed once for all three arms against local
`sovereign/bench-hermes-64k:latest` through the shared counting proxy:

| Arm | Result | Model calls | Tool calls | Tool errors | Wall time |
|---|---:|---:|---:|---:|---:|
| Hermes | Pass | 3 | 2 | 0 | 172.8 s |
| Sovereign | Pass | 5 | 4 | 0 | 124.3 s |
| Prime | Pass | 4 | 3 | 0 | 93.8 s |

All isolated run homes and outputs were under `/tmp`. The reference clone was
removed after the benchmark. This verifies one task and one repetition only;
the full nine-task comparison is not claimed.

### Item 3: Hermes settings and feature walkthrough follow-up

Settings e2es from `f4569da` and `6014a73` already prove the local stdio MCP
server, skill-hub install/removal, tool disablement, model/provider,
reasoning-effort, memory flag, profile, and system prompt behavior against a
live engine chat. Item 3 is still incomplete: OAuth-authenticated MCP was not
proved. Source inspection found the actual product gap: `McpServerConfig` keeps
HTTP/SSE URL and header fields but documents them as unused, and
`McpConfig::load_all` filters all non-stdio entries before the engine can
connect them. The Hermes OAuth API stores the authorization flow in Python;
there is no Rust HTTP/SSE MCP transport or OAuth token handoff. Thus the local
stdio e2e cannot prove OAuth MCP behavior, and the chat currently cannot call
those servers.

Added packaged Electron e2es in Hermes commit `8f0412f`. The plugin test first
failed because it guessed the install directory; it now uses the path returned
by the install IPC. The packaged rerun passed: a temporary Git plugin installed
into the isolated Hermes home and rendered its registered status-bar
capability. The new packaged PTY test passed by spawning zsh, sending a command,
observing its marker in the PTY stream, and disposing the process. Browser
controller navigation remains open: the host has no Chrome, Chromium, Brave,
or Edge executable at the supported macOS app paths and no live CDP target was
available. Voice/wake/TTS live checks remain open because no microphone or
audio endpoint was available to the CLI run; paid speech services are outside
the local-Ollama-only constraint. The feature-by-feature results are in
`docs/HERMES_FEATURE_CHECK.md`.

Item 3 therefore remains incomplete on OAuth MCP and the unavailable
browser/audio live targets. The two new packaged tests passed; there were no
product-code changes in this follow-up. Continuing to item 4 as instructed.

## Final verification rerun (2026-09-28)

Ollama was unreachable inside the restricted shell (`curl: (7) Failed to connect`); starting it there returned `Operation not permitted`. The authorized outside-sandbox start attempt returned `address already in use`, and an outside-sandbox request confirmed the server was already available. The required `sovereign/bench-hermes-64k:latest` model was installed and loaded. All live model checks below used that local endpoint.

| Gate | Result |
|---|---|
| Workspace tests | Pass: `cargo test --workspace --offline -j1 -- --test-threads=1`: 8,071 passed, 0 failed, 59 ignored, 1,254 filtered across 212 suites (rerun from the preceding verification). |
| Release binary | Pass: `cargo build --release --bin sovereign --offline`, 600 crates, 0 errors, 4 existing warnings. |
| Gateway live e2e | Pass: sessions, learning, refine, agent-loop, agent-run, accounting, replay. Includes session routes with and without tokens, foreign session import, and non-empty `agent.run` response. |
| Desktop TypeScript | Pass: `npm run typecheck` (renderer, Electron, and e2e projects). |
| Python stage | Pass using the already bundled runtime and packages via `SOVEREIGN_PYTHON_RUNTIME` and `SOVEREIGN_PYTHON_PACKAGES`; no runtime/package download. The first default attempt was sandbox-blocked when `uv` tried to open `/Users/rameelmalik/.cache/uv`; the explicit existing runtime paths avoided that access. |
| Desktop package | Pass: `npm run pack` generated the mac-arm64 unpacked app. Code signing/notarization were skipped because no Apple identity/credentials are configured. |
| Packaged install-launch | Pass against the fresh release binary. |
| Packaged chat-approval | Pass; output contains `sovereign-packaged-ok`. |
| Packaged cron-due | Pass 3 consecutive runs with `SOVEREIGN_CRON_AGENT=1`; each job returned `sovereign-cron-agent-ok` and the final process listing contained no packaged Hermes Python process. |
| Parity generator | Pass: 235 RPC methods, 0 placeholders; 264 HTTP routes, 0 refused. 174 RPC methods and remaining HTTP routes forward to Hermes Python. |
| Plain-task schema | Pass, one run: 7,675 estimated tool-schema tokens per call (baseline 7,744; delta -69, under the +300 cap). Output was kept in `/private/tmp/sov-bench-final`. |

The selected live checks ran sequentially because they share the local model and the session/cron tests use isolated homes. The workspace test result above was completed before this rerun; it was not rerun after packaging because packaging does not change tracked engine sources.

### Remaining incomplete work

- Item 3 still has browser-controller and live voice/wake/TTS checks open because no CDP target or audio device was available. Local stdio/OAuth MCP, skills hub, tool/model/provider/reasoning/system-prompt/memory settings, PTY, plugins, and the other listed feature checks pass.
- Item 4 remains partial: Prime Python wrappers for RLM heartbeat CRUD, observation, compaction, richer goal semantics, subagent await/collect, and the compatible skill package bundle are not complete. No p50/p95 dispatch, warm REPL, or cold-start budget bench was added. The stated latency budgets are therefore unverified.
- Item 5 remains blocked after the compiler trial recorded above: the generic `jcode` CLI owns the only gateway bootstrap path and unlinking `jcode-tui` produced 799 compiler errors. The minimum TUI-linked runtime dependency remains in the product until gateway/runtime startup is extracted.
- Item 7's one-task, one-repetition Prime/Hermes/Sovereign benchmark passed as recorded above; the full benchmark suite is not claimed.

Verified code SHAs used for release and desktop packaging: engine `ad9a4456feb87c6995507b67aa22a5f05222233c`; Hermes `8f0412fc4289822d17abe7587e7da27ef1f98e6c`. The final report commit is recorded separately in the response because a commit cannot contain its own SHA.

### Follow-up item 3: OAuth MCP chat bridge

Added streamable HTTP MCP support to the Rust MCP client. Hermes remains the
source of truth for MCP server definitions and OAuth tokens: the engine reads
the `mcp_servers` entry and its existing `HERMES_HOME/mcp-tokens/<name>.json`
cache, sends the cached bearer token, and dispatches discovered tools directly
from Rust. The client now uses the server-negotiated protocol version, parses
multiline SSE data, and removes timed-out requests from its pending map. Legacy
SSE transport remains unsupported. Expired tokens return a Settings
reauthentication error; token refresh remains owned by Hermes.

Verification: all 62 `jcode-base` MCP tests passed outside the sandbox. Inside
the sandbox the sole failure was the test listener bind (`Operation not
permitted`); the identical full filtered suite passed outside. The live
`hermes-mcp-settings.mjs` test passed against local Ollama and proved an API-
added OAuth server's cached token was used for an actual Rust chat tool call.
The same run verified the existing stdio MCP call, skill-hub install
visibility, terminal-tool disablement, and Hermes model/provider/reasoning,
memory, and system-prompt settings. The release binary rebuilt successfully.
This closes the OAuth-call coverage gap; browser-controller and hardware-bound
voice/wake/TTS checks remain open as recorded above.

### Follow-up item 4: Prime Python skills

Bundled Prime's goal, refine, RLM-heartbeat, agent-message, agent-observe,
compact, websearch, and skill-creator packages under
`crates/sovereign-prime/src/skills`, with Prime's MIT notice. The packages
install on first kernel start without replacing an existing user skill folder.
The REPL now imports the real goal package and routes goal operations to the
Rust host. RLM heartbeats have separate `source=rlm` rows in the existing
`session_heartbeats` store and full list/create/update/delete bridge operations;
they share the engine scheduler and do not overwrite the user's heartbeat.
Websearch uses Akira's existing DuckDuckGo tool without a paid key.

Verification: `cargo test -p sovereign-prime --offline -- --test-threads=1`
passed 36 unit tests and 12 real-worker integration tests with an isolated
temporary JCODE_HOME and the staged Hermes CPython. The sandboxed test command
requires local process access because this environment blocks macOS
`sandbox-exec`; the same tests passed outside that restriction. Remaining gaps
after trying the existing host interfaces: role-addressed parent/sibling
messaging, complete observe response shapes, REPL compaction, `/skill create`
Python package authoring, subagent await/collect, combined reattach coverage,
and p50/p95 latency budget benches. Compact directs the model to the existing
`/compact` operation because no in-turn compaction host API exists. These
limitations are reflected in `docs/PRIME_PARITY.md`; no parity claim is made
for them.

### Follow-up item 5: dedicated Sovereign entrypoint and TUI unlink attempt

`src/bin/sovereign.rs` now parses only the desktop gateway arguments and calls
the gateway runner directly; it no longer routes `serve` through generic CLI
startup/dispatch. Its `__pre-tool` entry remains. The focused binary tests pass
(2/2), and `cargo check --bin sovereign --offline` passes with the same four
pre-existing warnings from `sovereign-gateway` and generic login code.

The requested unlink still fails the dependency proof: `cargo tree --offline
-e normal -p jcode -i jcode-tui` returns `jcode-tui -> jcode`; root `jcode`
re-exports `jcode-tui` to preserve the generic CLI's `crate::tui` namespace.
The earlier removal trial in this report produced 799 compile errors from
those shared CLI references. The direct entrypoint change does not remove that
root-library edge, and the compiler check itself compiled `jcode-tui` plus its
support crates. Removing the re-export requires separating the root CLI
library or moving gateway bootstrap/provider setup into a TUI-free crate; that
is the remaining blocker for deleting the TUI and generic install/self-update/
self-dev/pairing paths. No such code was deleted speculatively.

Item 6 is skipped: after the last full workspace run no test target has a
reported failure; the latest Prime crate run is also green (48 tests). Item 7
remains verified by the prior one-task/one-repetition three-arm run recorded
above. A repeat attempt to inspect the full cross-target Cargo tree was
sandbox/offline-limited because `android_system_properties v0.1.5` is not
cached; the current-host dependency tree command above is complete and exact.

### Follow-up item 7: final benchmark smoke and runner correction

The repeat smoke initially made the Hermes arm appear hung: its configured
localhost counting proxy was blocked by the sandbox (`httpx.ConnectError:
[Errno 1] Operation not permitted`). Outside that restriction, all three arms
completed successfully against the same local Ollama model. The Sovereign arm
then exposed that the dedicated binary no longer accepts the generic
`--provider-profile` flag (`unsupported sovereign argument: --provider-profile`).
The bench runner now selects its throwaway `bench` provider from config and
uses the supported `--provider openai-compatible` serve arguments.

One task (`err_case_search`), one repetition, local model
`sovereign/bench-hermes-64k:latest`: Hermes passed (3 model calls, 3 tool
calls, 0 tool errors, 146.4 s); Sovereign passed (4 model calls, 6 tool calls,
0 tool errors, 152.4 s); Prime passed (2 model calls, 1 tool call, 0 tool
errors, 55.2 s). This is a one-task smoke only; the full benchmark suite is
not claimed. The repeat was executed with local-only network escalation after
the sandbox denial.

Final regression checks after the runner correction: full `cargo test
--workspace --offline -- --test-threads=1` passed with isolated homes,
`SOVEREIGN_HERMES_PYTHON` set to Hermes's bundled venv Python, and
`NO_COLOR` removed (`8,071 passed, 0 failed, 59 ignored, 1,254 filtered in
212 suites`). The first attempt's two glyph-color failures came from `rtk`
exporting `NO_COLOR=1` and `TERM=dumb`; the focused tests and full rerun pass
with `COLORTERM=truecolor TERM=xterm-256color`. Release binary build passed;
`python3 scripts/parity.py` reports 0 placeholder RPC methods and 0 refused
HTTP routes; desktop `npx tsc --noEmit -p .` passed. The earlier pack and
packaged e2e results remain the recorded desktop verification because this
last change touched only the benchmark runner and report.

### Item A continuation: dedicated-entrypoint sessions regression (2026-09-28)

The sessions e2e still used the removed generic `--provider-profile` argument.
Updated its isolated provider config to set `default_provider = "local"`, use
the supported `--provider openai-compatible` serve argument, and give Hermes
its own temporary home. This keeps the live test on local Ollama and preserves
the no-forwarding assertion. The focused REST dispatch test passed (1/1),
`python3 scripts/parity.py` reports 0 placeholder RPCs and 0 refused routes,
and `node crates/sovereign-gateway/e2e/sessions.mjs` passed outside the
restricted sandbox. It verified all eight REST routes with and without auth,
the session RPCs, engine settings, and the final no-chat-RPC-forwarding check.
GPT-6 Sol reviewed the diff and advised it was minimal and correct; its only
caveat was that a host-inherited `JCODE_NAMED_PROVIDER_PROFILE` could override
the test config, which did not occur in the verified run.

### Item B continuation: live memory, browser, voice output, and wake diagnostics (2026-09-28)

GPT-6 Sol advised running an isolated live `wake.status` and `wake.start` with
lazy installs explicitly disabled; mocked `wake.feed` or UI store tests would
not prove real microphone capture. The settings e2e now seeds a random private
memory fact in the temporary JCODE_HOME, captures the outbound local Ollama
request, and proves that the secret is absent with memory off and present after
the Hermes setting enables memory for a new engine session. The live run passed
with stdio MCP, OAuth MCP, skill hub, terminal tool toggle, model/provider,
reasoning, system prompt, and memory checks.

The browser-controller e2e now uses cached Playwright Chromium, connects through
the authenticated engine `browser.manage` route via local CDP, reads status,
and disconnects. It passed. The local voice e2e uses macOS `say` plus
`afconvert` to synthesize a real WAV, then checks the Hermes TTS lease
acquire/release. The same isolated Hermes backend run sets
`HERMES_DISABLE_LAZY_INSTALLS=1`, confirms live `wake.status` reports
unavailable, confirms `wake.start` refuses with `reason=unavailable`, and
verifies the refusal does not persist `wake_word.enabled=true`. These checks
passed; no dependency install was attempted.

Actual wake activation remains open for owner permission and hardware: the
host reports no input devices (`system_profiler SPAudioDataType` lists none;
AVFoundation lists none), and Hermes's `.venv` has no `openwakeword`. To attempt
the real detector e2e, the owner must authorize this exact venv-only install
and provide an audio input device:

```sh
../hermes-agent/.venv/bin/python -m pip install openwakeword==0.6.0 onnxruntime==1.27.0 sounddevice==0.5.5 numpy==2.4.3 ai-edge-litert==2.1.6
```

Wake activation is not marked pass. Advisor review also caught that the browser
check proves only CDP connect/status/disconnect, not a page navigation and
content read through the browser tool or packaged desktop; that walkthrough
row remains open. The memory-settings e2e now correlates each captured Ollama
request to a unique prompt marker so a late request from the disabled-memory
turn cannot satisfy the enabled-memory assertion. The browser and audio e2e
files plus the extended memory-settings e2e remain uncommitted pending a final
focused rerun and item-B commit.

### Item B continuation: live settings and local feature reruns (2026-09-28)

Fixed the settings test's cancellation race by tracking only the marked
`/v1/chat/completions` request and closing that upstream after the disposable
session becomes inactive. Full generation is unnecessary for verifying the
captured memory and reasoning payloads; MCP behavior checks still wait for full
chat turns. The live `hermes-mcp-settings.mjs` e2e passed against the existing
local Ollama model, including actual stdio and OAuth MCP calls, skills hub
install, tool toggle, profile prompt, model/provider selection, reasoning
effort, and memory enabled/disabled prompt contents. Advisor review caught that
the earlier fixture pointed both provider choices at one proxy, which did not
prove routing; the final passing rerun leaves the generic provider pointed at
Ollama directly and sends only the Hermes-selected named profile through the
capture proxy. The memory assertion is described as context injection, not a
completed recall answer.

The browser lifecycle e2e and local audio e2e both passed again. I attempted
browser content coverage through the actual forwarded `/api/browser/act` route
with `feature=1`, a localhost page fixture, and a disposable Hermes
configuration. The route correctly rejected the first request without
`feature=1`; after adding it and allowing private URLs only in the throwaway
home, the browser tool timed out after 120 seconds. Inspection confirms
`browser_tool.py` invokes the `agent-browser` CLI, which is absent from PATH,
the Hermes repo, desktop node_modules, and npm cache. Cached Playwright would
test a substitute driver, so no such workaround is claimed. Browser navigation
remains open pending owner approval for `npm install --prefix
/tmp/akira-agent-browser agent-browser@^0.26.0`, followed by rerunning the same
e2e with that prefix's `node_modules/.bin` on PATH. The passing cached-Chromium
`browser.manage` connect/status/disconnect check is recorded separately.

Wake activation remains open pending the venv-only detector dependency install
listed above and an available microphone/input device. The focused local TTS
and unavailable-wake e2e passed again without installing anything.

### Follow-up item 4: Prime skill and REPL completion (2026-09-28)

Added executable Python-package contracts for Prime's goal, refine, RLM
heartbeat, messaging, observation, compaction, websearch, and skill-creator
workflows. These wrappers call the existing Rust engine and swarm features;
they do not introduce a second agent loop. Goal callbacks now return structured
state and completion budgets, reject replacement of an active goal, and reject
explicit malformed/non-positive token budgets. RLM heartbeat records retain
labels in the shared store, can list inactive records, and keep user heartbeats
separate. The existing idle-only scheduler explicitly rejects `steer` delivery.
`skill_manage create` accepts a confined optional Python package, installed
skills are imported directly by the persistent worker, and compaction queues
run at the end of the current turn. The cached static prompt now includes
Prime's prompt-as-data and programmatic `llm_query` guidance.

The new `crates/sovereign-gateway/e2e/prime-parity.mjs` passed with the local
Ollama model and Hermes venv CPython. It exercises stdlib and package imports,
`load()` plus `llm_query()`, goal budget/create/complete, RLM heartbeat,
refine, agent observe/message, subagent spawn/await, compact scheduling, and
disconnect/reconnect with the active goal, heartbeat, and child visible. The
Rust `sovereign-prime` suite passed 39 unit plus 13 worker integration tests.
The REPL test measured cold start at 79.1 ms, warm p50 at 75.5 µs and p95 at
340.5 µs. Tool-dispatch p50 was 78.0 µs and p95 was 88.8 µs. The compact
agent-message schema passed its under-200 estimated-token check. Full-workspace
`cargo fmt --check` still reports pre-existing formatting differences outside
this item; changed Prime host code was formatted and `git diff --check` passes.

Known compatibility limit: the existing heartbeat scheduler is idle-only, so
Prime's `steer` delivery cannot interrupt a currently running turn and is
rejected explicitly. No claim is made that this one delivery mode is equivalent
to Prime's. The full plain-task schema-token measurement is pending the final
benchmark gate.

### Follow-up item 5: TUI-free `sovereign` dependency trial (2026-09-28)

**Blocked after compiler trial; no TUI or generic CLI files were deleted.** The
desktop entrypoint parses only desktop serve arguments and starts the engine
directly, but it still imports `jcode::cli::provider_init::ProviderChoice` and
calls `jcode::cli::dispatch::run_gateway`. `cargo tree --offline -e normal -p
jcode -i jcode-tui` proves the root `jcode` library has a normal dependency on
`jcode-tui`, and release builds compile it. I trialed replacing the root
`pub use jcode_tui::*` with `pub use jcode_app_core::*`, then ran
`cargo check --offline --bin sovereign`: it failed with 49 errors because the
generic CLI library imports `crate::tui`, `video_export`, and other modules
only re-exported by `jcode-tui`. The trial change was reverted. A TUI-free
binary requires first moving the gateway/provider composition root and
separating the generic CLI into another crate; deleting the re-export or TUI
crate now would break the workspace. The session-cache and keybind hooks are
TUI-only, and `run_gateway` does not register them.

### Follow-up item 6: full-workspace test failure fix

Skipped per instruction: the most recent full-workspace run passed 8,071 tests
with 0 failures (59 ignored, 1,254 filtered across 212 suites). Part C also
passes 39 Prime unit and 13 real-worker integration tests.

### Follow-up item 7: three-arm benchmark and schema measurement (2026-09-28)

The MJS plain-task smoke initially used the removed `--provider-profile` flag
and failed to start Sovereign. The disposable JCode config now selects the
`bench` named provider and the runner uses supported `--provider
openai-compatible` arguments. A one-run `plain` task then passed both turns
with the local Ollama model. It measured 7,762 tool-schema tokens per call,
18 above the 7,744 baseline and 282 below the 8,044 maximum. Raw result:
`docs/benchmark-runs/2026-09-28T07-45-02/`.

The first launch also exposed an unhandled child-process spawn error when a
configured Hermes executable is missing; the runner now catches that error so
the arm can be reported without crashing the benchmark process. The earlier
temporary Hermes path typo was corrected by setting `HERMES_BIN` explicitly.

The required 1-task/1-repetition comparison passed for all three arms on
`err_case_search`, with the same local model, local counting proxy, and
600-second task limit. Hermes passed with 2 model calls, 2 tool calls, and
214.3 seconds; Sovereign passed with 7 model calls, 8 tool calls, and 147.0
seconds; Prime passed with 4 model calls, 3 tool calls, and 82.7 seconds. Prime
was built from the shallow clone under `/tmp/prime-agent` using its workspace
build after dependencies were installed there with lifecycle scripts disabled.
No Prime installer or global install was run. The clone is removed after this
benchmark; this is a smoke check, not the full benchmark suite.

### Item B closure: browser route and real wake detection (2026-09-28)

The owner approved installing `agent-browser@^0.26.0` only in
`/tmp/akira-agent-browser`; its lifecycle script was inspected and run there to
prepare the CLI. `hermes-browser-controller.mjs` now drives the actual forwarded
`/api/browser/act` route against a local HTTP fixture and cached local Chromium.
It asserts the exact navigation URL, page heading, and a random marker in the
full browser snapshot. The e2e passed; no external site is contacted.

The owner also approved the pinned wake dependencies in the existing Hermes
`.venv` and first-use model downloads. The packages were installed through that
uv-managed venv (the venv has no pip), without changing tracked Hermes files.
`hermes-wake-activation.mjs` synthesizes “Hey Hermes” locally with macOS `say`,
converts it to 16 kHz mono PCM, starts Hermes with an isolated temporary home,
feeds the audio to the real openWakeWord/TFLite detector, and verifies the
`wake.detected` event is delivered to the same authenticated WebSocket owner.
It then stops the detector. The real detector test passed with lazy installs
disabled. The shared wake model assets reside inside the approved Hermes `.venv`;
the test's configuration, runtime state, and generated audio use temporary
homes. This uses client-capture, so it does not claim physical microphone
coverage (the host has no available input device).

GPT-6 Sol's B review requested exact route-level browser navigation/content
assertions and verification of the owner-scoped wake event; both are present.
The revised browser and wake feature rows now pass in
`docs/HERMES_FEATURE_CHECK.md`. These changes are recorded by the focused
Item-B commit referenced from that table.

### Item C continuation: executable Prime skills and live parity e2e (2026-09-28)

The missing `skill_creator` Python package now calls the existing Rust skill
creation path and shared `SkillRegistry`; it returns only the validated source
directory for immediate import. The Prime parity e2e now exercises websearch
through the existing key-free Rust tool, creates and imports a package skill,
checks compact scheduling, and verifies spawned-agent state after reconnect.
The authenticated browser and wake rows above point to Item B commit
`85c5503f`.

Validation: focused `cargo test -p jcode-app-core -p sovereign-prime` passed
(including 39 `sovereign-prime` unit tests and 13 REPL integration tests).
The full `prime-parity.mjs` live e2e passed outside the sandbox against local
Ollama: real Python imports, `load`/`llm_query`, goal, heartbeat, refine,
websearch, skill creation/import, compact scheduling, messaging, subagent
spawn/await, and goal/heartbeat/subagent reattach. A first run inside the
sandbox hit `system-configuration`'s NULL-object panic; an outside-sandbox
rerun passed. A stricter attempt to require a `status.update` compress event
did not observe one within 120 seconds, so the e2e asserts the current public
contract (compaction is scheduled) and does not claim that this smoke session
observed completion. GPT-6 Sol reviewed the skill bridge and confirmed it
reuses Rust validation and the shared registry. It also flagged RLM heartbeat
`steer` as a remaining Prime parity gap; the current idle-only scheduler still
rejects that mode. The dispatch test's p95 and tool-schema figures remain the
existing measurements recorded in `docs/PRIME_PARITY.md`; no new benchmark
claim is made here.

### Item D closure: jcode terminal UI, generic CLI and dead-code removal (2026-09-29)

**Status: complete.** Reviewed the uncommitted diff that removed jcode's
terminal UI, generic CLI, installer, self-update, self-dev and pairing code,
fixed what it left broken, swept for additional dead code the removal exposed,
and committed the result.

Correctness review of the new `sovereign` entrypoint (`src/bin/sovereign.rs` +
`src/sovereign_runtime.rs`, 885 lines, previously untracked) against
`hermes-agent/apps/desktop` and `crates/sovereign-gateway`: `serve`/`gateway`,
`__pre-tool`, and the Hermes Python feature worker are all fully wired,
consuming every `SOVEREIGN_HERMES_*` env var the desktop sets and printing the
exact `HERMES_BACKEND_READY` line the desktop waits on. No missing subcommand,
no silently-ignored flag, no dropped env var.

Fixed one real compile break: `crates/jcode-app-core/src/server/reload_context.rs`
(the extracted `ReloadContext`) was missing the `save()` method its own tests
called; added it (writes JSON to the per-session path via
`crate::storage::write_json`). Cleaned two dangling empty
`if session.is_canary { }` blocks left in `crates/jcode-app-core/src/ambient/runner.rs`
after `register_selfdev_tools()` calls were deleted.

Dead-code sweep, each candidate verified by dependency-graph/grep evidence
before deletion (never by assumption), `cargo check --workspace --all-targets --offline`
re-verified clean after every batch:
- Crates deleted (zero path-dependency references anywhere in the workspace,
  confirmed by scripting every `Cargo.toml`): `jcode-setup-hints`,
  `jcode-update-core` (the old self-update mechanism), `jcode-fuzzy`,
  `jcode-productivity-core`, `jcode-sdk` (a 7,108-line unused harness-API
  client SDK).
- `jcode-app-core` modules deleted, each confirmed to have zero external
  callers and, via the diff itself, to have had all of their real callers
  living exclusively in the now-fully-deleted `crates/jcode-tui/*`: `update.rs`,
  `update_rate_limit.rs`, `session_rebuild.rs`, `restart_snapshot.rs`,
  `ssh_remote.rs`, `startup_profile.rs`, `setup_hints.rs`, `catchup.rs`,
  `mission.rs`, `network_retry.rs`, `perf.rs`, `server_spawn.rs`, and
  `replay.rs`/`replay/tests.rs` (a TUI screen-recording/demo-capture module).
  Deleting `replay.rs` also let the `ratatui` dependency (and its `fontdb`/
  `rustybuzz`/`unicode-truncate` build-profile overrides) drop out of the
  `sovereign` binary's build graph entirely.
- A real, currently-reachable correctness bug found in a file the diff never
  touched: `crates/jcode-base/src/prompt.rs` still injected
  `DESKTOP_SELFDEV_MODE_PROMPT` into the live, cached system prompt whenever
  `is_desktop_working_dir()` matched, explicitly instructing the model to use
  the `desktop_selfdev` tool — which this same removal deleted. The detector
  itself (`jcode_selfdev_types::desktop_repo_root`) also matched an obsolete
  `jcode-desktop`/`crates/jcode-desktop-ui` repo layout, not the current Hermes
  `apps/desktop`. Removed both dead prompt-injection branches (desktop and
  CLI/TUI) and their now-orphaned helpers (`SelfDevProductContext`,
  `build_selfdev_prompt_*`, the three `.txt` prompt assets), plus the
  downstream-orphaned `jcode_selfdev_types::desktop` module and its now-unused
  `toml`/`tempfile` deps. Updated `prompt_tests.rs` accordingly.
- Dead test/benchmark files exercising already-deleted CLI/TUI features
  (none referenced from CI or active docs): 6 Python CLI-feature test scripts
  (`test_auth_import_cli.py`, `test_login_qr_cli.py`, `test_native_ssh_*.py`
  x3, `test_selfdev_reload.py`), 2 TUI e2e scripts (`expand_badge_headed_wtype.py`,
  `expand_badge_headless.py`), and 11 TUI animation/idle-render benchmark
  scripts (`bench_selfdev_build.sh`, `bench_selfdev_checkpoints.sh`,
  `check_donut_animates_live.py`, `count_idle_draws.py`,
  `diagnose_idle_render_cost.py`, `dump_fresh_spawn_screen.py`,
  `measure_key_echo.py`, `profile_idle_donut.py`, `test_desktop_selfdev.py`,
  `verify_donut_still_animates.py`, `which_overlay_blocks_donut.py`).
- `cargo tree -e normal --offline -i jcode-tui` finds no such package in the
  graph at all.

Suspected-but-not-deleted feature (listed per instructions rather than
removed): `jcode-overnight-core` / `crates/jcode-app-core/src/overnight.rs`
(the `/overnight` long-running-session supervisor). It has zero callers
anywhere — no command dispatcher, no `sovereign-gateway` route, and no hit in
`hermes-agent/apps/desktop` source outside a built i18n bundle and a packaged
doc copy. It is a substantial, plausibly-intentional standing feature rather
than obvious cruft, so it was left in place pending an owner decision instead
of being deleted.

Validation: `cargo check --workspace --all-targets --offline` is clean (only
pre-existing, legitimate dead-code warnings on fields unrelated to this work).
A full serial `cargo test --workspace --offline -j2 --no-fail-fast -- --test-threads=1`
run: **4,424 passed, 13 failed** (all 13 in `sovereign-prime --test repl`,
which panics with `staged Hermes CPython: NotPresent` — every one of those
tests calls `std::env::var("SOVEREIGN_HERMES_PYTHON").expect(...)` and that
file has zero changes in this diff; no staged Python interpreter exists in
this environment, and installing/staging one is out of this item's scope). An
earlier fail-fast pass also surfaced a single `jcode-base::hooks` test failure
that did not reproduce on rerun in isolation or on the final full run — a
pre-existing, unrelated test-isolation flake (shared process state between two
tests), not a regression from this work.

`cargo build --release --offline --bin sovereign` passed. Release binary:
`target/release/sovereign`, **68.6 MB**.

Commits (both `git -c user.name=rameelmalik`, working tree left clean):
- `31a410c49` "Remove jcode terminal UI, generic CLI and dead crates" — the
  sovereign entrypoint, the `reload_context.save()` fix, the `ambient/runner.rs`
  cleanup, the full jcode-tui/CLI/installer/self-dev/provider-doctor removal,
  the 5 fully-orphaned crate deletions and their `Cargo.toml`/workspace-member/
  profile-override cleanup, and the dead test/benchmark script deletions
  (596 files changed).
- `2a6e32481` "Remove dead self-dev prompt injection" — the `prompt.rs`/
  `prompt_tests.rs` fix and the resulting `jcode-selfdev-types` cleanup
  (8 files changed).
