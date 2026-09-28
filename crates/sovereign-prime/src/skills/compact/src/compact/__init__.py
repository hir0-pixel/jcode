"""Prime Agent compact skill: context compaction control from the kernel.

Compaction runs host-side (the same implementation as /compact); these
functions are thin typed wrappers over the generic host bridge
(`rlm.host_request`). They only work inside the Prime Agent Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request


async def status() -> dict[str, Any]:
    """Read whether compaction is pending for this session.
    """
    return await host_request("compact.status")


async def run(instructions: str | None = None) -> dict[str, Any]:
    """Schedule compaction after the current turn ends.

    The engine starts its existing background compactor at turn end and
    applies the summary when its normal poll runs. This does not start a new
    model turn. The turn-end attempt can still reject short or low-usage
    context. Optional `instructions` focus the resulting summary.
    """
    if instructions is not None and not isinstance(instructions, str):
        raise TypeError(f"instructions must be str or None, got {type(instructions).__name__}")
    payload: dict[str, Any] = {}
    if instructions is not None:
        payload["instructions"] = instructions
    return await host_request("compact.run", payload)
