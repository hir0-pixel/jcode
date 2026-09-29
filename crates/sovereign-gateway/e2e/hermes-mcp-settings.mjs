#!/usr/bin/env node
process.env.JCODE_RUNTIME_DIR ||= (await import('node:fs')).mkdtempSync('/tmp/sj-') // short private dir: never collide with a running engine
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
const memorySecret = `private-${crypto.randomBytes(12).toString('hex')}`
const memoryDir = path.join(jcodeHome, 'memory')
fs.mkdirSync(memoryDir, { recursive: true })
const memoryTimestamp = new Date().toISOString()
fs.writeFileSync(path.join(memoryDir, 'global.json'), JSON.stringify({
  graph_version: 2,
  memories: {
    'settings-memory-fixture': {
      id: 'settings-memory-fixture', category: 'fact',
      content: `My private recall token is ${memorySecret}.`,
      tags: ['private', 'recall', 'token'],
      search_text: `private recall token ${memorySecret}`,
      created_at: memoryTimestamp, updated_at: memoryTimestamp,
      access_count: 0, trust: 'high', strength: 1, active: true, confidence: 1,
    },
  },
  tags: {}, clusters: {}, edges: {}, reverse_edges: {}, metadata: {},
}))
fs.writeFileSync(
  path.join(jcodeHome, 'config.toml'),
  `[provider]\ndefault_provider = "openai-compatible"\n`,
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
const ollamaRequests = []
const ollamaProxyPaths = []
const ollamaProxyOutcomes = []
const activeChatRequests = new Set()
const ollamaRequestUpstreams = new WeakMap()
const ollamaProxy = http.createServer(async (request, response) => {
  ollamaProxyPaths.push(`${request.method} ${request.url}`)
  const chunks = []
  for await (const chunk of request) chunks.push(chunk)
  const body = Buffer.concat(chunks)
  let capturedRequest
  try {
    const parsed = JSON.parse(body.toString('utf8'))
    if (Array.isArray(parsed.messages)) {
      capturedRequest = parsed
      ollamaRequests.push(parsed)
    }
  } catch {}
  const upstream = http.request({
    hostname: '127.0.0.1', port: 11434, path: request.url, method: request.method,
    headers: { ...request.headers, host: '127.0.0.1:11434', connection: 'close' }, agent: false,
  }, upstreamResponse => {
    upstreamResponse.once('end', () => ollamaProxyOutcomes.push(`${request.method} ${request.url} -> ${upstreamResponse.statusCode} ended`))
    response.writeHead(upstreamResponse.statusCode || 502, upstreamResponse.headers)
    upstreamResponse.pipe(response)
  })
  if (request.method === 'POST' && request.url?.includes('/chat/completions') && capturedRequest) {
    ollamaRequestUpstreams.set(capturedRequest, upstream)
    activeChatRequests.add(upstream)
    upstream.once('close', () => activeChatRequests.delete(upstream))
  }
  response.on('close', () => {
    if (!response.writableEnded) upstream.destroy()
  })
  upstream.on('error', error => response.destroy(error))
  upstream.end(body)
})
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
    ollamaProxy.once('error', reject)
    ollamaProxy.listen(0, '127.0.0.1', resolve)
  })
  const ollamaProxyUrl = `http://127.0.0.1:${ollamaProxy.address().port}/v1`
