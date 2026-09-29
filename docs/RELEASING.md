# Releasing and updating the Akira desktop bundle

(This file is the desktop bundle release process; upstream jcode's tag/brew/AUR one was removed.)

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
(default `~/.jcode`, override `SOVEREIGN_UPDATE_DBDIR`) and removes `-wal`/`-shm`. Before restoring it quits the new app and its engine (`Resources/sovereign/sovereign`) and waits until both are gone, so nothing can checkpoint over the restored file. The used backup is then deleted: the engine writes `pre-v<N>.bak` only when none exists, so a stale one would leave a second failed attempt with nothing to roll back to. No backup newer than the start
means no migration ran and the file is left alone. Data written after the update started is lost; nothing else is touched.

Windows has no script. Manual rollback: quit Hermes, reinstall the previous NSIS build over the new one, then in
`%USERPROFILE%\.jcode` (or `%JCODE_HOME%`) delete `sovereign.db-wal` and `sovereign.db-shm` and copy the newest
`sovereign.db.pre-v*.bak` made during the failed update over `sovereign.db`. Skip the copy if there is none.

The desktop has no updater of its own. The stock Hermes git-based one (check via GitHub, `hermes update`,
the detached `posix.sh` hand-off) was removed from the app: "Check for updates" now reports "Updates are
installed by replacing the app" through the existing install-method notice, and points here. Only the Windows
bootstrap-recovery hand-off (`scripts/desktop-update/windows.ps1`) remains, for unpackaged Windows installs.

Future work: an update feed. Nothing publishes bundles yet, so there is nothing for the app to check; when one
exists, `hermes:updates:check` should read its manifest (compare `id`), and `apply` should download the bundle
and run `sovereign-update.sh`.

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
