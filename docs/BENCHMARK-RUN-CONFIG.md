# Running Factr-I in a benchmark: exact settings

Pin the binary (copy `target/release/sovereign` to a versioned name) and record its sha256 and the commit in the run's `versions.txt`. Use a FRESH `JCODE_HOME` and `HERMES_HOME` per task or run; never reuse a home across engine versions (schema v7 is not readable by older builds).

## Headline run (same conditions as Prime and stock Hermes)
Memory and learning OFF so each task stands alone; headless behaviours ON:
| Setting | Value | Why |
|---|---|---|
| `JCODE_MEMORY_SIDECAR_ENABLED` | `0` | no automatic memory extraction |
| `SOVEREIGN_LEARNING_ENABLED` | `0` | no learning reviews |
| `JCODE_HEADLESS` | `1` (or run through `/api/agent/run`) | headless nudges and environment snapshot |
| `JCODE_TURN_DEADLINE_S` | the task's time limit in seconds | deadline reminders at 70% and 90% (a runner-supplied budget, not a hack) |
| approvals | `approvals.mode: off` in the private `HERMES_HOME` config, or rely on the working-directory rule | sandboxed commands are not refused |
| `JCODE_AUTO_VERIFY` | default on; set `0` for the gate-off number | host-internal verify gate |
| `JCODE_VERIFY_ON_STOP` | default on; `0` disables the stop nudges | |
| `JCODE_ENV_SNAPSHOT` | default (headless only); `0` disables | |
| `JCODE_BEDROCK_MAX_TOKENS` | optional (default min(model max, 32k)) | Bedrock only |

## Separate labelled runs
- Gate off: `JCODE_AUTO_VERIFY=0`. Gate on: default. Report gate-off, gate-on and after-retry separately, with extra tokens and time per round. The gate internalizes a retry that the stock harnesses do not have.
- Memory/learning benefit: a clearly labelled run with the two memory/learning switches left at their defaults and a persistent home across tasks.

## Integrity
Only general capabilities were added (see `docs/CHANGELOG-FACTR-I.md`). No benchmark task ids, GAIA answers or per-task formats appear in the code. Edits to test files are flagged in the `loop.guard` spans (`tests_touched`), so those passes can be excluded. The format-compliance nudge reads an explicitly stated output label from the task text only.
