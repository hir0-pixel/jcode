# Prime Agent parity

Feature audit against Prime Agent README, `docs/usage.md`, `docs/rlm.md`,
`docs/rlm-runtime.md`, `docs/skills.md`, `docs/long-running-agents.md`,
`docs/architecture.md`, the RLM prompt, and built-in skill packages. Audited
against the desktop product as Akira; product and binary names are unchanged.

| Feature | Prime Agent | Akira implementation | Status |
|---|---|---|---|
| Persistent REPL variables | Long-lived CPython per session | `crates/sovereign-prime/src/host.rs` plus the `repl` tool; uses Monty, a Python subset without imports | Partial; real CPython is a gap |
| Python stdlib and package imports | Full Python kernel | Monty intentionally rejects imports | Gap |
| `load(path)` for large context | Host-mediated file loading | Monty host function in `crates/sovereign-prime/src/host.rs`, constrained to the worktree | Present |
| Recursive `llm_query` | Programmatic focused model calls | REPL host callback in `crates/jcode-app-core/src/tool/repl.rs` | Present; remains Monty-limited |
| Subagents from the REPL | `rlm.spawn`, admission handle and later result | `delegate` tool in `crates/jcode-app-core/src/tool/delegate.rs`; REPL `spawn_subagent` hook is unavailable by default | Partial; native delegation exists, kernel bridge is a gap |
| Agent-to-agent messaging | Role-addressed send/read/list and receipts | `delegate` routes through `CommunicateTool`; Monty host exposes a message hook, but no dedicated compact send/read/list model tool exists | Partial |
| Persistent goals | Goal lifecycle via host bridge | `/goal`, gateway autonomous endpoints, and `ControlStore` in `crates/sovereign-prime/src/agent_loop.rs` | Present in Rust; Python skill bridge missing |
| User heartbeat | Persistent scheduled user prompts | `/heartbeat` and the gateway wake path | Present |
| RLM internal heartbeat | Kernel-owned scheduled callbacks | No separate RLM heartbeat API | Gap |
| Continual harness / refine | Persistent local/global entries, rationale and rollback | `crates/sovereign-prime/src/{entries,harness,refine}.rs`, `/refine`, `/harness`, `/skill create`, and the refine tool | Present in Rust; Python skill package missing |
| Executable Python skills | `SKILL.md` plus importable package | Existing skill store and `/skill create` support instructions, not Python packages | Gap |
| Prime built-in skills: goal, refine, heartbeat, messages, observe, compact | Python packages forwarding to host | Corresponding Rust features exist for goal, refine, compaction, heartbeat, and swarm operations; package imports/host bridge are absent | Partial; Python package layer missing |
| Web search skill | Prime uses Serper | Rust `websearch` tool in `crates/jcode-app-core/src/tool/websearch.rs` uses configured key-free DuckDuckGo by default, with optional configured providers | Present in Rust; importable skill wrapper missing |
| Context compaction | Manual and automatic compaction | Engine compaction paths and desktop controls | Present; REPL skill wrapper missing |
| Long-running sessions and detach/reattach | Supervisor keeps sessions and children alive while clients detach | Gateway session recovery and persistent control stores | Present at the session layer; requested combined live reattach test is missing |
| Reattach with goal, heartbeat and subagent all alive | Explicit end-to-end continuity | Individual session, goal, heartbeat and delegation paths exist | Unverified; required Prime parity e2e missing |
| Cached RLM prompt guidance | Static prompt describes variable context and programmatic subcalls | No Prime RLM-specific static prompt section | Gap |
| Approval for real Python side effects | Python has host access and needs explicit policy | Existing approval hook in `crates/sovereign-gateway/src/approvals.rs` classifies shell commands only; Monty currently denies OS access | Gap blocks replacing Monty safely |
| Prime terminal UI, installer, hosted inference/team services | Prime-specific UI, setup and hosted products | Not part of the Hermes desktop product | Intentionally out of scope |

## Implementation decision

Monty remains in place. A real Python process could not meet the mandatory
memory cap on this macOS host: attempts to set `RLIMIT_DATA`, `RLIMIT_AS`, and
`RLIMIT_RSS` to a finite value fail with `EINVAL`/`ValueError: current limit
exceeds maximum limit`. The approval gate also only applies the shell risk
classifier to `bash`; arbitrary Python requires a separately verified policy
before execution. No incomplete or unrestricted Python replacement was kept.
