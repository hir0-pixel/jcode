# Hermes desktop feature check

CLI-driven evidence for the active desktop feature areas. A row marked **missing** is not accepted as parity; it records the remaining test or implementation work.

| Feature | How tested | Result | Fix commit |
|---|---|---|---|
| Chat | `node crates/sovereign-gateway/e2e/sessions.mjs`; packaged `node ../hermes-agent/apps/desktop/e2e/sovereign-packaged-chat-approval.mjs` | Pass: prompt/history and packaged approval flow. `sessions.mjs` verifies model/provider/reasoning/session prompt behavior. | pending |
| Sessions | `node crates/sovereign-gateway/e2e/sessions.mjs` | Pass: RPC session lifecycle and all eight repaired REST routes, with and without auth. | pending |
| Cron | `SOVEREIGN_CRON_AGENT=1 node ../hermes-agent/apps/desktop/e2e/sovereign-packaged-cron-due.mjs` | Existing packaged cron-due check; rerun in final gates. | pending |
| Bots / messaging | `node crates/sovereign-gateway/e2e/hermes-messaging-loopback.mjs` | Pass: isolated `api_server` config is saved/read with secret redaction, and a loopback message receives a local Ollama response. No external bot account is used. | pending |
| Kanban | `node crates/sovereign-gateway/e2e/hermes-kanban-cli.mjs` | Pass: isolated Hermes CLI created a board, claimed and completed a task, and read the persisted result. | pending |
| Skills | `node crates/sovereign-gateway/e2e/hermes-skill-hub.mjs`; `node crates/sovereign-gateway/e2e/hermes-mcp-settings.mjs` | Pass: Hermes hub installs/removes a categorized skill while the engine stays running; fresh Rust chat listing reflects both changes. | pending |
| MCP | `node crates/sovereign-gateway/e2e/hermes-mcp-settings.mjs` | Pass: Hermes API adds local stdio server and Rust chat calls its tool. OAuth-authenticated server coverage remains open. | pending |
| Browser controller | Hermes `../hermes-agent/apps/desktop` browser preview/component tests | UI unit coverage exists; missing packaged/CLI browser navigation e2e. | — |
| Terminal / shell pane | Packaged `../hermes-agent/apps/desktop/e2e/sovereign-packaged-chat-approval.mjs` exercises a shell approval; settings e2e disables Hermes `terminal` and verifies Rust chat does not call `bash`. | Chat shell path passes; missing terminal-pane/PTY e2e. | pending |
| Voice / wake / TTS | Hermes `../hermes-agent/apps/desktop` voice hooks, wake-word store, and TTS lease unit tests | Missing live packaged/CLI recording, wake-trigger, and speech-output e2e. | — |
| Plugins | Hermes `../hermes-agent/apps/desktop` plugin catalog and agent-plugin store tests | UI unit coverage exists; missing temp-home plugin install/enable/capability e2e. | — |
| Profiles | `node crates/sovereign-gateway/e2e/hermes-profile-settings.mjs` | Pass: `--profile research` selected profile model/provider and `SOUL.md` prompt in a live Rust chat using local Ollama. | pending |
| Settings | `node crates/sovereign-gateway/e2e/hermes-mcp-settings.mjs`; `node crates/sovereign-gateway/e2e/sessions.mjs` | Pass: MCP, hub install, toolset disable, memory enabled flag, model/provider, reasoning, and system prompt reach engine session/chat. OAuth is not covered. | pending |
| Activity | `node crates/sovereign-gateway/e2e/activity-learning.mjs` | Pass: isolated Activity API lists a completed chat run, its detail, chat span, and approvals. | pending |
| Learning / star map | `node crates/sovereign-gateway/e2e/learning.mjs`; `node crates/sovereign-gateway/e2e/activity-learning.mjs` | Pass: learning is applied in a later chat; star-map graph/node APIs list, edit, and delete an isolated learning node. | pending |

The feature walkthrough is incomplete until each **missing** row has a passing CLI-driven check and any failures are fixed.
