#!/usr/bin/env python3
"""Hermes feature parity report, generated from source (never hand-edited).

Every JSON-RPC method in the Hermes gateway contract and every HTTP route in
the Hermes Python routers is listed with its status in the sovereign engine:

  working      real behaviour backed by the engine
  placeholder  answers within the contract but reports the feature as
               off/empty until its module lands (the desktop degrades cleanly)
  forwarded    served by Hermes's own Python backend, started on demand
               (never on the chat hot path; ~1 ms per call once warm)

Usage: scripts/parity.py [HERMES_AGENT_DIR] > docs/PARITY.md
"""
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

ENGINE = Path(__file__).resolve().parent.parent
HERMES = Path(sys.argv[1] if len(sys.argv) > 1 else ENGINE.parent / "hermes-agent")

# Answered honestly as "off / empty" until the named module is built.
PLACEHOLDER = {
}
# Where each missing namespace gets built (docs/SOVEREIGN_PLAN.md modules).
PLAN = {
    "bot_relay": "M10 channels (ZeroClaw)", "connectors": "M10 channels (ZeroClaw)",
    "cron": "M10 cron (ZeroClaw)", "profiles": "M10 profiles", "pet": "M10 pet",
    "voice": "M10 voice", "wake": "M10 voice", "skills": "M10 skills hub",
    "groups": "bots keep Python groups; chat-engine groups.list/capabilities are empty stubs",
    "projects": "M10 projects", "vault": "M10 vault",
    "mcp": "M10 MCP management", "browser": "M10 browser pane", "display": "M10 display",
    "billing": "not applicable (hosted billing)", "subscription": "not applicable (hosted billing)",
    "free_tier": "not applicable (hosted free tier)",
    # learning / subagent / spawn_tree / delegation / session.control* → engine (M10a/b)
    "handoff": "M10 handoff", "image": "M10 images", "rollback": "M10 rollback",
}


def rpc_methods_handled() -> set[str]:
    src = (ENGINE / "crates/sovereign-gateway/src/rpc.rs").read_text()
    body = src[src.index("async fn dispatch("):]
    body = body[: body.index("_ => self.forward(")]
    handled = set()
    for arm in re.findall(r'^\s*((?:"[a-z_.]+"\s*\|\s*)*"[a-z_.]+")\s*=>', body, re.M):
        handled.update(re.findall(r'"([a-z_.]+)"', arm))
    # Groups of methods owned by a sub-module answer through its `handles()` list.
    for name in ("attach", "local_state"):
        module = (ENGINE / f"crates/sovereign-gateway/src/rpc/{name}.rs").read_text()
        start = module.index("fn handles(")
        handled.update(re.findall(r'"([a-z_.]+)"', module[start : module.index("\n}\n", start)]))
    return handled


# Served by crates/sovereign-gateway/src/sessions_rest.rs (Hermes path spelling).
SESSIONS_REST = {
    ("GET", "/api/sessions"), ("GET", "/api/sessions/search"), ("GET", "/api/sessions/{session_id}"),
    ("GET", "/api/sessions/{session_id}/messages"), ("GET", "/api/sessions/{session_id}/messages/around"),
    ("GET", "/api/sessions/{session_id}/timeline"), ("DELETE", "/api/sessions/{session_id}"),
    ("PATCH", "/api/sessions/{session_id}"),
    ("POST", "/api/sessions/bulk-delete"), ("POST", "/api/sessions/import"),
    ("GET", "/api/sessions/empty/count"), ("DELETE", "/api/sessions/empty"),
    ("GET", "/api/sessions/stats"),
    ("GET", "/api/sessions/{session_id}/latest-descendant"),
    ("GET", "/api/sessions/{session_id}/export"), ("POST", "/api/sessions/prune"),
}


# Served by crates/sovereign-gateway/src/memory_rest.rs over jcode's memory graph;
# memory-provider routes are refused there (one memory store), never forwarded.
MEMORY_REST = {
    ("GET", "/api/memory"), ("POST", "/api/memory/reset"), ("PUT", "/api/memory/provider"),
    ("GET", "/api/memory/providers/{name}/config"), ("POST", "/api/memory/providers/{name}/setup"),
    ("PUT", "/api/memory/providers/{name}/config"),
}


