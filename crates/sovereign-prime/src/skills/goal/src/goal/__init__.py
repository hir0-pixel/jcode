"""Prime Agent goal skill: manage the persistent thread goal from the kernel.

All goal state lives in the Rust host; these functions are thin typed
wrappers over the generic host bridge (`rlm.host_request`). They only work
inside the Prime Agent Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request


async def get() -> dict[str, Any]:
    """Read the current thread goal.

    Returns a dict with `goal` (None when no goal is set), `remaining_tokens`,
    and `completion_budget_report`. The `goal` dict carries objective, status,
    token and turn budgets/usage, and timestamps.
    """
    return await host_request("goal.get")


async def create(objective: str, token_budget: int | None = None) -> dict[str, Any]:
    """Start a new active thread goal.

    Fails while a goal is still pending (active, paused, or budget-limited);
    a completed or errored goal is replaced. Only create a goal when the user
    or system/developer instructions explicitly ask for a persistent
    long-running goal. Set `token_budget` only when an explicit token budget is
    requested.
    """
    if not isinstance(objective, str):
        raise TypeError(f"objective must be str, got {type(objective).__name__}")
    if token_budget is not None and not isinstance(token_budget, int):
        raise TypeError(f"token_budget must be int or None, got {type(token_budget).__name__}")
    payload: dict[str, Any] = {"objective": objective}
    if token_budget is not None:
        payload["token_budget"] = token_budget
    return await host_request("goal.create", payload)


async def progress(note: str, verification: str = "none", error: str = "") -> dict[str, Any]:
    """Record one short attempt-log line for the current continuation turn.

    `note` is what you tried this turn. `verification` should say what you ran
    and whether it passed ("pass: cargo test -p foo" / "fail: build error").
    `error` is the key error, if any. Kept short and capped by the host (last
    8 entries, ~80 chars each) and only ever shown in the next continuation
    prompt, never resent in full history. A failed tool call or failed
    verification does not end the goal — only `complete()`, budget
    exhaustion, or a user cancel do.
    """
    if not isinstance(note, str) or not note.strip():
        raise ValueError("note must be a non-empty str")
    if not isinstance(verification, str):
        raise TypeError(f"verification must be str, got {type(verification).__name__}")
    if not isinstance(error, str):
        raise TypeError(f"error must be str, got {type(error).__name__}")
    payload: dict[str, Any] = {"note": note, "verification": verification}
    if error:
        payload["error"] = error
    return await host_request("goal.progress", payload)


async def complete(verification: str) -> dict[str, Any]:
    """Mark the existing thread goal achieved.

    Use only when the objective has actually been achieved and no required
    work remains — not because the budget is nearly exhausted or because you
    are stopping work. `verification` is required: describe the test, build,
    or command you actually ran and its result (e.g. "ran `pytest`, 42
    passed"). Pause, resume, and budget-limit transitions are controlled by
    the user and the host.
    """
    if not isinstance(verification, str) or not verification.strip():
        raise ValueError("verification must be a non-empty str describing what you ran and its result")
    return await host_request("goal.complete", {"verification": verification})
