# Factr-I harness: as built

One harness, one store. jcode is the engine (low RAM and tokens), Prime Agent supplies the learning and goal loop, Hermes supplies the desktop-app features, EveStack records everything. Nothing is owned twice. Loop ports and their evidence are in `factr-i-loop.md`.

Paths: `base` = `crates/jcode-base/src`, `app` = `crates/jcode-app-core/src`, `gw` = `crates/sovereign-gateway/src`, `SP` = `crates/sovereign-prime/src`.

## 1. One memory store
- `sovereign.db` table `memories` (+ `memory_entries`, FTS5 `memories_fts`). Everything that is remembered lives here: facts, preferences, corrections, entities (jcode) and Prime's learned `prompt`/`skill`/`subagent` entries (stored as memories with those categories). No other table holds memory text. Prime keeps only `harness_changesets` (rollback log) and its counters.
- Scopes: `global`, `project:<hash>`, `session:<id>` (Prime's "local"). Schema version 7 (`base/migrate.rs`); migration 6 moved `harness_entries` into `memories`, 7 fixed scopes. A v5 binary refuses a v6+ file, so the pinned benchmark baseline must never open a migrated home.
- ONE write path: `memory_store::remember` (`base/memory_store.rs`) for the tool, REST, extraction and learning. It returns `Inserted | Reinforced | Merged`. Near-duplicates of the same category merge when the meaning tokens overlap >= 0.80; a negation, a different number, or a different code spelling blocks a merge. A longer wording replaces the survivor and the old wording is kept as an inactive row (`superseded_by`), so a merge can be undone (at most 3 kept per survivor; reinforcement list capped at 20). Learned entries upsert by exact id and never merge.
- Cross-scope: a project write first looks for a same-category near-duplicate in `global` and merges there, so the same text is never stored (or injected) twice; global writes never touch project rows. The desktop 'add memory' uses the same path.
- Model or user can expire a wrong memory (`memory` tool action `expire`, `POST /api/memory/entries/{id}/expire`): the row stays inactive and is never recalled.

## 2. Automatic memory (jcode, restored from c80f1170d^)
`base/memory_extract.rs`; triggers in `app`. Uses the active provider (`agents.memory_sidecar_enabled`, env `JCODE_MEMORY_SIDECAR_ENABLED=0` turns it off; optional `agents.memory_model`).
- Triggers: every 12 fresh user turns; session end (`app/server/client_disconnect_cleanup.rs`, also on window close, headless end, cron, bot); before compaction. No `/save`: neither Hermes nor Prime has it.
- First launch is safe on old data: a session with no marker is stamped at its current length the first time it is attached, so only new messages are ever extracted; deleting an old session never triggers a learning review.
- Each run reads only messages after the per-session `extracted_through` marker (kept in `memory_meta`), oldest-first, at most 24,000 chars, system reminders stripped, and moves the marker only over what it read. Floors: >= 200 chars and >= 4 messages. Periodic runs have a 60 s cooldown and an in-flight claim; session end and compaction are never skipped by it.
- The prompt lists up to 80 related existing memories (FTS) so the model does not re-extract them. Output `CATEGORY|CONTENT|TRUST`, stored at project scope (global when there is no directory).
- One process-wide permit (`aux_call_permit`) so extraction and learning never call the model together; waits and calls are bounded (120 s).

## 3. Recall and injection (one block)
- Each fresh user turn: FTS lookup over global + project + this session, top 5, term floor, already-injected filter. Injected as a trailing system-reminder (cached prefix stays stable). Cap: 700 tokens (400 chars per memory, 2,800 total); only memories actually shown are marked injected.
- Learned `prompt` notes are behaviour rules: always in the cached static prefix (6,000-char cap, snapshotted once per session) and excluded from recall so they never show twice. Skills and subagents come through normal recall.

## 4. Learning (Prime Agent's, on the same store)
`gw/learn.rs`, `SP/refine.rs`, `SP/entries.rs`, `SP/learned.rs`.
- Two stages as in Prime: a gate decides if a review is worth it, refine proposes small evidence-based edits (prompt notes, skills, subagent specs), applied as one changeset with rollback (`/refine rollback`).
- Triggers: 25 assistant messages per session (Prime's unit), 20 min cooldown; after a compaction (deferred while cooling); before a session is closed/deleted if due. Top-level sessions only. Counters persist.
- Gate sees the harness overview and refinement history; caps 40k chars (gate) / 80k (refine) with tool output compacted; Prime's "do not promote anything global unless asked" is back. Prime's output-token caps (4,096 / 32,000) are not restored: the completion API has no max-tokens argument.
- Scope: "local" edits go to `session:<id>` as in Prime; project or global only when the edit says so. `memory` kind edits do not exist in refine (fact memories are extraction's job, one owner).
- The agent (Hermes `memory` tool, Prime `rlm.harness`) may still write memories and prompt notes directly; that is the original behaviour.

## 5. Observability (EveStack)
`base/obs_sink.rs` is the hook; `gw/observability.rs` records into `spans` (same `sovereign.db`). Ids and counts only, never memory text. Kinds: `memory.write` (inserted/reinforced/merged/forgotten/expired/tagged), `memory.recall`, `memory.inject`, `memory.extract`, `memory.skip` (under_floor, cooldown, sidecar_off, no_new_messages), `learning.gate`, `learning.refine`, `learning.apply`, `learning.skip`, `loop.guard`. Sessionless spans use a fixed `system` run and never appear as sessions. Read: `GET /api/sovereign/observability/memory?session=&limit=` plus the existing run endpoints.

## 6. No overlap, nothing lost (Hermes vs Prime vs jcode)
- One owner per job: memory = jcode store (Hermes' Python memory tool and MEMORY.md/USER.md are not used), skills = one dir (`~/.jcode/skills`; bundled Hermes skills are merged in once, every Hermes Python skill path uses it, and `skill_manage` can patch/edit/write_file/remove_file like Hermes' skill manager), sessions and delegation = engine, goals = Prime (`/subgoal` served by the engine), cron timing = engine, job store = Hermes.
- Hermes tools the Rust agent lacks are reached through ONE lazy tool `hermes` (list/describe/call, schemas fetched from the backend, off the base prompt): browser vault and CDP, image generation, vision, TTS, Home Assistant, kanban, X search and so on. Hidden when a native tool does the job. `clarify` is native and asks the desktop window (headless runs get "no user available"). `send_message` and `mixture_of_agents` are not agent tools in Hermes itself.
- Slash commands: the engine owns goal, subgoal, refine, harness, compact; `/learn`, `/plan`, `/init` stay Hermes prompt builders; `/curator`, `/skills`, `/memory` are hidden (their capabilities stay reachable through Hermes' REST routes).
- Hermes tool names in prompts and learned skills (`terminal`, `read_file`, `write_file`, `patch`, `search_files`, `web_extract`, `web_search`) resolve to native tools.

## 7. Loop safety and cost
Repeat guard (warn at 3 identical calls, stop at 5, same failing result counts; `bg` and polls exempt). Tool output in history capped at 50 KB (40% head / 60% tail), full text spilled to a private file (0600, purged after 7 days). Compaction uses Prime's structured summary and re-attaches the live todo list. Steady-state cost: recall 0 model calls; injection <= 700 tokens; extraction about 9k input tokens per run (every 12 turns and at session end); learning gate about 10k per 25 assistant messages, refine only if approved. No new resident memory: the only new maps are pruned on session close.

## 8. Verified
Unit and integration tests pass in every touched crate. A real `sovereign serve` on temp homes with a fake model server showed: extraction at turn 12, a paraphrase merged (old wording kept inactive), recall in a fresh process injected once, a 27-message session ran gate/refine/apply, restart re-read only new messages, sidecar off, garbage and HTTP-500 extractor replies harmless, concurrent duplicates gave one row, expire worked, the repeat guard stopped at 5, a v5 database migrated with a backup, and `hermes`/`clarify` failed cleanly with no backend or window.

## 9. Known limits
- Gateway `session.close` does not trigger extraction itself; it fires when the connection drops (30 s grace) or the session is resumed. SIGTERM defers the last window to the next open (the marker keeps it).
- An extractor HTTP 500 shows as an error span only after the provider's own retries (about 100 s); turns are unaffected.
- The 60 s periodic cooldown can skip one window; the next trigger picks it up.
- Project scope uses the existing `DefaultHasher` key (paths are not stored, so it cannot be migrated); a test fails if a toolchain changes the hash.
- Benchmark policy: extraction adds about one model call per session of >= 4 messages. For a memory-off baseline set `JCODE_MEMORY_SIDECAR_ENABLED=0`; the pinned polyglot baseline binary predates all of this.
