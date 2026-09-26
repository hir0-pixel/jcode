#!/usr/bin/env python3
"""Hermes toolperf benchmark: Hermes, Sovereign, and Prime, same tasks/model.

Two arms on the SAME 9 trap tasks, imported (never copied) from hermes-agent's
own harness at evals/toolperf_abeval/ab_eval.py:

  hermes     stock Hermes exactly as ab_eval.py invokes it:
             `<hermes venv python> -m hermes_cli.main chat --query ...`.
  sovereign  the identical prompt in the identical sandbox, through
             Sovereign's one-shot endpoint: POST /api/agent/run
             {prompt, cwd, title, timeout_s} -> {ok, text, error, session_id,
             usage}.
  prime      Prime Agent JSON mode, installed in a disposable /tmp clone; its
             model provider points through the same counting proxy.

Fairness rules:
  - Same TASKS dict, same make_sandbox(), same SUCCESS checks (all imported
    from hermes-agent, not reimplemented) for both arms.
  - Every model call from all arms is routed through the shared counting
    proxy (scripts/sovereign-counting-proxy.mjs), so prompt/cached/completion
    tokens, dummy cost (scripts/lib/bench-prices.mjs's table, reimplemented
    here for Python) and wall time are measured identically regardless of
    which product is under test.
  - Tool-call / tool-error counts come from each product's OWN trace, because
    that is the only thing the proxy cannot see: Hermes from its NeMo Relay
    ATOF file (via hermes ab_eval.score_run), Sovereign from
    JCODE_HOME/sovereign.db (obs_runs/obs_spans, kind='execute_tool').
  - Every run gets a throwaway HOME/HERMES_HOME/JCODE_HOME; the user's real
    ~/.hermes and ~/.jcode are never touched. hermes-agent itself is read-only
    (imported, never modified).

Approval-policy finding (see also docs/BENCHMARK.md): Sovereign's
/api/agent/run runs the turn "headless" (approvals::Hub::mark_headless), which
denies every *shell-command* approval prompt outright. But (a) that gate only
fires for genuinely ambiguous/destructive commands - jcode's risk classifier
(jcode-command-risk) marks ordinary test/build commands and edits inside the
sandbox cwd as Safe/Low, which run immediately with no prompt at all - and
(b) non-bash tools (file edit/patch/read) are never gated in the first place
(approvals::hook::run returns 0 immediately unless the tool is "bash"). So
plain file edits and test commands (pytest/npm test/cargo test/...) in cwd
pass without approval; no bench-only allow-policy opt-in was needed.

Usage:
  python3 scripts/bench/abeval.py --dry-run
  python3 scripts/bench/abeval.py run --arm hermes    --reps 3
  python3 scripts/bench/abeval.py run --arm sovereign --reps 3
  python3 scripts/bench/abeval.py report

Environment (mirrors scripts/sovereign-vs-hermes-bench.mjs):
  BENCH_BASE_URL   upstream OpenAI-compatible endpoint (default: local Ollama)
  BENCH_MODEL      model id (default: sovereign/bench-hermes-64k:latest)
  BENCH_API_KEY    upstream API key (default: "ollama")
  BENCH_CONTEXT    context window, tokens (default: 65536)
  BENCH_PROXY      counting-proxy listen address (default: 127.0.0.1:18080)
  BENCH_OUT        results root (default: <engine>/bench-results/abeval)
  BENCH_TURN_TIMEOUT_S  per-task timeout, seconds (default: 600)
  HERMES_VENV_PY   python inside hermes-agent's venv (default: <hermes-agent>/.venv/bin/python3)
  SOVEREIGN_BIN    sovereign engine binary (default: <engine>/target/release/sovereign)
  PRIME_CLI        Prime Agent CLI bundle (default: /tmp/prime-agent/packages/coding-agent/dist/bundle/cli.js)

This script only BUILDS/verifies via --dry-run; it never starts Ollama or any
model server itself (that is the caller's job, same as the existing mjs bench).
"""
import argparse
import importlib.util
import json
import os
import signal
import socket
import sqlite3
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

ENGINE_ROOT = Path(__file__).resolve().parents[2]
HERMES_ROOT = ENGINE_ROOT.parent / "hermes-agent"
HERMES_EVAL_DIR = HERMES_ROOT / "evals" / "toolperf_abeval"

