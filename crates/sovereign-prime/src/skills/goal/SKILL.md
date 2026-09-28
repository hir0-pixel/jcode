---
name: prime-goal
description: Manage the persistent thread goal from the Python REPL. Use to read goal status and budget usage, to start a goal when the user explicitly asks for one, or to mark the active goal complete once its objective is fully achieved.
---

# Goal

The thread goal is a persistent objective the harness keeps re-prompting you to
pursue across turns until it is complete. Goal state (status, token budget,
usage accounting) lives in the host; this skill is the kernel-side interface to
it. Call it directly from the Python REPL:

```python
await goal.get()
await goal.create("ship the release notes")
# Only pass token_budget when the user explicitly asks for one:
# await goal.create("ship the release notes", token_budget=200000)
await goal.progress("added the changelog section", verification="none")
await goal.progress("ran the build", verification="pass: cargo build")
await goal.complete("ran `pytest`, 42 passed")
```

## API

- `await goal.get()` — current goal as a dict: `goal` (or `None` when no goal
  is set), `remaining_tokens`, and `completion_budget_report`. The `goal` dict
  carries `objective`, `status`, token and turn budgets/usage, timestamps,
  `attempt_log` (last few progress lines), `plateaued`, and
  `completion_verification`.
- `await goal.create(objective, token_budget=None)` — start a new active goal.
  Fails while a goal is still pending (active, paused, or budget-limited); a
  completed or errored goal is replaced by the new one. Only create a goal when
  the user or system/developer instructions explicitly ask for a persistent
  long-running goal; do not infer goals from ordinary tasks. Set `token_budget`
  only when an explicit token budget is requested.
- `await goal.progress(note, verification="none", error="")` — record a short
  one-line note on what you tried this continuation turn and whether it
  verified. Kept capped and short; never ends the goal, even on a failed tool
  call or failed verification — that is expected, not a stop condition.
- `await goal.complete(verification)` — mark the existing goal achieved.
  `verification` is required: say what you actually ran (tests/build/command)
  and its result. Use only when the objective has actually been achieved and
  no required work remains; do not call it merely because the budget is
  nearly exhausted or because you are stopping work. The result includes its
  final token and turn budget report plus the attempt log.

## Rules

- Goal status transitions other than completion (pause, resume, clear,
  budget-limiting) are controlled by the user and the host; there is no API for
  them here.
- Call `await goal.progress(...)` once per continuation turn so the attempt
  log (and plateau detection) has something to work with.
- When an active goal is actually complete, call `await goal.complete(...)`
  with a real verification; do not merely say it is done — the harness keeps
  continuing the goal until the completion call arrives.
- A failed tool call or failed verification is normal goal work, not a
  failure to report as done — keep going, try a different approach if the
  host tells you a plateau was detected, and only stop via `complete()`,
  running out of budget, or a user cancel.
