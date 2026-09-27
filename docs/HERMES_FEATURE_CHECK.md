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
| MCP | `node crates/sovereign-gateway/e2e/hermes-mcp-settings.mjs` | Pass: Hermes API adds local stdio server and Rust chat calls its tool. OAuth-authenticated server coverage remains open. | `f4569da` |
| Browser controller | `node ../hermes-agent/apps/desktop/e2e/...` with a local CDP target | Open: a packaged navigation e2e was not run; no Chrome, Chromium, Brave, or Edge executable exists at the supported macOS application paths, and no live CDP endpoint is available. | — |
| Terminal / shell pane | `node ../hermes-agent/apps/desktop/e2e/hermes-terminal-pty.mjs` | Pass: packaged Electron spawned zsh through the actual PTY IPC, accepted a command, streamed its marker, and disposed the terminal in a temporary home. | `8f0412f` |
| Voice / wake / TTS | Hermes `../hermes-agent/apps/desktop` voice hooks, wake-word store, and TTS lease unit tests | Open: unit coverage exists; no packaged live microphone, wake trigger, or speech-output check was run. A real microphone/audio endpoint was not available for this CLI run; paid speech APIs are outside the local-Ollama-only test constraint. | — |
| Plugins | `node ../hermes-agent/apps/desktop/e2e/hermes-plugin-install.mjs` | Pass: packaged Electron installed a local Git plugin into the isolated Hermes home and rendered its registered status-bar capability. | `8f0412f` |
| Profiles | `node crates/sovereign-gateway/e2e/hermes-profile-settings.mjs` | Pass: `--profile research` selected profile model/provider and `SOUL.md` prompt in a live Rust chat using local Ollama. | `6014a73` |
| Settings | `node crates/sovereign-gateway/e2e/hermes-mcp-settings.mjs`; `node crates/sovereign-gateway/e2e/sessions.mjs` | Pass: MCP, hub install, toolset disable, memory enabled flag, model/provider, reasoning, and system prompt reach engine session/chat. OAuth is not covered. | `6014a73` |
| Activity | `node crates/sovereign-gateway/e2e/activity-learning.mjs` | Pass: isolated Activity API lists a completed chat run, its detail, chat span, and approvals. | `f4569da` |
| Learning / star map | `node crates/sovereign-gateway/e2e/learning.mjs`; `node crates/sovereign-gateway/e2e/activity-learning.mjs` | Pass: learning is applied in a later chat; star-map graph/node APIs list, edit, and delete an isolated learning node. | `f4569da` |

The walkthrough remains incomplete until each **open** row has a passing CLI-driven check and any product gaps are fixed.
