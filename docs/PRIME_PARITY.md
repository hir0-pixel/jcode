# Prime Agent parity

Audit basis: Prime Agent README, `docs/usage.md`, `docs/rlm.md`,
`docs/rlm-runtime.md`, `docs/skills.md`, `docs/long-running-agents.md`,
`docs/architecture.md`, `src/core/prompts/rlm.ts`, and its built-in skill
packages. The Prime repository is reference-only; Akira keeps its single Rust
agent loop.

| Feature | Prime behavior | Akira implementation | Status |
|---|---|---|---|
| Persistent REPL state | Long-lived Python state per session | `crates/sovereign-prime/src/host.rs` and `python_worker.py`; lazy CPython worker, idle reap, session cleanup | Done; 13 REPL integration tests pass |
| Python imports | CPython stdlib and skill packages | Isolated CPython with `-I -S`; stdlib imports and installed skill paths are tested | Done for imports and user-installed packages |
| File context | `load(path)` reads workspace files | Host resolves symlinks, confines reads to workdir, caps at 8 MiB | Done |
| Recursive model calls | Programmatic `llm_query` | Existing-provider callback with per-run host-call cap | Done |
| Kernel lifecycle and limits | Lazy worker, timeout, memory cap, recovery | 20 s run timeout, 128 MiB RSS watchdog, 10 min idle reap, lazy restart; macOS RSS via `proc_pidinfo` | Done; tests enforce cold start <500 ms and warm p95 <5 ms |
| Kernel isolation | Restricted Python side effects | macOS `sandbox-exec` denies network and limits file writes to project/session temp; other platforms require approval per cell and headless sessions deny | Implemented; macOS sandbox tests pass outside restricted test sandbox |
| Spawn a subagent from REPL | Spawn, await, and inspect child result | `spawn_subagent` and `await_subagent` reuse the existing delegate/swarm path | Done; real-worker and reconnect e2e cover child lifecycle |
| Agent messaging | Send, read, list family agents | Python `agent_message` uses swarm internals; model-facing `agent_message` tool routes send/read/list through `CommunicateTool` | Done for engine family roles; schema remains below 200 estimated tokens |
| Goals | Get/complete persistent objective and apply token budget | Rust `ControlStore`, `/goal`, session goal tool, and REPL host callback | Done; structured state, positive token-budget validation, and completion report |
| User heartbeat | Schedule prompts for a session | `/heartbeat`, `ControlStore`, and Python host callback | Done for user heartbeats |
| RLM internal heartbeat | Kernel-managed recurring callbacks | `agent_loop_host::heartbeat_host` persists `source=rlm` records in the shared heartbeat table; Python wrapper bridges CRUD | Partial; follow-up delivery is supported; `steer` remains unsupported because the shared scheduler is idle-only |
| Refine / continual harness | Local/global CRUD, rollback, evidence and rationale | Rust `sovereign-prime` entries/harness/refine; bundled Python `prime-refine` wrapper | Rust feature, package, and host request mapping work |
| Executable Python skills | `SKILL.md` plus importable package | Bundled wrappers installed lazily under `~/.jcode/skills`; the worker adds each package `src` to `sys.path`; `/skill create` can author optional package source | Done; imported package, host calls, and package creation are covered by tests |
| Prime skill packages | goal, refine, heartbeat, messages, observe, compact, websearch, skill creator | Bundled package set under `crates/sovereign-prime/src/skills`, installed by `bundled_skills.rs` | Done for supported engine operations; unsupported heartbeat `steer` mode reports an explicit error |
| Web search | Prime Serper integration | Bundled `prime-websearch` calls the existing Rust DuckDuckGo tool | Done with the key-free backend; no Serper key or second implementation |
| Compaction | Check and schedule host compaction | Engine compaction is available to chat and desktop; `prime-compact` schedules compaction at the end of the current turn | Done for the host scheduling API; the live e2e verifies scheduling |
| RLM static prompt | Prompt-as-variable and programmatic subcalls | Concise cached static section in `crates/jcode-app-core/src/agent/prompting.rs` | Done |
| Detach / reattach | Long-running session survives client disconnect | Gateway persists sessions and control state | Done; `crates/sovereign-gateway/e2e/prime-parity.mjs` reconnects and checks goal, heartbeat, and subagent state |
| Prime terminal, installer, hosted services | Prime-specific UI, deployment, and paid/hosted services | Not included in Hermes desktop product | Intentionally out of scope |

## Item C validation

Monty is removed; real CPython runs lazily behind the engine-owned framed pipe.
The bundled Prime packages include goal, refine, RLM heartbeat, messaging,
observation, compact, websearch, and skill-creator. RLM heartbeats are stored
separately from user heartbeats. The live `prime-parity.mjs` e2e covers package
imports, `load`/`llm_query`, goal and heartbeat persistence, refine, compact,
websearch, skill creation/import, agent messaging, subagent spawn/await, and
reconnect. The Rust dispatch test measured p95 at 88.8 µs; REPL cold start and
warm p95 are gated at 500 ms and 5 ms. The plain-task tool-schema measurement
is 7,762 tokens, +18 from 7,744 and within the +300 budget. One known Prime
compatibility gap remains: RLM heartbeat `steer` delivery is rejected because
the shared engine scheduler is idle-only. A shallow read-only Prime clone was
used as reference and removed after the audit.
