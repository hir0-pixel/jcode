# Factr-I harness design: one store, jcode memory, Prime learning, EveStack observability

Status: draft v1 for opus-architect scoring. Nothing here is implemented yet.
Scope: `sovereign-engine` (Rust). The desktop UI is out of scope.

This is not a new design. It is two faithful ports glued to one SQLite store:

1. **Memory = jcode's memory**, as it worked before commit `c80f1170d` (automatic periodic and end-of-session extraction, trust levels, scopes, recall). Storage stays in `sovereign.db`.
2. **Learning = Prime Agent's learning** (`/tmp/prime-agent`, PrimeIntellect-ai/prime-agent @ 2d24ad4), as it works in the original TypeScript. Every difference in the current Rust port (`crates/sovereign-prime`) is listed in section 5 with a decision.
3. **Observability = EveStack**, extended so every memory and learning step leaves a span.

Notation. R = repo root. `base` = `crates/jcode-base/src`. `app` = `crates/jcode-app-core/src`. `gw` = `crates/sovereign-gateway/src`. `SP` = `crates/sovereign-prime/src`. `PA` = `/tmp/prime-agent/packages/coding-agent/src/core`. `c80` = `git show c80f1170d^:<path>` (upstream before the deletion).

## 0. Evidence base (what was read)

| Area | Where | Key facts |
|---|---|---|
| Store | `base/src/memory_store.rs:27-58` | `memories(rid,id,scope,active,content,tags)`, `memory_entries(rid,entry JSON,embedding)`, FTS5 `memories_fts` (porter unicode61) with triggers, `memory_graphs`, `memory_meta`. Scopes `global` and `project:<hash>`. |
| Dedupe today | `memory_store.rs:262-281` | Exact trimmed content + same category only, then `reinforce`. No normalized or similarity dedupe on write. |
| Recall | `base/src/memory_agent.rs:21-48`, `memory.rs:867-895`, `memory_recall.rs:24-50` | Every fresh user turn: FTS query from last 12 messages (8 KiB), BM25, 20 candidates, filter already-injected, term floor, top 5. No token budget. |
| Injection | `app/agent.rs:565-570`, `base/src/memory/pending.rs` | Trailing user message in a system-reminder block (prefix stays cached). Per-session injected-id set, 45 min TTL, 80% overlap suppression. |
| Extraction (deleted) | `c80` `memory_agent.rs:15,240-252,417-446`; `sidecar.rs:706-775`; `memory.rs:920-952` | Every 12 turns; on disconnect, swarm close, `/save`, TUI/REPL exit (>= 4 messages; >= 200 chars). Prompt lists 4 categories and a "Do NOT extract" list. Output `CATEGORY|CONTENT|TRUST`. Existing memories were passed (80 x 150 chars) only on the TUI end path. Stored at project scope. Gate `agents.memory_sidecar_enabled` default true. |
| Learning (port) | `SP/refine.rs`, `SP/entries.rs`, `gw/learn.rs`, `gw/rpc.rs:813-859` | Gate then refine, interval 25, cooldown 20 min, per session, on the chat model. Memory edits go through `remember_global`. |
| Learning (original) | `PA/refinement/refinement.ts`, `PA/agent-session.ts` | Same two stages. Triggers `turn_interval` and `compact`. Also runs before dispose if due. File-based JSON. Relevance-ranked digest injected. |
| Observability | `gw/observability.rs`, `observability/schema.rs` | Same `sovereign.db`. `spans` table with `attributes` JSON. Only `execute_tool` and model-usage spans exist. Memory and learning decisions are unobserved. |
| Loops | `app/agent/turn_loops.rs:45-1198`, `SP/agent_loop.rs`, `PA/../packages/agent/src/agent-loop.ts:304-449` | See section 6. |

Not verifiable: the claim that Prime "scores well on GAIA". `grep -ri gaia /tmp/prime-agent` finds only a compaction test fixture; the README and eval docs mention SWE-bench. I will not build on that claim.

## 1. The single memory system

