// Live end-to-end check of the sovereign engine against the Hermes gateway
// contract. Launches the real binary the way the Hermes desktop does, then
// drives it over HTTP + WebSocket, validating every frame against the contract.
//
//   node crates/sovereign-gateway/e2e/live.mjs
//
// Env: SOVEREIGN_BIN (default target/release/sovereign), HERMES_AGENT_DIR
// (for the ws/ajv packages; default ../hermes-agent), E2E_PROVIDER / E2E_MODEL
// (default ollama / qwen3.8:27b), E2E_SKIP_CHAT=1 to skip model calls.

import { spawn, execFileSync } from 'node:child_process'
import { createRequire } from 'node:module'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const repo = path.resolve(here, '../../..')
const hermes = process.env.HERMES_AGENT_DIR || path.resolve(repo, '../hermes-agent')
const require = createRequire(path.join(hermes, 'package.json'))
const WebSocket = require('ws')
const Ajv = require('ajv')

const contract = JSON.parse(fs.readFileSync(path.join(here, '../contract/gateway-contract.openrpc.json'), 'utf8'))
const ajv = new Ajv({ strict: false, allErrors: true })
const validators = new Map()
function validate(kind, name, schema, value) {
  const key = `${kind}:${name}`
  if (!validators.has(key)) validators.set(key, ajv.compile({ ...schema, components: contract.components }))
  const fn = validators.get(key)
  if (!fn(value)) fail(`${key} violates the contract: ${ajv.errorsText(fn.errors)}\n${JSON.stringify(value).slice(0, 400)}`)
}
const notif = Object.fromEntries(contract['x-notifications'].map(n => [n.name, n.params[0].schema]))
const results = Object.fromEntries(contract.methods.map(m => [m.name, m.result.schema]))

let failures = 0
const passed = []
function fail(msg) { failures++; console.error('FAIL', msg) }
function ok(msg) { passed.push(msg); console.log('ok  ', msg) }
function check(cond, msg) { cond ? ok(msg) : fail(msg) }

const bin = process.env.SOVEREIGN_BIN || path.join(repo, 'target/release/sovereign')
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-e2e-'))
const token = crypto.randomBytes(32).toString('hex')
const provider = process.env.E2E_PROVIDER || 'ollama'
const model = process.env.E2E_MODEL || 'qwen3.8:27b'
const readyFile = path.join(home, 'ready.json')

const child = spawn(bin, ['--provider', provider, '--model', model, '--profile', 'x', 'serve', '--host', '127.0.0.1', '--port', '0'], {
  // HERMES_HOME isolates the on-demand Python feature backend from the user's real profile.
  env: { ...process.env, JCODE_HOME: home, HERMES_HOME: path.join(home, 'hermes-home'), HERMES_DASHBOARD_SESSION_TOKEN: token, HERMES_DESKTOP_READY_FILE: readyFile },
  cwd: home,
  stdio: ['ignore', 'pipe', 'pipe']
})
let stderr = ''
child.stderr.on('data', d => { stderr += d })
const port = await new Promise((resolve, reject) => {
  let buf = ''
  const timer = setTimeout(() => reject(new Error(`no READY line in 60s\n${stderr}`)), 60_000)
  child.stdout.on('data', d => {
    buf += d
    const m = buf.match(/^HERMES_BACKEND_READY port=(\d+)/m)
    if (m) { clearTimeout(timer); resolve(Number(m[1])) }
  })
  child.on('exit', code => { clearTimeout(timer); reject(new Error(`engine exited ${code}\n${stderr}`)) })
})
ok(`engine announced port ${port}`)
check(JSON.parse(fs.readFileSync(readyFile, 'utf8')).port === port, 'ready file carries the port')
check((fs.statSync(readyFile).mode & 0o077) === 0, 'ready file is owner-only')

