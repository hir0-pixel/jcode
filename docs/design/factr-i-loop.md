# Factr-I loop: what was taken from Prime, Hermes and outside harnesses

jcode's loop (`crates/jcode-app-core/src/agent/turn_loops.rs`) stays the loop. Only mechanisms that an original (Hermes, Prime) has, or that outside evidence backs, were added. `PA` = `/tmp/prime-agent/packages/coding-agent/src/core`, `H` = stock Hermes.

## Taken (built and tested)
| Mechanism | Source | Where |
|---|---|---|
| Repeated tool-call guard (warn at 3, stop at 5; same failing result counts; `bg` and polls exempt) | Hermes `agent/tool_guardrails.py` | `agent/repeat_guard.rs`, span `loop.guard` |
| Tool output in history capped at 50 KB, 40% head / 60% tail, full text spilled to a private file (0600, purged after 7 days) | Hermes `tools/tool_output_truncate.py`, Prime `tools/truncate.ts` | `agent/tools.rs` |
| Structured compaction summary (Goal / Constraints / Progress / Key Decisions / Next Steps / Critical Context) and the live todo list re-attached | Prime `compaction/compaction.ts` | `jcode-compaction-core` |
| Old tool results pruned before the summary: identical results deduped, old large results reduced to a one-line gist, big tool-call arguments shrunk; last 4 messages protected | Hermes `agent/context_compressor.py` (`_prune_old_tool_results`); outside evidence: observation masking matched LLM summarization at about half the cost (arXiv 2508.21433) | `jcode-compaction-core/src/prune.rs` |
| Malformed tool-call JSON repaired before "invalid arguments" is reported | Hermes `_repair_tool_call_arguments` | `agent/tool_args_repair.rs` |
| Goals, subgoals, autonomous mode with gates and limits, verifier ratchet with checkpoints, supervisor on plateau | Prime + NVIDIA AVO | `sovereign-prime/src/agent_loop.rs`, `goal_ratchet.rs` |
| Response recovery (empty/partial/truncated replies) | jcode (no equivalent in Prime) | `agent/response_recovery.rs` |
| Hermes tool names resolve to native tools (`terminal`, `read_file`, `write_file`, `patch`, `search_files`, `web_extract`, `web_search`) | Hermes prompts/skills | `jcode-tool-types` |

## Not taken, with reason
- Prime's Python-REPL-only tool model and 6.9 KB doctrine: jcode already has a `repl` tool and a 1.3 KB base prompt; RAM would rise.
- Prime's small turn limits (12 turns / 3 continuations): Hermes and jcode are more generous.
- Wait-and-resume on provider usage limits (Prime `agent-session.ts:1148`): the harness runs on local Ollama.
- Later compaction threshold (Prime): unmeasured trade-off against the cache reset.

## Candidates that wait for benchmark data (no original has them, or unmeasured)
- Parallel tool execution (Prime `agent-loop.ts:667-726`): jcode runs calls one by one and offers `batch`. Port only if a benchmark shows wall-time loss.
- Text-parsed tool-call fallback for small local models (Goose "tool shim", outside): neither Hermes nor Prime has it; add only if tool-call failures show up in the local-model runs.
- OpenHands-style alternating-action stuck detection (outside): add only if the repeat guard misses real loops.

GAIA: no GAIA code exists in Prime (only SWE-bench in its docs); do not run it until asked. NVIDIA AVO (arXiv 2603.24517) reached 100% on the ARC-AGI-3 public set; its ratchet and supervisor are already in `goal_ratchet.rs`.

