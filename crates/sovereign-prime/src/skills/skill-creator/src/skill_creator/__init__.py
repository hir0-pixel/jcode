"""Create skills through Akira's Rust-owned skill store."""

from importlib import import_module, invalidate_caches
import sys

from rlm import host_request


async def create(
    name: str,
    description: str,
    instructions: str,
    *,
    package_name: str | None = None,
    package_code: str | None = None,
) -> dict:
    """Create a skill using the same validation and store as skill_manage."""
    payload = {"action": "create", "name": name, "description": description,
               "instructions": instructions}
    if package_name is not None or package_code is not None:
        payload.update(package_name=package_name, package_code=package_code)
    result = await host_request("skill.create", payload)
    if package_name is not None:
        # Rust returns the validated skill directory. The current worker can
        # import its new package immediately without accepting an arbitrary path.
        sys.path.insert(0, result["src"])
        invalidate_caches()
        import_module(package_name)
    return result
