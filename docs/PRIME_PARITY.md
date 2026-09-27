# Prime Agent parity

Audit basis: Prime Agent README, `docs/usage.md`, `docs/rlm.md`,
`docs/rlm-runtime.md`, `docs/skills.md`, `docs/long-running-agents.md`,
`docs/architecture.md`, `src/core/prompts/rlm.ts`, and its built-in skill
packages. The Prime repository is reference-only; Akira keeps its single Rust
agent loop.

| Feature | Prime behavior | Akira implementation | Status |
|---|---|---|---|
| Persistent REPL state | Long-lived Python state per session | `crates/sovereign-prime/src/host.rs` and `python_worker.py`; lazy CPython worker, idle reap, session cleanup | Done; 9 REPL integration tests pass |
| Python imports | CPython stdlib and skill packages | Isolated CPython with `-I -S`; stdlib imports and installed skill paths are tested | Done for imports and user-installed packages |
| File context | `load(path)` reads workspace files | Host resolves symlinks, confines reads to workdir, caps at 8 MiB | Done |
| Recursive model calls | Programmatic `llm_query` | Existing-provider callback with per-run host-call cap | Done |
| Kernel lifecycle and limits | Lazy worker, timeout, memory cap, recovery | 20 s run timeout, 128 MiB RSS watchdog, 10 min idle reap, lazy restart; macOS RSS via `proc_pidinfo` | Implemented; cold/warm latency budgets lack a bench gate |
| Kernel isolation | Restricted Python side effects | macOS `sandbox-exec` denies network and limits file writes to project/session temp; other platforms require approval per cell and headless sessions deny | Implemented; macOS sandbox tests pass outside restricted test sandbox |
| Spawn a subagent from REPL | Spawn and obtain child lifecycle/result | `spawn_subagent` calls the existing `delegate`/swarm path | Partial; spawn is bridged, but await/result lifecycle is not exposed |
| Agent messaging | Send, read, list family agents | Python `agent_message` uses swarm internals; model-facing `agent_message` tool routes send/read/list through `CommunicateTool` | Partial; compact schema stays below 200 estimated tokens and reads now target agent context, but Prime role-addressed family observation is not fully ported |
| Goals | Get/create/complete persistent objective | Rust `ControlStore`, `/goal`, session goal tool, and REPL host callback | Partial; basic operations work, Prime token budget and result shape are absent |
| User heartbeat | Schedule prompts for a session | `/heartbeat`, `ControlStore`, and Python host callback | Done for user heartbeats |
| RLM internal heartbeat | Kernel-managed recurring callbacks | No distinct internal RLM heartbeat storage or API | Gap |
| Refine / continual harness | Local/global CRUD, rollback, evidence and rationale | Rust `sovereign-prime` entries/harness/refine; REPL callback schedules or checks refine | Done in Rust; Prime Python skill package is missing |
| Executable Python skills | `SKILL.md` plus importable package | Existing store imports user-provided package paths; skill creation currently creates instructions only | Partial; package authoring and bundled Prime wrappers are missing |
| Prime skill packages | goal, refine, heartbeat, messages, observe, compact, websearch, skill creator | Equivalent Rust features exist for several jobs; no Prime-compatible package bundle/bridge for all listed APIs | Gap |
| Web search | Prime Serper integration | Existing Rust `websearch` tool uses its key-free configured backend | Rust tool works; Python skill wrapper missing |
| Compaction | Check and schedule host compaction | Engine compaction is available to chat and desktop | Rust path exists; REPL skill bridge is missing |
| RLM static prompt | Prompt-as-variable and programmatic subcalls | Concise static section added to `crates/jcode-base/src/prompt/system_prompt.md` | Done |
| Detach / reattach | Long-running session survives client disconnect | Gateway persists sessions and control state | Partial; combined session/goal/heartbeat/subagent reattach e2e is missing |
| Prime terminal, installer, hosted services | Prime-specific UI, deployment, and paid/hosted services | Not included in Hermes desktop product | Intentionally out of scope |

## Remaining Part 3 work

The Monty runtime and dependency were already removed; real CPython runs lazily
behind the existing engine-owned framed pipe. Part 3 is not complete. The
remaining work is the Prime skill package bundle and complete host API
(`rlm_heartbeat`, `agent_observe`, compaction, and exact goal semantics),
subagent result/await support, combined reattach e2e, and a failing latency
budget benchmark for dispatch, warm REPL, and cold start. No measurements are
claimed for those budgets. The required shallow clone attempt failed because
the sandbox could not resolve `github.com`; the Prime skill source bundle was
therefore not ported in this pass.
