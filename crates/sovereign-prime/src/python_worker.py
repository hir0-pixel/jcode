import ast
import asyncio
import contextlib
import inspect
import json
import os
from pathlib import Path
import sys
import traceback
import types

protocol_out = sys.stdout
sys.path.extend(sys.argv[1:])
skills_root = Path(sys.argv[2])
if skills_root.is_dir():
    for skill_dir in skills_root.iterdir():
        if skill_dir.is_dir():
            sys.path.extend([str(skill_dir), str(skill_dir / "src")])
namespace = {"__name__": "__main__"}
max_calls = 16
max_output = 65536


def write_frame(value):
    protocol_out.write(json.dumps(value, separators=(",", ":")) + "\n")
    protocol_out.flush()


async def host_call(name, *args):
    write_frame({"op": "call", "fn": name, "args": [str(x) for x in args]})
    line = sys.stdin.readline()
    if not line:
        raise RuntimeError("engine closed the REPL channel")
    reply = json.loads(line)
    if reply.get("error"):
        raise RuntimeError(reply["error"])
    return reply.get("value", "")


async def llm_query(prompt):
    return await host_call("llm_query", prompt)


async def load(path):
    return await host_call("load", path)


async def refine(op="run", instructions=None, global_=False):
    return await host_call("refine", json.dumps({"op": op, "instructions": instructions, "global": global_}))


async def goal(op="get", objective=None):
    return await host_call("goal", json.dumps({"op": op, "text": objective}))


async def heartbeat(op="list", **options):
    return await host_call("heartbeat", json.dumps({"op": op, **options}))


async def spawn_subagent(prompt, name="worker"):
    return await host_call("spawn_subagent", json.dumps({"prompt": prompt, "label": name}))


async def agent_message(action, message=None, target=None):
    return await host_call("agent_message", json.dumps({"action": action, "message": message, "target": target}))


async def host_request(name, payload=None):
    """Prime skill bridge for host operations already implemented by Rust."""
    payload = payload or {}
    if name.startswith("goal."):
        op = name.removeprefix("goal.")
        if op not in {"get", "create", "complete"}:
            raise ValueError(f"unsupported Prime host request: {name}")
        request = {"op": op}
        if op == "create":
            request["text"] = payload.get("objective", payload.get("text"))
        return json.loads(await host_call("goal", json.dumps(request)))
    if name.startswith("refine."):
        op = name.removeprefix("refine.")
        if op not in {"status", "run"}:
            raise ValueError(f"unsupported Prime host request: {name}")
        return json.loads(await host_call("refine", json.dumps({"op": op, **payload})))
    raise ValueError(f"unsupported Prime host request: {name}")


_rlm_module = types.ModuleType("rlm")
_rlm_module.host_request = host_request
sys.modules["rlm"] = _rlm_module


namespace.update({
    "llm_query": llm_query,
    "load": load,
    "refine": refine,
    "goal": goal,
    "heartbeat": heartbeat,
    "spawn_subagent": spawn_subagent,
    "agent_message": agent_message,
})
write_frame({"op": "ready", "pid": os.getpid()})


class _Output:
    def __init__(self, buffer):
        self.buffer = buffer
        self.size = 0

    def write(self, value):
        room = max_output - self.size
        if room > 0:
            piece = value[:room]
            self.buffer.append(piece)
            self.size += len(piece)
        return len(value)

    def flush(self):
        pass

for line in sys.stdin:
    try:
        message = json.loads(line)
        if message.get("op") != "run":
            continue
        code = message.get("code", "")
        if len(code.encode("utf-8")) > 1_048_576:
            write_frame({"op": "done", "stdout": "", "value": None, "error": "cell exceeds 1 MiB"})
            continue
        output = []
        call_count = [0]
        original_host_call = namespace["llm_query"].__globals__["host_call"]

        async def counted_host_call(name, *args):
            call_count[0] += 1
            if call_count[0] > max_calls:
                raise RuntimeError("host call budget (16) exhausted")
            return await original_host_call(name, *args)

        namespace["llm_query"].__globals__["host_call"] = counted_host_call
        tree = ast.parse(code, "<repl>", "exec")
        last = tree.body.pop() if tree.body and isinstance(tree.body[-1], ast.Expr) else None
        with contextlib.redirect_stdout(_Output(output)), contextlib.redirect_stderr(_Output(output)):
            if tree.body:
                result = eval(compile(ast.Module(body=tree.body, type_ignores=[]), "<repl>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT), namespace, namespace)
                if inspect.isawaitable(result):
                    asyncio.run(result)
            value = None
            if last is not None:
                result = eval(compile(ast.Expression(last.value), "<repl>", "eval", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT), namespace, namespace)
                value = asyncio.run(result) if inspect.isawaitable(result) else result
        value = repr(value)[:8192] if value is not None else None
        write_frame({"op": "done", "stdout": "".join(output)[:max_output], "value": value, "error": None, "host_calls": call_count[0]})
        namespace["llm_query"].__globals__["host_call"] = original_host_call
    except BaseException as error:
        namespace["llm_query"].__globals__["host_call"] = original_host_call
        write_frame({"op": "done", "stdout": "".join(output)[:max_output] if "output" in locals() else "", "value": None, "error": "".join(traceback.format_exception_only(type(error), error)).strip()[:8192], "host_calls": call_count[0] if "call_count" in locals() else 0})
