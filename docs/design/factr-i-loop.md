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
