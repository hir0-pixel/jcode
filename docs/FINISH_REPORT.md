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

**Status: blocked/incomplete after the prescribed implementation attempt.** The RPC changes implement buffered `session.events.since`/`stats`, transcript-derived `session.context_breakdown`, engine-backed `session.cwd.set`/`session.workspace.move`, and engine-metered `usage.bars`. The cwd change adds a harness request that persists the new directory. False Rust placeholders for methods already implemented by Hermes Python were removed so those methods forward to their real owner. `cargo check -p sovereign-gateway --tests --offline` passed with only the two pre-existing observability dead-code warnings; `git diff --check` passed. The focused replay test passed before the final compile.

I inspected the eight route implementations in Hermes `hermes_cli/web_routers/sessions.py` and the engine importer in `crates/jcode-base/src/import.rs`. The engine has file-backed snapshots and external resume-ID import, but no equivalent metadata store APIs for transactional bulk deletion, bulk import, empty-session selection/deletion, full session statistics, filtered prune, or compression-lineage latest-descendant. Its importer can import a known resume ID but cannot enumerate foreign sessions or preview one. The REST handler therefore still refuses these eight routes and the three `session.foreign.*` RPCs remain unsupported. `python3 scripts/parity.py` confirms 9 placeholder RPCs and 8 refused routes; no route curl assertions were added because the required handlers are not implemented. This item is not claimed as passing. Proceeding to item 3 per the queue instruction.

## Follow-up item 3: settings reach engine chat

**Status: blocked/incomplete after tracing the settings paths.** Rust session creation and state already accept per-session model and reasoning effort, and the engine's provider-control code persists reasoning changes. I did not run a live settings-change e2e in this item. MCP and skill-hub settings have no bridge to the engine's source of truth: the bundled engine's `/api/mcp/servers` and skill-hub endpoints return empty data, chat reads `.jcode/mcp.json` and `~/.jcode/skills`, while Hermes APIs persist settings on the Python side. This means changes made in those Hermes settings screens cannot yet be proven to affect chat. Tool enablement, personality/system prompt, profile, provider/model and memory settings also lack the requested live behavior tests and source-of-truth audit. No e2e or bridge was added; the requested coverage is therefore not claimed. Proceeding to item 4 per the queue instruction.

## Follow-up item 4: Prime Python skills and budgets

**Status: blocked/incomplete after the requested reference review.** A shallow read-only clone was made in `/tmp/prime-agent`; Prime's `docs/skills.md` and the requested skill package sources were inspected. Its Python packages call the generic `rlm.host_request` interface for `agent_observe`, compaction, goals, role-addressed messaging, and internal RLM heartbeats. Akira's `python_worker.py` exposes only fixed functions (`llm_query`, `load`, `refine`, `goal`, user `heartbeat`, `spawn_subagent`, `agent_message`), so several upstream packages would fail if copied as-is. Existing goal/refine/user-heartbeat packages can map to the fixed bridge, but porting only those would not satisfy the requested skill set. The compact Rust `agent_message` tool and static RLM prompt guidance already exist; no edits were needed there. `prime-parity.mjs`, the combined reattach e2e, and dispatch/warm/cold budget assertions are absent, so the latency budgets and full parity status are unproven. The current schema baseline is 7,744 tokens; this item has no verified after-measurement yet. No tests or skill code were added, and Prime parity is not claimed complete. Proceeding to item 5 per the queue instruction.

## Follow-up item 5: dedicated sovereign entrypoint / TUI removal

**Status: blocked/incomplete after trying the suggested entrypoint approach.** I ran `CARGO_TARGET_DIR=... cargo tree -e normal --target aarch64-apple-darwin -p jcode -i jcode-tui --offline`; it confirms the product's root `jcode` library depends on `jcode-tui`. `src/lib.rs` unconditionally re-exports it. `src/bin/sovereign.rs` delegates to `jcode::cli::startup::run_from`, so it still enters generic CLI dispatch. The actual gateway initializer `run_gateway` is private in `src/cli/dispatch.rs` and depends on private `provider_init`, `server`, warm-Ollama lifecycle, feature-backend, and provider-selection setup. The standalone `sovereign-gateway` crate serves only after the harness socket and provider/server have been initialized; it cannot replace that bootstrap by being called alone. A real minimal binary therefore requires extracting the shared provider/server bootstrap and separating root CLI modules/features before the compiler can remove the TUI dependency. No isolated bootstrap refactor or crate deletion was made; keeping the minimum current dependency is necessary to preserve product startup. Historical Part 6 evidence also confirms seven TUI crates compile into the binary today. Item 5 is not claimed complete.