def rest_routes_handled() -> set[tuple[str, str]]:
    src = (ENGINE / "crates/sovereign-gateway/src/lib.rs").read_text()
    inline = set(re.findall(r'\("(GET|POST|PUT|DELETE|PATCH)", "(/api/[^"]+)"\)', src))
    return inline | SESSIONS_REST | MEMORY_REST | {("GET", "/api/ws")}


def rest_route_refused(path: str) -> bool:
    """Chat-bound routes the engine answers 'not supported' instead of proxying."""
    return path.startswith("/api/sessions")


def hermes_rest_routes() -> list[tuple[str, str, str]]:
    routes = []
    for py in sorted((HERMES / "hermes_cli/web_routers").glob("*.py")):
        for method, path in re.findall(r'@\w*router\.(get|post|put|delete|patch)\(\s*"(/api/[^"]+)"', py.read_text()):
            routes.append((method.upper(), path, py.stem))
    return routes


def main() -> None:
    contract = json.loads((ENGINE / "crates/sovereign-gateway/contract/gateway-contract.openrpc.json").read_text())
    methods = sorted(m["name"] for m in contract["methods"])
    handled = rpc_methods_handled()
    status = {m: ("placeholder" if m in PLACEHOLDER else "working") if m in handled else "forwarded" for m in methods}
    by_ns = defaultdict(list)
    for m in methods:
        by_ns[m.split(".")[0]].append(m)
    counts = defaultdict(int)
    for s in status.values():
        counts[s] += 1

    out = ["# Hermes feature parity (generated by scripts/parity.py)", ""]
    out.append("Nothing in Hermes is removed. The Rust (jcode) harness serves the hot path; every other Hermes "
               "method and route is forwarded to Hermes's own Python backend, which starts only when one of them "
               "is first used and stops after 10 idle minutes.")
    out.append("")
    out.append(f"**JSON-RPC methods ({len(methods)}):** {counts['working']} working, {counts['placeholder']} placeholder, "
               f"{counts['forwarded']} forwarded to Hermes (Python). Events emitted: message.start/delta/complete, reasoning.delta, tool.start/"
               "complete, session.usage, session.title, status.update, error, gateway.ready. Server requests: approval.")
    out.append("")
    out.append("| Namespace | Rust (working) | Rust (placeholder) | Forwarded to Hermes (Python) |")
    out.append("|---|---|---|---|")
    for ns in sorted(by_ns):
        ms = by_ns[ns]
        pick = lambda s: ", ".join(f"`{m}`" for m in ms if status[m] == s) or "–"
        out.append(f"| {ns} | {pick('working')} | {pick('placeholder')} | {pick('forwarded')} |")

    rest = hermes_rest_routes()
    served = rest_routes_handled()
    rest_done = [r for r in rest if (r[0], r[1]) in served]
    out.append("")
    refused = [r for r in rest if (r[0], r[1]) not in served and rest_route_refused(r[1])]
    out.append(f"**HTTP routes in Hermes ({len(rest)}):** {len(rest_done)} served by the Rust engine, {len(refused)} "
               "chat-bound routes refused by the engine (not built yet; never proxied, since chats are not in Python's "
               "database); the rest are reverse-proxied to Hermes's Python backend.")
    out.append("")
    out.append("| Router | Rust | Refused by engine | Forwarded |")
    out.append("|---|---|---|---|")
    by_router = defaultdict(list)
    for r in rest:
        by_router[r[2]].append(r)
    for router in sorted(by_router):
        rs = by_router[router]
        done = [f"`{m} {p}`" for m, p, _ in rs if (m, p) in served]
        no = [f"`{m} {p}`" for m, p, _ in rs if (m, p) not in served and rest_route_refused(p)]
        todo = [f"`{m} {p}`" for m, p, _ in rs if (m, p) not in served and not rest_route_refused(p)]
        out.append(f"| {router} | {', '.join(done) or '–'} | {', '.join(no) or '–'} | {', '.join(todo) or '–'} |")
    print("\n".join(out))


if __name__ == "__main__":
    main()
