# M10b decisions (unattended)

Date: 2026-09-26. Worktree: sovereign-engine-m10b / m10b-loop.

## Wire format vs Prime semantics

- Desktop `session.control.read` is the Hermes contract (`goal` / `loop` / `heartbeat`).
  That shape is authoritative for the UI.
- Command surface matches Prime: `/goal`, `/autonomous`, `/heartbeat` (not Hermes `/loop`
  as the primary name). Autonomous state is *projected* into the `loop` field of the
  snapshot (mode=`self_paced`) so Activity/status chips keep working without a desktop
  schema change. `/loop` remains a thin alias of `/autonomous` for Hermes muscle-memory.
- Keep jcode's `initiative` / project Goal tool separate (project tracking ≠ session goal).

## Persistence

- Goals, autonomous mode, and heartbeats live in `sovereign.db` tables under the existing
  migrate path, so they survive engine restart and desktop disconnect/reattach.
- Heartbeats: one active user heartbeat per session (Prime user `/heartbeat`); list/cancel
  supports the same plus any RLM-created ones later under the same table with a `source`.

## Swarm / tools

- Full `swarm` tool stays in `JCODE_DISABLED_TOOLS` (schedule stays disabled too).
- Measured: swarm schema alone has ~35 property descriptions (~650 tokens) plus a large
  action enum; enabling it would exceed the ~1k-token growth budget.
- Instead: compact `delegate` tool (spawn / message / list / stop / status) over the same
  `communicate` internals — no second implementation. Spec spawn via
  `EntryStore::resolve_subagent_spec` when prompt starts with `spec: <name>`.

## groups.*

- Desktop Agents view is spawn-tree / subagent based, not Python multi-agent groups.
- Serve `subagent.*`, `spawn_tree.*`, `delegation.*` from the swarm; delete dead Python
  `groups.*` handlers only if nothing else in Hermes still needs them for bots (Hermes
  bots plugin may still use groups — keep bot groups, remove chat-engine groups if distinct).
- Chat-engine gateway answers `groups.list` with `{ "rooms": [] }` and `groups.capabilities`
  with driver disabled (no hosted rooms). Other `groups.*` calls may still forward to Hermes
  for the bots plugin; Python group code is unchanged.

## Continuations

- On `message.complete`, if an active goal or autonomous run should continue and no
  subagent wait barrier is held and no user input is pending, submit a follow-up prompt
  via the existing harness (same path as `prompt.submit`).
- Hitting a budget/limit is NOT success. Only `goal.complete` / gate-pass (autonomous) /
  explicit clear ends successfully.
