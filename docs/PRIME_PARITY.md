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
| Goals | Get/create/complete persistent objective | Rust `ControlStore`, `/goal`, session goal tool, and REPL host callback | Partial; REPL `rlm.host_request` now maps get/create/complete to the existing callback; Prime token budget and richer result shape remain absent |
| User heartbeat | Schedule prompts for a session | `/heartbeat`, `ControlStore`, and Python host callback | Done for user heartbeats |
| RLM internal heartbeat | Kernel-managed recurring callbacks | `agent_loop_host::heartbeat_host` persists `source=rlm` records in the shared heartbeat table; Python wrapper bridges CRUD | Partial; scheduling shares the engine scheduler, but Prime delivery modes are not represented |
| Refine / continual harness | Local/global CRUD, rollback, evidence and rationale | Rust `sovereign-prime` entries/harness/refine; bundled Python `prime-refine` wrapper | Rust feature, package, and host request mapping work |
| Executable Python skills | `SKILL.md` plus importable package | Bundled wrappers installed lazily under `~/.jcode/skills`; the worker adds each package `src` to `sys.path`; user directories are preserved | Partial; `/skill create` still authors markdown only |
| Prime skill packages | goal, refine, heartbeat, messages, observe, compact, websearch, skill creator | Bundled package set under `crates/sovereign-prime/src/skills`, installed by `bundled_skills.rs` | Partial; goal/refine/heartbeat/websearch and basic child messaging/observation bridge; role-addressing and full observe schemas are not equivalent |
| Web search | Prime Serper integration | Bundled `prime-websearch` calls the existing Rust DuckDuckGo tool | Done with the key-free backend; no Serper key or second implementation |
| Compaction | Check and schedule host compaction | Engine compaction is available to chat and desktop; `prime-compact` package is shipped | Partial; REPL bridge directs users to `/compact` because no in-turn compaction host API exists |
| RLM static prompt | Prompt-as-variable and programmatic subcalls | Concise static section added to `crates/jcode-base/src/prompt/system_prompt.md` | Done |
| Detach / reattach | Long-running session survives client disconnect | Gateway persists sessions and control state | Partial; combined session/goal/heartbeat/subagent reattach e2e is missing |
| Prime terminal, installer, hosted services | Prime-specific UI, deployment, and paid/hosted services | Not included in Hermes desktop product | Intentionally out of scope |

## Remaining Part 3 work

Monty is removed; real CPython runs lazily behind the engine-owned framed pipe.
The bundled Prime packages now include goal, refine, RLM heartbeat, messaging,
observation, compact, websearch, and skill-creator. RLM heartbeats are stored
separately from user heartbeats and covered by Rust plus real-worker tests. The
remaining gaps are exact role-addressed messaging/observation, REPL compaction,
Python package creation from `/skill create`, delegate await/result, the
combined reattach e2e, and latency budget benchmarks. No measurements are
claimed for those budgets; Part 3 is incomplete. A shallow read-only Prime
clone was used as reference and is removed before this task ends.