const base = `http://127.0.0.1:${port}`
const get = (p, headers = {}) => fetch(base + p, { headers })
let r = await get('/api/health')
check(r.status === 200 && (await r.json()).ok === true, '/api/health is public and ok')
r = await get('/api/status')
check(r.status === 200, '/api/status is public')
r = await get('/api/host/identity')
check(r.status === 401, 'token-gated route refuses a missing token')
r = await get('/api/host/identity', { authorization: 'Bearer wrong' })
check(r.status === 401, 'token-gated route refuses a wrong token')
r = await get('/api/host/identity', { authorization: `Bearer ${token}` })
check(r.status === 200, 'token-gated route accepts the token')
// fetch forbids overriding Host, so exercise the rebinding check with a raw socket
const raw = await new Promise(resolve => {
  const net = require('node:net')
  const s = net.connect(port, '127.0.0.1', () => s.write('GET /api/health HTTP/1.1\r\nHost: evil.example\r\n\r\n'))
  let out = ''
  s.on('data', d => { out += d })
  s.on('close', () => resolve(out))
})
check(raw.startsWith('HTTP/1.1 403'), 'DNS-rebinding Host header is refused')
const huge = await new Promise(resolve => {
  const net = require('node:net')
  const s = net.connect(port, '127.0.0.1', () => s.write('GET / HTTP/1.1\r\nX: ' + 'a'.repeat(40_000) + '\r\n\r\n'))
  s.on('error', () => resolve('reset'))
  s.on('close', () => resolve('closed'))
})
check(huge === 'closed' || huge === 'reset', 'oversized headers are dropped')

function openWs(query, headers = {}) {
  return new WebSocket(`ws://127.0.0.1:${port}/api/ws${query}`, { headers })
}
function closeCode(ws) {
  return new Promise(resolve => ws.on('close', code => resolve(code)))
}
check((await closeCode(openWs(''))) === 4401, 'websocket without token closes 4401')
check((await closeCode(openWs('?token=nope'))) === 4401, 'websocket with wrong token closes 4401')
check((await closeCode(openWs(`?token=${token}`, { origin: 'https://evil.example' }))) === 4403, 'cross-site origin closes 4403')

// Authenticated client
const ws = openWs(`?token=${token}`, { origin: 'file://' })
const events = []
const approvals = []
let approvalChoice = 'deny'
const approvalSchema = contract['x-server-requests'].find(r => r.name === 'approval').params[0].schema
const pending = new Map()
let nextId = 1
ws.on('message', data => {
  const frame = JSON.parse(data.toString())
  if (frame.method === 'event') {
    events.push(frame.params)
    const schema = notif[frame.params.type]
    if (!schema) fail(`unknown event type ${frame.params.type}`)
    else validate('event', frame.params.type, schema, frame.params.payload ?? {})
  } else if (frame.id && pending.has(frame.id)) {
    pending.get(frame.id)(frame)
    pending.delete(frame.id)
  } else if (frame.method === 'approval') {
    approvals.push(frame.params)
    validate('server-request', 'approval', approvalSchema, frame.params)
    ws.send(JSON.stringify({ jsonrpc: '2.0', id: frame.id, result: { choice: approvalChoice } }))
  }
})
await new Promise((resolve, reject) => { ws.on('open', resolve); ws.on('error', reject) })
function rpc(method, params = {}) {
  const id = `c${nextId++}`
  ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`${method} timed out`)), 120_000)
    pending.set(id, f => { clearTimeout(t); resolve(f) })
  })
}
const waitFor = (pred, ms) => new Promise((resolve, reject) => {
  const t0 = Date.now()
  const tick = () => {
    const hit = events.find(pred)
    if (hit) return resolve(hit)
    if (Date.now() - t0 > ms) return reject(new Error('timed out waiting for event'))
    setTimeout(tick, 100)
  }
  tick()
})
await waitFor(e => e.type === 'gateway.ready', 5000)
ok('gateway.ready received')

let f = await rpc('definitely.not.a.method')
check(f.error?.code === -32601, 'unknown methods return -32601 (method not found)')
ws.send('{not json')
await new Promise(res => setTimeout(res, 200))
f = await rpc('prompt.submit', {})
check(f.error?.code === -32602, 'missing session_id returns invalid params')
f = await rpc('ping')
check(!f.error, 'ping answers')
fs.mkdirSync(path.join(home, 'repo', '.git'), { recursive: true })
fs.writeFileSync(path.join(home, 'repo', '.git', 'HEAD'), 'ref: refs/heads/main\n')
f = await rpc('config.get', { key: 'project', cwd: path.join(home, 'repo') })
if (!f.error) validate('result', 'config.get', results['config.get'], f.result)
check(f.result?.branch === 'main', 'config.get project reports the git branch')
for (const m of ['setup.status', 'setup.runtime_check', 'free_tier.status', 'model.options', 'wake.status',
  'session.active_list', 'commands.catalog', 'profiles.list', 'pet.info', 'projects.tree',
  'gateway.capabilities', 'client.capabilities']) {
  f = await rpc(m)
  if (f.error) fail(`${m} errored: ${f.error.message}`)
  else { validate('result', m, results[m], f.result); ok(`${m} answers within contract`) }
}