// The engine's generic provider remains pointed directly at Ollama. Only
// Hermes's selected named profile uses the capture proxy, so the live request
// assertion proves provider selection changes the route.
env.JCODE_OPENAI_COMPAT_API_BASE = 'http://127.0.0.1:11434/v1'
  env.OPENAI_COMPAT_API_KEY = 'ollama'
  fs.writeFileSync(path.join(jcodeHome, 'config.toml'),
    `[provider]\ndefault_provider = "openai-compatible"\n\n[providers.local]\ntype = "openai-compatible"\nbase_url = "${ollamaProxyUrl}"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${model}"\nsupports_reasoning_effort = true\n\n[[providers.local.models]]\nid = "${model}"\ncontext_window = 65536\n`)
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
      providers: { local: {
        base_url: ollamaProxyUrl, api_key: 'ollama', model,
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
    body: JSON.stringify({ scope: 'main', provider: 'local', model, base_url: ollamaProxyUrl, api_key: 'ollama', confirm_expensive_model: true }),
  })
  if (!modelSetting.ok) throw new Error(`Hermes model setting returned ${modelSetting.status}: ${await modelSetting.text()}`)
  const soulSetting = await fetch(`${base}/api/profiles/default/soul`, {
    method: 'PUT',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ content: 'When a user asks for the settings marker, include SYSTEM_PROMPT_APPLIED in your response. If asked for the private recall token and it is not visible in the current conversation, respond with MEMORY_RECALL_DISABLED and do not guess or use tools.' }),
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
  if (created.result.info?.model !== model || created.result.info?.provider !== 'local' ||
    created.result.info?.reasoning_effort !== 'low' || created.result.info?.memory_enabled !== false) {
    throw new Error(`Hermes model/provider/reasoning/memory settings did not reach the engine session: ${JSON.stringify(created.result.info)}`)
  }
  const sendTurn = async (text, targetSid = sid, timeoutMs = 600_000) => {
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
  const setMemoryEnabled = async enabled => {
    const response = await fetch(`${base}/api/config`, {
      method: 'PUT',
      headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
      body: JSON.stringify({ config: { memory: { memory_enabled: enabled } } }),
    })
    if (!response.ok) throw new Error(`Hermes memory setting ${enabled} returned ${response.status}: ${await response.text()}`)
  }
  const waitModelRequest = async marker => {
    const deadline = Date.now() + 20_000
    while (Date.now() < deadline) {
      const matching = ollamaRequests.find(request => JSON.stringify(request.messages).includes(marker))
      if (matching) return matching
      await new Promise(resolve => setTimeout(resolve, 50))
    }
    return undefined
  }
  const chatDiagnostics = async sessionId => {
    const [active, history] = await Promise.all([
      rpc('session.active_list', { current_session_id: sessionId }),
      rpc('session.history', { session_id: sessionId }),
    ])
    const logDirectory = path.join(jcodeHome, 'logs')
    const logFiles = fs.existsSync(logDirectory)
      ? fs.readdirSync(logDirectory).filter(name => name.endsWith('.log')).sort()
      : []
    const logTail = logFiles.length
      ? fs.readFileSync(path.join(logDirectory, logFiles.at(-1)), 'utf8').split('\n')
          .filter(line => /API call starting|provider|model|turn|error/i.test(line)).slice(-30)
      : []
    const messages = history.result?.messages || []
    return JSON.stringify({
      active: active.result,
      history: messages.slice(-3).map(message => ({ role: message.role, name: message.name, type: message.type })),
      ollamaProxyPaths, ollamaProxyOutcomes, logTail,
    })
  }
  const interruptCapturedTurn = async (sessionId, marker) => {
    const captured = ollamaRequests.find(request => JSON.stringify(request.messages).includes(marker))
    const upstream = captured && ollamaRequestUpstreams.get(captured)
    if (!upstream) throw new Error(`no upstream Ollama request was captured for ${marker}`)
    const interrupted = await rpc('session.interrupt', { session_id: sessionId })
    if (interrupted.error) throw new Error(`could not stop captured settings turn: ${JSON.stringify(interrupted)}`)
    const deadline = Date.now() + 20_000
    while (Date.now() < deadline) {
      const active = await rpc('session.active_list', { current_session_id: sessionId })
      if (active.error) throw new Error(`could not check captured settings turn state: ${JSON.stringify(active)}`)
      if (!active.result?.sessions?.some(session => session.id === sessionId && session.current)) {
        if (activeChatRequests.has(upstream)) {
          const closed = new Promise(resolve => upstream.once('close', resolve))
          upstream.destroy()
          await Promise.race([closed, new Promise(resolve => setTimeout(resolve, 1000))])
        }
        activeChatRequests.delete(upstream)
        return
      }
      await new Promise(resolve => setTimeout(resolve, 100))
    }
    throw new Error(`captured settings turn remained active after interrupt: ${engine.output()}`)
  }
  const disabledMemory = (await rpc('session.create', { cwd: home })).result
  if (disabledMemory?.info?.memory_enabled !== false) {
    throw new Error(`Hermes memory disable setting did not reach its isolated engine session: ${JSON.stringify(disabledMemory?.info)}`)
  }
  const disabledMemorySid = disabledMemory.session_id
  const disabledReasoning = await rpc('config.set', { session_id: disabledMemorySid, key: 'reasoning', value: 'none' })
  if (disabledReasoning.error) throw new Error(`memory behavior check could not bound reasoning effort: ${JSON.stringify(disabledReasoning)}`)
  const disabledMarker = `memory-off-${crypto.randomBytes(8).toString('hex')}`
  const disabledPrompt = `For check ${disabledMarker}, what is my private recall token?`
  const disabledSubmit = await rpc('prompt.submit', { session_id: disabledMemorySid, text: disabledPrompt })
  if (disabledSubmit.error) throw new Error(`disabled-memory chat was rejected: ${JSON.stringify(disabledSubmit)}`)
  const disabledRequest = await waitModelRequest(disabledMarker)
  if (!disabledRequest) throw new Error(`disabled-memory chat never reached the model capture proxy (events=${JSON.stringify(events.slice(-10))}, diagnostics=${await chatDiagnostics(disabledMemorySid)}): ${engine.output()}`)
  if (JSON.stringify(disabledRequest.messages).includes(memorySecret)) {
    throw new Error('disabled Hermes memory leaked the seeded recall fact into the model request')
  }
  await interruptCapturedTurn(disabledMemorySid, disabledMarker)
  await setMemoryEnabled(true)
  const memoryEnabled = (await rpc('session.create', { cwd: home })).result
  if (memoryEnabled?.info?.memory_enabled !== true) {
    throw new Error(`Hermes memory enable setting did not reach the engine: ${JSON.stringify(memoryEnabled?.info)}`)
  }
  const memoryEffort = await rpc('config.set', { session_id: memoryEnabled.session_id, key: 'reasoning', value: 'none' })
  if (memoryEffort.error) throw new Error(`memory behavior check could not set reasoning to none: ${JSON.stringify(memoryEffort)}`)
  const enabledMarker = `memory-on-${crypto.randomBytes(8).toString('hex')}`
  const enabledPrompt = `For check ${enabledMarker}, what is my private recall token?`
  const enabledSubmit = await rpc('prompt.submit', { session_id: memoryEnabled.session_id, text: enabledPrompt })
  if (enabledSubmit.error) throw new Error(`enabled-memory prompt was rejected: ${JSON.stringify(enabledSubmit)}`)
  const enabledRequest = await waitModelRequest(enabledMarker)
  if (!enabledRequest) throw new Error(`enabled-memory chat never reached the model capture proxy (paths=${JSON.stringify(ollamaProxyPaths)}, events=${JSON.stringify(events.slice(-10))}): ${engine.output()}`)
  if (!JSON.stringify(enabledRequest.messages).includes(memorySecret)) {
    throw new Error(`enabled Hermes memory did not reach the local Ollama request: ${JSON.stringify(enabledRequest.messages)}`)
  }
  await interruptCapturedTurn(memoryEnabled.session_id, enabledMarker)
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
  const highReasoningUpdate = await fetch(`${base}/api/config`, {
    method: 'PUT',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ config: { agent: { reasoning_effort: 'high' } } }),
  })
  if (!highReasoningUpdate.ok) throw new Error(`Hermes reasoning setting returned ${highReasoningUpdate.status}: ${await highReasoningUpdate.text()}`)
  const highReasoningSession = (await rpc('session.create', { cwd: home })).result
  if (highReasoningSession?.info?.reasoning_effort !== 'high') {
    throw new Error(`Hermes high reasoning setting did not reach the engine session: ${JSON.stringify(highReasoningSession?.info)}`)
  }
  const reasoningMarker = `reasoning-high-${crypto.randomBytes(8).toString('hex')}`
  const reasoningSubmit = await rpc('prompt.submit', {
    session_id: highReasoningSession.session_id, text: `Reply READY for ${reasoningMarker}`,
  })
  if (reasoningSubmit.error) throw new Error(`high reasoning chat was rejected: ${JSON.stringify(reasoningSubmit)}`)
  const reasoningRequest = await waitModelRequest(reasoningMarker)
  if (!reasoningRequest) throw new Error(`high reasoning chat never reached local Ollama: ${engine.output()}`)
  if (reasoningRequest.model !== model || reasoningRequest.reasoning_effort !== 'high') {
    throw new Error(`Hermes-selected model/provider/reasoning did not reach the model request: ${JSON.stringify({
      model: reasoningRequest.model, reasoning_effort: reasoningRequest.reasoning_effort,
    })}`)
  }
  await interruptCapturedTurn(highReasoningSession.session_id, reasoningMarker)
  console.log('PASS Hermes API added MCP server; Rust engine chat called it and received its result')
  console.log('PASS Hermes API added OAuth MCP server; Rust engine chat used the cached OAuth bearer token')
  console.log('PASS Hermes skills hub installed into JCODE_HOME/skills and the Rust engine lists it')
  console.log('PASS Hermes terminal toolset toggle is persisted by Python and enforced by Rust chat')
  console.log('PASS Hermes memory toggle excludes/includes the seeded memory context in Rust chat requests')
  console.log('PASS Hermes model/provider and reasoning settings reach the local model request')
} finally {
  if (ws && ws.readyState < WebSocket.CLOSING) ws.close()
  for (const child of [...children].reverse()) stop(child)
  await Promise.all(children.map(child => new Promise(resolve => {
    if (child.exitCode !== null) return resolve()
    child.once('exit', resolve)
    setTimeout(() => { stop(child); resolve() }, 5000)
  })))
  fs.rmSync(root, { recursive: true, force: true })
  ollamaProxy.close()
  oauthMcp.close()
}
