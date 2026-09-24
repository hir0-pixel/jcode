#!/usr/bin/env node
// Live check of the Prime learning loop: a chat with a learning signal gets
// exactly one pass after it goes idle (memories and/or a rule, logged, the
// desktop told); a chat without one costs no learning call. Needs Ollama.
//
//   node crates/sovereign-gateway/e2e/learning.mjs
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const BIN = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const MODEL = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const IDLE_MS = 4000
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-learning-e2e-'))
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
const sleep = ms => new Promise(r => setTimeout(r, ms))

// Default mode (local-idle) must switch learning on for a loopback model.
const env = { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token, SOVEREIGN_LEARN_IDLE_MS: String(IDLE_MS) }
delete env.SOVEREIGN_LEARNING
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
  const id = `l${next++}`
  ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
  return new Promise(resolve => pending.set(id, resolve))
}
const turn = async (sid, text) => {
  const from = events.length
  await rpc('prompt.submit', { session_id: sid, text })
  const t0 = Date.now()
  while (Date.now() - t0 < 600_000) {
    const done = events.slice(from).find(e => e.session_id === sid && (e.type === 'message.complete' || e.type === 'error'))
    if (done) return done
    await sleep(200)
  }
  throw new Error('turn timed out')
}
const logFile = path.join(jcodeHome, 'harness', 'log.jsonl')
const learnLog = sid => (fs.existsSync(logFile) ? fs.readFileSync(logFile, 'utf8').trim().split('\n').filter(Boolean).map(l => JSON.parse(l)) : [])
  .filter(e => e.op === 'learn' && e.session === sid)
const watermark = sid => {
  const f = path.join(jcodeHome, 'harness', 'learned.json')
  return fs.existsSync(f) ? JSON.parse(fs.readFileSync(f, 'utf8'))[sid] : undefined
}

try {
  // 1. No signal: a plain question. The pass must not call the model.
  const plain = (await rpc('session.create', { cwd: home })).result.session_id
  await turn(plain, 'What is 2 plus 2? Reply with just the number.')
  const t0 = Date.now()
  while (watermark(plain) === undefined && Date.now() - t0 < IDLE_MS + 30_000) await sleep(500)
  check(watermark(plain) > 0, `no-signal chat is examined after idle (watermark ${watermark(plain)})`)
  check(learnLog(plain).length === 0, 'no-signal chat costs no learning call')

  // 2. Signal: an explicit "from now on ... remember".
  const taught = (await rpc('session.create', { cwd: home })).result.session_id
  const from = events.length
  await turn(taught, 'From now on, whenever I ask for a quick script, write it in Nim. Please remember that.')
  const t1 = Date.now()
  while (learnLog(taught).length === 0 && Date.now() - t1 < IDLE_MS + 300_000) await sleep(1000)
  const entries = learnLog(taught)
  check(entries.length === 1, `one learning pass for the signal chat (${entries.length})`)
  const e = entries[0] || {}
  check(e.signals?.explicit === true, 'the explicit request was the signal')
  check((e.memories_stored || 0) + (e.rule ? 1 : 0) >= 1, `something was learned (memories ${e.memories_stored}, rule ${JSON.stringify(e.rule)})`)
  const note = events.slice(from).find(ev => ev.type === 'status.update' && ev.session_id === taught && ev.payload?.kind === 'learning')
  check(Boolean(note), `the desktop is told what was learned (${note?.payload?.text})`)

  // 3. No second pass without new messages.
  await sleep(IDLE_MS + 3000)
  check(learnLog(taught).length === 1, 'no repeat pass over already-seen messages')

  // 4. The lesson is usable in a brand-new chat.
  const fresh = (await rpc('session.create', { cwd: home })).result.session_id
  const reply = await turn(fresh, 'Which programming language should you use when you write a quick script for me? Answer with just the language name.')
  const text = String(reply.payload?.text ?? reply.payload?.content ?? '')
  check(/nim/i.test(text), `a new chat applies it (${text.slice(0, 60)})`)
} catch (err) {
  failures++
  console.error('FAIL', err.message)
} finally {
  ws.close()
  engine.kill('SIGTERM')
  if (failures) console.error(stderr.split('\n').filter(l => /learning|sovereign:/.test(l)).slice(-10).join('\n'))
  fs.rmSync(home, { recursive: true, force: true })
}
console.log(failures ? `${failures} failure(s)` : 'all learning checks passed')
process.exit(failures ? 1 : 0)
