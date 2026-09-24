# Sovereign memory and storage: design and build plan

Status (2026-09-24): M1 built, M2 in progress. Where the build departed from this proposal:

- **Chats stay in jcode's session store** (snapshot + append-only journal), not Hermes's
  `state.db`: about 30 jcode modules read those files directly (crash recovery, replay,
  search), so moving them would be a large rewrite for no user-visible gain. The single-store
  goal is met differently: the engine answers every chat-bound RPC and REST method itself and
  never forwards one to Python, so Python never holds chat data (M1, commits 343029d,
  f817151, a52650f).
- **Memory lives in the engine's own `sovereign.db`** (jcode home, WAL), not `state.db`
  (M2). A lean `memories` table (searchable text, scope, active) carries an FTS5 index
  (Porter stemming) kept in sync by triggers; the full entry JSON and the embedding (as
  binary f32, not JSON numbers) sit in `memory_entries` and are fetched only for the final
  top results. Writes touch only changed rows. Measured on a worst-case corpus at 10k
  memories: per-turn recall 57 ms -> 6.2 ms, saving one memory 90 ms (21 MB rewrite) ->
  6.3 ms (`memory_store::tests::scaling`). Rejected on measurement: `detail=column` (2x
  slower here), `temp_store=MEMORY` (no gain), large cache/mmap pragmas (RAM matters more).
  jcode's behaviours are unchanged: recalled memory is appended at the end of the prompt,
  each memory at most once per session, nothing when nothing matches, at most 5.
- **No `memory_injections` table:** jcode already records each injection in the session
  (persisted) and restores the inject-once set when a session is resumed; a table would
  store the same fact twice.


Goal: one app with jcode-level token and RAM efficiency, Prime-style self-improvement and
Evestack-style observability, all on ONE store and ONE agent loop. Cheaper than stock Hermes
on every axis we measure, and at least as cheap as jcode.

---

## 1. What exists today (measured from source, not assumed)

### Stores on disk right now: eight, in two languages

| Store | Owner | Holds |
|---|---|---|
| `~/.hermes/state.db` (SQLite, WAL) | Hermes Python | sessions, messages, 3 full-text indexes on messages (unicode, CJK, trigram), usage, system prompts |
| `~/.hermes/projects.db` | Hermes Python | projects |
| `~/.hermes/shared-state.db` | Hermes Python | cross-process state |
| `MEMORY.md`, `USER.md` | Hermes Python | long-term memory (2,200 + 1,375 char caps) |
| skills (files) | Hermes Python | procedures |
| `~/.jcode/memory/global.json`, `memory/projects/*.json` + graph JSON | jcode (Rust) | long-term memory |
| jcode session JSON files | jcode (Rust) | chats made through the engine |
| `observability.sqlite3` | sovereign-gateway | runs, spans, content |

Consequence (a real bug, not just clutter): Hermes features that still run in Python
(session search, insights, handoff, cron) read `state.db` and cannot see chats made through
the engine. The user has two memories that never meet.

### Where Hermes spends tokens and compute on memory

1. **Background review** (`agent/background_review.py`): forks the whole agent, replays the
   conversation and asks "should a memory or skill be saved?". By default it fires every
   10 user turns for memory and every 10 tool iterations for skills (`agent/agent_init.py`),
   not after every turn. Each firing is a full extra agent run over the whole conversation;
   it shares the prompt cache on hosted APIs, but on a local model it is a second full
   inference that competes for the GPU and can evict the main KV cache.
2. **LLM title upgrade** (`agent/title_generator.py`): an instant deterministic title, then a
   small-model call to improve it.
3. **Auxiliary-model context compression** (`agent/context_compressor*.py`).
4. **MEMORY.md + USER.md in the system prompt**: about 900 tokens on every call. Frozen per
   session, so it caches well. This part is fine.
5. **Three FTS indexes over every message** (unicode + CJK + trigram): the trigram index is
   large on disk. Worth measuring.

### What jcode actually does (and which part is the "efficient" part)

Good, keep these behaviours:
- Recalled memory goes in as a `<system-reminder>` appended near the end of the
  conversation, **never** into the system prompt, so the cached prefix is untouched.
- **Inject once**: each memory is sent at most once per session (`mark_memories_injected`).
- **Never pad**: if nothing clearly matches, nothing is injected (zero tokens).
- Rich memory model: category (fact, preference, entity, correction), trust (user-stated,
  observed, inferred), strength (reinforcement count), confidence that decays and is boosted
  by use, **supersession instead of deletion**, graph links between memories.
