# Sovereign packaging handoff

Last updated: 2026-09-24 (continuing agent)

## Objective

ONE installable app for macOS and Windows that a non-developer can install on a fresh machine with nothing else installed, then prove with numbers that it is more efficient than stock Hermes.

## Step 0 — commits (done)

### hermes-agent `feature/sovereign-observability`

- `79e891073215ef1c62b66118dc3335c3188f5948` — desktop: bundle Sovereign engine and staged Python for packaged installs
  - Staged by hunk for `electron/main.ts` (packaging only: `IS_PACKAGED` sovereign spawn + `backend.kind === 'sovereign'` args short-circuit).
  - Whole files: `package.json`, `scripts/before-pack.mjs`, `scripts/stage-sovereign-python.mjs`, `e2e/sovereign-install-launch.mjs`.
  - Left unstaged (user work / not attributed): theme/marketplace deletions and edits in `main.ts`, `preload.ts`, themes tree, UI files, etc. Do **not** commit `vscode-marketplace*` or `themes/install*` deletions.

### sovereign-engine `feature/sovereign-observability`

- `a0600e2c402777efc69a90e3d81f4846b8a6589b` — feat(gateway): honor bundled Python env and testable feature idle timeout
- `cedc67e606e0ff01342d295222c7fcce0de0dc02` — feat(gateway): serve packaged startup routes without waking Python
- `870ec1325aff2df73e193c7bfccba3cdde0135c8` — feat(sovereign): exit when the Electron parent process dies

Evidence: `cargo test -p sovereign-gateway -p sovereign-prime` → 44 passed, 1 ignored.

## Step 1 — macOS arm64 (in progress)

Existing `apps/desktop/release/mac-arm64/Hermes.app` may predate engine commits above. Rebuild `target/release/sovereign`, restage, repack, then verify:

- [ ] Packaged Cron + Python starts then stops via `SOVEREIGN_FEATURE_IDLE_MS`
- [ ] Ollama chat + tool approval → Run → executes (`qwen3.8:27b`)
- [ ] API-key first-run; key in keychain or 0600 file; never logged
- [ ] Normal quit + `kill -9` Electron: zero orphan sovereign/python within 5s
- [ ] Final unsigned `.dmg` and `.zip`

## Step 2 — Windows x64 (open)

Audit Unix assumptions; GHA `windows-latest` NSIS; verify launch/chat/orphans; attempt macOS x64.

## Step 3 — Benchmark (open)

Counting proxy in front of Ollama; 5 tasks × 3 runs; write `docs/BENCHMARK.md`.

## Step 4 — Docs (open)

`docs/INSTALL.md` for non-developers (Gatekeeper + SmartScreen).

## Not verified yet

Everything in Steps 1–4 above. Prior agent claimed fresh-home packaged launch showed Electron + one sovereign and no Python — re-verify after rebuild with new idle/watchdog code.
