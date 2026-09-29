#!/usr/bin/env node
process.env.JCODE_RUNTIME_DIR ||= (await import('node:fs')).mkdtempSync('/tmp/sj-') // short private dir: never collide with a running engine
// Live check of the Prime learning loop (checkpoint gate, turn interval 1): after
// a turn the gate is asked once; when it approves, /refine CRUD stores the lesson
// (a memory lives once in jcode's memory store), the desktop is told, and a
// brand-new chat applies it. Needs Ollama.
//
//   node crates/sovereign-gateway/e2e/learning.mjs
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const BIN = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const MODEL = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-learning-e2e-'))
const jcodeHome = path.join(home, '.jcode')
fs.mkdirSync(jcodeHome, { recursive: true })
fs.writeFileSync(
  path.join(jcodeHome, 'config.toml'),
  `[provider]\ndefault_provider = "local"\n\n[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:11434/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${MODEL}"\n\n[[providers.local.models]]\nid = "${MODEL}"\ncontext_window = 65536\n`
)
const token = crypto.randomBytes(24).toString('hex')
let failures = 0
const check = (cond, msg) => {
  console.log(cond ? 'ok  ' : 'FAIL', msg)
  if (!cond) failures++
}
const sleep = ms => new Promise(r => setTimeout(r, ms))

// Default mode (local-idle) must switch learning on for a loopback model.
const env = { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token, SOVEREIGN_LEARN_TURN_INTERVAL: '1', SOVEREIGN_LEARN_COOLDOWN_MS: '0' }
delete env.SOVEREIGN_LEARNING
const engine = spawn(BIN, ['--provider', 'openai-compatible', '--model', MODEL, 'serve', '--host', '127.0.0.1', '--port', '0'], { env, cwd: home, stdio: ['ignore', 'pipe', 'pipe'] })
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
const learnedNote = sid => events.find(e => e.type === 'status.update' && e.session_id === sid && e.payload?.kind === 'learning' && /^Learned/.test(e.payload?.text || ''))
const gateRuns = async sid => ((await (await fetch(`http://127.0.0.1:${port}/api/sovereign/observability/runs?session=${sid}`, { headers: { Authorization: `Bearer ${token}` } })).json()).runs || []).map(r => r.title)
const graph = async () => (await fetch(`http://127.0.0.1:${port}/api/learning/graph`, { headers: { 'X-Hermes-Session-Token': token } })).json()

try {
  // 1. A plain question: the gate may say no; nothing must break.
  const plain = (await rpc('session.create', { cwd: home })).result.session_id
  const done = await turn(plain, 'What is 2 plus 2? Reply with just the number.')
  check(done.type === 'message.complete', 'a plain chat completes with learning on')

  // 2. An explicit "from now on ... remember" is a durable preference.
  const taught = (await rpc('session.create', { cwd: home })).result.session_id
  await turn(taught, 'From now on, whenever I ask for a quick script, write it in Nim. Please remember that.')
  // The gate is a model call and may decline (the chat model can also save the
  // preference itself with its memory tool): assert it ran, and that if it
  // approved the desktop was told and the lesson stored.
  const t1 = Date.now()
  while (!(await gateRuns(taught)).includes('Auto-refine gate') && Date.now() - t1 < 300_000) await sleep(1000)
  check((await gateRuns(taught)).includes('Auto-refine gate'), 'the checkpoint gate ran after the turn')
  await sleep(90_000)
  const approved = (await gateRuns(taught)).includes('Auto-refine')
  if (approved) check(Boolean(learnedNote(taught)), `the gate approved and the desktop is told what was learned (${learnedNote(taught)?.payload?.text})`)
  const g = await graph()
  const savedByChat = events.some(e => e.type === 'tool.start' && e.session_id === taught && e.payload?.name === 'memory')
  check((g.memory || []).some(m => /nim/i.test(m.body || '')) || savedByChat, `the preference is stored (approved=${approved}, chat memory tool=${savedByChat})`)

  // 3. No repeat pass without new turns beyond the interval: one note per turn at most.
  await sleep(3000)
  check(events.filter(e => e.session_id === taught && e.payload?.kind === 'learning' && /^Learned/.test(e.payload?.text || '')).length <= 1, 'no repeat pass over already-seen messages')

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
