# Factr-I harness design v2: ONE memory store, jcode memory, Prime learning, EveStack

Status: v2 for opus-architect re-scoring (v1 scored 4.5/10; every finding is addressed below or listed as an open decision). Nothing is implemented yet.
Scope: `sovereign-engine` (Rust). Desktop UI is out of scope. Loop ports moved to `factr-i-loop.md`.

Not a new design. Two faithful ports on one store:
1. **Memory = jcode's memory** as it was before `c80f1170d`.
2. **Learning = Prime Agent's learning** (`/tmp/prime-agent` @ 2d24ad4).
3. **Store = `sovereign.db` `memories`**, the only place any memory-like thing lives.
4. **Observability = EveStack spans** for every step.

Notation: `base` = `crates/jcode-base/src`, `app` = `crates/jcode-app-core/src`, `gw` = `crates/sovereign-gateway/src`, `SP` = `crates/sovereign-prime/src`, `PA` = `/tmp/prime-agent/packages/coding-agent/src/core`, `c80` = `git show c80f1170d^:<path>`.

## 0. Rules this design enforces
- R1 One store. Every fact, preference, correction, entity, and every learned prompt note, skill or subagent spec is a row in `memories`. No other table holds memory text.
- R2 One write path. Every writer calls `memory_store::remember`, which returns `Inserted | Reinforced | Merged(target)`.
- R3 One owner per job. jcode extraction turns conversations into fact-type memories. Prime refine turns conversations into prompt, skill and subagent entries. Refine no longer writes `memory` kind edits (D20).
- R4 One injection channel and budget. Recalled memories and learned entries share one block, one token cap and one injected-id record.
- R5 One observability channel: spans. The content-bearing JSONL (`memory_log.rs`, `memory/activity.rs`) is deleted.
- R6 Delete, never disable. Replaced code goes in the same commit.

## 1. The store