## Follow-up item 6: remaining failing test targets

**Skipped as instructed.** The latest recorded complete workspace run is green: 8,056 passed, 0 failed, 59 ignored across 212 targets. This item applies only if a target still failed after that full run.

## Follow-up item 7: Prime benchmark arm

**Status: blocked/incomplete after tracing and attempting the prescribed disposable install.** The Prime arm is present in `scripts/bench/abeval.py` and accepts `--arm prime`; the harness configures its `models.json` provider to the counting proxy, tags calls before prompt submission, uses Prime's documented JSON-RPC JSONL framing, and records tool events. Its command also selects `--daemon-socket`, which routes RPC mode through Prime's daemon client and makes the temporary socket path a plausible failure point; removing daemon coupling or establishing readiness is a candidate fix, but cannot be validated without running the CLI. This fresh shallow clone has no built CLI or `node_modules`. An offline install into a separate `/tmp` build copy failed exactly with `npm error code ENOTCACHED` for `@anthropic-ai/sandbox-runtime`. The network install of dependencies solely into that disposable copy was then rejected by automatic approval review because the Prime dependency tree may execute lifecycle/build scripts and download many third-party packages. Per the rejection, I did not retry through another route. Therefore the Prime arm was not runnable here and no 1-task/1-rep result is claimed; Hermes/Sovereign benchmark completion is recorded separately when the ongoing local-only run exits.

## Follow-up queue verification (2026-09-27)

This checkpoint verifies, but does not overstate, the earlier queue work.

| Item | Result | Verification / remaining gap |
| --- | --- | --- |
| 1. Cron worker leak and empty agent reply | Passed again | Release build succeeded. Packaged install-launch and chat-approval passed. The packaged cron-due test passed three consecutive times against this package. All three returned `sovereign-cron-agent-ok`, recorded `last_status=ok`, had `python:false` before delivery and after completion, and only showed Python running while delivery was active. |
| 2. Session RPC/REST parity | Implemented; later final audit passed | See “Final verification of Item A” below. The earlier failure report here is historical and superseded by the committed implementation and live rerun at `f0ba1ff`. |
| 3. Hermes settings reach engine chat | Incomplete | No per-setting live e2es or single-source-of-truth bridges were added. The MCP OAuth and skills-hub changes still do not have proof that they reach the engine chat. The earlier trace and gaps remain in the Follow-up item 3 section. |
| 4. Prime skill packages, messaging, reattach, budgets | Incomplete | Existing CPython kernel integration tests pass (9/9) and `sovereign-prime` tests pass (33 unit + 9 REPL); however Prime's package ports, combined `prime-parity.mjs`, dispatch/warm/cold p50/p95 bench, reattach scenario, and verified schema delta are still absent. Existing baseline is 7,744 schema tokens; the previously measured plain-task estimate was Sovereign 7,675 (delta -69), Hermes 10,664. |
| 5. Dedicated sovereign entrypoint and TUI removal | Incomplete | Release build succeeds but still compiles/links the generic `jcode` CLI and `jcode-tui`. The earlier dependency-tree and bootstrap trace shows the extraction work remains. |
| 6. Remaining test failures | Passed | Full workspace command with isolated homes, packaged Hermes Python, terminal color env, and local-only process permission exited 0: 8,056 passed, 0 failed, 59 ignored across 212 targets. SDK `set_working_dir` protocol snapshot/type and OpenRouter temporary-home isolation fixes are committed. |
| 7. Prime benchmark arm | Blocked after prescribed attempt | `abeval.py` has the third arm, but Prime CLI dependencies are absent. Offline install failed with `ENOTCACHED` for `@anthropic-ai/sandbox-runtime`; an escalated network install into the throwaway prefix was rejected by automatic review due to third-party lifecycle/build execution and broad dependency retrieval. No workaround was attempted. Hermes and Sovereign benchmark arms completed earlier; the Prime task did not run. |

Other final checks: live gateway `learning`, `refine`, `agent-loop`, `agent-run`, `accounting`, and `replay` passed; the `sessions` live script reported 16 successful behavior checks and one failing final forward-trace assertion. Desktop `npx tsc --noEmit -p .` passed. `stage:sovereign-python` and `pack` passed after the initial sandbox cache permission denial was retried with local-only escalation. `install-launch` and packaged chat-approval passed. The packaged `SOVEREIGN_CRON_AGENT=1` test passed three consecutive runs after the final pack; each job returned `sovereign-cron-agent-ok`, `last_status=ok`, and the bundled Python process was absent at idle after delivery.

