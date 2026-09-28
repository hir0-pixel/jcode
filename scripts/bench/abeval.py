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
import re
import secrets
import shutil
import signal
import socket
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
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
        "PRIME_KERNEL_VENV": os.environ.get("PRIME_AGENT_KERNEL_VENV", "/tmp/prime-agent/kernel-venv"),
        "PRICE_TABLE": os.environ.get("SOVEREIGN_PRICE_TABLE", str(ENGINE_ROOT / "scripts" / "sovereign-prices.json")),
        "LEARN_IDLE_S": float(os.environ.get("BENCH_LEARN_IDLE_S", "15")),
        "LEARN_REPS": int(os.environ.get("BENCH_LEARN_REPS", "5")),
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


# ---------------------------------------------------------------- process-tree RSS sampling
def ps_snapshot():
    """One `ps -A` sample as {pid: (ppid, rss_kib)}. macOS/BSD and Linux both
    support `ps -o pid=,ppid=,rss= -A`. Never raises; returns {} on failure so a
    sampling hiccup can't take down a benchmark run."""
    try:
        out = subprocess.run(
            ["ps", "-ww", "-o", "pid=,ppid=,rss=,command=", "-A"],
            capture_output=True, text=True, timeout=5,
        ).stdout
    except Exception:
        return {}
    table = {}
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 3:
            continue
        try:
            pid, ppid, rss_kib = int(parts[0]), int(parts[1]), int(parts[2])
        except ValueError:
            continue
        table[pid] = (ppid, rss_kib, parts[3] if len(parts) > 3 else "")
    return table


def descendants(root_pid, table):
    """pids of root_pid plus every descendant (inclusive), from a
    {pid: (ppid, rss_kib)} snapshot. Pure function of its inputs so it can be
    unit-tested against a fake table (see _selfcheck_process_tree)."""
    if root_pid not in table:
        return set()
    children = {}
    for pid, (ppid, *_rest) in table.items():
        children.setdefault(ppid, []).append(pid)
    seen = {root_pid}
    frontier = [root_pid]
    while frontier:
        nxt = [c for pid in frontier for c in children.get(pid, []) if c not in seen]
        seen.update(nxt)
        frontier = nxt
    return seen


def tree_rss_mib(root_pid, table, marker=None):
    """Summed RSS (MiB) of root_pid's whole process tree. Attribution note:
    this only ever walks DOWN from the arm's own root pid, so the counting
    proxy and Ollama - separate processes the arm never spawns - are excluded
    by construction, with no denylist needed."""
    pids = descendants(root_pid, table)
    # Detached helpers (e.g. a daemon that re-parents to launchd) escape the
    # tree walk; count them too when their command line names this run's
    # unique work dir.
    if marker:
        for pid, (_ppid, _rss, *cmd) in table.items():
            if cmd and (marker(pid, cmd[0]) if callable(marker) else marker in cmd[0]):
                pids |= descendants(pid, table)
    if not pids:
        return 0.0
    return sum(table[pid][1] for pid in pids) / 1024.0


def _rss_sample_loop(proc, samples, stop_evt, interval_s=0.2, marker=None):
    while not stop_evt.is_set() and proc.poll() is None:
        samples.append(tree_rss_mib(proc.pid, ps_snapshot(), marker))
        stop_evt.wait(interval_s)


def short_socket_dir():
    """macOS caps AF_UNIX paths at 104 bytes; bench work dirs are longer, so
    Prime's daemon socket lives in a short /tmp dir."""
    return tempfile.mkdtemp(prefix="pd-", dir="/tmp")


def _named(table, name):
    return {pid for pid, (_ppid, _rss, *cmd) in table.items() if cmd and cmd[0].strip().startswith(name)}


def reap_named(name, before):
    """Stop processes named `name` that were not running in `before`."""
    for pid in _named(ps_snapshot(), name) - before:
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass


def run_with_rss(args, cwd, env, timeout_s, input_text=None, marker=None, daemon_name=None, reap=True):
    """Like subprocess.run(capture_output=True), but samples the child's whole
    process tree's RSS every ~200ms while it runs. Returns
    (stdout, returncode, rss_peak_mib, rss_mean_mib).

    daemon_name: a detached helper that renames itself (Prime's `prime-agent`
    daemon) matches neither the tree nor a path marker; any NEW process with
    that name is counted while the run lasts and stopped when it ends, so it
    can't leak into the next run."""
    if daemon_name:
        before = _named(ps_snapshot(), daemon_name)
        marker = lambda pid, cmd: pid not in before and cmd.strip().startswith(daemon_name)
    proc = subprocess.Popen(
        args, cwd=cwd, env=env,
        stdin=subprocess.PIPE if input_text is not None else None,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, encoding="utf-8", errors="replace",
    )
    samples = []
    stop_evt = threading.Event()
    sampler = threading.Thread(target=_rss_sample_loop, args=(proc, samples, stop_evt, 0.2, marker), daemon=True)
    sampler.start()
    try:
        out, _err = proc.communicate(input=input_text, timeout=timeout_s)
        rc = proc.returncode
    except subprocess.TimeoutExpired:
        proc.kill()
        out, _err = proc.communicate()
        rc = -9
    finally:
        stop_evt.set()
        sampler.join(timeout=2)
    if daemon_name and reap:
        reap_named(daemon_name, before)
    rss_peak = max(samples) if samples else 0.0
    rss_mean = (sum(samples) / len(samples)) if samples else 0.0
    return out, rc, rss_peak, rss_mean


def _selfcheck_process_tree():
    # Fake ps table: pid -> (ppid, rss_kib). 100 is the arm's root; 200 is an
    # unrelated sibling tree (stands in for the counting proxy / Ollama) that
    # must never be counted.
    table = {
        1: (0, 1000),
        100: (1, 5000),
        101: (100, 2000),
        102: (101, 3000),
        200: (1, 9999),
        201: (200, 1234),
        300: (1, 700, "prime-daemon --socket /tmp/run-x/prime-daemon.sock"),
    }
    assert descendants(100, table) == {100, 101, 102}
    assert descendants(1, table) == {1, 100, 101, 102, 200, 201, 300}
    assert descendants(999, table) == set()
    assert tree_rss_mib(100, table) == (5000 + 2000 + 3000) / 1024.0
    assert tree_rss_mib(999, table) == 0.0
    assert tree_rss_mib(100, table, "/tmp/run-x") == (5000 + 2000 + 3000 + 700) / 1024.0
    print("process-tree self-check: OK")


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
                "[provider]",
                'default_provider = "bench"',
                "",
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
    args = [c["SOVEREIGN_BIN"], "--provider", "openai-compatible", "--model", c["MODEL"], "serve", "--host", "127.0.0.1", "--port", "0"]
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
    out, rc, rss_peak, rss_mean = run_with_rss(
        [c["HERMES_VENV_PY"], "-m", "hermes_cli.main", "chat", "--query", q, "--quiet", "--max-turns", "30", "--accept-hooks", "--model", c["MODEL"]],
        cwd=str(work),
        env=env,
        timeout_s=timeout_s,
        marker=str(work),
    )
    out = (out or "").strip()
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
        "rss_peak_mib": round(rss_peak, 1),
        "rss_mean_mib": round(rss_mean, 1),
    }, out, work


