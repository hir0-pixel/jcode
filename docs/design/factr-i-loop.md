# Factr-I loop: what to take from Prime (separate from the memory design)

Decision: keep jcode's loop (`crates/jcode-app-core/src/agent/turn_loops.rs`). Port individual mechanisms only when Prime's source shows a win and an eval confirms it. Evidence below is code reading, not benchmark data. Paths: `PA` = `/tmp/prime-agent/packages/coding-agent/src/core`, agent loop in `/tmp/prime-agent/packages/agent/src/agent-loop.ts`.

| # | Mechanism (Prime source) | Verdict | Reason and precondition |
|---|---|---|---|
| L1 | Parallel tool execution (`agent-loop.ts:667-726`, sequential opt-out `:593-596`) | candidate | jcode runs calls one by one (`turn_streaming_mpsc.rs:1362`, `turn_loops.rs:961`). Before porting, resolve the per-tool urgent-interrupt behaviour and keep result order (`turn_streaming_mpsc.rs:1362-1366`). `bash`/`edit`/`write` stay sequential. |
| L2 | 50 KB / 2000-line tool output cap, tail plus spill file (`PA/tools/truncate.ts:11-12`, `bash.ts:651`) | candidate | jcode caps history at 512K chars (`app/agent/tools.rs:5`). |
| L3 | Completion-audit wording (`PA/goals.ts:213-231`) | candidate | text only, goal mode. |
| L4 | Provider retry policy (`PA/provider-retry.ts:60-107`) | check first | verify whether provider crates already retry (`turn_loops.rs:185-198` retries only context-limit errors). |
| L5 | Structured compaction template (`PA/compaction/compaction.ts:434-467`) | candidate | Prime's template is about 1.5 KB vs jcode's about 0.6 KB `SUMMARY_PROMPT`; token neutrality is unverified, measure it. |
| L6 | Later compaction threshold (`compaction.ts:221-225`) | no | jcode resets its cache on compaction; unmeasured trade-off. |
| L7 | Python-REPL-only tool model, 6.9 KB doctrine (`PA/prompts/rlm.ts:14-52`) | no | jcode already has a `repl` tool and a about 1.3 KB base prompt. Prime's higher RAM (about 510 MiB mean) is measured, its cause is not attributed. |
| L8 | Keep jcode's empty/partial response recovery (`response_recovery.rs:241-375`), cache discipline | keep | no Prime equivalent found. |
| L9 | Todo gate digest (`base/src/todo.rs:400`) | verify, delete if unwired | no caller found outside tests. |

GAIA: the code has no GAIA references (grep finds one compaction fixture); docs mention SWE-bench. The claim that Prime scores well on GAIA is unverified. Its plausible edge is the persistent Python REPL plus Serper search, and a search key is the user's to supply. No GAIA until the user says go.

Eval (only with the user's approval; the polyglot benchmark stays with the benchmarks chat): pick exercises from the 34 Python set with the highest tool-call counts, run each candidate against the current binary with at least 3 repeats on the same local model, compare first-try pass, tokens, wall time and RSS. Keep a port only if there is no pass loss beyond a stated tolerance (set before running) and tokens or time drop by at least a set threshold. Measure L1, L2 and L5 one at a time.