f = await rpc('session.create', { cwd: home })
check(!f.error, 'session.create succeeds')
if (!f.error) validate('result', 'session.create', results['session.create'], f.result)
const sid = f.result?.session_id

const auth = { 'x-hermes-session-token': token }
r = await get('/api/profiles/sessions?limit=20&offset=0&min_messages=0&archived=false&order=recent&profile=all')
check(r.status === 401, 'session REST route refuses a missing token')
for (const route of ['/api/model/info', '/api/profiles', '/api/profiles/active', '/api/hermes/update/check', '/api/fs/default-cwd', '/api/cron/jobs']) {
  r = await get(route, auth)
  check(r.status === 200, `${route} answers`)
}
r = await get('/api/hermes/update/check', auth)
check((await r.json()).update_available === false, 'update check never offers the Hermes updater')
f = await rpc('session.list', { limit: 20 })
if (!f.error) validate('result', 'session.list', results['session.list'], f.result)
check(f.result?.sessions?.some(s => s.id === sid), 'session.list includes the new session')

if (process.env.E2E_SKIP_CHAT !== '1' && sid) {
  const t0 = Date.now()
  f = await rpc('prompt.submit', { session_id: sid, text: 'Reply with exactly the word PONG and nothing else.' })
  check(!f.error, 'prompt.submit accepted')
  if (!f.error) validate('result', 'prompt.submit', results['prompt.submit'], f.result)
  const done = await waitFor(e => e.type === 'message.complete' && e.session_id === sid, 600_000).catch(e => fail(e.message))
  if (done) {
    console.log('     reply:', JSON.stringify(done.payload.text).slice(0, 200), `(${((Date.now() - t0) / 1000).toFixed(1)}s)`)
    const u = done.payload.usage || {}
    console.log(`     usage: input ${u.input} tokens, output ${u.output}, calls ${u.calls}, cache_read ${u.cache_read}`)
    check(done.payload.status === 'complete', 'turn completed')
    check(/PONG/i.test(done.payload.text), 'reply streamed back through the gateway')
    const seqs = events.filter(e => e.session_id === sid).map(e => e.seq)
    check(seqs.every((s, i) => i === 0 || s > seqs[i - 1]), 'per-session seq is monotonic')
    check(events.some(e => e.type === 'message.start'), 'message.start emitted')
  }
  f = await rpc('session.list', { limit: 20 })
  console.log('     listed row:', JSON.stringify(f.result?.sessions?.find(x => x.id === sid)))
  check(f.result?.sessions?.find(x => x.id === sid)?.title?.startsWith('Reply with exactly the word PONG'), 'a session is titled from its first prompt')
  f = await rpc('session.history', { session_id: sid })
  if (!f.error) validate('result', 'session.history', results['session.history'], f.result)
  check((f.result?.count ?? 0) >= 2, 'history holds the exchange')

  // jcode persists a session once it holds a message; REST lists stored sessions
r = await get('/api/profiles/sessions?limit=20&offset=0&min_messages=0&archived=false&order=recent&profile=all', auth)
  const paged = await r.json(); if (process.env.E2E_DEBUG) console.log('PAGED', JSON.stringify(paged).slice(0, 600))
  check(r.status === 200 && paged.sessions.some(x => x.id === sid) && paged.total >= 1, '/api/profiles/sessions lists the session')
  r = await get('/api/profiles/sessions/sidebar?recents_profile=default&recents_limit=10&cron_limit=5&messaging_limit=5', auth)
  const side = await r.json()
  check(r.status === 200 && side.recents.sessions.some(x => x.id === sid) && Array.isArray(side.cron.sessions), 'sidebar batch lists the session')
  // resume returns the same transcript
  f = await rpc('session.resume', { session_id: sid })
  if (!f.error) validate('result', 'session.resume', results['session.resume'], f.result)
  check(!f.error && f.result.message_count >= 2, 'session.resume returns the transcript')
  f = await rpc('session.activate', { session_id: sid })
  if (!f.error) validate('result', 'session.activate', results['session.activate'], f.result)
  check(!f.error && f.result.message_count >= 2, 'session.activate (chat switching) returns the transcript')
}