### 1.1 Schema
Unchanged: the tables in `memory_store.rs:27-58` are the one memory store. Learned memories, model-written memories, and extracted memories are all rows in `memories` plus a `memory_entries` JSON (`MemoryEntry`: category, trust, strength, reinforcements, source, created_at, updated_at, access_count). Prime's `harness_entries` keeps only prompt, skill, subagent entries and a label plus `reference.memory_id` for memory-kind edits (`SP/refine.rs:274-278`), so memory text exists exactly once.

Additions (one migration via `base/src/migrate.rs`, bumping `user_version`):
- `memory_meta` key `norm_v1`: marks that existing rows had their normalized key computed.
- Column `memories.norm TEXT` plus index `(scope, norm)`. `norm` is the normalized content key (section 1.3). This avoids re-normalizing on each write.
No other schema change. RAM: no new resident structures.

### 1.2 Scopes and trust
Unchanged from jcode: `global`, `project:<hash>`; trust high/medium/low; categories fact, preference, correction, entity.
- Extraction stores at **project** scope (as upstream did, `c80` `memory_agent.rs:86-101`).
- The `memory` tool default stays project, `scope:"global"` for global.
- Learning memory edits: **project scope** by default. Prime's refine prompt says "local by default, global only for stable cross-session lessons" and the auto-instructions say "Do not promote anything global" (`PA/agent-session.ts:1180-1186`). The port's always-global write (`src/sovereign_runtime.rs:857-880`) is a difference, and is reverted (D9).

### 1.3 Automatic extraction (restore)
Restore from `c80`, delete nothing else that stayed deleted (jev, rerank, judge metrics stay gone).

| Trigger | Upstream source | Restored as |
|---|---|---|
| Every 12 turns of a session | `memory_agent.rs:15,240-252` | Counter per session in the gateway/agent; on multiple of 12, run extraction over the last 40 messages / 24,000 chars (`memory_prompt.rs:4-5,234-256`). |
| Session end / disconnect / swarm close | `client_disconnect_cleanup.rs:~266`, `comm_session.rs:1125` | Gateway `session.close`, `session.delete`, WebSocket disconnect of an idle session, and process shutdown call `trigger_final_extraction`. Skip if transcript < 200 chars or < 4 messages. |
| Manual save | `/save`, `TriggerMemoryExtraction` (`wire.rs:393`) | RPC `memory.extract` (gateway) and the existing UI hook. |
| Compaction | **not in upstream** | Added (D3): extraction runs on the pre-compaction messages just before they are summarized, because compaction discards the detail. Subject to the same 4-message floor and a cooldown (below). |

Extraction call:
- Prompt and `CATEGORY|CONTENT|TRUST` format from `c80` `sidecar.rs:706-775`, kept verbatim. Parser `sidecar.rs:908-914`.
- **Existing memories are passed on every path** (upstream only did on the TUI end path): up to 80 related memories, 150 chars each, chosen by FTS against the transcript terms, not by recency (D2). Prompt line "Already known (do NOT re-extract these or close paraphrases)".
- Model: the session's own provider through `complete_simple_with_usage`, as the learning path already does (`src/sovereign_runtime.rs:552-557`). `agents.memory_model` optional override restored.
- Gate: `agents.memory_sidecar_enabled` default true, env `JCODE_MEMORY_SIDECAR_ENABLED`.
- One extraction per session per trigger kind within a cooldown (default 60 s) so end-of-session after a periodic run does not double-spend. Overlap is also harmless because of 1.4.
- Runs off the turn's critical path (spawned task). A failure logs a span with `status=error` and retries at the next trigger. Nothing blocks or fails the user turn.
- Result stored with `remember_project(entry.with_source(session_id).with_trust(t))`, then `mark_memories_known` so the same session is not re-injected with what it just produced (`memory.rs:167` pattern).

