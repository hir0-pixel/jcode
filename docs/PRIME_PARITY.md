# Prime Agent parity

Audit basis: Prime Agent README, `docs/usage.md`, `docs/rlm.md`,
`docs/rlm-runtime.md`, `docs/skills.md`, `docs/long-running-agents.md`,
`docs/architecture.md`, `src/core/prompts/rlm.ts`, and its built-in skill
packages. The Prime repository is reference-only; Akira keeps its single Rust
agent loop.

| Feature | Prime behavior | Akira implementation | Status |
|---|---|---|---|
| Persistent REPL state | Long-lived Python state per session | `crates/sovereign-prime/src/host.rs` and `python_worker.py`; lazy CPython worker, idle reap, session cleanup | Done; 13 REPL integration tests pass |
| Python imports | CPython stdlib and skill packages | Isolated CPython with `-I -S`; stdlib imports and installed skill paths are tested | Done for imports and user-installed packages |
| File context | `load(path)` reads workspace files | Host resolves symlinks, confines reads to workdir, caps at 8 MiB | Done |
| Recursive model calls | Programmatic `llm_query` | Existing-provider callback with per-run host-call cap | Done |
| Kernel lifecycle and limits | Lazy worker, timeout, memory cap, recovery | 20 s run timeout, 128 MiB RSS watchdog, 10 min idle reap, lazy restart; macOS RSS via `proc_pidinfo` | Done; tests enforce cold start <500 ms and warm p95 <5 ms |
| Kernel isolation | Restricted Python side effects | macOS `sandbox-exec` denies network and limits file writes to project/session temp; other platforms require approval per cell and headless sessions deny | Implemented; macOS sandbox tests pass outside restricted test sandbox |
| Spawn a subagent from REPL | Spawn, await, and inspect child result | `spawn_subagent` and `await_subagent` reuse the existing delegate/swarm path | Done; real-worker and reconnect e2e cover child lifecycle |
| Agent messaging | Send, read, list family agents | Python `agent_message` uses swarm internals; model-facing `agent_message` tool routes send/read/list through `CommunicateTool` | Done for engine family roles; schema remains below 200 estimated tokens |
| Goals | Get/complete persistent objective and apply token budget | Rust `ControlStore`, `/goal`, session goal tool, and REPL host callback | Done; structured state, positive token-budget validation, and completion report — see "Goals: long-horizon harness" below for the attempt log, plateau/steer, and verified-completion additions |
| User heartbeat | Schedule prompts for a session | `/heartbeat`, `ControlStore`, and Python host callback | Done for user heartbeats |
| RLM internal heartbeat | Kernel-managed recurring callbacks | `agent_loop_host::heartbeat_host` persists `source=rlm` records (with a persisted `delivery_mode`) in the shared heartbeat table; Python wrapper bridges CRUD | Done; `follow_up` delivers once idle (`due_heartbeat`), `steer` delivers at the next tool-call boundary of a busy turn via a soft interrupt (`due_steer_heartbeat`, `Conn::maybe_steer_heartbeat` in `crates/sovereign-gateway/src/rpc.rs`) — see `agent_loop_host_tests`/`agent_loop` tests `due_steer_heartbeat_ignores_follow_up_and_fires_rlm_steer` |
| Refine / continual harness | Local/global CRUD, rollback, evidence and rationale | Rust `sovereign-prime` entries/refine (one CRUD path, changesets, rollback); bundled Python `prime-refine` wrapper | Rust feature, package, and host request mapping work |
| Executable Python skills | `SKILL.md` plus importable package | Bundled wrappers installed lazily under `~/.jcode/skills`; the worker adds each package `src` to `sys.path`; `/skill create` can author optional package source | Done; imported package, host calls, and package creation are covered by tests |
| Prime skill packages | goal, refine, heartbeat, messages, observe, compact, websearch, skill creator | Bundled package set under `crates/sovereign-prime/src/skills`, installed by `bundled_skills.rs` | Done for supported engine operations, including `rlm_heartbeat.create(..., delivery_mode="steer")` |
| Web search | Prime Serper integration | Bundled `prime-websearch` calls the existing Rust DuckDuckGo tool | Done with the key-free backend; no Serper key or second implementation |
| Compaction | Check and schedule host compaction | Engine compaction is available to chat and desktop; `prime-compact` schedules compaction at the end of the current turn | Done for the host scheduling API; the live e2e verifies scheduling |
| RLM static prompt | Prompt-as-variable and programmatic subcalls | Concise cached static section in `crates/jcode-app-core/src/agent/prompting.rs` | Done |
| Detach / reattach | Long-running session survives client disconnect | Gateway persists sessions and control state | Done; `crates/sovereign-gateway/e2e/prime-parity.mjs` reconnects and checks goal, heartbeat, and subagent state |
| Prime terminal, installer, hosted services | Prime-specific UI, deployment, and paid/hosted services | Not included in Hermes desktop product | Intentionally out of scope |

