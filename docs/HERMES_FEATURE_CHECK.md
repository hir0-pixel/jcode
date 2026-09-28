# Hermes desktop feature check

CLI-driven evidence for the active desktop feature areas. A row marked **missing** is not accepted as parity; it records the remaining test or implementation work.

| Feature | How tested | Result | Fix commit |
|---|---|---|---|
| Chat | `node crates/sovereign-gateway/e2e/sessions.mjs`; packaged `node ../hermes-agent/apps/desktop/e2e/sovereign-packaged-chat-approval.mjs` | Pass: prompt/history and packaged approval flow. `sessions.mjs` verifies model/provider/reasoning/session prompt behavior. | `003970f` |
| Sessions | `node crates/sovereign-gateway/e2e/sessions.mjs` | Pass: RPC session lifecycle and all eight repaired REST routes, with and without auth. | `003970f` |
| Cron | `SOVEREIGN_CRON_AGENT=1 node ../hermes-agent/apps/desktop/e2e/sovereign-packaged-cron-due.mjs` | Pass: packaged cron-due and idle-stop test passed three consecutive times. | `3c13bea` |
| Bots / messaging | `node crates/sovereign-gateway/e2e/hermes-messaging-loopback.mjs` | Pass: isolated `api_server` config is saved/read with secret redaction, and a loopback message receives a local Ollama response. No external bot account is used. | `6014a73` |
| Kanban | `node crates/sovereign-gateway/e2e/hermes-kanban-cli.mjs` | Pass: isolated Hermes CLI created a board, claimed and completed a task, and read the persisted result. | `f4569da` |
| Skills | `node crates/sovereign-gateway/e2e/hermes-skill-hub.mjs`; `node crates/sovereign-gateway/e2e/hermes-mcp-settings.mjs` | Pass: Hermes hub installs/removes a categorized skill while the engine stays running; fresh Rust chat listing reflects both changes. | `6014a73` |
| MCP | `node crates/sovereign-gateway/e2e/hermes-mcp-settings.mjs` | Pass: Hermes API adds local stdio and OAuth-authenticated HTTP servers; Rust chat calls both tools using the cached Hermes bearer token. | pending commit |
| Browser controller | `node crates/sovereign-gateway/e2e/hermes-browser-controller.mjs` | Pass: authenticated `browser.manage` connects, reads status and disconnects local Chromium; forwarded `/api/browser/act` navigates to a local page and its full snapshot contains a random page marker. | `85c5503f` |
| Terminal / shell pane | `node ../hermes-agent/apps/desktop/e2e/hermes-terminal-pty.mjs` | Pass: packaged Electron spawned zsh through the actual PTY IPC, accepted a command, streamed its marker, and disposed the terminal in a temporary home. | `8f0412f` |
| Voice / wake / TTS | `node crates/sovereign-gateway/e2e/hermes-audio-local.mjs`; `node crates/sovereign-gateway/e2e/hermes-wake-activation.mjs` | Pass: local `say` produced WAV and the TTS lease acquired/released. The real openWakeWord/TFLite detector recognized generated “Hey Hermes” PCM and emitted `wake.detected` to its owning client. This verifies client-capture without physical-microphone hardware. | `85c5503f` |
| Plugins | `node ../hermes-agent/apps/desktop/e2e/hermes-plugin-install.mjs` | Pass: packaged Electron installed a local Git plugin into the isolated Hermes home and rendered its registered status-bar capability. | `8f0412f` |
| Profiles | `node crates/sovereign-gateway/e2e/hermes-profile-settings.mjs` | Pass: `--profile research` selected profile model/provider and `SOUL.md` prompt in a live Rust chat using local Ollama. | `6014a73` |
| Settings | `node crates/sovereign-gateway/e2e/hermes-mcp-settings.mjs`; `node crates/sovereign-gateway/e2e/sessions.mjs` | Pass: live chat request proves stdio/OAuth MCP calls, hub install, tool disablement, memory context injection off/on, model/provider routing (selected profile uses capture proxy; default goes directly to local Ollama), reasoning effort, and system prompt behavior. | this commit |
| Activity | `node crates/sovereign-gateway/e2e/activity-learning.mjs` | Pass: isolated Activity API lists a completed chat run, its detail, chat span, and approvals. | `f4569da` |
| Learning / star map | `node crates/sovereign-gateway/e2e/learning.mjs`; `node crates/sovereign-gateway/e2e/activity-learning.mjs` | Pass: learning is applied in a later chat; star-map graph/node APIs list, edit, and delete an isolated learning node. | `f4569da` |

All listed desktop feature areas have a CLI-driven passing check. Wake is exercised through Hermes's client-capture feed using locally generated audio; a physical microphone is not available on this host.