# ---------------------------------------------------------------- sovereign arm
def run_sovereign_task(c, proxy, work, run_id, task_name, timeout_s):
    home = work / "home"
    token = __import__("secrets").token_hex(24)
    proc, port = start_sovereign(c, home, token)
    # Idle RSS: the gateway's own tree right after it's ready, before any task
    # traffic. Useful even though this script restarts the gateway per task
    # (see start_sovereign/stop_sovereign above); if that changes and the
    # gateway becomes long-lived across tasks, this is measured fresh here on
    # every task anyway.
    rss_idle = tree_rss_mib(proc.pid, ps_snapshot(), str(work))
    proxy.tag(run_id)
    q = TASKS[task_name].replace("{WORK}", str(work))
    samples = []
    stop_evt = threading.Event()
    sampler = threading.Thread(target=_rss_sample_loop, args=(proc, samples, stop_evt, 0.2, str(work)), daemon=True)
    sampler.start()
    t0 = time.time()
    try:
        result = agent_run(port, token, q, str(work), run_id, timeout_s)
        ok = bool(result.get("ok"))
        text = result.get("text") or ""
        session_id = result.get("session_id")
        error = result.get("error")
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        ok, text, session_id, error = False, "", None, str(exc)
    finally:
        stop_evt.set()
        sampler.join(timeout=2)
    wall_s = time.time() - t0
    rss_peak = max(samples) if samples else rss_idle
    rss_mean = (sum(samples) / len(samples)) if samples else rss_idle
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
        "rss_idle_mib": round(rss_idle, 1),
        "rss_peak_mib": round(rss_peak, 1),
        "rss_mean_mib": round(rss_mean, 1),
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
    env.update({"HOME": str(home), "PRIME_AGENT_CODING_AGENT_DIR": str(agent_dir), "PRIME_AGENT_KERNEL_VENV": c["PRIME_KERNEL_VENV"]})
    proxy.tag(run_id)
    prompt = TASKS[task_name].replace("{WORK}", str(work))
    started = time.time()
    sock_dir = short_socket_dir()
    raw, rc, rss_peak, rss_mean = run_with_rss(
        ["node", str(cli), "--mode", "rpc", "--provider", "bench", "--model", f'bench/{c["MODEL"]}',
         "--cwd", str(work), "--daemon-socket", f"{sock_dir}/d.sock",
         "--offline", "--no-session"],
        cwd=str(work), env=env, timeout_s=timeout_s, daemon_name="prime-agent",
        input_text=json.dumps({"type": "prompt", "message": prompt}) + "\n",
    )
    raw = raw or ""
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
        "rss_peak_mib": round(rss_peak, 1),
        "rss_mean_mib": round(rss_mean, 1),
    }, output, work


# ---------------------------------------------------------------- learn mode (self-improvement)
# Same trap repo as crates/sovereign-gateway/e2e/prime-trap.mjs's makeTrapRepo,
# ported to Python so this file has no runtime dependency on that script: the
# real test convention (`./scripts/check.sh --fast`) can only be discovered by
# first failing the obvious `npm test`.
LEARN_TASK_PROMPT = "Run this project's test suite and tell me whether it passes."
LEARN_TRAP_RE = re.compile(r"broken stub|npm test is a broken stub", re.I)
LEARN_SUCCESS_RE = re.compile(r"all checks passed", re.I)


