# Releasing and updating the Akira desktop bundle

(The root `RELEASING.md` is the upstream jcode release process; this file is the desktop bundle.)

## What ships together

One `Hermes.app` (or NSIS install) holds three parts that must match:

| Part | Where in the bundle | Built by |
| --- | --- | --- |
| Sovereign engine binary | `Resources/sovereign/sovereign` | `cargo build --release` in this repo, copied by `before-pack.mjs` |
| Python runtime + Hermes source + packages | `Resources/sovereign-python/` | `npm run stage:sovereign-python` |
| Desktop (Electron) | the app itself | `npm run dist:mac` |

`before-pack.mjs` writes `Resources/sovereign/manifest.json` (`apps/desktop/scripts/sovereign-manifest.mjs`):
build `id`, engine `{version, sha}` (asked from the binary itself with `sovereign __version`, so it can't
drift from what is packed), Hermes source `sha`, Python version. A binary that can't run on the build machine
(cross-arch) records the version from `Cargo.toml` and leaves the sha unchecked.

## Launch-time check

The engine reports `{engine, version, sha}` on the public `GET /api/status`. After the packaged desktop spawns
its engine, or considers attaching to a running backend, it compares that with the manifest
(`electron/sovereign-manifest.ts`). A mismatch (a stale engine left by another install, a half-copied update):

- attach path: the backend is not attached, the desktop spawns its own engine;
- spawn path: the normal boot-failure screen shows the message (both versions, what to do); nothing runs against the wrong Python source.

Dev runs (unpackaged) and bundles without a manifest skip the check.

## Updating

There is no auto-updater (`electron-updater` is not configured: no signing identity, and nothing here
configures one). The procedure is a swap with a health check, `apps/desktop/scripts/sovereign-update.sh`:

```sh
# 1. build: cargo build --release (engine), npm run stage:sovereign-python, npm run dist:mac
# 2. quit Hermes, then:
apps/desktop/scripts/sovereign-update.sh apps/desktop/release/mac-arm64/Hermes.app /Applications/Hermes.app
```

It refuses a bundle missing the engine, the Python runtime or the manifest; moves the installed app to
`Hermes.app.previous`, installs the new one (all three parts at once), launches it, and waits for the app to
write `<userData>/launch-ok.json` with the new manifest `id` (written once the engine answered and matched).
If that doesn't happen within 120 s (`SOVEREIGN_UPDATE_WAIT`) it quits the new app, restores
`Hermes.app.previous` and exits 1; the failed bundle is kept at `Hermes.app.failed`. Manual rollback is the
same swap by hand. The rollback also restores `sovereign.db`: the new engine migrates it (and takes
`sovereign.db.pre-v<N>.bak`) before the app can report healthy, and an older engine refuses a newer file, so the
script copies the newest `sovereign.db.pre-v*.bak` created after the update started over `<JCODE_HOME>/sovereign.db`
(default `~/.jcode`, override `SOVEREIGN_UPDATE_DBDIR`) and removes `-wal`/`-shm`. No backup newer than the start
means no migration ran and the file is left alone. Data written after the update started is lost; nothing else is touched.

Windows has no script. Manual rollback: quit Hermes, reinstall the previous NSIS build over the new one, then in
`%USERPROFILE%\.jcode` (or `%JCODE_HOME%`) delete `sovereign.db-wal` and `sovereign.db-shm` and copy the newest
`sovereign.db.pre-v*.bak` made during the failed update over `sovereign.db`. Skip the copy if there is none.

The Hermes source-checkout updater (`hermes update`, `scripts/desktop-update/`) is a different path (dev
installs that rebuild from git) and is untouched.

## User data is never touched

`~/.hermes`, `~/.jcode` and `sovereign.db` are outside the bundle and the script never opens them.
`sovereign.db` carries a schema version (`PRAGMA user_version`, `sovereign-prime/src/migrate.rs`):

- every opener (harness entries, session control) applies its idempotent schema, then the ordered migrations
  up to the current version, forward only (append a migration; never edit or reorder);
- before the first migration of a file that already has data, a consistent copy is written next to it:
  `sovereign.db.pre-v<N>.bak` (N = the version being migrated TO; kept once, never overwritten);
- a file from a NEWER engine is refused with a message (an older bundle after a rollback can't silently
  run on a newer schema). To go back: quit, copy the matching `sovereign.db.pre-v*.bak` over `sovereign.db`
  (delete `sovereign.db-wal` / `-shm`), start the old bundle. Anything written since the update is lost;
  the observability and memory tables opened by other components are additive and carry no separate version.