- Conversation compaction and a stable, timestamp-free system prompt.

Not efficient, replace these:
- **Whole-file JSON stores**: every recall loads the full memory file into RAM; every save
  rewrites it. Fine at 100 memories, poor at 10,000.
- **Relevance through Jev, a remote service.** Sovereign already switched it off; what runs
  now is a plain in-memory BM25 scan (`memory_jev::select_local`). So today's "jcode memory"
  is jcode's format plus a simple local ranker, not jcode's full pipeline.
- **MiniLM embeddings via tract-onnx**: ~90 MB model downloaded from Hugging Face at first
  use, plus a cross-encoder reranker; resident while loaded (unloads when idle).
- **Memory-extraction agent**: LLM calls to pull memories out of transcripts.

**Conclusion:** jcode's savings come from *behaviours* (where and when memory enters the
prompt, inject-once, never-pad, compaction, stable prefix), not from its storage. Keep the
behaviours and rebuild the storage.

---

## 2. What the research says (working patterns, with evidence)

- **Mem0** (arXiv 2504.19413 and later posts): selective retrieval beats full context at a
  fraction of the tokens (under 7k tokens per retrieval call vs 25k+ full context). Their
  newer extraction is **ADD-only, one LLM call**: never overwrite, keep history, rerank
  toward what is current. About 2x faster extraction, and better for temporal questions.
  Matches jcode's supersession model.
- **Letta sleep-time compute**: do memory consolidation between interactions, not on the
  critical path. Their own docs warn that running it too often burns tokens with diminishing
  returns. So: run on idle, rate-limited, and never per turn.
- **SQLite FTS5 + sqlite-vec + Reciprocal Rank Fusion**: hybrid keyword and semantic search in
  one file, no server. At tens of thousands of memories, a linear scan over packed vectors is
  fast enough. FTS-first is a strong baseline on its own.
- **Model2Vec (potion models, `model2vec-rs`)**: static embeddings in pure Rust, a few hundred
  microseconds per text, tens of MB, no ONNX runtime. A much lighter semantic stage than
  MiniLM + tract.
