#!/usr/bin/env node
// Live check that every chat-bound session method is answered by the engine
// (never forwarded to Hermes's Python backend) and really changes the
// engine's store. Needs Ollama with the model below.
//
//   node crates/sovereign-gateway/e2e/sessions.mjs
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const BIN = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const MODEL = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-sessions-e2e-'))
const jcodeHome = path.join(home, '.jcode')
fs.mkdirSync(jcodeHome, { recursive: true })
fs.writeFileSync(
  path.join(jcodeHome, 'config.toml'),
  `[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:11434/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${MODEL}"\n\n[[providers.local.models]]\nid = "${MODEL}"\ncontext_window = 65536\n`
)
const token = crypto.randomBytes(24).toString('hex')
let failures = 0
const check = (cond, msg) => {
  console.log(cond ? 'ok  ' : 'FAIL', msg)
  if (!cond) failures++
}

const env = { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token, SOVEREIGN_TRACE_FORWARD: '1' }
const engine = spawn(BIN, ['--provider-profile', 'local', '--model', MODEL, 'serve', '--host', '127.0.0.1', '--port', '0'], { env, cwd: home, stdio: ['ignore', 'pipe', 'pipe'] })
let stderr = ''
engine.stderr.on('data', d => { stderr += d })
const port = await new Promise((resolve, reject) => {
  let buf = ''
  engine.stdout.on('data', d => {
    buf += d
    const m = buf.match(/HERMES_BACKEND_READY port=(\d+)/)
    if (m) resolve(Number(m[1]))
  })
  engine.on('exit', code => reject(new Error(`engine exited ${code}\n${stderr}`)))
  setTimeout(() => reject(new Error('engine did not start')), 120_000)
})

const ws = new WebSocket(`ws://127.0.0.1:${port}/api/ws?token=${token}`)
const events = []
const pending = new Map()
let next = 1
ws.addEventListener('message', m => {
  const f = JSON.parse(String(m.data))
  if (f.method === 'event') events.push(f.params)
  else if (f.id !== undefined && pending.has(f.id)) { pending.get(f.id)(f); pending.delete(f.id) }
  else if (f.method === 'approval') ws.send(JSON.stringify({ jsonrpc: '2.0', id: f.id, result: { choice: 'once' } }))
})
await new Promise((resolve, reject) => { ws.addEventListener('open', resolve); ws.addEventListener('error', reject) })
const rpc = (method, params = {}) => {
  const id = `e${next++}`
  ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
  return new Promise(resolve => pending.set(id, resolve))
}
const turn = async (sid, text) => {
  const from = events.length
  await rpc('prompt.submit', { session_id: sid, text })
  const t0 = Date.now()
  while (Date.now() - t0 < 600_000) {
    if (events.slice(from).some(e => e.session_id === sid && (e.type === 'message.complete' || e.type === 'error'))) return
    await new Promise(r => setTimeout(r, 200))
  }
  throw new Error('turn timed out')
}
const listIds = async () => ((await rpc('session.list', {})).result?.sessions || []).map(s => s.id)
const history = async sid => (await rpc('session.history', { session_id: sid })).result?.messages || []

try {
  const a = (await rpc('session.create', { cwd: home })).result.session_id
  await turn(a, 'Reply with exactly the word ONE.')
  await turn(a, 'Reply with exactly the word TWO.')
  const h0 = await history(a)
  check(h0.filter(m => m.role === 'user').length === 2, `two user turns stored (${h0.length} messages)`)

  const status = (await rpc('session.status', { session_id: a })).result
  check(status?.output?.includes(a), 'session.status reports the session')

  const saved = (await rpc('session.save', { session_id: a })).result
  check(saved?.file && fs.readFileSync(saved.file, 'utf8').includes('TWO'), 'session.save writes the transcript to a file')

  const branch = (await rpc('session.branch', { session_id: a, name: 'Side quest' })).result
  const c = branch?.session_id
  check(c && c !== a && branch.parent === a, 'session.branch returns a new child session')
  check(branch?.messages?.length === h0.length, `branch carries the parent history (${branch?.messages?.length}/${h0.length})`)

  const undo = (await rpc('session.undo', { session_id: a })).result
  const h1 = await history(a)
  check(undo?.removed >= 1 && h1.filter(m => m.role === 'user').length === 1, `session.undo drops the last user turn (removed ${undo?.removed}, ${h1.length} left)`)
  check(!JSON.stringify(h1).includes('TWO'), 'undone turn is gone from history')

  await rpc('session.set_hidden', { session_id: a, hidden: true })
  check(!(await listIds()).includes(a), 'hidden session leaves the list')
  await rpc('session.set_hidden', { session_id: a, hidden: false })
  check((await listIds()).includes(a), 'unhidden session returns to the list')

  const recent = (await rpc('session.most_recent', {})).result
  check(Boolean(recent?.session_id), 'session.most_recent returns a session')

  const closed = (await rpc('session.close', { session_id: a })).result
  check(closed?.closed === true, 'session.close tears down the live link')
  check((await history(a)).length === h1.length, 'closed session is still resumable')

  // Refused while a turn is running.
  const from = events.length
  await rpc('prompt.submit', { session_id: c, text: 'Count slowly from 1 to 40, one number per line.' })
  while (!events.slice(from).some(e => e.session_id === c && e.type === 'message.start')) await new Promise(r => setTimeout(r, 100))
  const busy = await rpc('session.delete', { session_id: c })
  check(Boolean(busy.error), 'session.delete is refused while the session is running')
  await rpc('session.interrupt', { session_id: c })
  await new Promise(r => setTimeout(r, 1500))

  const del = (await rpc('session.delete', { session_id: c })).result
  check(del?.deleted === true, 'session.delete succeeds once idle')
  check(!(await listIds()).includes(c), 'deleted session is gone from the list')
  check(!fs.existsSync(path.join(jcodeHome, 'sessions', `${c}.json`)), 'deleted session file is removed from disk')

  check(!/forward RPC session\./.test(stderr), 'no session method was forwarded to Python')
} catch (err) {
  failures++
  console.error('FAIL', err.message)
} finally {
  ws.close()
  engine.kill('SIGTERM')
  fs.rmSync(home, { recursive: true, force: true })
}
console.log(failures ? `${failures} failure(s)` : 'all session checks passed')
process.exit(failures ? 1 : 0)