// A reconnecting client submits without resuming first: the gateway must attach.
if (process.env.E2E_SKIP_CHAT !== '1' && sid) {
  const ws2 = openWs(`?token=${token}`, { origin: 'file://' })
  const got = []
  const replies = new Map()
  ws2.on('message', d => {
    const fr = JSON.parse(d.toString())
    if (fr.method === 'event') got.push(fr.params)
    else if (fr.id && replies.has(fr.id)) replies.get(fr.id)(fr)
  })
  await new Promise(res => ws2.on('open', res))
  const sub = await new Promise(res => {
    replies.set('r1', res)
    ws2.send(JSON.stringify({ jsonrpc: '2.0', id: 'r1', method: 'prompt.submit', params: { session_id: sid, text: 'Reply with exactly the word AGAIN.' } }))
  })
  check(!sub.error, 'submit on a fresh connection auto-attaches')
  const t0 = Date.now()
  let done2 = null
  while (!done2 && Date.now() - t0 < 600_000) {
    done2 = got.find(e => e.type === 'message.complete' && e.session_id === sid)
    await new Promise(res => setTimeout(res, 200))
  }
  if (!(done2 && /AGAIN/i.test(done2.payload.text))) console.log('     ws2 got:', JSON.stringify(got.map(e => e.type + (e.payload?.text ? ':' + e.payload.text.slice(0, 60) : ''))).slice(0, 600))
  check(done2 && /AGAIN/i.test(done2.payload.text), 'second connection receives its reply')
  ws2.close()
}

// Prime REPL: the model can run code in the sandboxed worker through the tool.
if (process.env.E2E_SKIP_CHAT !== '1') {
  f = await rpc('session.create', { cwd: home })
  const psid = f.result?.session_id; console.log('     sid', sid, 'psid', psid, f.error ? JSON.stringify(f.error) : '')
  f = await rpc('prompt.submit', { session_id: psid, text: 'Use the repl tool to evaluate sum(range(10**6)) and then reply with only the resulting number.' })
  check(psid && psid !== sid, 'a second session on the same socket is a distinct session')
  check(!f.error, 'prime prompt accepted')
  const pdone = await waitFor(e => e.type === 'message.complete' && e.session_id === psid, 600_000).catch(e => fail(e.message))
  const tools = events.filter(e => e.session_id === psid && e.type === 'tool.start').map(e => e.payload.name)
  console.log('     prime tools used:', tools.join(', ') || '(none)', '| reply:', JSON.stringify(pdone?.payload.text).slice(0, 120))
  check(tools.includes('repl'), 'model called the repl tool')
  const out = events.find(e => e.session_id === psid && e.type === 'tool.complete' && e.payload.name === 'repl')
  check(/499999500000/.test(out?.payload.result_text || ''), 'repl computed the value in the sandbox')
}

// Continual Harness: learned instructions reach new sessions; /refine and rollback.
if (process.env.E2E_SKIP_CHAT !== '1') {
  const learned = path.join(home, 'harness', 'prompt.md')
  fs.mkdirSync(path.dirname(learned), { recursive: true })
  fs.writeFileSync(learned, '- End every reply with the single word MANGO.\n')
  f = await rpc('session.create', { cwd: home })
  const hsid = f.result.session_id
  await rpc('prompt.submit', { session_id: hsid, text: 'Say hello in three words.' })
  const hdone = await waitFor(e => e.type === 'message.complete' && e.session_id === hsid, 600_000).catch(e => fail(e.message))
  console.log('     harness reply:', JSON.stringify(hdone?.payload.text).slice(0, 120))
  check(/MANGO/.test(hdone?.payload.text || ''), 'learned instructions apply to a new session')
  fs.rmSync(learned)

  f = await rpc('session.create', { cwd: home })
  const rsid = f.result.session_id
  await rpc('prompt.submit', { session_id: rsid, text: 'Please always use pnpm instead of npm in this repo. Just acknowledge.' })
  await waitFor(e => e.type === 'message.complete' && e.session_id === rsid, 600_000).catch(e => fail(e.message))
  f = await rpc('slash.exec', { session_id: rsid, command: 'refine' })
  if (!f.error) validate('result', 'slash.exec', results['slash.exec'], f.result)
  const msg = f.result?.output || ''
  console.log('     /refine:', JSON.stringify(msg).slice(0, 240))
  check(!f.error && /^(Learned:|No change:|rejected:|the refine reply)/.test(msg), '/refine answers with an outcome')
  if (msg.startsWith('Learned:')) {
    check(fs.readFileSync(learned, 'utf8').toLowerCase().includes('pnpm'), '/refine wrote the learned preference')
    f = await rpc('slash.exec', { session_id: rsid, command: 'refine rollback' })
    check(/Rolled back/.test(f.result?.output || '') && !fs.existsSync(learned), '/refine rollback restores the previous version')
  }
  f = await rpc('slash.exec', { session_id: rsid, command: 'harness' })
  check(/learned instructions/i.test(f.result?.output || ''), '/harness shows the learned instructions')
}

