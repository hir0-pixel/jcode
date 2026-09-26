# Finish report

## Part 1: cron Python leak

**Status: blocked; no leak fix claimed.** The packaged trace was enabled with `SOVEREIGN_TRACE_FEATURE_ACTIVITY=1` and recorded forwarded feature routes, cron lease acquire/release, every feature activity touch, and idle-stop decisions. For the observed run, the cron lease released when `jobs.json` changed to `state=completed` and `next_run_at=null`; the gateway then made its idle-stop decision. The run did not produce the expected response marker: the cron record ended with `last_status=error` and `last_error="Agent completed but produced empty response (model error, timeout, or misconfiguration)"`. The e2e now reports that completed error immediately instead of polling until its ten-minute deadline.

The local Ollama `/v1/chat/completions` endpoint answered the same prompt with `sovereign-cron-agent-ok` when checked outside the sandbox. This does not establish why the packaged engine's `/api/agent/run` returned an empty response. No root cause for the originally reported post-completion Python lifetime is proven by this run, so no timeout or lease behavior was changed. The requested three consecutive passing `SOVEREIGN_CRON_AGENT=1` runs remain unverified. Continue with Part 2 and revisit Part 1 if later diagnostics identify a product fault.

Validation so far: `cargo check -p sovereign-gateway --tests` passed (two existing dead-code warnings); `cargo build --release --bin sovereign` passed. `node --check e2e/sovereign-packaged-cron-due.mjs` passed. `cargo fmt --all -- --check` fails on repository-wide pre-existing formatting differences, including untouched files; no workspace-wide formatting was applied.
