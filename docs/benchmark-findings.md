# Factr-I benchmark findings: failures caused by the harness

Purpose: every stop or failure in the polyglot run that the Factr-I harness (engine) caused is recorded here with its cause and evidence, so it can be fixed and the affected exercises re-run after Prime and stock Hermes have finished. Run: `docs/benchmark-runs/polyglot-factr-i-d4afb3deb` (engine `d4afb3deb`, pinned binary). Model failures that are not the harness's fault are noted but are not fixes.

## F1. Bash commands outlive the exercise (leaked children, runaway memory)
- Seen: 2026-09-30 12:14 UTC, `python-go-counting`: pass@1 and pass@2 false, rss peak 18,092 MiB.
- Cause: the model's own `python -m unittest` commands (pids 5069, 10567 under engine pid 3685 via `bash -c` wrappers) never exited and stayed alive after the exercise ended, holding about 17 GB and starving Ollama. The runner samples the engine's whole process tree, so 11 following exercises (`go-counting` through `proverb`) show inflated memory (1 GB+ instead of about 50 MiB).
- Engine root cause: the bash tool lets a foreground command outlive its turn and session, with no kill at run end. Hermes' terminal tool and Prime's bash both time out and kill.
- Fix to make (engine, after the run): kill the session's running bash children (process group) at turn end and session end; enforce a default per-command timeout and memory guard; emit a span `loop.guard`/`tool.timeout` when it fires.
- Runner side (done by the benchmark chat, commit `95c71b6ee`): reap every process whose cwd is inside an exercise sandbox after the exercise, for all arms; effective from the next language.
- Re-run after fixing: the 11 contaminated exercises (memory numbers only; pass/fail results are valid), and `go-counting`.

## F1 status: FIXED (commits 0c6db05d2, f1a62f74f)
A cancelled or timed-out turn left its bash child running (the tool task was never aborted), timeout-promoted commands survived the run, the session and SIGTERM, and cancelling a background task only detached it. Now: a dropped foreground bash kills its process group, cron runs cancel their session's leftover tasks, session.delete/REST delete cancel the session's commands, and SIGTERM reaps live tasks. Runner side: the benchmark chat reaps processes whose cwd is inside an exercise sandbox (95c71b6ee).

## F2. Turn ended early while a tool was still running (FIXED, commit 9d1f67918)
- Cause: a bash command running more than about 300 ms made the bridge answer the daemon's stdin probe with id 0; the daemon's `done{0}` was read as "turn complete", so `message.complete` fired with the tool still running and `/api/agent/run` returned early. The benchmark's own test run could then overlap the agent, and headless approval marking was dropped after that point.
- Impact: the local-model polyglot run `polyglot-factr-i-d4afb3deb` (engine d4afb3deb) and any earlier Factr-I run may contain exercises that were scored while the agent was still working, or whose agent lost approvals mid-turn. Treat their pass/fail as unreliable wherever a bash call took more than about 300 ms; the clean re-run on the fixed engine is the valid measurement.
- Fix: the ack id is now a registered control id.

## Other audit fixes since the pushed harness
Bedrock: stop reasons were Rust Debug text so truncation recovery never fired; Sonnet 5.x had no model_info entry so tools were stripped; no retry/backoff; no prompt caching; cache usage hard-coded to none; max tokens unset. All fixed (1b8aa332d). Streamed tool calls without an id are no longer dropped; truncated tool calls from a length stop are discarded unless they parse unrepaired; the repeat guard blocks the call at 5 and ends the turn only at 10; pruning no longer shrinks edit bodies; fuzzy edit, syntax notes, prompt guidance and stop nudges added (2e57cb37f).