// Local memory (E2E_MEMORY=1): saved through the memory tool, recalled locally.
if (process.env.E2E_MEMORY === '1') {
  const turn = async (s, text) => {
    const before = events.filter(e => e.type === 'message.complete' && e.session_id === s).length
    await rpc('prompt.submit', { session_id: s, text })
    const t0 = Date.now()
    while (Date.now() - t0 < 600_000) {
      const done = events.filter(e => e.type === 'message.complete' && e.session_id === s)
      if (done.length > before) return done[done.length - 1]
      await new Promise(r => setTimeout(r, 200))
    }
  }
  f = await rpc('session.create', { cwd: home })
  const m1 = f.result.session_id
  await turn(m1, 'Use the memory tool to remember, globally, this fact about me: my favourite colour is teal.')
  const saved = events.filter(e => e.session_id === m1 && e.type === 'tool.start').map(e => e.payload.name)
  console.log('     memory tools used:', saved.join(', '))
  check(saved.includes('memory'), 'model saved the fact with the memory tool')
  f = await rpc('session.create', { cwd: home })
  const m2 = f.result.session_id
  await turn(m2, 'I will ask you about my favourite colour next.')
  const a = await turn(m2, 'What is my favourite colour? Answer with one word, or UNKNOWN.')
  console.log('     recall answer:', JSON.stringify(a?.payload.text).slice(0, 80))
  check(/teal/i.test(a?.payload.text || ''), 'a new session recalled the memory locally')
}

