#!/usr/bin/env node
// Proves a server added through Hermes's local dashboard API becomes callable
// from a new Rust-engine chat. Requires Hermes's checkout/.venv and local Ollama.
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import http from 'node:http'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const engineBin = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const hermesRoot = process.env.HERMES_REPO || path.resolve('../hermes-agent')
const python = process.env.HERMES_PYTHON || path.join(hermesRoot, '.venv/bin/python')
const model = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-hermes-mcp-'))
const home = path.join(root, 'home')
const hermesHome = path.join(root, '.hermes')
const jcodeHome = path.join(root, '.jcode')
fs.mkdirSync(home, { recursive: true })
fs.mkdirSync(hermesHome, { recursive: true })
fs.mkdirSync(jcodeHome, { recursive: true })
const oauthAccessToken = crypto.randomBytes(24).toString('hex')
fs.mkdirSync(path.join(hermesHome, 'mcp-tokens'), { recursive: true })
fs.writeFileSync(path.join(hermesHome, 'mcp-tokens/settings-oauth.json'), JSON.stringify({
  access_token: oauthAccessToken,
  expires_at: Date.now() / 1000 + 3600,
}), { mode: 0o600 })
const token = crypto.randomBytes(24).toString('hex')
fs.writeFileSync(
  path.join(jcodeHome, 'config.toml'),
  `[provider]\ndefault_provider = "local"\n\n[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:11434/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${model}"\nsupports_reasoning_effort = true\n\n[[providers.local.models]]\nid = "${model}"\ncontext_window = 65536\n`,
)
const env = {
  ...process.env,
  HOME: home,
  HERMES_HOME: hermesHome,
  JCODE_HOME: jcodeHome,
  HERMES_DASHBOARD_SESSION_TOKEN: token,
  // Hermes's skills hub uses this signal to put installs in the shared engine store.
  SOVEREIGN_ENGINE_URL: 'http://127.0.0.1:1',
  PYTHONDONTWRITEBYTECODE: '1',
}
delete env.SOVEREIGN_PROVIDER
delete env.SOVEREIGN_MODEL
const children = []
const oauthCalls = []
const oauthMcp = http.createServer(async (request, response) => {
  const chunks = []
  for await (const chunk of request) chunks.push(chunk)
  const body = Buffer.concat(chunks).toString('utf8')
  if (request.headers.authorization !== `Bearer ${oauthAccessToken}`) {
    response.writeHead(401).end('OAuth bearer token missing')
    return
  }
  let message
  try { message = JSON.parse(body) } catch {
    response.writeHead(400).end()
    return
  }
  oauthCalls.push(message.method)
  if (message.id === undefined) {
    response.writeHead(202, { 'mcp-session-id': 'hermes-oauth-session' }).end()
    return
  }
  let result = {}
  if (message.method === 'initialize') result = {
    protocolVersion: '2024-11-05', capabilities: { tools: {} },
    serverInfo: { name: 'hermes-oauth-settings-test', version: '1' },
  }
  if (message.method === 'tools/list') result = { tools: [{
    name: 'oauth_probe', description: 'Return the exact supplied text over OAuth.',
    inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] },
  }] }
  if (message.method === 'tools/call') result = {
    content: [{ type: 'text', text: 'HERMES_MCP_OAUTH_OK:' + message.params.arguments.text }],
    isError: false,
  }
  const encoded = JSON.stringify({ jsonrpc: '2.0', id: message.id, result })
  response.writeHead(200, {
    'content-type': 'application/json', 'mcp-session-id': 'hermes-oauth-session',
    'content-length': Buffer.byteLength(encoded),
  }).end(encoded)
})
const stop = child => {
  if (child && child.exitCode === null) child.kill('SIGTERM')
}
const start = (command, args, cwd, childEnv) => {
  const child = spawn(command, args, { cwd, env: childEnv, stdio: ['ignore', 'pipe', 'pipe'] })
  children.push(child)
  let output = ''
  child.stdout.on('data', chunk => { output += chunk })
  child.stderr.on('data', chunk => { output += chunk })
  return { child, output: () => output }
}
const waitPort = async (proc, marker) => {
  const until = Date.now() + 120_000
  while (Date.now() < until) {
    const match = proc.output().match(new RegExp(`${marker} port=(\\d+)`))
    if (match) return Number(match[1])
    if (proc.child.exitCode !== null) throw new Error(`${marker} process exited: ${proc.output()}`)
    await new Promise(resolve => setTimeout(resolve, 100))
  }
  throw new Error(`timed out waiting for ${marker}: ${proc.output()}`)
}