### 1.4 Dedupe and merge (hard guarantee in the store)
All writers (tool, extractor, learning, REST) go through `memory_store::remember`/`save_graph`. In `remember`, before insert, in this order:
1. **Exact**: existing behavior (`memory_store.rs:262-281`).
2. **Normalized**: `norm = lowercase, strip punctuation, collapse whitespace, drop stopwords (memory_recall.rs:24-36 list), fold plurals`. Same `(scope, norm)` → reinforce, keep the longer content.
3. **Similar**: FTS candidate query with the new content's terms (limit 8, same scope, same category), then token-set Jaccard on the normalized token sets; `>= 0.80` → merge into the best hit: reinforce, keep the longer content, union tags, keep the higher trust. Below 0.80 → insert.
Threshold and the pipeline are the only deviations from upstream (D1), justified by the requirement of no repetition. Merges emit a `memory.write` span with `action=merged`, the target id and the similarity value, so wrongly merged memories are visible and reversible (`memory_deletions` audit trigger already covers deletes).

Recall-side: keep `is_memory_injected` and the 80% overlap suppression (`pending.rs:48-53`). Add: the injected set is rebuilt from the session on resume (already `sync_injected_memories`), and is also cleared for ids whose content changed (merge) so an updated memory can be shown once more.

### 1.5 Recall and injection budgets
Unchanged mechanics (section 0), plus a hard cap (D4): total injected memory block <= 700 tokens (~2,800 chars, per-memory 400 chars), highest BM25 first, stop when the cap hits. Rationale: upstream had no token budget, which is the one place memory could inflate input tokens unboundedly. Recall itself is local FTS: zero LLM calls, single-digit ms.

## 2. Prime learning on the same store

- `memory`-kind edits: `refine::apply` calls the same `remember` closure into the one store (already true). After section 1.4 they dedupe like every other write. They no longer duplicate what extraction already stored (the refine request lists existing entries and, new, the top related memories).
- `prompt`, `skill`, `subagent` kinds stay in `harness_entries` (Prime's own structure; it has no equivalent in jcode memory). This is not a second memory system: it is Prime's behavior store, which the owner map already assigns to Prime learning.
- Injection of learned entries: see 3.3.

## 3. Learning: faithful triggers

### 3.1 Triggers (D5, D6)
Original: `turn_interval` and `compact`, plus a due review before dispose (`PA/agent-session.ts:3410-3438,4658-4659,5046-5072`); top-level sessions only (`_rlmDepth === 0`, `:9016`).
- `turn_interval`: 25 assistant turns per session, 20 min cooldown (kept).
- `compact`: restored. After a context compaction, run the review regardless of turn count, still subject to the cooldown (deferred while cooling).
- Dispose: on `session.close`/`delete`/shutdown, run the review if due (restored).
- Sub-agents (RLM depth > 0) do not trigger review (port did not check; add).
- Counters stay persisted in `harness_learn_state` (port improvement, keep: a restart does not lose the count).

What this does and does not cover, honestly: a one-turn session (the benchmark) still never reaches a learning review, exactly as in the original. That case is covered by memory extraction at session end (section 1.3), which is jcode's own mechanism. Cross-session: outputs cross sessions (project/global memories, global harness entries when explicitly requested); counters do not. A global counter would be a deviation from Prime and is left as open decision O1.

### 3.2 Review content (D7, D8)
- Gate: restore the current harness overview and refinement history in the gate input (`PA/refinement.ts:1338`, last 40,000 chars); the port's gate sees neither (`gw/learn.rs:164-178`).
- Refine: full conversation last 80,000 chars is the original (`:1245`); the port uses 60,000 chars with tool output compacted to one line each. Keep the port's compaction as a token saving (D7, kept: tool output is excluded by Prime's own prompt rule "never tool output" anyway). Restore refinement history in the refine input.
- Model: original uses the auxiliary model setting, else the session model (`agent-session.ts:9503-9510`). Add `agents.learning_model` optional override (D10).

### 3.3 Injection of learned entries (D11)
Original: a digest of all four kinds, relevance-ranked (IDF term overlap), 6 entries per kind, 180 chars each, last 5 refinement events, delivered as a message on the first turn and at cold boundaries, skipped when its fingerprint is unchanged, re-attached after compaction (`PA/refinement.ts:591-792,794`; `agent-session.ts:7233-7242,8951-8961`).
Port: only `prompt` kind, 6,000 chars, newest first, in the static system prompt of new sessions, gated on `SOVEREIGN_REPL_WORKER` (`SP/entries.rs:459-474`, `app/agent/prompting.rs:165-190`).
Restore the original digest. Budget cap 3,000 chars total (D12, smaller than the port's 6,000 and roughly the original's 6x4x180 = 4,320 worst case). Delivered as a trailing system-reminder message like memory, not into the static prompt, so the cached prefix is stable. The fingerprint check prevents re-injecting an unchanged digest (no repetition).

