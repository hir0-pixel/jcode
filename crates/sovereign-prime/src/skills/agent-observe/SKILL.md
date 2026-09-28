---
name: prime-agent-observe
description: Read-only roster and observation of an agent's parent, siblings, and direct children. Use to discover reachable agents and to inspect family status and bounded recent-message previews without mutating sessions.
---

# Agent Observe

Observe the current session's visible family roster through the local daemon:
parent, siblings, direct children, and self when those members are returned by
the swarm. The roster is constrained to the family used by `agent_message.send`.
`get_agent` and `recent_messages` query the selected session through the engine;
they can fail when that session's context is unavailable.
This skill is read-only: it can list family sessions, inspect one session, and fetch
bounded recent message previews. It cannot prompt, steer, clear, kill, rename, or
otherwise mutate another session.

Call directly from the kernel:

```python
agents = await agent_observe.list_agents()
child = next((item for item in agents["agents"] if item["relationship"] == "child"), None)
if child is not None:
    worker = await agent_observe.get_agent(child["sessionId"])
    recent = await agent_observe.recent_messages(child["sessionId"], limit=6)
```

## API

- `await agent_observe.list_agents()` returns `current` and `agents`. Each
  summary can include `sessionId`, `sessionName`, `activeSessionId`,
  `relationship`, `status`, `activity`, `isSessionActive`, `taskLabel`,
  `detail`, and `latestMessage` (the stored completion report).
- `await agent_observe.get_agent(target)` returns one family member summary.
  `target` may be its session id, session name, task label, or unambiguous id
  suffix.
- `await agent_observe.recent_messages(target, limit=8, max_chars=800)`
  returns up to `limit` recent bounded message previews for the target session.
  `limit` must be 1-50, and `max_chars` must be 80-2000.

## Safety

- This skill is read-only and exposes no mutation commands.
- Targets outside the nuclear family are rejected; transcript reads follow the
  same family rule as messaging.
- Message access is bounded by count and per-message character limit.
- Prefer status and recent previews for orchestration. Ask the user before
  using observed context to steer or message another session.