let ws
try {
  await new Promise((resolve, reject) => {
    oauthMcp.once('error', reject)
    oauthMcp.listen(0, '127.0.0.1', resolve)
  })
  const oauthMcpUrl = `http://127.0.0.1:${oauthMcp.address().port}/mcp`
  const fakeMcp = path.join(root, 'mcp-server.mjs')
  fs.writeFileSync(fakeMcp, `
    import readline from 'node:readline'
    const rl = readline.createInterface({ input: process.stdin })
    for await (const line of rl) {
      let request
      try { request = JSON.parse(line) } catch { continue }
      if (request.id === undefined) continue
      let result = {}
      if (request.method === 'initialize') result = {
        protocolVersion: '2024-11-05', capabilities: { tools: {} },
        serverInfo: { name: 'hermes-settings-test', version: '1' }
      }
      if (request.method === 'tools/list') result = { tools: [{
        name: 'prove_settings', description: 'Return the exact supplied text.',
        inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] }
      }] }
      if (request.method === 'tools/call') result = {
        content: [{ type: 'text', text: 'HERMES_MCP_SETTINGS_OK:' + request.params.arguments.text }],
        isError: false
      }
      process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }) + '\\n')
    }
  `)

  const hermes = start(python, ['-m', 'hermes_cli.main', 'serve', '--host', '127.0.0.1', '--port', '0', '--skip-build'], hermesRoot, env)
  const hermesPort = await waitPort(hermes, 'HERMES_BACKEND_READY')
  const base = `http://127.0.0.1:${hermesPort}`
  const added = await fetch(`${base}/api/mcp/servers`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ name: 'settings-test', command: process.execPath, args: [fakeMcp] }),
  })
  if (!added.ok) throw new Error(`Hermes MCP add API returned ${added.status}: ${await added.text()}`)
  const persisted = await fetch(`${base}/api/mcp/servers`, {
    headers: { 'X-Hermes-Session-Token': token },
  }).then(response => response.json())
  if (!persisted.servers?.some(server => server.name === 'settings-test')) {
    throw new Error(`Hermes API did not list the saved server: ${JSON.stringify(persisted)}`)
  }
  const oauthAdded = await fetch(`${base}/api/mcp/servers`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ name: 'settings-oauth', url: oauthMcpUrl, auth: 'oauth' }),
  })
  if (!oauthAdded.ok) throw new Error(`Hermes OAuth MCP add API returned ${oauthAdded.status}: ${await oauthAdded.text()}`)

  const skillName = 'agent-merge-conflict-arbiter'
  const skillInstall = await fetch(`${base}/api/skills/hub/install`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ identifier: 'official/autonomous-ai-agents/agent-merge-conflict-arbiter' }),
  })
  if (!skillInstall.ok) throw new Error(`Hermes skill hub install returned ${skillInstall.status}: ${await skillInstall.text()}`)
  const skillPath = path.join(jcodeHome, 'skills', 'autonomous-ai-agents', skillName, 'SKILL.md')
  const skillDeadline = Date.now() + 30_000
  while (Date.now() < skillDeadline && !fs.existsSync(skillPath)) await new Promise(resolve => setTimeout(resolve, 100))
  if (!fs.existsSync(skillPath)) throw new Error(`Hermes skills hub did not install into the shared engine store: ${skillPath}`)

  const terminalToggle = await fetch(`${base}/api/tools/toolsets/terminal`, {
    method: 'PUT',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ enabled: false }),
  })
  if (!terminalToggle.ok) throw new Error(`Hermes terminal toolset toggle returned ${terminalToggle.status}: ${await terminalToggle.text()}`)
  const toolsets = await fetch(`${base}/api/tools/toolsets`, {
    headers: { 'X-Hermes-Session-Token': token },
  }).then(response => response.json())
  if (toolsets.find(toolset => toolset.name === 'terminal')?.enabled !== false) {
    throw new Error(`Hermes API did not persist the terminal toolset toggle: ${JSON.stringify(toolsets)}`)
  }
  const memorySetting = await fetch(`${base}/api/config`, {
    method: 'PUT',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ config: {
      memory: { memory_enabled: false },
      agent: { reasoning_effort: 'low' },
      providers: { 'openai-compatible': {
        base_url: 'http://127.0.0.1:11434/v1', api_key: 'ollama', model,
        context_length: 65536, models: [{ id: model, context_length: 65536 }],
      } },
    } }),
  })
  if (!memorySetting.ok) throw new Error(`Hermes memory setting returned ${memorySetting.status}: ${await memorySetting.text()}`)
  const savedMemorySetting = await fetch(`${base}/api/config?include_defaults=false`, {
    headers: { 'X-Hermes-Session-Token': token },
  }).then(response => response.json())
  if (savedMemorySetting.memory?.memory_enabled !== false) {
    throw new Error(`Hermes API did not persist the memory setting: ${JSON.stringify(savedMemorySetting.memory)}`)
  }
  const modelSetting = await fetch(`${base}/api/model/set`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ scope: 'main', provider: 'openai-compatible', model, confirm_expensive_model: true }),
  })
  if (!modelSetting.ok) throw new Error(`Hermes model setting returned ${modelSetting.status}: ${await modelSetting.text()}`)
  const soulSetting = await fetch(`${base}/api/profiles/default/soul`, {
    method: 'PUT',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ content: 'When a user asks for the settings marker, include SYSTEM_PROMPT_APPLIED in your response.' }),
  })
  if (!soulSetting.ok) throw new Error(`Hermes profile prompt setting returned ${soulSetting.status}: ${await soulSetting.text()}`)

  const engine = start(engineBin, ['serve', '--host', '127.0.0.1', '--port', '0'], home, env)
  const port = await waitPort(engine, 'HERMES_BACKEND_READY')
  const engineSkills = await fetch(`http://127.0.0.1:${port}/api/skills`, {
    headers: { Authorization: `Bearer ${token}` },
  }).then(response => response.json())
  if (!engineSkills.some(skill => skill.name === skillName)) {
    throw new Error(`installed hub skill is absent from the engine skill list: ${JSON.stringify(engineSkills)}`)
  }
  ws = new WebSocket(`ws://127.0.0.1:${port}/api/ws?token=${token}`)
  const events = []
  const pending = new Map()
  let next = 0
  ws.addEventListener('message', message => {
    const frame = JSON.parse(String(message.data))
    if (frame.method === 'event') events.push(frame.params)
    else if (frame.id !== undefined && pending.has(frame.id)) {
      pending.get(frame.id)(frame)
      pending.delete(frame.id)
    } else if (frame.method === 'approval') {
      ws.send(JSON.stringify({ jsonrpc: '2.0', id: frame.id, result: { choice: 'once' } }))
    }
  })
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true })
    ws.addEventListener('error', reject, { once: true })
  })
  const rpc = (method, params = {}) => {
    const id = `m${++next}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise(resolve => pending.set(id, resolve))
  }
  const created = await rpc('session.create', { cwd: home })
  const sid = created.result?.session_id
  if (!sid) throw new Error(`session.create failed: ${JSON.stringify(created)}`)
  if (created.result.info?.model !== model || created.result.info?.provider !== 'openai-compatible' ||
    created.result.info?.reasoning_effort !== 'low' || created.result.info?.memory_enabled !== false) {
    throw new Error(`Hermes model/provider/reasoning/memory settings did not reach the engine session: ${JSON.stringify(created.result.info)}`)
  }
  const changedModel = await rpc('config.set', { session_id: sid, key: 'model', value: `${model} --provider local` })
  if (changedModel.error || changedModel.result?.value !== `${model} --provider local`) {
    throw new Error(`model/provider setting failed: ${JSON.stringify(changedModel)}`)
  }
  const changedEffort = await rpc('config.set', { session_id: sid, key: 'reasoning', value: 'high' })
  if (changedEffort.error || changedEffort.result?.value !== 'high') {
    throw new Error(`reasoning setting failed: ${JSON.stringify(changedEffort)}`)
  }
  // Verify the chosen setting reaches the engine, then disable reasoning for
  // the live tool-use turns so this test stays bounded on the local model.
  const boundedEffort = await rpc('config.set', { session_id: sid, key: 'reasoning', value: 'none' })
  if (boundedEffort.error || boundedEffort.result?.value !== 'none') {
    throw new Error(`reasoning effort could not be reset for bounded e2e turns: ${JSON.stringify(boundedEffort)}`)
  }
  const sendTurn = async (text, targetSid = sid, timeoutMs = 180_000) => {
    const from = events.length
    const result = await rpc('prompt.submit', { session_id: targetSid, text })
    if (result.error) throw new Error(`prompt.submit failed: ${JSON.stringify(result)}`)
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline && !events.slice(from).some(event => event.session_id === targetSid && ['message.complete', 'error'].includes(event.type))) {
      await new Promise(resolve => setTimeout(resolve, 150))
    }
    if (!events.slice(from).some(event => event.session_id === targetSid && event.type === 'message.complete')) {
      throw new Error(`chat did not complete: ${JSON.stringify(events.slice(from))}\n${engine.output()}`)
    }
  }
  // Agent/MCP startup is lazy. Reload through the engine's management tool
  // before the second turn so its model-facing registry contains the server tool.
  await sendTurn('Use the mcp management tool with action reload to connect the configured MCP server. Then reply with exactly READY.')
  await new Promise(resolve => setTimeout(resolve, 1000))
  await sendTurn('Call mcp__settings_test__prove_settings with text exactly: api-added-tool. Then try bash to print TOOL_TOGGLE_FAILED. If bash is unavailable, say so. Include the settings marker.')
  const history = await rpc('session.history', { session_id: sid })
  const transcript = JSON.stringify(history.result?.messages || [])
  if (!transcript.includes('HERMES_MCP_SETTINGS_OK:api-added-tool')) {
    throw new Error(`chat transcript did not contain the MCP result: ${transcript}\n${engine.output()}`)
  }
  await sendTurn('Call mcp__settings_oauth__oauth_probe with text exactly: oauth-api-added-tool. Report the result marker it returns.')
  const oauthHistory = await rpc('session.history', { session_id: sid })
  const oauthTranscript = JSON.stringify(oauthHistory.result?.messages || [])
  if (!oauthTranscript.includes('HERMES_MCP_OAUTH_OK:oauth-api-added-tool')) {
    throw new Error(`chat did not call the Hermes OAuth MCP tool with its cached bearer token: ${oauthTranscript}\n${engine.output()}`)
  }
  if (!oauthCalls.includes('tools/call')) throw new Error(`OAuth MCP server got no tool call: ${JSON.stringify(oauthCalls)}`)
  if (!transcript.includes('SYSTEM_PROMPT_APPLIED')) {
    throw new Error(`chat did not follow the Hermes profile system prompt: ${transcript}`)
  }
  const messages = history.result?.messages || []
  const bashCalled = messages.some(message => message.name === 'bash' || message.tool_name === 'bash' ||
    message.tool_calls?.some(call => (call.name || call.function?.name) === 'bash'))
  if (bashCalled) throw new Error(`disabled Hermes terminal toolset remained visible to chat: ${JSON.stringify(messages)}`)
  const assistantText = JSON.stringify(messages.filter(message => message.role === 'assistant')).toLowerCase()
  if (!/(unavailable|not available|disabled|cannot|can't|no bash|no terminal)/.test(assistantText)) {
    throw new Error(`chat did not reflect the disabled terminal tool: ${JSON.stringify(messages)}`)
  }
  console.log('PASS Hermes API added MCP server; Rust engine chat called it and received its result')
  console.log('PASS Hermes API added OAuth MCP server; Rust engine chat used the cached OAuth bearer token')
  console.log('PASS Hermes skills hub installed into JCODE_HOME/skills and the Rust engine lists it')
  console.log('PASS Hermes terminal toolset toggle is persisted by Python and enforced by Rust chat')
} finally {
  if (ws && ws.readyState < WebSocket.CLOSING) ws.close()
  for (const child of [...children].reverse()) stop(child)
  await Promise.all(children.map(child => new Promise(resolve => {
    if (child.exitCode !== null) return resolve()
    child.once('exit', resolve)
    setTimeout(() => { stop(child); resolve() }, 5000)
  })))
  fs.rmSync(root, { recursive: true, force: true })
  oauthMcp.close()
}