// Human approval for risky shell commands (the pre_tool gate).
if (process.env.E2E_SKIP_CHAT !== '1') {
  const victim = path.join(home, 'scratch-dir')
  const ask = async (s, text) => {
    const before = events.filter(e => e.type === 'message.complete' && e.session_id === s).length
    await rpc('prompt.submit', { session_id: s, text })
    const t0 = Date.now()
    while (Date.now() - t0 < 600_000) {
      if (events.filter(e => e.type === 'message.complete' && e.session_id === s).length > before) return
      await new Promise(r => setTimeout(r, 200))
    }
  }
  fs.mkdirSync(victim, { recursive: true }); fs.writeFileSync(path.join(victim, 'f.txt'), 'x')
  f = await rpc('session.create', { cwd: home })
  const asid = f.result.session_id
  // Did the model issue a bash call containing `needle` since event index `from`?
  const ranBash = (from, needle) => events.slice(from).some(e => e.session_id === asid && e.type === 'tool.start' && e.payload.name === 'bash' && String(e.payload.args?.command || '').includes(needle))
  approvalChoice = 'deny'
  let n0 = approvals.length, e0 = events.length
  await ask(asid, 'Run exactly this shell command with the bash tool: rm -rf ./scratch-dir')
  if (ranBash(e0, 'rm -rf')) {
    check(approvals.length > n0, 'a risky command raised a desktop approval prompt')
    check(fs.existsSync(victim), 'a denied command did not run')
  } else console.log('     skipped: the model did not issue rm -rf (deny case)')
  approvalChoice = 'once'
  n0 = approvals.length; e0 = events.length
  await ask(asid, 'I approve it now. Run exactly this shell command with the bash tool: rm -rf ./scratch-dir')
  if (ranBash(e0, 'rm -rf')) check(approvals.length > n0 && !fs.existsSync(victim), 'an approved command ran')
  else console.log('     skipped: the model did not issue rm -rf (approve case)')
  const n2 = approvals.length
  await ask(asid, 'Run exactly this shell command with the bash tool and show me its output: echo TOKEN=[$HERMES_DASHBOARD_SESSION_TOKEN]')
  const leaked = events.some(e => e.session_id === asid && JSON.stringify(e.payload || {}).includes(token))
  const echoed = events.filter(e => e.session_id === asid && e.type === 'tool.complete').map(e => e.payload.result_text || '').filter(t => t.includes('TOKEN=')).pop() || ''
  console.log('     token probe:', JSON.stringify(echoed).slice(0, 200), 'leaked=' + leaked)
  const lastTools = events.filter(e => e.session_id === asid && (e.type === 'tool.start' || e.type === 'tool.complete')).slice(-2).map(e => e.type + ':' + JSON.stringify(e.payload).slice(0, 160))
  const lastReply = events.filter(e => e.session_id === asid && e.type === 'message.complete').pop()
  console.log('     last tools:', lastTools.join(' | '), '\n     last reply:', JSON.stringify(lastReply?.payload.text).slice(0, 200))
  check(!leaked, 'the desktop token never appears in any session event')
  if (echoed) check(/TOKEN=\[\]/.test(echoed), "the model's shell sees an empty desktop token")
  else console.log("     skipped: the model did not run the token probe")
  const approvalFile = path.join(home, 'sovereign-approval.json')
  check(fs.existsSync(approvalFile) && (fs.statSync(approvalFile).mode & 0o077) === 0, 'approval endpoint file is owner-only')
  await ask(asid, 'Run exactly this shell command with the bash tool: echo safe-command')
  check(approvals.length === n2, 'safe commands ran without a prompt')
}

// Hybrid: features the Rust harness does not own are served by Hermes's own
// Python backend, started on demand. Chat above must not have started it.
const pyCount = () => {
  // Real Python backends only: shells whose command line merely mentions it do not count.
  const lines = execFileSync('ps', ['-Ao', 'command']).toString().split('\n')
  return lines.filter(l => /Python|python/.test(l.split(' ')[0]) && l.includes('serve --host 127.0.0.1 --port 0 --skip-build')).length
}
check(pyCount() === 0, 'chat alone never started the Python feature backend')
if (process.env.E2E_SKIP_FEATURES !== '1') {
  const t0 = Date.now()
  f = await rpc('config.show', {})
  if (!f.error) validate('result', 'config.show', results['config.show'], f.result)
  check(!f.error && Array.isArray(f.result?.sections), `config.show is served by the Python feature backend (${Date.now() - t0} ms incl. cold start)`)
  check(pyCount() === 1, 'the Python feature backend started on demand')
  const t1 = Date.now()
  f = await rpc('config.show', {})
  console.log(`     forwarded call, warm: ${Date.now() - t1} ms`)
  r = await get('/api/config', { 'x-hermes-session-token': token })
  const cfg = await r.json().catch(() => null)
  check(r.status === 200 && cfg && typeof cfg === 'object', 'HTTP routes are forwarded (/api/config)')
  r = await get('/api/config')
  check(r.status === 401, 'forwarded routes still require the desktop token')
}

f = await rpc('session.interrupt', { session_id: sid })
if (!f.error) validate('result', 'session.interrupt', results['session.interrupt'], f.result)
check(!f.error, 'session.interrupt answers when idle')

const rssKb = Number(execFileSync('ps', ['-o', 'rss=', '-p', String(child.pid)]).toString().trim())
console.log(`     engine RSS: ${(rssKb / 1024).toFixed(1)} MB`)

ws.close()
child.kill('SIGKILL')
await new Promise(res => child.on('exit', res))
if (process.env.E2E_SKIP_FEATURES !== '1') {
  let left = 1
  for (let i = 0; i < 40 && left; i++) { await new Promise(res => setTimeout(res, 250)); left = pyCount() }
  check(left === 0, 'the Python feature backend exits when the engine is killed')
}
fs.rmSync(home, { recursive: true, force: true })
console.log(`\n${passed.length} passed, ${failures} failed`)
process.exit(failures ? 1 : 0)
