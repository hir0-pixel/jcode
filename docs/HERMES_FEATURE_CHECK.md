# Hermes desktop feature check

CLI-driven evidence for the active desktop feature areas. A row marked **missing** is not accepted as parity; it records the remaining test or implementation work.

| Feature | How tested (debug engine, local Ollama `sovereign/bench-hermes-64k:latest`, isolated HOME/JCODE_HOME/HERMES_HOME, private `JCODE_RUNTIME_DIR`) | Result 2026-09-29 | Fix commit |
|---|---|---|---|
| Chat | `sessions.mjs`, `refine.mjs`, `accounting.mjs`, `agent-run.mjs`, `replay.mjs`, `repeat-task.mjs` | Pass: all checks; no chat-bound method forwarded to Python; proxy accounting 5/5 with gap 0. | `36f4d72fc` (scripts) |
| Sessions | `sessions.mjs` | Pass. | `36f4d72fc` |
| Cron | packaged `sovereign-packaged-cron-due.mjs` (hermes-agent repo) | Not rerun in this pass (needs the packaged app). Last result: pass. | `3c13bea` |
| Bots / messaging | `hermes-messaging-loopback.mjs` | Pass: `api_server` config saved with key redaction; loopback message answered by local Ollama. A real Telegram/Discord token needs an account, so the loopback stands in. | `36f4d72fc` |
| Kanban | `hermes-kanban-cli.mjs` | Pass: board, claim, complete, persisted read. | `36f4d72fc` |
| Skills | `hermes-skill-hub.mjs`; unit `tool::skill::tests::disabled_skill_is_not_listed_or_loadable` | Pass. Found: a skill switched off in Settings (`.disabled` marker) was already dropped from the system prompt but the `skill` tool still listed and loaded it. Fixed. | `36f4d72fc` |
| MCP | `hermes-mcp-settings.mjs`; unit `hermes_mcp_settings_are_loaded_for_engine_chats`, `mcp_list_mirrors_config_yaml_without_env_values` | Pass live: server added through Hermes's `/api/mcp/servers`, engine chat called its tool (stdio and OAuth HTTP). Owner: `HERMES_HOME/config.yaml` `mcp_servers`; under Hermes the engine also stopped merging `~/.claude.json`, `~/.claude/mcp.json` and project `.mcp.json` (not editable in Settings). Boot-probe `GET /api/mcp/servers` now lists that map (was `[]`, wrong shape, hid real servers). Startup live route check is by unit test only. | `36f4d72fc` |
| Browser controller | `hermes-browser-controller.mjs` | Pass: connect, status, disconnect, navigate and read a local page. | `36f4d72fc` |
| Terminal / shell pane | packaged `hermes-terminal-pty.mjs` (hermes-agent repo) | Not rerun in this pass. Last result: pass. | `8f0412f` |
| Voice / wake / TTS | `hermes-audio-local.mjs`, `hermes-wake-activation.mjs` | Pass: local `say` WAV and TTS lease; real openWakeWord detector recognised synthesized "Hey Hermes" PCM. No physical microphone used. | `36f4d72fc` |
| Plugins | packaged `hermes-plugin-install.mjs` (hermes-agent repo) | Not rerun in this pass. Last result: pass. | `8f0412f` |
| Profiles | `hermes-profile-settings.mjs` | Pass: `--profile` model/provider and SOUL.md reach a live chat. | `36f4d72fc` |
| Settings | `hermes-mcp-settings.mjs`, `sessions.mjs` | Pass: terminal toolset toggle, memory toggle, model/provider and reasoning reach the model request. | `36f4d72fc` |
| Activity | `activity-learning.mjs` | Pass. | `36f4d72fc` |
| Learning / star map | `learning.mjs`, `activity-learning.mjs`, `prime-parity.mjs`, `agent-loop.mjs` | Pass, except `learning.mjs` failed once in three runs: the gate approved but the learning pass logged "no durable lesson in this session" (`learn.rs`, model-dependent), then the new chat answered "I need to load the `refine` tool first". Two reruns passed. Left for the learning owner. | none |

Not run: `live.mjs` (needs an installed Hermes CLI at `~/.hermes/hermes-agent`), `prime-trap.mjs`. Forwarded-to-Python RPCs in engine-owned areas: `skills.manage` (hub install) is proven by the skill-hub script; `tools.list` / toolsets are proven by the terminal-toolset toggle in `hermes-mcp-settings.mjs`.