def make_trap_repo(work):
    (work / "scripts").mkdir(parents=True, exist_ok=True)
    (work / "package.json").write_text(
        json.dumps(
            {
                "name": "trap-project",
                "version": "1.0.0",
                "scripts": {"test": 'echo "npm test is a broken stub here - use ./scripts/check.sh --fast instead" && exit 1'},
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    check_sh = work / "scripts" / "check.sh"
    check_sh.write_text(
        "\n".join(["#!/bin/sh", "set -e", 'echo "checking..."', "test -f package.json", 'echo "all checks passed"', ""]),
        encoding="utf-8",
    )
    check_sh.chmod(0o755)
    (work / "index.js").write_text("module.exports.add = (a, b) => a + b\n", encoding="utf-8")
    subprocess.run(["git", "init", "-q"], cwd=str(work), capture_output=True)


def _learn_signals(text):
    text = text or ""
    return {"hit_trap": bool(LEARN_TRAP_RE.search(text)), "success": bool(LEARN_SUCCESS_RE.search(text))}


# --- hermes: one-shot `hermes chat` per session, HERMES_HOME kept across A/B so
# any memory/skill file the model (or a background review) writes on session A
# is still on disk for session B. Each session gets its own cwd (fresh trap
# repo) and its own ATOF trace file (mode=overwrite, so reusing one path per
# session is safe), matching run_hermes_task's mechanics exactly.
def learn_hermes_session(c, proxy, home, cwd, tag, timeout_s):
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
    atof = cwd / "run.atof.jsonl"
    relay_config = cwd / "relay-plugins.toml"
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
    proxy.tag(tag)
    t0 = time.time()
    out, rc, rss_peak, rss_mean = run_with_rss(
        [c["HERMES_VENV_PY"], "-m", "hermes_cli.main", "chat", "--query", LEARN_TASK_PROMPT, "--quiet", "--max-turns", "30", "--accept-hooks", "--model", c["MODEL"]],
        cwd=str(cwd), env=env, timeout_s=timeout_s, marker=str(cwd),
    )
    out = (out or "").strip()
    wall_s = time.time() - t0
    tool = score_hermes_run(atof) or {}
    atof_text = atof.read_text(encoding="utf-8") if atof.exists() else ""
    sig = _learn_signals(out + "\n" + atof_text)
    pm = proxy_metrics(proxy.calls_for(tag), c["MODEL"], c["PRICE_TABLE"])
    return {
        "ok": rc == 0,
        "success": bool(rc == 0 and sig["success"]),
        "hit_trap": sig["hit_trap"],
        "tool_calls": tool.get("tools"),
        "failed_tool_calls": tool.get("errs"),
        "wall_s": round(wall_s, 1),
        "rss_peak_mib": round(rss_peak, 1),
        **pm,
        "tail": "\n".join(out.splitlines()[-12:]),
    }


# --- sovereign: one persistent gateway process spans both sessions of a
# repetition (started/stopped by the caller), so its own idle-triggered
# learning pass (SOVEREIGN_LEARN_IDLE_MS/SOVEREIGN_LEARNING=local-idle, the
# engine default for a loopback model - see prime-trap.mjs) has a live process
# to run in during the idle window between A and B.
def sovereign_session_spans(port, token, session_id):
    def api(p):
        req = urllib.request.Request(f"http://127.0.0.1:{port}{p}", headers={"authorization": f"Bearer {token}"})
        return json.loads(urllib.request.urlopen(req, timeout=10).read())

    try:
        runs = [r for r in (api("/api/sovereign/observability/runs?limit=500").get("runs") or []) if r.get("session_id") == session_id]
    except Exception:
        return []
    spans = []
    for r in runs:
        try:
            detail = api(f"/api/sovereign/observability/run?id={urllib.parse.quote(r['id'])}")
        except Exception:
            continue
        spans.extend(detail.get("spans") or [])
    return spans


def learn_sovereign_call(c, proxy, port, token, cwd, tag, timeout_s):
    proxy.tag(tag)
    t0 = time.time()
    try:
        result = agent_run(port, token, LEARN_TASK_PROMPT, str(cwd), tag, timeout_s)
        ok, text, session_id, error = bool(result.get("ok")), result.get("text") or "", result.get("session_id"), result.get("error")
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        ok, text, session_id, error = False, "", None, str(exc)
    wall_s = time.time() - t0
    spans = sovereign_session_spans(port, token, session_id) if session_id else []
    tool_spans = [s for s in spans if s.get("kind") == "execute_tool"]
    failed = [s for s in tool_spans if s.get("status") == "error" or s.get("error")]
    spans_text = "\n".join(json.dumps(s) for s in spans)
    sig = _learn_signals(text + "\n" + spans_text + "\n" + (error or ""))
    pm = proxy_metrics(proxy.calls_for(tag), c["MODEL"], c["PRICE_TABLE"])
    return {
        "ok": ok,
        "success": bool(ok and sig["success"]),
        "hit_trap": sig["hit_trap"],
        "tool_calls": len(tool_spans),
        "failed_tool_calls": len(failed),
        "wall_s": round(wall_s, 1),
        "error": error,
        **pm,
        "tail": text[-2000:],
    }


# --- prime: one-shot `--mode rpc` process per session (like run_prime_task),
# but WITHOUT --no-session (which drops the on-disk session artifact dir that
# a *local*-scope /refine entry needs) and with PRIME_AGENT_CODING_AGENT_DIR
# (agent_dir) kept across A and B, since that is where GLOBAL-scope harness
# entries live (crates' refinement.ts getGlobalHarnessStateDir(agentDir)) -
# only a global refine, or a lesson the model records some other durable way
# under agent_dir, can possibly survive into session B's brand-new chat.
def learn_prime_session(c, proxy, home, cwd, tag, timeout_s):
    cli = Path(c["PRIME_CLI"])
    if not cli.is_file():
        raise RuntimeError(f"Prime CLI missing at {cli}; shallow-clone and build it under /tmp")
    agent_dir = home / ".prime" / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    models_json = agent_dir / "models.json"
    if not models_json.exists():
        models_json.write_text(
            json.dumps(
                {
                    "providers": {
                        "bench": {
                            "baseUrl": f'http://{c["PROXY"]}{c["PROXY_PATH"]}',
                            "api": "openai-completions",
                            "apiKey": c["API_KEY"],
                            "models": [{"id": c["MODEL"], "contextWindow": c["NUM_CTX"], "maxTokens": 4096}],
                        }
                    }
                }
            ),
            encoding="utf-8",
        )
    env = {k: v for k, v in os.environ.items() if not any(s in k for s in ("API_KEY", "TOKEN", "SECRET"))}
    env.update({"HOME": str(home), "PRIME_AGENT_CODING_AGENT_DIR": str(agent_dir), "PRIME_AGENT_KERNEL_VENV": c["PRIME_KERNEL_VENV"]})
    proxy.tag(tag)
    prompt = LEARN_TASK_PROMPT
    started = time.time()
    sock_dir = short_socket_dir()
    raw, rc, rss_peak, rss_mean = run_with_rss(
        ["node", str(cli), "--mode", "rpc", "--provider", "bench", "--model", f'bench/{c["MODEL"]}',
         "--cwd", str(cwd), "--daemon-socket", f"{sock_dir}/d.sock", "--offline"],
        cwd=str(cwd), env=env, timeout_s=timeout_s, daemon_name="prime-agent", reap=False,
        input_text=json.dumps({"type": "prompt", "message": prompt}) + "\n",
    )
    raw = raw or ""
    text_parts, tool_calls, tool_errors = [], 0, 0
    for line in raw.splitlines():
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
    sig = _learn_signals(raw)
    pm = proxy_metrics(proxy.calls_for(tag), c["MODEL"], c["PRICE_TABLE"])
    return {
        "ok": rc == 0,
        "success": bool(rc == 0 and sig["success"]),
        "hit_trap": sig["hit_trap"],
        "tool_calls": tool_calls,
        "failed_tool_calls": tool_errors,
        "wall_s": round(time.time() - started, 1),
        "rss_peak_mib": round(rss_peak, 1),
        **pm,
        "tail": output[-2000:],
    }


def do_learn(c, arm, reps, idle_s):
    if arm == "hermes" and IMPORT_ERROR:
        print(f"cannot import hermes ab_eval.py: {IMPORT_ERROR}", file=sys.stderr)
        return 2
    out_dir = c["OUT"] / "results" / f"{arm}-learn"
    out_dir.mkdir(parents=True, exist_ok=True)
    meta_path = out_dir / "meta.jsonl"
    done = set()
    if meta_path.exists():
        for line in meta_path.read_text(encoding="utf-8").splitlines():
            try:
                done.add(json.loads(line)["run_id"])
            except (ValueError, KeyError):
                continue
    proxy = Proxy(c, c["OUT"])
    time.sleep(0.5)
    try:
        for rep in range(reps):
            run_id = f"learn-r{rep}"
            if run_id in done:
                continue
            run_root = c["OUT"] / "runs" / f"{arm}-learn" / run_id
            if run_root.exists():
                shutil.rmtree(run_root)
            home = run_root / "home"
            dir_a, dir_b = run_root / "repo-a", run_root / "repo-b"
            home.mkdir(parents=True)
            dir_a.mkdir(parents=True)
            dir_b.mkdir(parents=True)
            make_trap_repo(dir_a)
            make_trap_repo(dir_b)
            tag_a, tag_idle, tag_b = f"{arm}|{run_id}|a", f"{arm}|{run_id}|idle", f"{arm}|{run_id}|b"

            if arm == "hermes":
                sess_a = learn_hermes_session(c, proxy, home, dir_a, tag_a, c["TURN_TIMEOUT_S"])
                proxy.tag(tag_idle)
                time.sleep(idle_s)
                sess_b = learn_hermes_session(c, proxy, home, dir_b, tag_b, c["TURN_TIMEOUT_S"])
            elif arm == "prime":
                prime_before = _named(ps_snapshot(), "prime-agent")
                sess_a = learn_prime_session(c, proxy, home, dir_a, tag_a, c["TURN_TIMEOUT_S"])
                proxy.tag(tag_idle)
                time.sleep(idle_s)
                sess_b = learn_prime_session(c, proxy, home, dir_b, tag_b, c["TURN_TIMEOUT_S"])
                reap_named("prime-agent", prime_before)
            elif arm == "sovereign":
                jcode_home = home / ".jcode"
                jcode_home.mkdir(parents=True, exist_ok=True)
                token = secrets.token_hex(24)
                proc, port = start_sovereign(c, home, token)
                try:
                    sess_a = learn_sovereign_call(c, proxy, port, token, dir_a, tag_a, c["TURN_TIMEOUT_S"])
                    proxy.tag(tag_idle)
                    time.sleep(idle_s)
                    sess_b = learn_sovereign_call(c, proxy, port, token, dir_b, tag_b, c["TURN_TIMEOUT_S"])
                finally:
                    stop_sovereign(proc)
            else:
                raise ValueError(f"unknown arm {arm!r}")

            idle_pm = proxy_metrics(proxy.calls_for(tag_idle), c["MODEL"], c["PRICE_TABLE"])
            rec = {"run_id": run_id, "arm": arm, "a": sess_a, "b": sess_b, "idle": idle_pm}
            with open(meta_path, "a", encoding="utf-8") as f:
                f.write(json.dumps(rec) + "\n")
            print(
                f"[{arm}] {run_id} A: success={sess_a['success']} failed_tools={sess_a['failed_tool_calls']} "
                f"hit_trap={sess_a['hit_trap']} calls={sess_a['model_calls']} {sess_a['wall_s']}s | "
                f"B: success={sess_b['success']} failed_tools={sess_b['failed_tool_calls']} "
                f"hit_trap={sess_b['hit_trap']} calls={sess_b['model_calls']} {sess_b['wall_s']}s | "
                f"idle_calls={idle_pm['model_calls']}",
                flush=True,
            )
            shutil.rmtree(run_root, ignore_errors=True)
    finally:
        proxy.stop()
    return 0


def do_learn_report(c, arms):
    for arm in arms:
        meta_path = c["OUT"] / "results" / f"{arm}-learn" / "meta.jsonl"
        if not meta_path.exists():
            print(f"[{arm}] no learn results (run `learn --arm {arm}` first)")
            continue
        rows = [json.loads(l) for l in meta_path.read_text(encoding="utf-8").splitlines() if l.strip()]
        if not rows:
            print(f"[{arm}] no completed repetitions")
            continue
        a_failed = [r["a"]["failed_tool_calls"] for r in rows if r["a"].get("failed_tool_calls") is not None]
        b_failed = [r["b"]["failed_tool_calls"] for r in rows if r["b"].get("failed_tool_calls") is not None]
        avoided = sum(1 for r in rows if r["b"].get("hit_trap") is False)
        med_a = statistics.median(a_failed) if a_failed else None
        med_b = statistics.median(b_failed) if b_failed else None
        passed = med_a is not None and med_b is not None and med_b < med_a and avoided >= 3
        print(f"\n[{arm}] n={len(rows)} reps")
        print(f"  A median failed_tool_calls: {med_a if med_a is not None else 'n/a'}")
        print(f"  B median failed_tool_calls: {med_b if med_b is not None else 'n/a'}")
        print(f"  B avoided the trap in {avoided}/{len(rows)} runs")
        print(f"  pass criterion (B median failed < A median, and B avoids trap >= 3/{len(rows)}): {'PASS' if passed else 'FAIL'}")
        for side in ("a", "b", "idle"):
            mc = [r[side].get("model_calls") for r in rows if r.get(side, {}).get("model_calls") is not None]
            pt = [r[side].get("prompt_tokens") for r in rows if r.get(side, {}).get("prompt_tokens") is not None]
            ct = [r[side].get("completion_tokens") for r in rows if r.get(side, {}).get("completion_tokens") is not None]
            cost = [r[side].get("cost_usd") for r in rows if r.get(side, {}).get("cost_usd") is not None]
            print(
                f"  {side}: model_calls_med={statistics.median(mc) if mc else 'n/a'} "
                f"prompt_tok_med={statistics.median(pt) if pt else 'n/a'} "
                f"completion_tok_med={statistics.median(ct) if ct else 'n/a'} "
                f"cost_usd_med={round(statistics.median(cost), 4) if cost else 'n/a'}"
            )


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
                    "rss_peak_mib": summary.get("rss_peak_mib"),
                    "rss_mean_mib": summary.get("rss_mean_mib"),
                    "rss_idle_mib": summary.get("rss_idle_mib"),
                }
                with open(meta_path, "a", encoding="utf-8") as f:
                    f.write(json.dumps(rec) + "\n")
                print(
                    f"[{arm}] {run_id} success={rec['success']} calls={pm['model_calls']} tools={rec['tool_calls']} "
                    f"errs={rec['tool_errors']} {rec['wall_s']}s rss_peak={rec['rss_peak_mib']}MiB rss_mean={rec['rss_mean_mib']}MiB",
                    flush=True,
                )
                __import__("shutil").rmtree(work, ignore_errors=True)
    finally:
        proxy.stop()
    return 0


def _med_or_na(key, rows):
    if any(r.get(key) is not None for r in rows):
        return round(statistics.median(r[key] for r in rows if r.get(key) is not None), 1)
    return "n/a"


def do_report(c, arms):
    hdr = (
        "| task | arm | n | success% | model_calls | prompt_tok | cached_tok | completion_tok | cost_usd | "
        "tool_calls | tool_errors | wall_s | rss_peak_mib | rss_mean_mib |"
    )
    sep = "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
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
                f"{med('tool_errors') if any(r.get('tool_errors') is not None for r in rows) else 'n/a'} | {med('wall_s'):.0f} | "
                f"{_med_or_na('rss_peak_mib', rows)} | {_med_or_na('rss_mean_mib', rows)} |"
            )
    print(sep)
    for arm in arms:
        rows = totals[arm]
        if not rows:
            continue
        n = len(rows)
        succ = 100 * sum(r["success"] for r in rows) / n
        med = lambda k: statistics.median(r[k] for r in rows if r.get(k) is not None)  # noqa: E731
        print(
            f"| TOTAL | {arm} | {n} | {succ:.0f}% | {med('model_calls'):.0f} | {med('prompt_tokens'):.0f} | "
            f"{med('cached_tokens'):.0f} | {med('completion_tokens'):.0f} | | | | {med('wall_s'):.0f} | "
            f"{_med_or_na('rss_peak_mib', rows)} | {_med_or_na('rss_mean_mib', rows)} |"
        )


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
    ap.add_argument("--selfcheck", action="store_true", help="run internal assert-based self-checks (e.g. process-tree RSS walk) and exit")
    sub = ap.add_subparsers(dest="cmd")
    p_run = sub.add_parser("run")
    p_run.add_argument("--arm", required=True, choices=list(ARM_RUNNERS.keys()))
    p_run.add_argument("--reps", type=int, default=None)
    p_run.add_argument("--only", default=None, help="comma-separated task names")
    p_learn = sub.add_parser("learn", help="self-improvement: cold session A, idle window, brand-new session B, same product home")
    p_learn.add_argument("--arm", required=True, choices=list(ARM_RUNNERS.keys()))
    p_learn.add_argument("--reps", type=int, default=None)
    p_learn.add_argument("--idle-s", type=float, default=None, help="idle window between session A and B, seconds (default: $BENCH_LEARN_IDLE_S or 15)")
    p_report = sub.add_parser("report")
    p_report.add_argument("--learn", action="store_true", help="report learn-mode results instead of the run-mode toolperf table")
    args = ap.parse_args()

    if args.selfcheck:
        _selfcheck_process_tree()
        return 0

    c = cfg()
    if args.dry_run or args.cmd is None:
        print(json.dumps(resolved_config_summary(c), indent=2))
        return 0
    if args.cmd == "run":
        return do_run(c, args.arm, args.reps or c["REPS"], args.only.split(",") if args.only else None)
    if args.cmd == "learn":
        return do_learn(c, args.arm, args.reps or c["LEARN_REPS"], args.idle_s if args.idle_s is not None else c["LEARN_IDLE_S"])
    if args.cmd == "report":
        if args.learn:
            do_learn_report(c, list(ARM_RUNNERS.keys()))
            return 0
        return do_report(c, list(ARM_RUNNERS.keys()))
    ap.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