### 1.1 Schema
Existing (`base/src/memory_store.rs:27-58`): `memories(rid,id,scope,active,content,tags)`, `memory_entries(rid,entry JSON,embedding)`, FTS5 `memories_fts`, `memory_graphs`, `memory_meta`.
Changes, one migration:
- `memories.norm TEXT` (normalized key) + index `(scope, norm)`, added to `SCHEMA` for fresh files and by a guarded `Step::Code` for old files (fresh file: `migrate::run` runs before `SCHEMA`, `memory_store.rs:117-118`). `user_version` bump is the only version marker (no `norm_v1`).
- `MemoryEntry` (`crates/jcode-memory-types/src/lib.rs:223-262`) gains `kind`: `fact|preference|correction|entity` (existing categories) or `prompt|skill|subagent` (learned). Skill entries keep `path`/`reference`/`arguments` in the entry JSON. Kind is also FTS-indexed through `tags` so recall can filter.
- Scope values: `global`, `project:<hash>`, and `session:<id>` (Prime's "local", see D9). Session scope rows are dropped on session delete.
- Prime's `harness_changesets` stays only as a rollback log (ids + before/after JSON per edit). It holds history, not memory text a recall would read. `harness_entries` is deleted; its rows migrate into `memories` (kind prompt/skill/subagent) by the same migration.
- `superseded_by` (already on `MemoryEntry`) is used by merges (1.3).
Baseline warning: `user_version` bump makes the pinned benchmark binary refuse any `sovereign.db` this build opened (`base/src/migrate.rs:7`). Benchmark homes must never be opened by the new build.

### 1.2 One write path
- `remember_project`, `upsert_*_memory` (`base/src/memory.rs:317-415`), REST add/edit (`gw/memory_rest.rs:85-104`), the tool, the extractor and learning all call `memory_store::remember`. `save_graph` remains only for tag/link/backfill of existing rows and cannot create content.
- `remember` returns the outcome and the affected id. Learning rollback forgets only ids it `Inserted` and un-merges `Merged` outcomes (restore the superseded row) instead of forgetting whatever id came back (today `refine.rs:380-386`).
- `remember_project` with no cwd falls back to `global` scope rather than failing (today `memory.rs:318-321`, which rolls back a whole changeset).

### 1.3 Dedupe and merge (guarantee applies to every writer)
In order: (1) exact trimmed content + category (existing); (2) same category and equal `norm`; (3) same scope and category, FTS top-8 candidates, token-set Jaccard >= 0.80 **with guards**.
- `norm` = lowercase, strip punctuation, collapse whitespace, plural fold. Polarity words are kept (`not`, `no`, `never`, `don't`, `without`, `instead of`), stopword list from `memory_recall.rs:24-36` minus those. Numbers and version strings are kept as tokens.
- Guards on step 3: no merge if polarity-token sets differ, or numeric tokens differ.
- On a merge the survivor keeps the higher trust and the union of tags, and its content becomes the longer one. The loser is not deleted: it becomes `active=0`, `superseded_by=survivor`. So every merge is reversible; `memory.write` spans carry both ids and the similarity (D1).

### 1.4 Recall and the single injection block
- Recall unchanged (`base/src/memory_agent.rs:21-48`, `memory.rs:867-895`): FTS from recent messages, top 5, term floor, injected-id filter.
- Learned entries (kind prompt/skill/subagent) are found by the same FTS query with the ranking Prime uses as a tie-break (term overlap, `PA/refinement/refinement.ts:591-660`); the query is the same text (last 12 messages, 8 KiB). Prime's own version uses the goal objective plus the last 4 messages; this is D11.
- One block: `# Memory` sections then `# Learned` sections, one cap **700 tokens** (about 2,800 chars, per entry 400 chars) (D4). Prime's digest boilerplate (about 2,645 chars even when empty, measured on 31/31 benchmark sessions) is dropped, and the block is omitted when empty (D12).
- Same `INJECTED_MEMORY_IDS` record for both. A merge clears its id from the record; the clearing is applied after `sync_injected_memories` on resume (`app/agent.rs:531-535`) so a merged memory can appear once more, not repeatedly.
- Injected ids for each turn are stored in the turn's span (`memory.inject`), so recall quality can be compared with outcomes.
- The `memory` kind no longer exists in refine, so no memory is injected by two routes.

## 2. Automatic extraction (jcode's, restored from c80)

### 2.1 Owner and triggers
One owner: `app` (jcode-app-core). No trigger logic in the gateway. All triggers call `trigger_final_extraction` (restored, `c80 memory_agent.rs:417-446`).

| Trigger | Where | Notes |
|---|---|---|
| Every 12 fresh user turns | in-process `MemoryAgent` (`c80 memory_agent.rs:15,213,242-251`) | counter is in memory and resets on restart (ledgered, as upstream). Sessions map pruned on session close (fixes upstream's unbounded map). |
| Session end | `app/server/client_disconnect_cleanup.rs` (single site, as upstream `~266`) | fires for gateway close, window close, headless `end_run` (`gw/rpc.rs:2887-2897`), cron and bot, and for Crashed and Reloading disconnects as upstream did. |
| `/save` | `TriggerMemoryExtraction` (`wire.rs:393`), gateway RPC forwards it | manual. |
| Pre-compaction | hook inside `base` compaction, before the summary replaces messages | only the compacting site sees the full messages (gateway only sees `compacted` after the fact, `gw/map.rs:248`) (D3). |

- **Extracted-through index**: `memory_meta`-style per-session row `extracted_through=<message index>`; each trigger extracts only messages after it. Prevents re-extracting whole transcripts for bot sessions resumed per message (`gw/rpc.rs:2748-2758`) and window reopens.
- Floors (ledgered D21): 200 chars on all paths (upstream `memory_agent.rs:422`); the upstream 4-message floor applied only on REPL exit and is applied on every path here.
- Transcripts: periodic = last 40 messages / 24,000 chars (`c80 memory_prompt.rs:4-5,234-256`), system-reminder blocks **stripped** (upstream did not); end/compaction = messages after `extracted_through`, capped at 24,000 chars newest-first (upstream end-of-session was uncapped; cap is D22).
- Shutdown: the process awaits pending extractions with a 5 s bounded timeout, otherwise the pending window is retried on the next open via `extracted_through`.

### 2.2 The call
- Prompt and `CATEGORY|CONTENT|TRUST` parser verbatim from `c80 sidecar.rs:706-775,908-914`.
- Existing memories: up to 80 related (FTS against transcript terms) x 150 chars on **every** path (upstream only TUI end, D2).
- Trust mapping: one, the agent-path `from_extracted` (unknown = Medium) (D23).
- Model (D24): the active provider through `provider::active_provider_fork()` (`base` provider/mod.rs:106; `compaction.rs:1945` already calls `complete_simple_with_usage`), so no function pointer or gateway dependency. Upstream preferred cheap Codex/Claude sidecars first (`c80 sidecar.rs:190-238`); optional `agents.memory_model` override restored.
- Gate: `agents.memory_sidecar_enabled` default true, env `JCODE_MEMORY_SIDECAR_ENABLED`.
- Aux calls run through one semaphore (1 concurrent extraction/learning call per process) so a local Ollama model is not hit in parallel with the user turn's own KV slot.
- Stored via `remember` at project scope, `source=session_id`, then `mark_memories_known`. Writes always through 1.2.
- **Benchmark policy** is a user decision (O5): a real benchmark session has 14 messages, so extraction fires once per exercise on the same Ollama model. The pinned baseline stays memory-off; a "memory on" run is separate.

## 3. Prime learning (faithful port on the one store)

### 3.1 Triggers
- Counter unit (D17): Prime counts **assistant messages** that are not error/abort (`PA/agent-session.ts:4656-4659`) and checks at the end of the agent run (`:9090-9107`). The port counts once per completed user turn (`gw/rpc.rs:676-696`, `SP/entries.rs:913-930`), which is why the benchmark sessions had `max(turns)=1`. Restore Prime's unit: increment per assistant message.
- `turn_interval` 25, cooldown 20 min, per session, persisted (port improvement, D15).
- `compact`: restored (D5), subject to the cooldown; reviewed after compaction.
- Before dispose: restored (D6). Triggered from the same `client_disconnect_cleanup` site as extraction.
- Top-level sessions only (Prime `_rlmDepth===0`). Whether sub-agent sessions ever reach `schedule_learning` is **unverified**; step 0 (below) confirms it before the claim is kept.
- Headless runs: `end_run` aborts link tasks right after `message.complete` while the pass needs `conn.history` (`gw/learn.rs:204`). Whether a headless run can finish a pass is **unverified**; step 0 confirms it.

### 3.2 Review content
- Windowing (D18): Prime reads the whole conversation (last 40,000 chars gate, 80,000 chars refine, `PA/refinement.ts:1258,1344`) and has no watermark. The port reviews only messages after a `harness_watermark` (`gw/learn.rs:247-249`). Keep the watermark (it prevents re-reviewing the same messages after a restart) and raise the cap to Prime's 40k/80k with tool output compacted to one line (D7).
- Gate gets the current harness overview and refinement history (D8), as the original. The gate cap is 40k, not the port's shared 60k (`refine.rs:18,36`).
- Restore Prime's output caps: 4,096 tokens (gate) and 32,000 tokens (refine) (`refinement.ts:194-195`).
- Restore the sentence "Do not promote anything global unless explicitly requested" (`agent-session.ts:1185`), dropped in `learn.rs:110-121`.
- Retry (D25): the port adds one corrective retry after an approved gate (`learn.rs:126-161`); keep (bounded, one call), ledgered.
- Model: session provider, plus optional `agents.learning_model` (D10).
- Refine input includes the existing related entries (kind prompt/skill/subagent and related memories) so it does not duplicate what extraction stored.

### 3.3 What refine writes (D20, D9)
- Kinds: `prompt`, `skill`, `subagent` only. `memory` kind is removed from the refine schema and prompt: fact-type memories are jcode extraction's job (R3). This departs from Prime (whose refine writes memories) to satisfy one owner per job; ledgered.
- Scope (D9): Prime's "local" means this session only (`refinement.ts:681-699`). In a single store that is `session:<id>`. **Default scope is an open decision (O1)**: faithful = `session:<id>` (lessons help only that session, as in Prime); recommended = `project:<hash>` so lessons reach later sessions. Global only when the user or the model explicitly asks (Prime's rule).
- Apply (D13, D26): keep the all-or-nothing changeset and guards (max 8 edits, 4,000 chars, secret scan) (`refine.rs:185-463`). Cost: the original records `applied:false` rows for rejected edits and applies the rest; here a rejected edit rejects the changeset and the history records one failed changeset with the reason per edit.
- Rollback restores via the change log; the `forget` closure is fixed to cover all scopes (today `MemoryManager::new()` global-only, `src/sovereign_runtime.rs:883-897`).

## 4. Observability
A sink trait lives in `base` (`ObservabilitySink`), installed by the runtime like `runtime_memory_log::install_event_sink` (`app/server.rs:1457`); the gateway implements it over `Observer`. Extend `Observer` with `Op::Span{kind,name,status,started,ended,attributes,tokens}` (today only `execute_tool` spans, `gw/observability.rs:442-448`). The content-bearing memory JSONL is deleted and replaced by spans (R5). Extraction gets its own aux kind `memory` (not `learning`).

| Event | span kind | attributes (ids and counts; content only under `capture_content`) |
|---|---|---|
| Recall | `memory.recall` | terms, candidates, returned ids, suppressed-injected, floor drops, ms |
| Injection | `memory.inject` | ids in context this turn, chars, tokens, cap-trimmed |
| Write | `memory.write` | outcome, id, target id, scope, kind, trust, source, similarity |
| Extraction | `memory.extract` | trigger, chars, existing shown, extracted, written, merged, tokens, model, error |
| Skips | `memory.skip`, `learning.skip` | reason: under floor, cooldown, sidecar off, no new messages, depth>0 |
| Gate / refine / apply | `learning.gate`, `learning.refine`, `learning.apply` | trigger, approved, edits per kind, rejected + reason, changeset id, tokens |
| Learned injection | inside `memory.inject` | learned entry ids, fingerprint |
Alerts: extraction error rate, merge rate. Read path: `/api/sovereign/observability/run?id=` plus new `/api/sovereign/observability/memory` (writes, merges, recalls per session).

## 5. Budgets (per "assistant model call" and per "user turn", stated)
| Item | When | Tokens | Notes |
|---|---|---|---|
| Recall | each fresh user turn | 0 LLM | local FTS |
| Injected block | when hits exist | <= 700 in | shared by memory and learned entries |
| Extraction | every 12 user turns, session end, compaction, `/save` | up to ~9k in (24k chars transcript + 80x150 chars existing), ~300 out | about 750 in per user turn amortized; skipped under floors; end trigger extracts only unseen messages |
| Gate | when the counter is due (25 assistant messages, 20 min cooldown) | up to ~10k in, <= 4,096 out | |
| Refine | only if gate approves | up to ~20k in, <= 32k out cap | budget separately, bounded |
| Compaction boundary | rare | summary + extraction + possibly gate/refine land together; serialized by the aux semaphore | |
RAM: no new resident maps. Prune per-session maps (`MemoryAgent.sessions`, `INJECTED_MEMORY_IDS`, pending map) on session close and TTL, since upstream's are unbounded (same class as the leak fixed in `570a32a25`).

## 6. Difference ledgers
### Prime (port vs original)
| # | Item | Decision |
|---|---|---|
| D5 | `compact` trigger dropped | restore |
| D6 | review before dispose missing | restore |
| D7 | tool output compacted | keep |
| D8 | gate context (overview + history) missing | restore |
| D9 | "local" = this session vs port's global | session scope faithful; default per O1 |
| D10 | aux model setting | add `agents.learning_model` |
| D11 | digest of all kinds, query terms | one shared block, same recall query |
| D12 | digest boilerplate 2,645 chars | dropped, omitted when empty |
| D13 | per-edit apply | keep atomic (D26 consequences) |
| D14 | JSON files | SQLite, one table (required) |
| D15 | in-memory counter | persisted |
| D16 | background planning during tools | not ported |
| D17 | counter unit: assistant messages vs user turns | restore Prime's unit |
| D18 | whole conversation vs watermark | keep watermark, Prime caps |
| D20 | refine writes memory kind | removed (R3) |
| D25 | corrective retry | keep |
### jcode memory (restored vs upstream)
D1 similarity merge (added, guarded, reversible), D2 existing list on every path, D3 pre-compaction trigger, D4 injection cap, D21 floors on every path, D22 end transcript cap, D23 one trust mapping, D24 provider choice, plus stripped system reminders and the extracted-through index (new).

## 7. Implementation order
0. **Spikes (no behavior change)**: confirm headless `end_run` can finish a learning pass; confirm sub-agent sessions and `schedule_learning`; confirm `client_disconnect_cleanup` runs for headless runs. Write results here.
1. Store: `norm`, migration (fresh + old + baseline-refusal tests), `remember` outcome enum, all writers through it, guarded merge with `superseded_by`. Delete the non-`remember` write paths.
2. Sink trait + `Observer::span` + `memory.write` spans; delete `memory_log` JSONL.
3. Restore extractor (module from `c80`, provider via `active_provider_fork`), transcript builders, extracted-through index, floors, semaphore.
4. Triggers in `app`: periodic, single disconnect site, `/save`, pre-compaction. Prune maps.
5. Single injection block with cap, learned entries in `memories`, merge-aware injected set. Migrate `harness_entries` rows, delete the table and its code.
6. Learning: counter unit, `compact` and dispose triggers, gate context, caps, scope, remove `memory` kind, fix `forget`, rollback by outcome.
7. Remaining spans, alerts, summary endpoint, docs.
Each step: commit before and after, `cargo test` on touched crates, temp `JCODE_HOME`/`HERMES_HOME`, replaced code deleted in the same commit.

## 8. Test and eval plan
Unit (temp homes): merge matrix including negation ("prefers not to use X" vs "prefers to use X" must not merge) and number guards; dedupe through tool, project, upsert, REST edit and learning paths; rollback after a merge does not delete a pre-existing memory and restores the superseded row; project-scope forget; no-cwd fallback; fresh, old and baseline-refusing migrations; extraction parser cases; extracted-through index (bot resume, window reopen, link drop, Reloading disconnect); floors; periodic at 12; injection cap, no id twice, merged id re-injectable after resume; learned entries and recalled memories never repeat; counter parity (one prompt with 25 assistant messages triggers a review); compact and dispose triggers; every span kind emitted from the lower crates through the sink.
Integration (fake provider): session A 14 turns with facts and a paraphrase, extraction at turn 12 and close, second extraction only merges; session B in a fresh process recalls once; a 25-message session yields gate, refine, apply; a later session receives the learned entry in the shared block.
Quality evals: extraction precision on a labelled transcript set; a multi-session A/B showing learned entries help a later session.
Runtime: real `sovereign serve`, temp homes, local model: show rows, merges, recall in a second session, learning firing, spans through the API.
No GAIA until the user says go. Benchmarks belong to the benchmarks chat.

## 9. Open decisions for the user
- O1 default scope of learned prompt/skill/subagent entries: session (faithful) or project (recommended).
- O2 keep atomic apply (recommended) or Prime's per-edit apply.
- O3 keep the additions: pre-compaction extraction (D3), injection cap (D4), guarded similarity merge (D1).
- O4 benchmark leftovers (baseline binary, worktree, ~25 GB cache) belong to the benchmarks chat; untouched.
- O5 benchmark policy: pinned baseline stays memory-off; run a separate memory-on arm (extraction adds one aux call per exercise).