## 4. Learning apply semantics

Original: per-edit, not atomic; invalid edits recorded `applied:false`, the rest apply; dedupe by id only; stale-baseline guard (`PA/refinement.ts:1066-1161`).
Port: all-or-nothing changeset; guards (max 8 edits, 4,000 chars, secret scanner, no `/Users/`), learned-skill claim (`SP/refine.rs:185-463`).
Decision D13: keep all-or-nothing and the guards. They are safety properties with no behavior the original relies on, and rollback is cheaper to reason about. Flagged for the architect: this is a deliberate deviation.

## 5. Difference ledger: current port vs Prime original

| # | Aspect | Prime original | Current port | Decision |
|---|---|---|---|---|
| D5 | `compact` trigger | `PA/agent-session.ts:3410-3423` | dropped (`gw/learn.rs:173`) | restore |
| D6 | Review before dispose | `agent-session.ts:5046-5072` | absent (`gw/rpc.rs:1572-1577` no hook) | restore |
| D7 | Transcript | full 40k/80k chars | tool output to one line, 60k | keep port (token saving) |
| D8 | Gate context | harness overview + history | none (`gw/learn.rs:164-178`) | restore |
| D9 | Memory scope | local by default | always global (`sovereign_runtime.rs:857-880`) | revert to project |
| D10 | Review model | aux setting else session | session only (`sovereign_runtime.rs:552-557`) | add optional override |
| D11 | Injection | ranked digest, all kinds, fingerprint | `prompt` kind only, static | restore digest |
| D12 | Injection budget | 6 x 4 x 180 | 6,000 chars | 3,000 chars total |
| D13 | Apply | per-edit | atomic changeset + guards | keep port |
| D14 | Storage | JSON files | SQLite | keep (required: one store) |
| D15 | Counter | in memory | persisted per session | keep port |
| D16 | Background planning during tools | yes (`agent-session.ts:4660+`) | no | not ported (RAM/complexity; measure first) |

Memory ledger vs upstream jcode: D1 similarity merge (added), D2 existing memories on every path (upstream only TUI end path), D3 compaction trigger (added), D4 injection token cap (added). Everything else is restored as it was.

## 6. Loop choices, with evidence

Decision: keep jcode's loop (`app/agent/turn_loops.rs`) as the loop. Do not adopt Prime's. Port specific mechanisms where the source shows a measurable win. Evidence is from code reading (loop report) and is labelled as such. None of it is benchmarked yet.

| # | Mechanism (Prime source) | Port into jcode? | Reason |
|---|---|---|---|
| L1 | Native parallel tool execution (`packages/agent/src/agent-loop.ts:667-726`, per-tool sequential opt-out `:593-596`) | Yes | jcode runs calls sequentially (`turn_streaming_mpsc.rs:1362`, `turn_loops.rs:961`) and only nudges toward `batch` (`turn_loops.rs:30-31,142-149`). Saves round trips. `bash`/`edit`/`write` stay sequential. |
| L2 | Tool result cap 50 KB / 2000 lines with tail + spill file (`PA/tools/truncate.ts:11-12`, `bash.ts:651`) | Yes | jcode caps history at 512K chars (`app/agent/tools.rs:5`), about 10x more tokens per runaway output. Spill keeps data reachable. |
| L3 | Completion audit wording (`PA/goals.ts:213-231`) | Yes (text only) | Add to goal continuation (`SP/agent_loop.rs:586-611`). No RAM, no tokens outside goal mode. |
| L4 | Provider retry policy with jitter and Retry-After (`PA/provider-retry.ts:60-107`) | Only after checking the provider crates | jcode retries only context-limit errors in `run_turn` (`turn_loops.rs:185-198`). Unverified whether providers retry internally. |
| L5 | Structured compaction template (`PA/compaction/compaction.ts:434-467`) | Yes | Text swap of `SUMMARY_PROMPT` (`compaction-core lib.rs:77-86`); same token budget, better resume. |
| L6 | Later compaction threshold (`compaction.ts:221-225`) | No | jcode resets the tool/prompt cache on compaction (`turn_loops.rs:92-94`); a trade-off, unmeasured. |
| L7 | Persistent REPL as the only tool, 6.9 KB doctrine (`PA/prompts/rlm.ts:14-52`) | No | jcode's prompt is about 1.3 KB (`system_prompt.md`) vs Prime about 9 KB. Prime's Python kernel is where its 510 MiB mean RSS comes from. jcode already has a `repl` tool. |
| L8 | jcode strengths to keep | keep | Empty/partial response recovery (`response_recovery.rs:241-375`) has no Prime equivalent; prompt-cache discipline; memory as trailing message. |
| L9 | Todo quality-gate digest (`base/src/todo.rs:400`) | Verify, then delete if unwired | Found no caller outside tests. Delete-never-disable rule applies. |