## Added after the tool-call audit (all tested; each justified by Hermes, Prime or jcode)
| Mechanism | Source | Notes |
|---|---|---|
| Fuzzy `edit` fallback (line-trimmed, then whitespace/smart-quote normalized; unique whole-line match only; relative indentation must match; CRLF kept; result says "fuzzy match at line N") | Hermes `tools/fuzzy_match.py`, Prime `edit-diff.ts` | `tool/edit_fuzzy.rs` |
| One-line syntax note after edit/write/patch (Python, JSON; JS/Go/Rust only if a checker is on PATH; never refuses the write; skips when unsure) | Hermes `file_operations.py` | `tool/syntax_check.rs` |
| Short completion guidance in the prompt; tool-use enforcement line only for Hermes' model families (not Claude) | Hermes `prompt_builder.py` | `system_prompt.md`, `prompting.rs` |
| Stop nudges: run tests after code edits; continue after an announced action without a tool call; one per turn; `JCODE_VERIFY_ON_STOP=0` disables | Hermes `verification_stop.py` / `turn_stop_gates.py` (opt-in there, default on here) | `agent/stop_nudge.rs` |
| Repeat guard blocks the call at 5 (same tool and input) and ends the turn only at 10 | Hermes `tool_guardrails.py` | `agent/repeat_guard.rs` |
| Bedrock: retry with jittered backoff, prompt caching on supported models, cache usage reported, snake_case stop reasons, tools enabled for newer Claude ids, 32k default max tokens, consecutive same-role messages merged | Hermes/Prime Bedrock adapters | `jcode-provider-bedrock` |
| Streamed tool calls without an id get one; truncated (length-stop) tool calls are discarded unless their JSON parses unrepaired | Hermes | `jcode-provider-openrouter`, `response_recovery.rs` |
| Bash: 48 KB output cap (12 KB head + tail, full text spilled), timeout promotes to a background task, process groups killed on turn cancel, session delete, cron run end and SIGTERM | Prime `bash.ts`, Hermes terminal tool | `tool/bash.rs`, `background.rs` |
| Unattended runs allow commands whose every target is inside the session working directory (symlink escapes, `..`, `/`, `$HOME`, unresolved variables still denied) | Hermes `approvals.mode` semantics, Prime (no gate) | `jcode-command-risk`, `approvals.rs` |
| Bridge answers the daemon's stdin probe without ending the turn early (was F2) | jcode fix | `docs/benchmark-findings.md` |

## AVO mapping: validate through execution (plain-mode auto-verify gate)
NVIDIA AVO's ingredients (NVIDIA blog on AVO reaching 100% on ARC-AGI-3): persistent memory of prior attempts and evaluation results, a supervisor that redirects on stagnation, tools to edit and validate through execution, an iterative inspect/plan/implement/evaluate loop. The blog gives no per-ingredient ablation. Factr-I already had memory (one store), the ratchet, plateau detection and supervisor (goal mode, `sovereign-prime/src/agent_loop.rs`, `goal_ratchet.rs`). The missing piece for ordinary prompts was the execute-and-evaluate loop, added as a host-side gate:
- After code edits, when a turn ends with no tool call, the host finds the project's test command by marker file (Cargo.toml, go.mod, package.json, gradle, pom.xml, CMakeLists.txt, `*_test.py`/`test_*.py`), runs it in the session cwd (120 s, own process group, pipes drained), feeds the last 3000 characters of a failure back and continues (up to 3 rounds; stops when two consecutive failures hash the same, timings ignored), ends on a pass. No marker: only the test nudge. Skipped for subagents, goal and autonomous sessions, and when the last test run came after the last edit and passed. Edits to test files are flagged (`tests_touched`) in the `loop.guard` spans (`auto_verify_pass|fail|timeout|skip_no_marker`).
- Config `agents.auto_verify` (default on), `JCODE_AUTO_VERIFY=0` disables; `agents.auto_verify_timeout_s`, `agents.auto_verify_rounds`.
- Reporting rule for benchmarks: this internalizes a retry that stock Prime and Hermes do not have. Report three numbers: gate off, gate on, and after the runner's external retry; label the gate-on number "host-internal verify, up to 3 rounds" and report extra tokens and wall time per round.
- Sources: Prime autonomous gates (`PA/autonomous.ts`), Hermes verify detection (`coding_context.py`, `verification_stop.py`), AVO's evaluate-through-execution loop. Bugs fixed while verifying end to end: the unittest fallback skipped `*_test.py` (false "0 tests OK"), and the identical-failure hash included timings.
