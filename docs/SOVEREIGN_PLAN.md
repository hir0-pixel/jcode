# Sovereign engine: replacement plan for the Hermes backend

Working name `sovereign-engine` (product name to be chosen later).

## Goal

A cheap Hermes clone: Hermes's desktop experience on an engine with jcode's memory and
token efficiency, Prime Agent's task completion and self-improvement, and Evestack-style
observability. Everything runs locally except calls to the model provider.

## Decision criteria

Every choice below is scored on: efficiency (RAM/CPU), scalability (sessions, history),
speed (startup, latency), token usage, and convenience (reuse over rewrite, one thing to
ship).

## Decisions

| Component | Choice | Efficiency | Scale | Speed | Tokens | Convenience |
|---|---|---|---|---|---|---|
| Agent loop, sessions, providers | Fork of jcode (Rust) | ~28 MB + ~10 MB/session vs Hermes Python 115 MB idle (measured) | one daemon, many sessions | 14 ms first frame (jcode's figure) | agent grep, cache-safe input, no background replay | exists, MIT |
| Desktop seam | Hermes-compatible gateway inside the engine binary: HTTP + `/api/ws` JSON-RPC | no extra process | same | same | none | desktop unchanged: it already attaches to a remote gateway |
| Storage | SQLite (WAL) | in-process | about 1 write/s average; batching gives thousands/s | local file | none | nothing to install |
| Prime REPL | Pydantic Monty (Rust Python subset); CPython only on demand | about 6 µs start, KB snapshots | per-session | fast | context-as-variable keeps large text out of the prompt | exists, MIT |
| Self-improvement | Prime-style Continual Harness plus `/refine`, on demand | none resident | files | none | no per-turn background replay | small |
| Memory recall | jcode memory store, local recall (no remote Jev) | in-process | bounded to 5 items | local | at most ~500 tokens per turn | small change |
| Observability | Evestack two-tier model: SQL run records + optional content spans, OpenTelemetry GenAI names | in-process | retention + summaries | local | none | backend first, GUI later |
| Channels, cron | ZeroClaw crates, compiled in (later phase) | in-process | exists | exists | none | exists, MIT/Apache |

Language is chosen per component. Rust wins here because the reusable assets (jcode,
Monty, ZeroClaw) are Rust. CPython stays available only as an on-demand REPL fallback.

## The seam

The desktop app speaks the `tui_gateway` protocol. It is JSON-RPC 2.0 over
`ws://host/api/ws?token=…`, plus HTTP endpoints such as `/api/health`, `/api/status`, and
`/api/config`. The machine-readable contract is `apps/shared/src/gateway-contract.openrpc.json`:
235 methods, 73 notification types, and 13 server-to-client requests. Notifications are
`{"jsonrpc":"2.0","method":"event","params":{"type","session_id","seq","payload"}}`.

The desktop can already attach to a remote gateway through `HERMES_DESKTOP_REMOTE_URL` and
`HERMES_DESKTOP_REMOTE_TOKEN`. A native "local engine" launch is a small Electron change on
top of that.

### One process, three tasks

```
sovereign-engine gateway --host 127.0.0.1 --port 0     (one binary, one process)
 ├─ task: jcode Server::new_with_name(provider, name).run()    daemon on a private socket
 ├─ task per WebSocket client: run_bridge_stream(duplex half)   jcode harness API v1 (NDJSON)
 └─ task: HTTP + /api/ws gateway (Hermes tui_gateway JSON-RPC) ⇄ translator ⇄ other duplex half
```

The translator is a harness-API client, so it depends only on the stable, versioned
`jcode-harness-api` crate. jcode's internal protocol can change without breaking it.
A private `JCODE_HOME` and socket keep it isolated from any jcode the user runs.

Core mapping (harness API ⇄ Hermes):

| Hermes | jcode harness API |
|---|---|
| session.create / list / resume / history | CreateSession / ListSessions / AttachSession / GetHistory |
| prompt.submit | SendMessage |
| session.interrupt / steer | Cancel / SoftInterrupt |
| session.undo / compress / title | Rewind(Undo) / Compact / RenameSession |
| approval request → approval.respond | PermissionRequest → PermissionResponse |
| model.options / set model | ListModels, RuntimeInfo / SetModel, SetReasoningEffort |
| message.start / delta / complete | first TextDelta / TextDelta / TextDone + TurnDone |
| reasoning.delta | ReasoningDelta |
| tool.start / tool.complete | ToolStart (+ToolCall input) / ToolDone |
| session.usage | TokenUsage |
| error | TurnStopped (error reasons), Error |
| session.title | SessionRenamed |

Rule: every method in the contract either works or returns the JSON-RPC error `-32601` with
`data.reason = "not_supported_by_engine"`. The UI then degrades cleanly and never hangs.

## Modules (build order; one commit or more per module)

| # | Module | Delivers | Pass condition |
|---|---|---|---|
| M0 | Fork + build | jcode builds and runs from our fork; baseline RAM recorded | `cargo build --release` passes; daemon RSS measured |
| M1 | Gateway transport | HTTP (`/api/health`, `/api/status`, `/api/ws`), token auth, JSON-RPC framing, event envelope, `gateway.ready` | contract tests: responses validate against the OpenRPC schemas; auth and security tests |
| M2 | Chat bridge | `session.create/list/resume/history/interrupt/title/usage/status`, `prompt.submit` mapped onto jcode sessions; jcode events → `message.*`, `tool.*`, `thinking/reasoning.delta`, `status.update`, `session.usage`, `error`; the `approval` server request | a scripted WebSocket client runs a full chat against Ollama; every event validates against the schema |
| M3 | Config + models | `config.get/set/show`, `model.options`, `/api/config`, `/api/model/*` backed by jcode config and logins | desktop settings screens load |
| M4 | Desktop launch | Electron starts the engine instead of `hermes serve` when the engine setting is on; otherwise unchanged | app boots to chat with no Python process |
| M5 | Prime REPL | `repl` tool on Monty: sandboxed (no files, network, or environment), step, memory and time limits, per-session state; large inputs held as REPL variables | sandbox-escape tests; recursive `llm_query` works |
| M6 | Recursive subagents | `spawn` from the REPL mapped to jcode subagents, with depth and budget limits | depth and budget caps enforced |
| M7 | Continual Harness | `~/.sovereign/harness/{prompts,memories,skills,subagents}`; `/refine` proposes small, evidence-backed diffs; versioned with rollback | refine produces a reviewable diff; rollback restores the previous version |
| M8 | Local memory recall | replace Jev recall with local SQLite FTS5 (BM25), top 5, ~500-token cap; import `MEMORY.md` and `USER.md` | zero remote calls during recall |
| M9 | Run and event store | SQLite tier 1 (runs: session, turn, subagent; parent, root, tokens, status) and tier 2 (content spans, can be switched off); crash-safety log; startup marks orphaned runs `interrupted`; cost computed locally, `unpriced` when unknown | force-kill test; history survives |
| M10+ | Parity | ZeroClaw channels and cron; profiles; skills hub; kanban; pet; voice; remaining methods, prioritised from the parity matrix | per-feature tests |

Observability GUI views wait until the backend (M9) is proven.

## Security requirements (checked in every module)

- Bind to `127.0.0.1` by default. Binding elsewhere requires an explicit flag plus a token.
- Token required on every HTTP and WebSocket request, compared in constant time. Random
  256-bit per-launch token, stored in a file with mode 0600.
- WebSocket `Origin` allowlist, to block DNS-rebinding and cross-site WebSocket hijacking.
- JSON-RPC limits: maximum frame size, maximum in-flight requests, parse errors without echoing input.
- No secrets in logs or error messages. Provider responses are never echoed back.
- Tool approvals keep jcode's risk classes. The REPL has no host access unless explicitly granted.
- The model gateway is the only network exit that is not an explicit tool call.

## Testing strategy

- Contract tests generated from `gateway-contract.openrpc.json`: every implemented method and
  event is validated against its JSON Schema.
- End-to-end: a headless WebSocket client drives real chats against local Ollama. That costs
  zero cloud tokens.
- Security tests: missing or wrong token, bad Origin, oversized frames, malformed JSON-RPC,
  REPL escape attempts.
- Budget tests: engine RSS idle and per session; tokens per turn.
- Left for the user: visual checks of the desktop with real accounts (Codex OAuth), Windows,
  and anything that needs their credentials.

## Repositories and commits

- `Sovereign AI/sovereign-engine`: fork of jcode. Upstream remote `upstream`. Work branch
  `sovereign/main`. A commit after each module step.
- `Sovereign AI/hermes-agent`: branch `sovereign-engine`. Only the files we change are
  staged. The user's existing uncommitted work is never staged or committed.

## References

- jcode: single daemon, harness API, TypeScript SDK, memory architecture (docs/MEMORY_ARCHITECTURE.md)
- Prime Agent (arXiv 2608.23552): RLM loop, Continual Harness, `/refine`
- Recursive Language Models (Zhang, Kraska, Khattab, arXiv 2512.24601)
- Pydantic Monty: sandboxed Python subset in Rust
- OpenAI codex-rs: a Rust agent workspace, sandboxing, and app-server protocol
- Evestack observability: two-tier run records and spans; cost computed, never reported
- OpenTelemetry GenAI semantic conventions: `invoke_agent`, `chat`, `execute_tool`
- Durable execution: Restate; Gunnar Morling, "Building a Durable Execution Engine With SQLite"
- Alex Xu, *System Design Interview*: requirements, then estimates, then components, then operations