## Item C validation

Monty is removed; real CPython runs lazily behind the engine-owned framed pipe.
The bundled Prime packages include goal, refine, RLM heartbeat, messaging,
observation, compact, websearch, and skill-creator. RLM heartbeats are stored
separately from user heartbeats. The live `prime-parity.mjs` e2e covers package
imports, `load`/`llm_query`, goal and heartbeat persistence, refine, compact,
websearch, skill creation/import, agent messaging, subagent spawn/await, and
reconnect. The Rust dispatch test measured p95 at 88.8 µs; REPL cold start and
warm p95 are gated at 500 ms and 5 ms. The plain-task tool-schema measurement
is 7,762 tokens, +18 from 7,744 and within the +300 budget. A shallow
read-only Prime clone was used as reference and removed after the audit.

RLM heartbeat `steer` is now implemented rather than rejected: `Heartbeat`
persists a `delivery_mode` (`follow_up` default, or `steer` for RLM-sourced
heartbeats only). `follow_up` is unchanged — delivered once the session is
fully idle via `due_heartbeat`. `steer` is delivered as soon as it is due, at
the session's next tool-call boundary even while a turn is busy, via the same
soft-interrupt primitive the user-facing `session.steer` RPC already used
(`due_steer_heartbeat` in `crates/sovereign-prime/src/agent_loop.rs`, wired at
the `tool.complete` event in `Conn::on_harness_frame` /
`Conn::maybe_steer_heartbeat` in `crates/sovereign-gateway/src/rpc.rs`). This
matches Prime's real semantics per its source (`cron-jobs.ts`,
`agent-session.ts`): `steer` does not abort an in-flight model call either —
it is queued for delivery at the next turn/tool boundary
(`_deliveryPolicy("steer") = "next_turn_boundary"`), not a raw stream
interrupt. `message.complete` in Akira only fires once per whole agentic turn
(`turn_done` in `map.rs`), so `tool.complete` — which fires once per tool call
inside a busy turn — is the equivalent boundary.

## Goals: long-horizon harness

Prime vs Akira, how a goal is driven to completion (Prime source: `core/goals.ts`,
`agent-session.ts`):

- Continuation: Prime injects a `continuation` goal-context message (objective,
  tokens used/budget/remaining, "audit every requirement before completing") after
  each idle turn while `status=active`; Akira's `after_turn` returns
  `Continuation::Goal` and the gateway sends `SessionGoal::continuation_prompt`.
- Driver (`sovereign-gateway/src/rpc/driver.rs`): one engine-level task owns
  continuations, not a desktop window. It holds its own engine link, watches only
  sessions with an active goal, loop or heartbeat (`ControlStore::active_sessions`),
  and yields to any window that has the session open. With no window, goals keep
  going; after an engine restart active goals and loops resume once at startup
  (that resume counts as one turn against the budget); due heartbeats fire from a
  15 s tick that exists only while something is active (otherwise the task parks
  on a poke: finished turn, `/goal`, `/heartbeat`, `session.control`). Continuations
  go through `prompt.submit`, so tracing and learning see them. With no window,
  tool approvals are denied like any headless run. Test:
  `driver_resumes_goals_once_and_fires_due_heartbeats_only_when_idle`.
- Completion: Prime completes via `goal.complete()` in the REPL and builds a
  `completion_budget_report`; Akira uses `session_goal op=complete` (now
  requiring a cited verification) with the same report.
- Budget: Prime accounts input+output tokens per turn, flips to `budget_limited`
  and sends one wrap-up prompt; Akira accounts tokens/turns in `after_turn`,
  pauses on exhaustion and keeps the attempt log in the report (no extra wrap-up
  model turn - intentionally, to avoid spending past the budget).
- Heartbeats: Prime `follow_up` waits for idle, `steer` lands at the next turn
  boundary; Akira matches both (above). Subagent await: both pause continuations
  while children run (`waiting_on_subagents`).
