# Sovereign packaging handoff

Last updated: 2026-09-24

## Objective

ONE installable app for macOS and Windows that a non-developer can install on a fresh machine with nothing else installed, then prove with numbers that it is more efficient than stock Hermes.

## Step 0 — commits (done)

### hermes-agent `feature/sovereign-observability`

- `79e891073215ef1c62b66118dc3335c3188f5948` — desktop: bundle Sovereign engine and staged Python for packaged installs
- `d3532d02c9795f15b5b63132d71a8e160545bb6a` — desktop e2e: packaged cron idle, due-wake, and orphan cleanup checks

Left unstaged (user theme/marketplace work): do **not** commit `vscode-marketplace*` or `themes/install*` deletions.

### sovereign-engine `feature/sovereign-observability`

- `a0600e2c402777efc69a90e3d81f4846b8a6589b` — bundled Python env + `SOVEREIGN_FEATURE_IDLE_MS`
- `cedc67e606e0ff01342d295222c7fcce0de0dc02` — packaged startup routes without waking Python
- `870ec1325aff2df73e193c7bfccba3cdde0135c8` — parent-death watchdog
- `b24ccc9409033dae423133c226a214760c7136f7` — wake Python before due cron jobs (`cron_wake.rs` + `HERMES_DESKTOP=1`)

## Cron scheduler finding + fix (verified)

**Where it runs:** On Hermes Desktop, the cron scheduler is **not** a separate `hermes gateway` process. It ticks **inside the desktop-owned `hermes serve` / dashboard backend** when `HERMES_DESKTOP=1` **and** `HERMES_DASHBOARD_SESSION_TOKEN` is set (`hermes_cli/web_server.py` → `_start_desktop_cron_ticker`). The messaging gateway also has a ticker when that process is running; stock Desktop relies on the in-process ticker because no gateway is started.

**Bug:** Sovereign idle-stopped that Python process after Cron UI closed, which also killed the ticker — scheduled jobs would never fire.

**Fix:** Engine reads `$HERMES_HOME/cron/jobs.json` (and profile stores), starts the feature backend `SOVEREIGN_CRON_WAKE_LEAD_MS` (default 45s) before `next_run_at`, sets `HERMES_DESKTOP=1` so the ticker runs, holds until the job’s `next_run_at` advances (or hold cap), then idle-stop reclaims Python.

**Packaged proof** (`e2e/sovereign-packaged-cron-due.mjs`, output `apps/desktop/release/sovereign-cron-due/log.json`):

| Event | Time (UTC) | Python |
| --- | --- | --- |
| Cron closed / idle | 10:27:57 | stopped |
| Engine wake (~28s before due) | 10:29:23 | started |
| Job fired (`last_status=ok`, marker written) | 10:30:23 | running |
| After completion / idle | 10:30:27 | stopped again |

Marker: `fired-at=2026-09-24T10:30:23Z`. Job record: `state=completed`, `repeat.completed=1`.

Also verified earlier: Cron open starts Python; close + idle (`SOVEREIGN_FEATURE_IDLE_MS=1500`) stops it (`e2e/sovereign-install-launch.mjs`). Orphan cleanup quit + `kill -9` clean within 5s (`e2e/sovereign-packaged-orphan-cleanup.mjs`).

## Step 1 — macOS arm64 (partial)

- [x] Packaged Cron opens; Python starts only then; stops after idle override
- [x] Due job still fires after idle stop (wake-before-due)
- [x] Quit + `kill -9`: no orphan sovereign/python within 5s
- [ ] Ollama chat + tool approval → Run → executes (`qwen3.8:27b`) — script written, not yet green
- [ ] API-key first-run; key in keychain or 0600 file; never logged
- [ ] Final unsigned `.dmg` and `.zip`

## Step 2 — Windows x64 (open)

## Step 3 — Benchmark (open)

## Step 4 — Docs (open)

## Not verified yet

Chat+approval e2e, API-key first-run storage, final DMG/ZIP, Windows, macOS x64, benchmarks, INSTALL.md.
