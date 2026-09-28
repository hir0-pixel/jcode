---
name: prime-compact
description: Check queued compaction and schedule a conversation summary from the Python REPL.
---

# Compact

Compaction replaces older conversation history with a dense summary, freeing
context so long-running work can continue. The implementation lives in the
host (the same one behind the user's `/compact` command); this skill is the
kernel-side interface to it. Call it directly from the Python REPL:

```python
await compact.status()
await compact.run()
await compact.run("keep the failing test names and the migration checklist")
```

## API

- `await compact.status()` — returns `scheduled`, indicating whether a request
  is waiting for the current turn to end.
- `await compact.run(instructions=None)` — queue one compaction attempt for
  turn end and optionally focus the summary. The existing Rust compactor may
  reject the attempt if context is short or below its usage threshold.

## Rules

- Compaction never runs mid-cell. The engine starts its existing background
  compactor when the current turn ends; it applies the summary through its
  normal completion poll. This does not create another model turn.
- The Python kernel persists through compaction — variables, imports, and
  helpers you defined all remain available.
- Compact at a natural boundary when context usage is high and substantial
  work remains, instead of becoming terse or returning to the user early.
  Check `await compact.status()` when unsure.
- One request per turn is enough. A repeated request before turn end is
  ignored and the first instructions are retained.