# ---------------------------------------------------------------- import, don't copy
def _load_hermes_ab_eval():
    spec = importlib.util.spec_from_file_location("hermes_ab_eval", HERMES_EVAL_DIR / "ab_eval.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


try:
    hermes_ab_eval = _load_hermes_ab_eval()
    TASKS = hermes_ab_eval.TASKS
    SUCCESS = hermes_ab_eval.SUCCESS
    make_sandbox = hermes_ab_eval.make_sandbox
    score_hermes_run = hermes_ab_eval.score_run
    IMPORT_ERROR = None
except Exception as exc:  # pragma: no cover - reported via --dry-run/config
    hermes_ab_eval = None
    TASKS, SUCCESS, make_sandbox, score_hermes_run = {}, {}, None, None
    IMPORT_ERROR = str(exc)


# ---------------------------------------------------------------- config
def cfg():
    base_url = os.environ.get("BENCH_BASE_URL", "http://127.0.0.1:11434")
    from urllib.parse import urlsplit

    parsed = urlsplit(base_url)
    upstream_origin = f"{parsed.scheme}://{parsed.netloc}"
    proxy_path = parsed.path if parsed.path not in ("", "/") else "/v1"
    proxy = os.environ.get("BENCH_PROXY", "127.0.0.1:18080")
    return {
        "MODEL": os.environ.get("BENCH_MODEL", "sovereign/bench-hermes-64k:latest"),
        "NUM_CTX": int(os.environ.get("BENCH_CONTEXT", os.environ.get("BENCH_NUM_CTX", "65536"))),
        "API_KEY": os.environ.get("BENCH_API_KEY", "ollama"),
        "BASE_URL": base_url,
        "UPSTREAM_ORIGIN": upstream_origin,
        "PROXY_PATH": proxy_path,
        "PROXY": proxy,
        "TURN_TIMEOUT_S": int(os.environ.get("BENCH_TURN_TIMEOUT_S", "600")),
        "REPS": int(os.environ.get("BENCH_REPS", "3")),
        "OUT": Path(os.environ.get("BENCH_OUT", str(ENGINE_ROOT / "bench-results" / "abeval"))),
        "HERMES_VENV_PY": os.environ.get("HERMES_VENV_PY", str(HERMES_ROOT / ".venv" / "bin" / "python3")),
        "SOVEREIGN_BIN": os.environ.get("SOVEREIGN_BIN", str(ENGINE_ROOT / "target" / "release" / "sovereign")),
        "PRIME_CLI": os.environ.get("PRIME_CLI", "/tmp/prime-agent/packages/coding-agent/dist/bundle/cli.js"),
        "PRICE_TABLE": os.environ.get("SOVEREIGN_PRICE_TABLE", str(ENGINE_ROOT / "scripts" / "sovereign-prices.json")),
    }


# ---------------------------------------------------------------- dummy cost (port of scripts/lib/bench-prices.mjs)
def resolve_price(model, table_path):
    price_in = os.environ.get("BENCH_PRICE_IN")
    price_out = os.environ.get("BENCH_PRICE_OUT")
    if price_in and price_out:
        return {
            "input": float(price_in),
            "cached": float(os.environ.get("BENCH_PRICE_CACHED", price_in)),
            "output": float(price_out),
            "source": "env (BENCH_PRICE_IN/_CACHED/_OUT)",
        }
    try:
        table = json.loads(Path(table_path).read_text(encoding="utf-8"))
        if model in table:
            return {**table[model], "source": table_path}
    except (OSError, ValueError):
        pass
    return None


def cost_of(usage, price):
    if not price:
        return None
    prompt = usage.get("prompt_tokens") or 0
    cached = min(usage.get("cached_tokens") or 0, prompt)
    completion = usage.get("completion_tokens") or 0
    return ((prompt - cached) * price["input"] + cached * price["cached"] + completion * price["output"]) / 1e6


# ---------------------------------------------------------------- counting proxy
class Proxy:
    def __init__(self, c, out_dir):
        self.calls_path = out_dir / "calls.jsonl"
        env = dict(os.environ)
        env.update(
            {
                "SOVEREIGN_PROXY_LISTEN": c["PROXY"],
                "SOVEREIGN_PROXY_UPSTREAM": c["UPSTREAM_ORIGIN"],
                "SOVEREIGN_PROXY_STATS": str(out_dir / "proxy-stats.json"),
                "SOVEREIGN_PROXY_CALLS": str(self.calls_path),
            }
        )
        self.proc = subprocess.Popen(
            ["node", str(ENGINE_ROOT / "scripts" / "sovereign-counting-proxy.mjs")],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.host, self.port = c["PROXY"].split(":")
        time.sleep(0.8)

    def tag(self, tag_value):
        url = f"http://{self.host}:{self.port}/__tag?tag={urllib.parse.quote(tag_value)}"
        try:
            urllib.request.urlopen(url, timeout=5).read()
        except Exception:
            pass

    def calls_for(self, tag):
        if not self.calls_path.exists():
            return []
        rows = []
        for line in self.calls_path.read_text(encoding="utf-8").splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if row.get("tag") == tag:
                rows.append(row)
        return rows

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def proxy_metrics(rows, model, price_table):
    prompt = sum(r.get("prompt_tokens") or 0 for r in rows)
    cached = sum(r.get("cached_tokens") or 0 for r in rows)
    completion = sum(r.get("completion_tokens") or 0 for r in rows)
    price = resolve_price(model, price_table)
    cost = cost_of({"prompt_tokens": prompt, "cached_tokens": cached, "completion_tokens": completion}, price)
    return {
        "model_calls": len(rows),
        "prompt_tokens": prompt,
        "cached_tokens": cached,
        "completion_tokens": completion,
        "cost_usd": cost,
    }


# ---------------------------------------------------------------- sovereign backend
def start_sovereign(c, home, token):
    jcode_home = home / ".jcode"
    jcode_home.mkdir(parents=True, exist_ok=True)
    (jcode_home / "config.toml").write_text(
        "\n".join(
            [
                "[providers.bench]",
                'type = "openai-compatible"',
                f'base_url = "http://{c["PROXY"]}{c["PROXY_PATH"]}"',
                f'api_key = "{c["API_KEY"]}"',
                "requires_api_key = false",
                f'default_model = "{c["MODEL"]}"',
                "",
                "[[providers.bench.models]]",
                f'id = "{c["MODEL"]}"',
                f'context_window = {c["NUM_CTX"]}',
                "",
            ]
        ),
        encoding="utf-8",
    )
    env = {k: v for k, v in os.environ.items() if not any(s in k for s in ("API_KEY", "TOKEN", "SECRET")) and not k.startswith(("HERMES_", "JCODE_", "SOVEREIGN_"))}
    env.update({"HOME": str(home), "JCODE_HOME": str(jcode_home), "HERMES_DASHBOARD_SESSION_TOKEN": token, "SOVEREIGN_PRICE_TABLE": c["PRICE_TABLE"]})
    args = [c["SOVEREIGN_BIN"], "--provider-profile", "bench", "--model", c["MODEL"], "serve", "--host", "127.0.0.1", "--port", "0"]
    proc = subprocess.Popen(args, env=env, cwd=str(home), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True, text=True)
    port = None
    deadline = time.time() + 180
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line:
            if proc.poll() is not None:
                raise RuntimeError(f"sovereign exited {proc.returncode} before READY")
            continue
        m = __import__("re").search(r"HERMES_BACKEND_READY port=(\d+)", line)
        if m:
            port = int(m.group(1))
            break
    if port is None:
        proc.kill()
        raise RuntimeError("sovereign: no READY line in 180s")
    return proc, port


def stop_sovereign(proc):
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except ProcessLookupError:
            pass


def agent_run(port, token, prompt, cwd, title, timeout_s):
    body = json.dumps({"prompt": prompt, "cwd": cwd, "title": title, "timeout_s": timeout_s}).encode("utf-8")
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/api/agent/run",
        data=body,
        method="POST",
        headers={"content-type": "application/json", "authorization": f"Bearer {token}"},
    )
    with urllib.request.urlopen(req, timeout=timeout_s + 10) as resp:
        return json.loads(resp.read())


def sovereign_tool_metrics(jcode_home, session_id):
    db_path = jcode_home / "sovereign.db"
    if not db_path.exists():
        return {"tool_calls": None, "tool_errors": None}
    con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        run_ids = [r[0] for r in con.execute("SELECT id FROM obs_runs WHERE session_id=?", (session_id,))]
        if not run_ids:
            return {"tool_calls": 0, "tool_errors": 0}
        qmarks = ",".join("?" * len(run_ids))
        total = con.execute(
            f"SELECT COUNT(*), SUM(CASE WHEN status='error' OR error IS NOT NULL THEN 1 ELSE 0 END) "
            f"FROM obs_spans WHERE root_id IN ({qmarks}) AND kind='execute_tool'",
            run_ids,
        ).fetchone()
        return {"tool_calls": total[0] or 0, "tool_errors": total[1] or 0}
    finally:
        con.close()


# ---------------------------------------------------------------- hermes arm
def run_hermes_task(c, proxy, work, run_id, task_name, timeout_s):
    home = work / "home"
    hermes_home = home / ".hermes"
    hermes_home.mkdir(parents=True, exist_ok=True)
    (hermes_home / "config.yaml").write_text(
        "\n".join(
            [
                "model:",
                f'  default: "{c["MODEL"]}"',
                "  provider: custom",
                f'  base_url: "http://{c["PROXY"]}{c["PROXY_PATH"]}"',
                f'  api_key: "{c["API_KEY"]}"',
                f'  context_length: {c["NUM_CTX"]}',
                f'  ollama_num_ctx: {c["NUM_CTX"]}',
                "terminal:",
                "  backend: local",
                "",
            ]
        ),
        encoding="utf-8",
    )
    (hermes_home / ".env").write_text(f'OPENAI_API_KEY={c["API_KEY"]}\n', encoding="utf-8")
    atof = work / "run.atof.jsonl"
    relay_config = work / "relay-plugins.toml"
    relay_config.write_text(
        "\n".join(
            [
                "version = 1",
                "",
                "[[components]]",
                'kind = "observability"',
                "enabled = true",
                "",
                "[components.config]",
                "version = 3",
                "",
                "[components.config.atof]",
                "enabled = true",
                "",
                "[[components.config.atof.sinks]]",
                'type = "file"',
                f"output_directory = {json.dumps(str(atof.parent))}",
                f"filename = {json.dumps(atof.name)}",
                'mode = "overwrite"',
                "",
            ]
        ),
        encoding="utf-8",
    )
    env = {k: v for k, v in os.environ.items() if not any(s in k for s in ("API_KEY", "TOKEN", "SECRET")) and not k.startswith(("HERMES_", "JCODE_", "SOVEREIGN_"))}
    env.update({"HOME": str(home), "HERMES_HOME": str(hermes_home), "OPENAI_API_KEY": c["API_KEY"], "HERMES_NEMO_RELAY_PLUGINS_TOML": str(relay_config)})
    proxy.tag(run_id)
    q = TASKS[task_name].replace("{WORK}", str(work))
    t0 = time.time()
    try:
        p = subprocess.run(
            [c["HERMES_VENV_PY"], "-m", "hermes_cli.main", "chat", "--query", q, "--quiet", "--max-turns", "30", "--accept-hooks", "--model", c["MODEL"]],
            cwd=str(work),
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout_s,
        )
        out, rc = (p.stdout or "").strip(), p.returncode
    except subprocess.TimeoutExpired:
        out, rc = "", -9
    wall_s = time.time() - t0
    tool = score_hermes_run(atof) or {}
    return {
        "ok": rc == 0,
        "exit": rc,
        "tail": "\n".join(out.splitlines()[-12:]),
        "wall_s": round(wall_s, 1),
        "tool_calls": tool.get("tools"),
        "tool_errors": tool.get("errs"),
        "model_calls_trace": tool.get("llm"),
    }, out, work


# ---------------------------------------------------------------- sovereign arm
def run_sovereign_task(c, proxy, work, run_id, task_name, timeout_s):
    home = work / "home"
    token = __import__("secrets").token_hex(24)
    proc, port = start_sovereign(c, home, token)
    proxy.tag(run_id)
    q = TASKS[task_name].replace("{WORK}", str(work))
    t0 = time.time()
    try:
        result = agent_run(port, token, q, str(work), run_id, timeout_s)
        ok = bool(result.get("ok"))
        text = result.get("text") or ""
        session_id = result.get("session_id")
        error = result.get("error")
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        ok, text, session_id, error = False, "", None, str(exc)
    wall_s = time.time() - t0
    tool = sovereign_tool_metrics(home / ".jcode", session_id) if session_id else {"tool_calls": None, "tool_errors": None}
    stop_sovereign(proc)
    return {
        "ok": ok,
        "exit": 0 if ok else 1,
        "tail": text[-2000:],
        "wall_s": round(wall_s, 1),
        "tool_calls": tool["tool_calls"],
        "tool_errors": tool["tool_errors"],
        "error": error,
    }, text, work


# ---------------------------------------------------------------- Prime Agent arm
def run_prime_task(c, proxy, work, run_id, task_name, timeout_s):
    cli = Path(c["PRIME_CLI"])
    if not cli.is_file():
        raise RuntimeError(f"Prime CLI missing at {cli}; shallow-clone and build it under /tmp")
    home = work / "home"
    agent_dir = home / ".prime" / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    (agent_dir / "models.json").write_text(json.dumps({"providers": {"bench": {
        "baseUrl": f'http://{c["PROXY"]}{c["PROXY_PATH"]}',
        "api": "openai-completions",
        "apiKey": c["API_KEY"],
        "models": [{"id": c["MODEL"], "contextWindow": c["NUM_CTX"], "maxTokens": 4096}],
    }}}), encoding="utf-8")
    env = {k: v for k, v in os.environ.items() if not any(s in k for s in ("API_KEY", "TOKEN", "SECRET"))}
    env.update({"HOME": str(home), "PRIME_AGENT_CODING_AGENT_DIR": str(agent_dir)})
    proxy.tag(run_id)
    prompt = TASKS[task_name].replace("{WORK}", str(work))
    started = time.time()
    try:
        proc = subprocess.run(
            ["node", str(cli), "--mode", "rpc", "--provider", "bench", "--model", c["MODEL"],
             "--cwd", str(work), "--daemon-socket", str(work / "prime-daemon.sock"),
             "--offline", "--no-session", "--no-extensions", "--no-skills", "--no-context-files"],
            input=json.dumps({"type": "prompt", "message": prompt}) + "\n",
            cwd=str(work), env=env, capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout_s,
        )
        raw, rc = proc.stdout or "", proc.returncode
    except subprocess.TimeoutExpired as exc:
        raw, rc = (exc.stdout or ""), -9
    lines = raw.splitlines()
    text_parts, tool_calls, tool_errors = [], 0, 0
    for line in lines:
        try:
            event = json.loads(line)
        except (TypeError, ValueError):
            continue
        if event.get("type") == "message_end":
            message = event.get("message", {})
            if message.get("role") == "assistant":
                text_parts.extend(block.get("text", "") for block in message.get("content", []) if block.get("type") == "text")
        elif event.get("type") == "tool_execution_end":
            tool_calls += 1
            tool_errors += int(bool(event.get("isError")))
    output = "\n".join(text_parts) or raw
    return {
        "ok": rc == 0, "exit": rc, "tail": output[-2000:],
        "wall_s": round(time.time() - started, 1),
        "tool_calls": tool_calls, "tool_errors": tool_errors,
    }, output, work


# ---------------------------------------------------------------- run / report
ARM_RUNNERS = {"hermes": run_hermes_task, "sovereign": run_sovereign_task, "prime": run_prime_task}


def do_run(c, arm, reps, only):
    if IMPORT_ERROR:
        print(f"cannot import hermes ab_eval.py: {IMPORT_ERROR}", file=sys.stderr)
        return 2
    out_dir = c["OUT"] / "results" / arm
    out_dir.mkdir(parents=True, exist_ok=True)
    meta_path = out_dir / "meta.jsonl"
    done = set()
    if meta_path.exists():
        for line in meta_path.read_text(encoding="utf-8").splitlines():
            try:
                done.add(json.loads(line)["run_id"])
            except (ValueError, KeyError):
                continue
    c["OUT"].mkdir(parents=True, exist_ok=True)
    proxy = Proxy(c, c["OUT"])
    time.sleep(0.5)
    try:
        for rep in range(reps):
            for task_name in TASKS:
                if only and task_name not in only:
                    continue
                run_id = f"{task_name}-r{rep}"
                if run_id in done:
                    continue
                work = c["OUT"] / "runs" / arm / run_id
                if work.exists():
                    __import__("shutil").rmtree(work)
                work.mkdir(parents=True)
                make_sandbox(work)
                tag = f"{arm}|{run_id}"
                summary, full_text, _ = ARM_RUNNERS[arm](c, proxy, work, tag, task_name, c["TURN_TIMEOUT_S"])
                try:
                    ok_check = SUCCESS[task_name](full_text, work)
                except Exception:
                    ok_check = False
                proxy_rows = proxy.calls_for(tag)
                pm = proxy_metrics(proxy_rows, c["MODEL"], c["PRICE_TABLE"])
                rec = {
                    "run_id": run_id,
                    "task": task_name,
                    "rep": rep,
                    "arm": arm,
                    "success": bool(summary["ok"]) and bool(ok_check),
                    **pm,
                    "tool_calls": summary.get("tool_calls"),
                    "tool_errors": summary.get("tool_errors"),
                    "wall_s": summary["wall_s"],
                }
                with open(meta_path, "a", encoding="utf-8") as f:
                    f.write(json.dumps(rec) + "\n")
                print(f"[{arm}] {run_id} success={rec['success']} calls={pm['model_calls']} tools={rec['tool_calls']} errs={rec['tool_errors']} {rec['wall_s']}s", flush=True)
                __import__("shutil").rmtree(work, ignore_errors=True)
    finally:
        proxy.stop()
    return 0


def do_report(c, arms):
    hdr = f"| task | arm | n | success% | model_calls | prompt_tok | cached_tok | completion_tok | cost_usd | tool_calls | tool_errors | wall_s |"
    sep = "|---|---|---|---|---|---|---|---|---|---|---|---|"
    print(hdr)
    print(sep)
    totals = {a: [] for a in arms}
    for task_name in TASKS:
        for arm in arms:
            meta_path = c["OUT"] / "results" / arm / "meta.jsonl"
            if not meta_path.exists():
                continue
            rows = [json.loads(l) for l in meta_path.read_text(encoding="utf-8").splitlines() if json.loads(l)["task"] == task_name]
            if not rows:
                continue
            totals[arm].extend(rows)
            n = len(rows)
            succ = 100 * sum(r["success"] for r in rows) / n
            med = lambda k: statistics.median(r[k] for r in rows if r.get(k) is not None)  # noqa: E731
            cost_rows = [r["cost_usd"] for r in rows if r.get("cost_usd") is not None]
            cost = f"{statistics.median(cost_rows):.4f}" if cost_rows else "n/a"
            print(
                f"| {task_name} | {arm} | {n} | {succ:.0f}% | {med('model_calls'):.0f} | {med('prompt_tokens'):.0f} | "
                f"{med('cached_tokens'):.0f} | {med('completion_tokens'):.0f} | {cost} | "
                f"{med('tool_calls') if any(r.get('tool_calls') is not None for r in rows) else 'n/a'} | "
                f"{med('tool_errors') if any(r.get('tool_errors') is not None for r in rows) else 'n/a'} | {med('wall_s'):.0f} |"
            )
    print(sep)
    for arm in arms:
        rows = totals[arm]
        if not rows:
            continue
        n = len(rows)
        succ = 100 * sum(r["success"] for r in rows) / n
        med = lambda k: statistics.median(r[k] for r in rows if r.get(k) is not None)  # noqa: E731
        print(f"| TOTAL | {arm} | {n} | {succ:.0f}% | {med('model_calls'):.0f} | {med('prompt_tokens'):.0f} | " f"{med('cached_tokens'):.0f} | {med('completion_tokens'):.0f} | | | | {med('wall_s'):.0f} |")


def resolved_config_summary(c):
    return {
        **{k: v for k, v in c.items() if k != "OUT"},
        "OUT": str(c["OUT"]),
        "tasks": list(TASKS.keys()),
        "arms": list(ARM_RUNNERS.keys()),
        "import_error": IMPORT_ERROR,
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dry-run", action="store_true")
    sub = ap.add_subparsers(dest="cmd")
    p_run = sub.add_parser("run")
    p_run.add_argument("--arm", required=True, choices=list(ARM_RUNNERS.keys()))
    p_run.add_argument("--reps", type=int, default=None)
    p_run.add_argument("--only", default=None, help="comma-separated task names")
    sub.add_parser("report")
    args = ap.parse_args()

    c = cfg()
    if args.dry_run or args.cmd is None:
        print(json.dumps(resolved_config_summary(c), indent=2))
        return 0
    if args.cmd == "run":
        return do_run(c, args.arm, args.reps or c["REPS"], args.only.split(",") if args.only else None)
    if args.cmd == "report":
        return do_report(c, list(ARM_RUNNERS.keys()))
    ap.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
