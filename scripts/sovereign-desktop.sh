#!/usr/bin/env bash
# Run the Hermes desktop on the sovereign engine.
#
# Starts the engine (jcode + Prime REPL + local memory) with a fresh private
# token, then launches the unmodified Hermes desktop app in remote-gateway mode
# pointed at it. Quitting the app stops the engine.
#
#   scripts/sovereign-desktop.sh
#
# Env:
#   SOVEREIGN_PROVIDER  model provider (default: ollama). After
#                       `sovereign login openai` (ChatGPT/Codex) use `openai`.
#   SOVEREIGN_MODEL     model id (default: qwen3.8:27b for ollama)
#   SOVEREIGN_HOME      engine state dir (default: ~/.sovereign)
#   HERMES_APP          path to Hermes.app
#   SOVEREIGN_BIN       engine binary (default: target/release/sovereign)
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
bin="${SOVEREIGN_BIN:-$here/target/release/sovereign}"
app="${HERMES_APP:-$here/../hermes-agent/apps/desktop/release/mac-arm64/Hermes.app}"
provider="${SOVEREIGN_PROVIDER:-ollama}"
model="${SOVEREIGN_MODEL:-}"
if [[ -z "$model" && "$provider" == "ollama" ]]; then model="qwen3.8:27b"; fi
export JCODE_HOME="${SOVEREIGN_HOME:-$HOME/.sovereign}"

[[ -x "$bin" ]] || { echo "engine binary not found: $bin (run: cargo build --release --bin sovereign)" >&2; exit 1; }
[[ -d "$app" ]] || { echo "Hermes.app not found: $app (set HERMES_APP)" >&2; exit 1; }
mkdir -p "$JCODE_HOME"
chmod 700 "$JCODE_HOME"

token="$(openssl rand -hex 32)"
log="$JCODE_HOME/engine.log"
out="$(mktemp -t sovereign-ready)"
trap 'kill "$engine" 2>/dev/null || true; rm -f "$out"' EXIT

args=(--provider "$provider")
[[ -n "$model" ]] && args+=(--model "$model")
HERMES_DASHBOARD_SESSION_TOKEN="$token" "$bin" "${args[@]}" serve --host 127.0.0.1 --port 0 >"$out" 2>>"$log" &
engine=$!

port=""
for _ in $(seq 1 600); do
  port="$(sed -nE 's/^HERMES_BACKEND_READY port=([0-9]+).*/\1/p' "$out" | head -1)"
  [[ -n "$port" ]] && break
  kill -0 "$engine" 2>/dev/null || { echo "engine exited; see $log" >&2; exit 1; }
  sleep 0.1
done
[[ -n "$port" ]] || { echo "engine did not become ready; see $log" >&2; exit 1; }
echo "sovereign engine ready on 127.0.0.1:$port (provider: $provider${model:+, model: $model}); log: $log"

HERMES_DESKTOP_REMOTE_URL="http://127.0.0.1:$port" \
HERMES_DESKTOP_REMOTE_TOKEN="$token" \
  "$app/Contents/MacOS/Hermes"