GAIA: the report found no GAIA-specific code in Prime. Its plausible edge is the persistent Python REPL and Serper search. A Serper-style search would need an API key, which I will not enter; the user can add one. Factr-I already has `websearch` (scraping), `webfetch`, `browser`, `jcode-pdf` and a `repl`. No GAIA work until the user says go.

Targeted eval (section 10) decides L1, L2, L5 with data before they stay.

## 7. Observability (EveStack) coverage

Extend `Observer` (`gw/observability.rs`): add `Op::Span { run, kind, name, status, started, ended, attributes, tokens }` and `Observer::span(...)`, since today only `execute_tool` can start a span (`obs.rs:442-448`) and attributes exist only on usage spans (`obs.rs:1120-1131`). Spans attach to the active turn run, or to an aux run if there is none (extraction at session end).

| Event | span kind | attributes (never content unless `capture_content`) |
|---|---|---|
| Recall | `memory.recall` | scopes, query term count, candidates, returned ids, suppressed-as-injected count, term-floor drops, duration_ms |
| Injection | `memory.inject` | ids, chars, est tokens, cap-trimmed count |
| Write | `memory.write` | action (`inserted|reinforced|merged_norm|merged_sim`), id, scope, category, trust, source, similarity, target_id |
| Extraction | `memory.extract` | trigger (`periodic|end|save|compact`), transcript chars, existing shown, extracted N, written N, merged N, tokens in/out, model, error |
| Learning gate | `learning.gate` | trigger, approved, rationale length, tokens |
| Learning refine | `learning.refine` | edits proposed, retry used, tokens |
| Learning apply | `learning.apply` | edits applied per kind, rejected + reason, changeset id, memory ids written |
| Learning inject | `learning.inject` | entries per kind, chars, fingerprint, skipped_unchanged |
| Session-end hook | `session.close_hooks` | which triggers ran, skipped reason |

The existing aux-run records for the model calls (`learn.rs:227,256`; `rpc.rs:2283`) stay; the new spans hold the decisions. Read paths already exist (`/api/sovereign/observability/run?id=`). Add `GET /api/sovereign/observability/memory` summarizing writes/merges/recalls per session so runtime verification and UI both have a stable read.

## 8. Cost per turn (budgets)

Estimates use 4 chars per token. Actuals will be measured in the eval and written back here.

| Item | When | LLM tokens | Notes |
|---|---|---|---|
| Recall | each fresh user turn | 0 | local FTS |
| Injection | when hits exist | <= 700 in | hard cap 1.5 |
| Learned digest | first turn, cold boundary, fingerprint change | <= 750 in | 3,000 chars cap |
| Extraction | every 12 turns + session end + compaction | ~6k transcript + ~3k existing + ~300 out per run, about 750 in/turn amortized | up to ~9k in per run; skipped under 4 messages |
| Learning gate | every 25 turns (cooldown 20 min) | up to ~10k in, ~200 out | about 400 in/turn amortized |
| Learning refine | only if gate approves | up to ~15k in (60k chars), up to ~2k out | about 600 in/turn amortized worst case |
| RAM | steady state | +0 resident | sqlite handles already cached; transcripts transient; no worker unless `SOVEREIGN_REPL_WORKER` |

