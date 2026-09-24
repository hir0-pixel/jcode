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

## Step 1 — macOS arm64 (verified)

- [x] Packaged Cron opens; Python starts only then; stops after idle override
- [x] Due job still fires after idle stop (wake-before-due)
- [x] Quit + `kill -9`: no orphan sovereign/python within 5s
- [x] Ollama chat + tool approval → Run → executes (`qwen3.8:27b`)
- [x] API-key first-run; key in `…/config/jcode/openai.env` mode **600**; full key never in stdout
- [x] Unsigned `.dmg` + `.zip` on disk (`Hermes-0.17.6-mac-arm64.{dmg,zip}`)

### Chat + approval root causes fixed

1. **Ollama 4k budget:** Tool prefix alone is ~8–10k tokens. Engine now warms the model (`SOVEREIGN_OLLAMA_NUM_CTX`, default 32768) then refreshes the model catalog so `context_window()` is 32k (not the hard 4k fallback).
2. **Composer Enter:** Multi-line `keyboard.type` submitted on the first `\n`; e2e uses a single-line `fill()`.
3. **Safe vs Low:** In-cwd `echo > out.txt` is `RiskLevel::Safe` (no approval card). Truncating redirect **outside** session cwd is `Low` → approval UI. E2e writes to `$sandbox/approval-out.txt`.

**Proof:** `apps/desktop/release/sovereign-chat/` (`approval.png`, `chat-done.png`, `ok: true`, contents `sovereign-packaged-ok`).

**API-key proof:** `apps/desktop/release/sovereign-apikey/result.json` (`ok: true`, `mode: "600"`, `stdoutHasFullKey: false`).

## Step 2 — Windows x64 (verified build)

- [x] Audit: `dist:win:nsis` + `extraResources` sovereign/sovereign-python already wired
- [x] `stage-sovereign-python.mjs` extended for `win32-x64` (`runtime/python.exe`)
- [x] `before-pack.mjs` validates Windows python + `sovereign.exe`
- [x] GHA workflow `.github/workflows/sovereign-windows-nsis.yml`
- [x] Engine: link `sovereign-gateway` on Windows (`Cargo.toml`); CI green
- [x] Unsigned NSIS artifact from https://github.com/hir0-pixel/hermes-agent/actions/runs/35994371563
- [ ] Smoke-launch on a real Windows box (not done from this Mac)

Engine checkout: `hir0-pixel/jcode@feature/sovereign-observability`.

## Step 3 — Benchmark (open)

## Step 4 — Docs (open)

## Not verified yet

Live Windows smoke test, macOS x64, benchmarks vs stock Hermes, INSTALL.md. Desktop `main.ts` still has uncommitted theme/marketplace noise — only packaging e2e/scripts should be committed until those land separately.