Final `scripts/parity.py` still reports 235 RPC methods (52 Rust working, 9 placeholder, 174 Python-forwarded) and 8 refused REST routes. This is an explicit failed acceptance gate, not a sandbox limitation. No feature styling or themes were changed.

Verified source revision for the final gates: engine `cbe57e7834be337ee04d57f9562bcb0e92a1e36c`; Hermes `f242618d0ab4844b17e3cdacbd0e16e3e8d27d66`. The later engine commits only update this report and retain the already measured benchmark run; no source code changed after the successful build and gates.

## Follow-up Item 2: engine-owned session parity

**Status: implemented and committed.** The Rust gateway now handles the requested engine-backed session RPC methods and all eight `/api/sessions` routes that were previously refused. Session detail/transcript/export reads load the persisted session and replay its journal directly; export envelopes are accepted by import, and import validates the whole batch before writing. Global stats/empty/prune/descendant scans request complete session enumeration, prune rejects negative or non-finite ages, and mutation errors are returned instead of being counted as success. `session.foreign.*` uses the engine's Claude and Codex import readers; Codex listing uses the same non-writing parser as import.

Verification: `cargo test -p sovereign-gateway --offline` passed (47 passed, 2 ignored); `cargo build --release --bin sovereign` passed; `node crates/sovereign-gateway/e2e/sessions.mjs` passed outside the sandbox with local Ollama. Its session RPC coverage and all eight REST routes were exercised; each REST path returned 401 without a token and 2xx with a token using `curl`. The e2e also round-tripped the engine export envelope through import. In-sandbox execution still fails before startup with the known `system-configuration` NULL-object panic, so the local-only test was rerun outside the sandbox. `python3 scripts/parity.py` reports 0 placeholder RPC methods and 0 refused routes.

Remaining scope: this item makes the listed session routes real against engine storage; it does not claim the later feature-walkthrough/settings requirements or the entire follow-up queue are complete. Existing parity inventory still forwards the unrelated Hermes-owned features to Python.

## Follow-up Item 3: Prime REPL parity

**Status: blocked/incomplete after the prescribed source-fetch attempt.** The existing CPython worker already imports user skill packages from `~/.jcode/skills`, and the compact model-facing `agent_message` tool already had a schema-size assertion. This pass corrected agent transcript reads to call the swarm context-history action, enforced the 16-call host limit again on the Rust side, corrected the goal wrapper's create field and heartbeat's default operation, and clarified the cached RLM prompt with callable signatures. `cargo test -p jcode-app-core --lib agent_message::tests` passed (2 tests); `cargo test -p sovereign-prime --lib` passed (33 tests).

The required shallow clone command was attempted: `git clone --depth 1 https://github.com/PrimeIntellect-ai/prime-agent.git /tmp/prime-agent`; it failed with `fatal: unable to access ... Could not resolve host: github.com`. There was no existing `/tmp/prime-agent` or alternate local clone. Consequently the actual Prime skill package sources could not be audited and reused, and the Prime skill bundle, host parity for `rlm_heartbeat`/`agent_observe`/compaction, subagent result-await semantics, combined reattach e2e, `prime-parity.mjs`, and latency budget bench remain incomplete. This is the exact blocking condition for Item 3; continue with Item 4 as requested.

## Follow-up Item 4: Prime paper and component audit

**Status: paper audit complete; component parity and shared Prime execution remain incomplete.** Rechecked arXiv `2608.23552v1` §§2–3 and updated `docs/PRIME_PAPER_AUDIT.md`. It lists all benchmark outcomes and qualitative component credits. The paper gives no isolated numeric deltas for RLM or Continual Harness (and explicitly says targeted training is needed to isolate them), so no per-component percentages are claimed. The existing `abeval.py` has Hermes, Sovereign, and Prime arms on the same imported task set through the counting proxy; CLI `--help` confirms the arm interface. The Prime CLI itself is absent because Item 3's required clone failed DNS, so no new same-task 3-arm run or budget measurement was possible. This component limitation is reported rather than treated as a paper-proven parity result.


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

The new `rlm.host_request` mapping supports `goal.get/create/complete` and
`refine.status/run` by calling the existing Rust host callbacks. It rejects
unknown request names instead of fabricating support. The goal budget/result
shape and Prime's heartbeat, observe, compaction, role-based messaging, and
`rlm.collect` remain gaps. The targeted host-request integration test and all
10 REPL integration tests pass with the staged Hermes CPython runtime.

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

### Item 6: workspace test targets

Skipped as directed: the latest full workspace run recorded above passed 8,056
tests with 0 failures and 59 ignored across 212 targets.

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
