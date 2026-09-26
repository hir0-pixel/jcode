# Prime parity implementation report

## Outcome

Part A cleanup verification is complete. Part B is **not complete**: Monty was
retained, and the required Python kernel, Python skills, RLM messaging API,
Prime prompt guidance, and `prime-parity.mjs` were not shipped. The feature
audit is in [`PRIME_PARITY.md`](PRIME_PARITY.md). No Monty changes or partial
kernel code remain in the worktree.

The blocker is reproducible on the product host: finite `RLIMIT_DATA`,
`RLIMIT_AS`, and `RLIMIT_RSS` limits fail on macOS (`EINVAL` from the child
setup, and `ValueError: current limit exceeds maximum limit` from Python's
`resource` module). Without an enforceable memory cap, and with the current
approval hook classifying only bash commands, switching from Monty's sandbox to
unrestricted CPython would violate the explicit safety requirements. The
temporary implementation was reverted before it could be built into the
product.

## Part A: cleanup review and gates

Reviewed the cleanup inventory, all changed engine paths, registry removals,
desktop launch path, RPC/REST parity inventory, and forwarded Hermes routes.
No product implementation required restoration. One erroneous unit-test
assertion was corrected in commit `2083ba33fd6f7aa98774651cd894fda376591d0c`;
that correction restored the intended generic disabled-tool assertion, not a
product capability. `jcode-tui` and delegate's swarm internals remain because
the sovereign startup and delegate call paths use them.

| Gate | Result |
|---|---|
| Full workspace tests, isolated home, serial, no-fail-fast | 8,030 passed, 25 failed, 59 ignored across 213 test executables; 11 targets failed. Failures span legacy stdin/platform/config/provider/UI assertions, including an SDK `deleteSession` parity assertion. The complete rerun was not green. |
| Live gateway e2e | 7/7 passed: sessions, learning, refine, agent-loop, agent-run, accounting (4/4 proxy calls), replay. Local Ollama only. |
| Desktop TypeScript | Passed: `npx tsc --noEmit -p .` |
| Desktop Python stage | Passed using the already packaged Python runtime and packages; no download. |
| Desktop pack | Passed. |
| Packaged install/launch | Passed; one engine starts, Python starts for Cron and stops on close. |
| Packaged chat approval | Passed; response `sovereign-packaged-ok`. |
| Packaged cron-due with `SOVEREIGN_CRON_AGENT=1` | Failed after the cron agent returned `sovereign-cron-agent-ok` and the job completed: the Hermes Python process remained alive beyond the configured 180-second hold cap. This reproduces the existing cleanup task's failure. |
| `python3 scripts/parity.py` | Passed; 235 JSON-RPC methods and 264 HTTP routes inventoried. No desktop-called route was reported unserved. |

The previous inventory also records the baseline and cleanup deltas. During
this verification, the single-call plain-task schema measurement completed
with local Ollama: Sovereign 7,744 estimated schema tokens (26 tools), Hermes
10,664 (25 tools). No Part B implementation survived, so there is no after
measurement. Budget percentiles and a before/after build-time comparison were
not measured for Part B.

## Part B: Prime capability status

Already implemented and retained in Rust: continual harness entries and
rollback, `/refine`, `/harness`, `/skill create`, the refine model tool,
`/goal`, `/autonomous`, `/heartbeat`, compact `delegate` over swarm internals,
learning, observability, monitors, replay, and the Rust websearch tool. The
current REPL is Monty with persistent per-session variables, `load`,
`llm_query`, and host-call limits. It is not a full CPython kernel.

No Prime skill Python source was copied into the product. Prime's Python
packages, host bridge, and RLM prompt text were inspected from a shallow,
read-only clone; the Prime clone was removed after inspection. The Prime
Serper implementation was not reused because Akira's existing Rust websearch
is the key-free backend requested.

No new Part B gates were run because no Part B feature was retained:

- Python kernel protocol/lifecycle/host-call/skill/messaging tests: not added.
- Warm/cold kernel and Rust dispatch percentile bench: not added or measured.
- `crates/sovereign-gateway/e2e/prime-parity.mjs`: not added or run.
- New disconnect/reconnect combined continuity test: not added or run.
- Tool schema after implementation: not applicable; the measured baseline is 7,744 tokens.

Prime's own terminal UI, installer, hosted services, and Prime credential/trace
flows remain intentionally out of scope. The exact parity rows and additional
gaps are listed in `PRIME_PARITY.md`.

## Revisions

- Engine: `2083ba33fd6f7aa98774651cd894fda376591d0c` before this report commit.
- Hermes: `3c13bea16c780d4cfa5e7a6ccb2123d6d77d8a88`.