- Tokens: the continuation prompt is short and per-turn; the static prefix is
  untouched, the attempt log is at most 8 x 80 chars.

On top of the existing `session_goal`/`ControlStore` goal, `SessionGoal` now
carries a bounded attempt log and a required completion verification
(`crates/sovereign-prime/src/agent_loop.rs`, `crates/sovereign-prime/src/agent_loop_host.rs`):

- **Attempt log.** `session_goal op=progress {note, verification, error}`
  appends one capped line (`SessionGoal::record_attempt`, `MAX_ATTEMPT_LOG =
  8` entries, `MAX_ATTEMPT_LEN = 80` chars, oldest dropped first). It is only
  ever injected into the per-turn continuation prompt
  (`SessionGoal::continuation_prompt`), never the cached static prompt
  prefix, so it adds a small bounded amount of text per turn and never grows
  the stable cached prefix. Test: `attempt_log_is_capped_and_truncated`.
- **Plan → implement → evaluate.** The continuation prompt asks the model to
  verify by execution before calling `complete`, and `session_goal
  op=complete` now requires a non-empty `verification` string (what was run
  and its result); completion without one is rejected. Tests:
  `goal_complete_requires_verification_and_stores_it`,
  `goal_skill_host_returns_structured_state_and_enforces_budget_and_completion`.
- **Plateau detection and steer, without a model call.**
  `SessionGoal::plateaued()` is a cheap string-equality check: the last 3
  attempt-log entries are identical (same repeated failing verification/tool
  error, phrased consistently by the caller). When true,
  `continuation_prompt()` appends a short `[Plateau detected]` steer note
  telling the model to abandon the current approach — this reuses the same
  "steer" idea as the RLM heartbeat feature above (a short injected
  redirection, not a new model call), but is delivered inline in the next
  continuation turn since goal continuations are already the next-turn
  boundary. Tests: `plateau_detection_needs_no_model_call`,
  `continuation_prompt_injects_attempt_log_and_steer_on_plateau`.
- **Automatic attempt lines.** The gateway calls `agent_loop::observe_tool` at
  each `tool.complete`; `after_turn` drains it and appends
  `auto t<tools> f<failed> | files=yes/no | ver=pass/none | err=<normalized first error>`
  (paths/digits stripped) with no model call and no prompt tokens, so the log
  and plateau detection do not depend on `op=progress`. Plateau = last 3 auto
  lines with no passing verification AND (same error signature OR no file
  changes). Tests: `auto_attempt_line_recorded_without_op_progress`,
  `repeated_normalized_error_is_a_plateau`,
  `file_change_plus_passing_test_is_not_a_plateau`.
- **Failure recovery.** A failed tool call or failed verification only ever
  becomes an attempt-log entry; `after_turn` never stops a goal for it — only
  `complete`, budget exhaustion (`out_of_budget`), or a user interrupt do.
  Test: `failed_verification_never_ends_the_goal_only_budget_does`.
- **Budget-exhaustion report.** `goal_result`'s
  `completion_budget_report` includes `attempt_log` so a paused/expired goal
  still shows what was tried and verified. Test:
  `budget_exhaustion_report_carries_the_attempt_log`.

### AVO ratchet (NVIDIA arXiv 2603.24517, sec. 3)

Goal runs follow AVO's committed-lineage loop on top of the attempt log
(`crates/sovereign-prime/src/goal_ratchet.rs`, wired in `after_turn_in`):

- **Vector score from real execution.** `observe_tool` records, per
  verification bash command (test/build/check/lint/clippy/pytest/tsc), the
  outcome the engine already sees: exit code, plus pass/fail counts parsed
  from cargo `test result:`, pytest, jest and vitest summaries. The goal keeps
  the best `command -> (passed, failed)` map. A non-zero exit never scores as
  passing. Test: `runner_output_becomes_a_score_vector`.
- **Ratchet checkpoints.** A turn that matches or improves every re-run
  command with no new failures AND changed files is snapshotted to a hidden
  ref `refs/akira/goals/<session>/<n>` (throwaway index + `commit-tree`, so
  untracked files are included and the user's branch, HEAD and index are never
  touched). `(ref, score, attempt line)` goes into a 20-entry lineage; older
  refs are deleted as entries roll off. Failed attempts stay only in the
  attempt log. Non-git workspaces record `no-vcs`.
- **Regression.** If a tracked command gets more failures than its best, the
  continuation prompt names the last good ref and the commands to inspect or
  restore it (`git diff <ref>`, `git checkout <ref> -- <paths>`). Nothing is
  reset automatically. Test: `ratchet_checkpoints_hidden_ref_and_regression_points_at_it`.
