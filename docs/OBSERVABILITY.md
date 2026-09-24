# Local observability

## Decision

Sovereign keeps its run ledger in `observability.sqlite3` under `JCODE_HOME`
(`~/.sovereign` in the desktop build). The engine and gateway emit facts at the
point where work is accepted or completed. A single bounded channel feeds a
dedicated SQLite writer; the chat path never waits for a disk write. The view
reads this database through authenticated Rust gateway routes. No trace or
content is exported by default.

This is an *observability ledger*, not a durable-execution engine. On startup,
unfinished work is marked `interrupted`; it is never replayed. Replaying a tool
after a crash could repeat a shell command or another side effect.

## Records

Tier 1 is always enabled. `runs` has one row per accepted turn or subagent:
`id`, `session_id`, `parent_id`, `root_id`, `kind`, `model`, `provider`,
`status`, `started_at_ms`, `ended_at_ms`, `input_tokens`, `output_tokens`,
`cache_read_tokens`, `cache_write_tokens`, `cost_usd`, and `error`. `spans`
has one row per model call or tool invocation: `id`, `run_id`, `parent_id`,
`kind`, `name`, `status`, `started_at_ms`, `ended_at_ms`, token counts, and
`error`. The `kind` vocabulary is OpenTelemetry GenAI's `invoke_agent`,
`chat`, and `execute_tool`. IDs come from session/turn sequence and harness
tool call IDs. A root turn owns its child runs and spans.

Tier 2 adds `content(id, input, output)` for prompt, tool arguments,
and result text. It is disabled by default and can be enabled in local
`observability.json` with `{"capture_content":true}`. Content is size capped
before enqueue; the queue discards tier 2 first when under pressure. Tier 1
does not depend on content rows. The UI makes missing content explicit.

Cost is calculated from a bundled, reviewed USD-per-million-token table, with
separate input, output, cache read, and cache write prices. Ollama has an
explicit zero *API* price. An unknown model has `NULL` cost and appears as
`unpriced`, never `$0`. Prices are versioned with the binary, so historic cost
is the estimate recorded at the time of each call; a later catalog change
does not silently rewrite history.

For local Ollama runs, the desktop separately shows a Grok 4.7 xHigh
equivalent estimate using the user-provided rates of $2 per million input
tokens and $6 per million output tokens. Cache-read tokens are already part of
the input count and are not added twice. This comparison is not an API charge.

## Writer and retention

SQLite uses WAL, `synchronous=FULL`, one writer, 4 KiB pages, and transactions of at most 128
events or 100 ms. `busy_timeout` is confined to the writer. The chat path does
one bounded `try_send`; no `await`, SQLite call, network call, or model call is
added. A separate read connection serves UI queries concurrently with WAL
writes. A measured 1,000-turn run with one model call per turn
used 2,793,176 bytes including indexes and SQLite pages; enqueue cost
was 0.9 microseconds per event in the release benchmark. A 1,024-event
queue is bounded well under the 5 MB idle RAM budget. The writer reports a
dropped tier-1 counter if even the reserved queue
fills, so a disk failure cannot be mistaken for complete history. We do not
promise impossible lossless capture under an indefinitely stalled disk while
also bounding memory and refusing to block the agent.

Tier-2 content is removed after 7 days. Detailed tool and model spans are
removed after 30 days, while per-run tier-1 summaries remain indefinitely.
Pruning occurs in the writer on startup and daily, never on the chat path.
On startup, queued and running runs become `interrupted`. Spawned children are
first reconciled from their persisted session transcripts, then any unfinished
children become `interrupted`. The writer refreshes child transcripts while
running. SQLite auto-checkpoints WAL; the writer also checkpoints on clean
shutdown. A hard kill can lose events still in the queue, so the database is
authoritative only for committed records.

## Source and tradeoffs

Evestack's published [repository](https://github.com/SammyTourani/evestack)
describes sessions with turn/subagent trees, cached tokens, priced turns,
traces, failures and cancellation. Its hosted observability and dashboard docs
were unavailable during this design pass; the repository description is the
Evestack source used here. The [OpenTelemetry GenAI conventions](https://opentelemetry.io/docs/specs/semconv/registry/attributes/gen-ai/)
provide the operation names and optional content attributes. [Restate's
architecture](https://www.restate.dev/blog/building-a-modern-durable-execution-engine-from-first-principles)
uses a log to separate event capture from materialized state; [Morling's SQLite
example](https://www.morling.dev/blog/building-durable-execution-engine-with-sqlite/)
shows why an embedded, single-writer store fits a self-contained agent, and
why side-effect replay is a separate, harder contract. Jcode's
`jcode-harness-api` already emits tool, token-usage and turn-boundary events;
the gateway consumes those rather than reconstructing work from the UI.

SQLite gives local durability and indexed timeline queries without a server,
but it has one writer. Batching and WAL absorb this workload; a genuinely
multi-writer or remote deployment would need a different store. OTLP export is
deferred: adding network egress by default would violate Sovereign's local
privacy rule. If offered later, it must be an explicit opt-in.

The system-design diagnostic scores this local design 6/10 (five of eight
checks). Requirements, measured storage and latency, a single-writer scaling
ceiling, queued writes, and failure counters are covered. Replicas, a read
cache, and a rolling deployment plan are absent because this is one local
desktop process with a small, indexed read view. A remote multi-user version
would need replicated storage, measured read caching, and a staged rollout.
