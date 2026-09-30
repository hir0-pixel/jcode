# Factr-I benchmark findings: failures caused by the harness

Purpose: every stop or failure in the polyglot run that the Factr-I harness (engine) caused is recorded here with its cause and evidence, so it can be fixed and the affected exercises re-run after Prime and stock Hermes have finished. Run: `docs/benchmark-runs/polyglot-factr-i-d4afb3deb` (engine `d4afb3deb`, pinned binary). Model failures that are not the harness's fault are noted but are not fixes.

## F1. Bash commands outlive the exercise (leaked children, runaway memory)
- Seen: 2026-09-30 12:14 UTC, `python-go-counting`: pass@1 and pass@2 false, rss peak 18,092 MiB.
- Cause: the model's own `python -m unittest` commands (pids 5069, 10567 under engine pid 3685 via `bash -c` wrappers) never exited and stayed alive after the exercise ended, holding about 17 GB and starving Ollama. The runner samples the engine's whole process tree, so 11 following exercises (`go-counting` through `proverb`) show inflated memory (1 GB+ instead of about 50 MiB).
- Engine root cause: the bash tool lets a foreground command outlive its turn and session, with no kill at run end. Hermes' terminal tool and Prime's bash both time out and kill.
- Fix to make (engine, after the run): kill the session's running bash children (process group) at turn end and session end; enforce a default per-command timeout and memory guard; emit a span `loop.guard`/`tool.timeout` when it fires.
- Runner side (done by the benchmark chat, commit `95c71b6ee`): reap every process whose cwd is inside an exercise sandbox after the exercise, for all arms; effective from the next language.
- Re-run after fixing: the 11 contaminated exercises (memory numbers only; pass/fail results are valid), and `go-counting`.
