# Factr-I harness change log (since the design started, base `c6804a330`)

Every capability change, why it was made, and where it is. Commit ids are short hashes on `feature/sovereign-observability`. All changes are general harness capabilities; none uses benchmark task ids, GAIA questions or answers, or benchmark-specific formats. Evidence labels: **O** = an original harness has it (Hermes/Prime/jcode), **D** = found in benchmark traces or audits, **W** = outside source (web/paper).

## One memory store, automatic memory, learning (the design)
| Change | Why | Commits |
|---|---|---|
| One write path with guarded near-duplicate merge; cross-scope dedupe; expire a wrong memory | one memory system, no repeats (O jcode, user requirement) | 80741ddf5, 75732765b, f6c8f81d2 |
| jcode's automatic extraction restored (every 12 turns, session end, pre-compaction), oldest-first windows, in-flight claim | automatic memory (O jcode pre-c80f1170d) | ec7a992b5, f1eac07d7, 038d226a8 |
| Recall/injection capped at 700 tokens, only shown memories marked injected | low tokens, no repetition | 6057688b9, e3fb165ce |
| Prime learning: counts assistant messages, compaction and before-close triggers, gate with harness context, session-local scope | faithful port (O Prime) | 0f5f43775, 04dcba631, 5bbe3adb9 |
| Prime's learned prompt/skill/subagent entries are rows in the one memory store; migrations 6 and 7 | single store | 593ba4212, 9797a3720, 39134b908, 653a47902 |
| EveStack spans for every memory/learning step; content-bearing JSONL log deleted | observability | b482916d2, d0b8da730, ec8461fea, ff8e51b9c |
| `SOVEREIGN_LEARNING_ENABLED=0` | run with learning off | 588783a86 |

## Hermes capabilities kept without overlap
| Change | Why | Commits |
|---|---|---|
| One lazy `hermes` tool reaches Hermes-only tools (browser vault, image generation, ...); native `clarify`; aliases for Hermes tool names | nothing from Hermes lost (O) | 76478cd1a, 1618f2fab, fef14a6e1, 35c008905 |
| One skills directory; `skill_manage` patch/edit/write_file/remove_file; Hermes bundled skills merged once | one owner per job (O Hermes) | 6ab76389e |
| One front door per capability; `/subgoal` from the goal store; `/learn` restored | no overlap (O) | f38658cbf, 5a88153e0 |

## Loop safety and cost
| Change | Why | Commits |
|---|---|---|
| Repeated-call guard: warn at 3, block the call at 5, end the turn at 10 | O Hermes tool_guardrails; D | 6a915ec5b, a34601bd1 |
| Tool output in history capped at 50 KB with spill file; bash cap 48 KB | O Hermes/Prime; D (traceback loss) | 06467ebe5, de7dacf9b, 9fbc3cccb |
| Old tool results pruned before compaction; Prime's structured summary; live todo list re-attached | O Hermes/Prime; W arXiv 2508.21433 | 71414242c, 9f4370c74, a68a4b089, 593619188 |
| Malformed tool-call JSON repaired; truncated (length-stop) calls discarded unless they parse unrepaired; streamed calls without an id get one | O Hermes; D audit | 5d4d3b939, a34601bd1, f0da98312 |
| Bash: process groups killed on turn cancel, session delete, cron end and SIGTERM; bounded pipe drain; hook not spawned for non-bash tools | D (F1 leak, F2 early turn end) | 0c6db05d2, f1a62f74f, f25ff54c3, ad1e2d661 |
| Turn no longer ends early while a tool runs | D (F2) | 9d1f67918 |
| Unattended runs may tidy their own working directory | O Hermes approvals semantics; D | 3ccb2beb0 |
| Bedrock: retry with backoff, prompt caching, cache usage reported, correct stop reasons, tools enabled for newer Claude ids, 32k default max tokens | D audit (would have stripped tools and broken recovery) | 1b8aa332d, 2b2ceecf4 |

## Task-completion quality
| Change | Why | Commits |
|---|---|---|
| Fuzzy `edit` fallback (unique match, indentation- and CRLF-safe) | O Hermes fuzzy_match, Prime edit-diff; D | 2e57cb37f, 2b2ceecf4 |
| One-line syntax note after edit/write (never refuses the write) | O Hermes file_operations | 2e57cb37f, 2b2ceecf4 |
| Short completion guidance; tool-use enforcement only for Hermes' model families | O Hermes prompt_builder | 2e57cb37f |
| Stop nudges: run the tests; action announced without a tool call (incl. numbered plans); question/offer endings in headless runs; failing last check; finish checklist; at most 2 per turn | O Hermes verification_stop; D traces (plan-only endings, Prime dot-dsl; question endings, broken-python) | 2e57cb37f, 0a067c8fc, 2a624f343 |
| Plain-mode auto-verify gate: the host runs the project's tests after code edits and feeds failures back (3 rounds, identical-failure stop, AVO-style stall steer, 3x timeout for cold builds) | NVIDIA AVO (validate through execution); O Prime autonomous gates, Hermes verify detection; D | f7baac850, e070e64d0, b1e877585, c12ab2e98, a6474a5f4 |
| Turn deadline reminders (70% and 90%) when a runner sets a budget; best-effort output before giving up | D (a timed-out task and a no-output give-up in Prime traces) | 0a067c8fc |
| First-message environment snapshot (headless only) | W Droid/Meta-Harness; D | c12ab2e98, d5d68cfb5 |

## Research tools
| Change | Why | Commits |
|---|---|---|
| `webfetch`: 12K window with spill file, `find`/`offset`, PDF branch, binaries saved not decoded, browser UA, retries, Wayback fallback, Wikipedia raw | D GAIA traces (fetch errors 18-21%, stub pages 32-35%); O Hermes web_extract; W smolagents text browser | b666bad68 |
| `websearch`: 5 results, 200-char snippets, 2 per host, Wikipedia fallback | D (success falls with more searches) | 05f79d574 |
| `read` opens xlsx/docx/pptx as text; search-stall nudge | D GAIA (13 xlsx files); W Magentic-One stall replanning | 8a7636ec9 |

## Removed
About 39k lines of unused jcode-era files, docs, scripts and modules (`f6deb85e0`, `5fd6867a5`, `bb8eab337`, `9e8b719b9`, `ecd7d4928`), the unwired todo gate digest (`e73e9ff6b`) and the memory JSONL log (`ec8461fea`).

## Known limits
Gateway `session.close` triggers extraction only when the connection drops (30 s grace); a grandchild process holding a pipe is reaped at shutdown only; Bedrock paths were tested offline only; the Python REPL tool is off by default and cannot fetch or parse web pages; no audio transcription.
