#!/usr/bin/env node
process.env.JCODE_RUNTIME_DIR ||= (await import('node:fs')).mkdtempSync('/tmp/sj-') // short private dir: never collide with a running engine
// Live, local accounting check. Each upstream model response must have one
// ledger span with the same model and token usage. Requires local Ollama.
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

const root = path.resolve(import.meta.dirname, '../../..')
const bin = process.env.SOVEREIGN_BIN || path.join(root, 'target/release/sovereign')
const model = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-accounting-'))
const jcodeHome = path.join(home, '.jcode')
const callsFile = path.join(home, 'calls.jsonl')
const token = crypto.randomBytes(24).toString('hex')
fs.mkdirSync(jcodeHome)
fs.writeFileSync(path.join(jcodeHome, 'config.toml'), `[provider]\ndefault_provider = "local"\n\n[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:18080/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${model}"\n\n[[providers.local.models]]\nid = "${model}"\ncontext_window = 65536\n`)
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const proxy = spawn(process.execPath, [path.join(root, 'scripts/sovereign-counting-proxy.mjs')], {
  env: { ...process.env, SOVEREIGN_PROXY_LISTEN: '127.0.0.1:18080', SOVEREIGN_PROXY_CALLS: callsFile },
  stdio: ['ignore', 'ignore', 'pipe'],
})
let proxyError = ''
proxy.stderr.on('data', chunk => { proxyError += chunk })
let engine
let ws
try {
  for (let i = 0; i < 100; i++) {
    if (proxyError.includes('listening on')) break
    if (proxy.exitCode !== null) throw new Error(`proxy exited: ${proxyError}`)
    await sleep(100)
  }
  if (!proxyError.includes('listening on')) throw new Error(`proxy did not start: ${proxyError}`)
  engine = spawn(bin, ['--provider', 'openai-compatible', '--model', model, 'serve', '--host', '127.0.0.1', '--port', '0'], {
    cwd: home,
    env: { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token, SOVEREIGN_LEARN_TURN_INTERVAL: '1', SOVEREIGN_LEARN_COOLDOWN_MS: '0' },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  let stderr = ''
  engine.stderr.on('data', chunk => { stderr += chunk })
  const port = await new Promise((resolve, reject) => {
    let out = ''
    const timer = setTimeout(() => reject(new Error(`engine did not start: ${stderr}`)), 120_000)
    engine.stdout.on('data', chunk => {
      out += chunk
      const match = out.match(/HERMES_BACKEND_READY port=(\d+)/)
      if (match) { clearTimeout(timer); resolve(Number(match[1])) }
    })
    engine.on('exit', code => { clearTimeout(timer); reject(new Error(`engine exited ${code}: ${stderr}`)) })
  })
  const api = async (route, options = {}) => {
    const response = await fetch(`http://127.0.0.1:${port}${route}`, {
      ...options,
      headers: { Authorization: `Bearer ${token}`, ...(options.body ? { 'Content-Type': 'application/json' } : {}) },
    })
    if (!response.ok) throw new Error(`${route}: ${response.status} ${await response.text()}`)
    return response.json()
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
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve); ws.addEventListener('error', reject) })
  const rpc = (method, params = {}) => {
    const id = `a${++next}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise(resolve => pending.set(id, resolve))
  }
  const turn = async (session, prompt) => {
    const from = events.length
    const submitted = await rpc('prompt.submit', { session_id: session, text: prompt })
    if (submitted.error) throw new Error(JSON.stringify(submitted.error))
    for (let i = 0; i < 3000; i++) {
      const done = events.slice(from).find(e => e.session_id === session && ['message.complete', 'error'].includes(e.type))
      if (done) return done
      await sleep(200)
    }
    throw new Error('chat timed out')
  }
  const session = (await rpc('session.create', { cwd: home })).result.session_id
  await turn(session, 'Use your shell tool to run pwd, then tell me the current directory. From now on, write quick scripts in Nim. Please remember that.')
  // Engine titles the first turn. Trigger a separate explicit rename as well.
  const renamed = await rpc('session.title', { session_id: session, title: 'Accounting check' })
  if (renamed.error) throw new Error(`title rename failed: ${JSON.stringify(renamed.error)}`)
  for (let i = 0; i < 310; i++) {
    if (fs.existsSync(callsFile) && fs.readFileSync(callsFile, 'utf8').includes('"learning pass"')) break
    await sleep(1000)
  }
  await sleep(2000)
  const cron = await api('/api/agent/run', { method: 'POST', body: JSON.stringify({ prompt: 'What is 3 plus 4? Reply with only the number.', cwd: home, title: 'Accounting cron', timeout_s: 300 }) })
  if (!cron.ok) throw new Error(`agent.run failed: ${JSON.stringify(cron)}`)
  const calls = fs.readFileSync(callsFile, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse).filter(call => call.status === 200)
  let db
  for (const file of [path.join(jcodeHome, 'sovereign.db'), path.join(jcodeHome, 'observability.sqlite3')].filter(fs.existsSync)) {
    const candidate = new DatabaseSync(file, { readOnly: true })
    if (candidate.prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='spans'").get()) { db = candidate; break }
    candidate.close()
  }
  if (!db) throw new Error('observation database missing')
  let rows = []
  for (let i = 0; i < 50; i++) {
    rows = db.prepare(`SELECT s.*, r.model, r.session_id, r.kind AS run_kind, r.title AS run_title FROM spans s JOIN fact_turn r ON r.id=s.run_id WHERE s.input_tokens>0 OR s.output_tokens>0`).all()
    if (rows.length >= calls.length) break
    await sleep(100)
  }
  db.close()
  const expectedKind = purpose => ({ 'main turn': 'chat', 'tool follow-up': 'tool_followup', 'learning pass': 'learning', title: 'title' })[purpose]
  const remaining = [...rows]
  const matched = []
  for (const call of calls) {
    const kind = call.last_user_head?.includes('3 plus 4') ? 'cron' : expectedKind(call.purpose)
    const index = remaining.findIndex(row => row.model === call.model && row.input_tokens === call.prompt_tokens && row.output_tokens === call.completion_tokens && row.cache_read_tokens === (call.cached_tokens || 0) && (!kind || row.kind === kind))
    if (index >= 0) matched.push(remaining.splice(index, 1)[0])
    else console.error('UNMATCHED', JSON.stringify({ purpose: call.purpose, kind, model: call.model, input: call.prompt_tokens, output: call.completion_tokens, cached: call.cached_tokens }))
  }
  console.log(`accounting: ${matched.length}/${calls.length} proxy calls matched; gap ${calls.length - matched.length}`)
  console.log(`purposes: ${JSON.stringify(calls.reduce((out, call) => (out[call.purpose] = (out[call.purpose] || 0) + 1, out), {}))}`)
  const cronCall = calls.some(call => call.last_user_head?.includes('3 plus 4'))
  const cronSpan = matched.some(row => row.kind === 'cron' && row.run_kind === 'cron' && row.run_title === 'Accounting cron')
  if (!calls.length || !calls.some(call => call.purpose === 'tool follow-up') || !calls.some(call => call.purpose === 'learning pass') || !cronCall || !cronSpan || calls.some(call => call.prompt_tokens === null || call.completion_tokens === null) || matched.length !== calls.length || remaining.length) process.exitCode = 1
} finally {
  ws?.close()
  engine?.kill('SIGTERM')
  engine?.stdout.destroy()
  engine?.stderr.destroy()
  proxy.kill('SIGTERM')
  proxy.stderr.destroy()
  if (!process.env.KEEP_ACCOUNTING_HOME) fs.rmSync(home, { recursive: true, force: true })
  else console.log(`accounting home: ${home}`)
}