- **Lazy tool and skill loading** (Hermes issues #6839 and #71894, "Tool Attention" arXiv
  2604.21816, GitHub's reported 62% cut): keep names and one-line summaries in the prompt,
  load full schemas or skill bodies on demand. Same idea as memory: pay only for what is used.
- **LongMemEval** (ICLR 2025): the standard memory eval. Five abilities: information
  extraction, multi-session reasoning, temporal reasoning, knowledge updates, abstention.
  Use a subset to prove quality did not drop while cost did.

---

## 3. Target design: one brain, one store

### 3.1 One SQLite file

Everything lives in **`state.db`** (Hermes's existing file, WAL mode): chats, memory, the
learned harness, traces. Reasons to extend Hermes's file rather than start a new one: the UI
and the remaining Python features already read it, and Hermes already runs it in WAL mode
with a busy timeout.

- **Concurrency:** WAL gives many readers and one writer across processes. Rust and Python
  open the same file directly: no RPC, no translation layer. One user on one desktop is far
  below SQLite's write ceiling.
- **Schema ownership:** Hermes's existing tables stay as they are. New tables and columns are
  **added only**, never altered, through one versioned migration list in Rust
  (`schema_version`), so Hermes's Python code keeps working unchanged.
- `projects.db` and `shared-state.db` stay as they are for now. Merging them is optional,
  later, and only if it measurably helps.
- `observability.sqlite3` moves into `state.db`. Exception: if the benchmark shows trace
  writes contending with chat writes, traces go back to their own file.

### 3.2 Tables (new, added alongside Hermes's)

```
memories(
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL,          -- 'user' | 'project:<abs path>'
  kind TEXT NOT NULL,           -- fact | preference | entity | correction | procedure
  content TEXT NOT NULL,
  tags TEXT,                    -- space-separated
  trust INTEGER NOT NULL,       -- 2 user-stated, 1 observed, 0 inferred
  strength INTEGER NOT NULL DEFAULT 1,
  confidence REAL NOT NULL DEFAULT 1.0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  last_used_at INTEGER, use_count INTEGER NOT NULL DEFAULT 0,
  superseded_by TEXT,           -- never delete on update; supersede
  source_session_id TEXT, source_message_id TEXT,
  embedding BLOB, embedding_model TEXT   -- optional, phase 2b
)
memories_fts  USING fts5(content, tags, content='memories', content_rowid='rowid',
                         tokenize='porter unicode61')
memory_links(from_id, to_id, kind, weight)          -- jcode's graph edges
memory_injections(session_id, memory_id, at)        -- inject-once, survives restarts
harness_revisions(id, parent_id, text, evidence, created_at, active)   -- Prime /refine
skills_index(name, description, path, uses, last_used_at) + skills_fts -- skill bodies stay files
runs, spans, content                                -- observability, moved in as-is
engine_session_state(session_id, data)              -- jcode-only session fields (compaction, tool state)
```

Chats: the jcode loop writes to Hermes's own `sessions` and `messages` tables through a
storage adapter. That is the "keep the engine, replace its plumbing" swap. Anything jcode
needs that Hermes's schema has no column for goes in `engine_session_state`.

### 3.3 Write path: zero extra model calls per turn

| Tier | When | Cost | What |
|---|---|---|---|
| 0: explicit | During the turn | 0 extra calls | The model calls the `memory` tool (remember, update, forget), as both jcode and Hermes already do. Plain rules also capture user statements like "remember...", "I prefer...", "always/never..." at trust 2. |
| 1: consolidation | App idle, or session ended, at most once per session, skipped for sessions under 4 turns | 1 call per session, on idle | One batched ADD-only extraction (Mem0 pattern): new facts only; conflicts become supersessions. Off by default for paid APIs; on for local models. |
| 2: maintenance | Nightly or idle | 0 calls | Decay confidence, merge duplicates, archive long-superseded rows, `fts5 optimize`, incremental vacuum. |

On every write: if FTS (and, when enabled, embedding cosine > 0.9) finds the same memory,
reinforce it (strength + 1, confidence up) instead of inserting a duplicate.

**Removed:** Hermes's periodic background review (every 10 turns or 10 tool steps), and LLM title upgrades on local models
(keep the instant deterministic title).

### 3.4 Read path: under 1 ms, zero model calls

Per user turn:
1. Query = the user's message (plus the tail of the last assistant message if it is short).
2. FTS5 BM25 top 20 within scope (`user` + current project).
3. Score = bm25 x trust weight x confidence x recency; drop anything below the threshold.
   **Never pad.**
4. Drop memories already injected this session (`memory_injections`).
5. Take at most 5 memories and at most **300 tokens**; append them as one `<system-reminder>`
   at the end of the prompt (jcode's placement, safe for the prompt cache).
6. Log what was injected (ids, token count) as a span, so observability shows exactly what
   memory cost each turn.

**Always-on profile:** a small, user-stated preferences block (at most 500 tokens), built
once at session start and frozen for the session, goes into the system prompt. This is
Hermes's frozen-snapshot pattern, and it caches perfectly.

**Past chats:** not injected. The model uses a `session_search` tool over the existing
`messages_fts` when it needs something (jcode already has this tool; point it at `state.db`).
It costs tokens only when used.

**Semantic stage (phase 2b, only if the eval shows keyword recall missing things):**
Model2Vec potion embeddings in pure Rust, stored in `memories.embedding`, fused with FTS via
Reciprocal Rank Fusion. Replaces MiniLM + tract + the cross-encoder.

### 3.5 In-session memory (the biggest token lever in long chats)

Keep jcode's compaction; store compaction summaries in `engine_session_state`. Keep the
system prompt byte-stable (no timestamps; dynamic content only at the end) so the local
server's KV prefix cache survives between turns.

### 3.6 Budgets (definition of "efficient")

| Metric | Target |
|---|---|
| Extra model calls per turn for memory | **0** |
| Memory tokens injected per turn | p95 ≤ 300 |
| Recall latency at 10k memories | p95 ≤ 5 ms |
| Memory subsystem RAM (no embeddings / with Model2Vec) | ≤ 5 MB / ≤ 40 MB |
| Engine idle RSS | ≤ 40 MB (today ~31 MB) |
| Files on disk for chats + memory + traces | 1 (`state.db`) |
| Recall quality vs stock Hermes (LongMemEval subset) | not worse |

---

## 3.7 One owner per job (no overlaps)

Each job has exactly one implementation. The others are removed from the running path, not
kept side by side.

| Job | Hermes | jcode | Prime | Owner |
|---|---|---|---|---|
| Agent loop (turns, tools, streaming) | Python `AIAgent` | Rust loop | - | **jcode** |
| Hard multi-step tasks | - | - | REPL with `llm_query` recursion | **Prime**, as a tool inside the jcode loop |
| Remembering facts and preferences | memory tool + background review every 10 turns | memory tool + extraction agent | - | **One memory** (§3.3/§3.4): tool + rules, one idle ADD-only pass per session |
| Learning how to behave better | background review writes skills | - | `/refine`: evidence-checked, capped, rollback | **Prime learning loop** (below) |
| Learned procedures (skills) | skill files + hub + review-created skills | - | - | Hermes skill **format and hub** as storage; **created only by the Prime loop** |
| Long-chat compression | aux-model compressor | compaction | - | **jcode** |
| Titles | instant + LLM upgrade | - | - | instant deterministic only |
| Past-chat search | FTS5 on `state.db` | `session_search` tool | - | jcode tool over Hermes's index |
| Usage / traces | insights, usage tables | - | - | **Evestack-style** runs/spans in `state.db` |

### The Prime learning loop (replaces Hermes's periodic background review)

Today `/refine` is manual only, so the engine learns nothing unless the user types it. The
loop makes it automatic, with a cost ceiling of one model call per session:

- **Automatic, like Hermes; nobody has to type `/refine`.** Two triggers:
  (a) the session ends or the app goes idle, and (b) for sessions that never end (messaging
  bots, forever-chats) a Hermes-style cadence: every 10 user turns or 10 tool steps. Either
  trigger fires the pass **only if** a learning signal was seen since the last pass: a user
  correction ("no, do X"), a task that succeeded after several tool steps, a failure or
  rollback, or an explicit "remember/learn". No signal, no call. (Hermes fires on the cadence
  whether or not anything was learned.)
- **One call, three outputs:** (a) new facts and preferences → `memories` (ADD-only),
  (b) at most one reusable procedure → a skill proposal, (c) at most one behaviour rule →
  a `harness_revisions` entry.
- **Prime's gates on everything:** every output must quote a user or assistant message
  (never tool output); small diff; size caps (harness about 1k tokens); snapshot and rollback.
  The first skills need user approval; harness edits are shown in Activity.
- **Cost policy (default favours zero paid tokens and zero extra RAM):** the loop runs inside
  the engine process (no new process, no resident model of its own) and, by default, only on
  the local model while the app is idle (`learning = local-idle`). With only a paid API
  configured it stays off unless the user switches it on (`learning = off | local-idle | on`).
  It never sends data to any party the chat itself does not already use.
- **Proof it works (added to the benchmark):** repeat a task class a week of sessions later.
  Pass means fewer turns and tokens than the first time, and fewer than stock Hermes, at a
  lower learning cost (Hermes: a full review run every 10 turns or 10 tool steps, whether or not anything was learned; ours: at most one call per session, only when there is a learning signal).

## 4. Build plan (each phase is one self-contained task; no screenshots needed)

Order matters: a shared store first, then everything that depends on it.

**P0 Baseline** (Cursor is producing it now): `docs/BENCHMARK.md` gives tokens, calls, RAM
and latency for stock Hermes vs Sovereign. Every later phase reports against it.

**M1 One store for chats**
- New crate `sovereign-store`: opens `state.db` (WAL, busy_timeout), runs add-only migrations.
- jcode session persistence writes to Hermes's `sessions`/`messages` (+ `engine_session_state`).
- Importer for existing jcode session JSON (idempotent, dry-run flag, backup first).
- Done when: a chat made through the engine appears in Hermes's Python session search and
  session list; a cross-language test proves it (Rust writes, Python `hermes_state` reads);
  no JSON session files are written anymore.

**M2 Memory core**
- `memories`, `memories_fts`, `memory_links`, `memory_injections`; write path tier 0; read
  path §3.4; inject-once across restarts.
- Importers: jcode memory JSON + graph, and `MEMORY.md`/`USER.md` (trust 2). The `.md` files
  become exports the user can still read and edit, synced back on change.
- Retire the JSON memory store and the Jev path from the sovereign build.
- Done when: the retrieval eval (seeded memories, scripted queries, precision@5, no LLM) runs
  in CI; recall p95 ≤ 5 ms at 10k synthetic memories; 0 extra model calls per turn in the
  counting proxy.

**M3 Stop hidden spend**
- Confirm through the counting proxy that no forwarded path runs Hermes's background review,
  LLM title upgrade or auxiliary compression on the user's model; disable or reroute them.
- Tier 1 consolidation (idle, once per session, ADD-only) and tier 2 maintenance.
- The Prime learning loop (§3.7) lands in this same phase, so removing Hermes's review and
  turning on its replacement happen together; M5 then adds lazy skill loading and polish.
- Done when: model calls per task equal the user-visible turns plus at most one idle
  consolidation per session.

**M4 One agent loop**
- Cron jobs and messaging replies run through the engine's loop with the shared memory, not
  Hermes's Python agent. Python keeps only the platform connectors and the schedule store.
- Done when: a cron job and a messaging reply show up as runs in observability, use the
  engine's prompt, and see the user's memories.

**M5 Self-improvement on the store (Prime)**
- `/refine` revisions in `harness_revisions` (evidence, rollback); skills indexed in
  `skills_index` and loaded lazily (name and description in the prompt, body on demand).
- Procedural memory: after a successful multi-step run, idle-time proposal of a skill, with
  user approval for the first ones.
- Done when: the prompt contains skill summaries only, and a refine/rollback cycle is covered
  by tests.

**M6 Observability on the store (Evestack-style)**
- Move runs/spans into `state.db`; name spans after the OpenTelemetry GenAI semantic
  conventions; optional OTLP export, off by default. Memory recall and injection are spans
  with token counts.
- Done when: every model call, tool call, memory injection and consolidation appears in
  Activity with its tokens.

**M7 API cost layer** (the shipped app uses a cloud API model; local Ollama is only the free
test stand-in, so llama.cpp bundling, KV-cache compression and warm-up are out of scope)
- Lazy tool loading: the ~8k-token tool-schema prefix is billed on every request. Keep tool
  names and one-line summaries in the prompt; load full schemas on demand.
- Provider prompt caching: keep the prefix byte-identical across turns (CI check) and use
  the provider's cache mechanism (explicit cache breakpoints where required, automatic
  prefix caching otherwise), so repeated prefix tokens bill at the cached rate.
- Fewer calls per task: batched tool calls (one response, several independent reads run in
  parallel) and compacted tool results instead of full output pasted back.
- Report cost in money per task (input, cached input, output at the provider's prices),
  not just tokens.

**M8 Production hardening**
- Crash safety (WAL checkpoints, integrity check at start), backup/export/import, "forget
  me" (delete all memories and chats), migration tests from every schema version, Windows
  paths and file locking, benchmark + retrieval eval in CI.

**M9 Fresh-eyes audit** (after M8, by a reviewer who treats the codebase as unseen)
- Because the system is assembled from Hermes, jcode, Prime and Evestack-style parts, audit
  for: dead code (unreachable jcode/Hermes paths, unused crates, disabled-forever features),
  redundancy (two implementations of one job, violating §3.7), duplicated data (anything
  stored twice), and bottlenecks (locks on the hot path, synchronous I/O in the turn loop,
  unbounded queues or scans, per-turn work that could be per-session).
- Evidence required per finding (file, why it is dead/duplicate/slow, measured impact);
  remove or merge, then re-run the benchmark and the test suites to prove nothing regressed.

UI wording and visual polish come after M9.

---

## 5. Sources

- Mem0 paper: https://arxiv.org/abs/2504.19413 ; token-efficient algorithm: https://mem0.ai/blog/mem0-the-token-efficient-memory-algorithm ; benchmarks: https://mem0.ai/research
- Letta sleep-time agents: https://docs.letta.com/guides/agents/architectures/sleeptime/ ; https://www.letta.com/blog/sleep-time-compute/
- Hybrid search in SQLite: https://simonwillison.net/2024/Oct/4/hybrid-full-text-search-and-vector-search-with-sqlite/ ; https://dev.to/soytuber/building-a-hybrid-rag-in-200-lines-sqlite-fts5-sqlite-vec-rrf-38h1
- Model2Vec in Rust: https://github.com/MinishLab/model2vec-rs
- Lazy tool loading: https://github.com/NousResearch/hermes-agent/issues/6839 ; https://github.com/NousResearch/hermes-agent/issues/71894 ; https://arxiv.org/abs/2604.21816
- LongMemEval: https://github.com/xiaowu0162/LongMemEval
- KV cache reuse: https://github.com/ggml-org/llama.cpp/discussions/13606 ; https://github.com/ggml-org/llama.cpp/discussions/20572