- **Supervisor.** When the plateau detector fires, the driver makes ONE cheap
  aux call (`complete`, recorded with `observer.record_aux`) with the
  objective, best score, last 6 checkpoints and the attempt log (capped at
  4.5k chars), asking for 2-3 distinct strategies, injected once into the next
  continuation prompt (never the cached prefix). At most once per plateau
  episode and never within 5 turns of the last call; on failure the existing
  text steer is used. Test: `supervisor_is_once_per_plateau_and_rate_limited`.
- **Completion.** `session_goal op=complete` / `goal.complete()` append the
  best score to the recorded verification and the completion report carries
  `best_score` and `lineage`. Test: `completion_cites_best_score_and_lineage`.
- **Token cost.** Per turn ~0 extra model tokens: parsing and git are local; the
  prompt gains at most a best-score line and 3 checkpoint lines (plus the
  regression note when applicable). The only model spend is the rare
  supervisor call.

## Automatic learning (auto-refine)

One data flow, all Prime's design (`refinement.ts` `reviewAutoRefine`,
`agent-session.ts` `_maybeAutoRefine`):

1. **Trigger** (`sovereign-gateway/src/rpc.rs` `schedule_learning`): a per-chat
   assistant-turn counter, persisted per session in `sovereign.db`
   (`harness_learn_state`, so a reconnect or restart keeps counting); due at
   `turnInterval` (25) turns and `cooldownMs` (20 min) since the last gate call.
   On by default for every provider (local or cloud); the one switch is the
   persisted engine setting `learning.enabled` (`config.get` / `config.set` RPC,
   stored in `sovereign.db` `engine_settings`; the old `SOVEREIGN_LEARNING` env
   var is gone). A `refine` scheduled by the model-callable
   tool or the REPL runs at the end of the turn regardless. No idle timer, no
   keyword pre-filter.
2. **Gate** (`learn.rs`): one cheap model call over the unexamined messages
   (watermark in `EntryStore`); `shouldRefine=false` costs only that call.
3. **Refine CRUD** (`sovereign-prime/src/refine.rs`): one call producing
   create/update/delete edits for prompt, memory, skill and subagent entries,
   applied as a changeset (rollback via `/refine rollback`). Skill edits must
   carry `reference{type:"python",import,callable|call_pattern}` and `arguments`
   (Prime's `validateEdit`) and are materialized as `~/.jcode/skills/<slug>/SKILL.md`
   for the existing Python worker import path. There is no evidence-substring
   gate (Prime has none).
4. **Storage and recall**: prompt entries are appended to the cached static
   prefix of new sessions, capped at 6,000 chars in total (newest entries win the
   budget, emitted oldest-first so the prefix is stable turn to turn), and no
   longer depend on Python being present (only the REPL guidance does); a memory's text is stored once in jcode's Rust memory
   store (its `EntryStore` row is a label plus `reference.memory_id`), and is
   recalled and injected once, capped, by jcode's own recall.

Token cost per turn: unchanged on the hot path (static prefix and per-turn
injection are the same). Background calls: before, one gate-free pass after each
signal-bearing idle chat (keyword filter, ~120 s idle, cadence 10 turns) and
`learning.review` extra calls; now one gate call per 25 turns at most every
20 minutes (plus the refine call only when the gate approves). The refine
request is about 200 tokens larger (skill reference schema), paid only then.

Hermes side agents: `prompt.background`, `prompt.btw` and `preview.restart` are
served by the engine (`sovereign-gateway/src/rpc/side_agents.rs`), so no Python
`AIAgent` runs and Hermes's Python background review and curator never start.
Background and preview runs are hidden headless sessions (approvals denied);
`/btw` is one tool-less model call over a transcript snapshot.

Rollback safety: a refine delete of a memory keeps the memory's text and category
in the changeset, so `/refine rollback` restores it to jcode's memory store
(`Forget` returns the text); rolling back a skill create or update removes or
restores the `SKILL.md` it wrote (also on an all-or-nothing apply failure).
Side-agent runs (`prompt.background`, `preview.restart`) are labeled
`background` / `preview` in observability instead of `cron`.
`spawn_tree.list/load/save` are derived from the engine's child sessions
(`sovereign-gateway/src/rpc/spawn_tree.rs`; virtual path `spawn-tree:<parent>`).
