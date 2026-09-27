"""Use Akira's existing key-free Rust web search tool."""

from __future__ import annotations

from rlm import host_request


async def run(query: str, *, max_output: int = 8192, num_results: int = 5) -> str:
    """Search with the same no-key backend available to chat tools."""
    result = await host_request(
        "websearch.run", {"query": query, "num_results": num_results}
    )
    text = result.get("results", "") if isinstance(result, dict) else str(result)
    return text[:max_output]