Benchmark implication: one-turn sessions with < 4 messages skip extraction, so the polyglot baseline does not pay. A `JCODE_MEMORY_SIDECAR_ENABLED=0` switch exists for benchmark arms that must stay clean.

## 9. Implementation order (small commits, each with tests)

1. Store: `norm` column + migration + normalized/similar merge in `remember`. Unit tests on temp DBs (exact, normalized, paraphrase merge, below-threshold insert, trust/tag union, all writers go through it).
2. Observer `span()` API + `memory.write` spans. Tests with an in-memory observer.
3. Restore extraction: sidecar `extract_memories(_with_existing)` module from `c80` (only that part), memory agent counters, prompt, parser. Provider-agnostic function pointer supplied by the runtime so `jcode-base` does not depend on the gateway. Tests with a fake provider.
4. Triggers: periodic (12), session close/delete/disconnect/shutdown in the gateway, `/save` RPC, pre-compaction hook. Cooldown. Tests.
5. Injection cap and merge-aware injected-set. Tests.
6. Learning: `compact` and dispose triggers; sub-agent exclusion; gate context; refine history; project-scope memory writes; `agents.learning_model`.
7. Learning injection digest (ranked, fingerprint, 3,000 char cap) replacing the static `prompt`-only path; delete the old path.
8. Loop ports L1, L2, L5, L3 (each separately, each behind the eval); L4 after checking providers; L9 cleanup.
9. Remaining spans and the summary endpoint; docs.

Every step: commit before and after, `cargo test` on touched crates, temp `JCODE_HOME`/`HERMES_HOME`, delete replaced code in the same commit.

## 10. Test and eval plan

Unit (temp homes only):
- Store: merge matrix (section 9.1). Concurrent writers do not create duplicates (two writers, same paraphrase).
- Extraction parser: malformed lines, empty output, low trust handling, secret-looking content rejected.
- Triggers: periodic at 12, skip under floor, end-of-session once, cooldown, compaction.
- Injection: cap respected, no id injected twice per session, merged memory re-injectable, unchanged digest not re-sent.
- Learning: compact trigger, dispose trigger, sub-agent excluded, project scope writes, atomic apply still rolls back.
- Observability: every action above yields the specified span with the specified attributes.

Integration (scripted fake provider, deterministic):
- Session A: 14 turns with two facts and one paraphrase. Expect extraction at turn 12 and at close; `memories` has N distinct rows; second extraction produces only merges.
- Session B (fresh process, same `JCODE_HOME`): the relevant fact appears in the injected block once, not twice.
- Learning: session of 25 turns produces a gate + refine + apply; digest injected in a later session and reported in `learning.inject`.

Runtime verification (real `sovereign serve`, temp `JCODE_HOME`/`HERMES_HOME`, real local model via Ollama, not the polyglot run): show rows in `sovereign.db`, merges, recall in a second session, learning firing, and the spans via `/api/sovereign/observability/*`.

Targeted loop eval (only after the user approves, and separate from the polyglot benchmark, which the benchmarks chat owns): about 10 of the 29 Python exercises with the highest tool-call counts, same local model, A/B for each of L1, L2, L5 against the current binary: first-try pass, input/output tokens, wall time, RSS. Keep a port only if it does not lose passes and reduces tokens or time. GAIA only when the user says go.

## 11. Open decisions for the user
- O1: add a global (cross-session) turn counter for learning? Not in Prime. Without it, short sessions rely on memory extraction only.
- O2: D13 keep atomic apply (recommended) or revert to Prime's per-edit apply?
- O3: extraction on compaction (D3) and the injection token cap (D4) are additions; accept or drop?
- O4: the pinned baseline binary, its git worktree and the roughly 25 GB build cache under `/private/tmp/claude-501/.../scratchpad/baseline-src` and `baseline-target` belong to the benchmarks chat; I have not touched them.
