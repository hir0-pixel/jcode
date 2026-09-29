#!/usr/bin/env node
// Live check of the M10a Continual Harness /refine engine: /refine creates an
// evidence-backed entry with a rationale, /refine rollback restores the prior
// state, a local entry never leaks into another session while a --global one
// does, and the native learning.* RPCs list it. Needs Ollama.
//
//   node crates/sovereign-gateway/e2e/refine.mjs
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const BIN = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const MODEL = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-refine-e2e-'))
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

const env = { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token }
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
  const id = `r${next++}`
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
const slash = (sid, command) => rpc('slash.exec', { session_id: sid, command })
// The chat model may save the sentence itself with its memory tool (a global memory), so compare counts against a baseline.
const visible = async sid => Number(((await slash(sid, '/refine status')).result?.output || '').match(/^(\d+) entries visible/)?.[1] ?? NaN)
const refineWithEvidence = async (sid, sentence, instruction) => {
  await turn(sid, `Please remember this exact sentence: "${sentence}"`)
  for (let attempt = 0; attempt < 3; attempt++) {
    const result = await slash(sid, `/refine ${instruction}`)
    if (result.result?.status === 'ok') return result
    const output = `${result.result?.output || ''} ${result.error?.message || ''}`
    if (!/rejected:.*(?:evidence|proposal cites no evidence)/i.test(output)) return result
    console.log(`retryable /refine evidence-gate rejection (${attempt + 1}/3): ${output.trim().slice(0, 180)}`)
  }
  return { error: { message: 'evidence gate rejected all 3 attempts' } }
}

try {
  // 1. /refine with instructions creates an evidence-backed entry.
  const sid = (await rpc('session.create', { cwd: home })).result.session_id
  const refined = await refineWithEvidence(sid, 'For quick scripts, use Nim.', 'For quick scripts, use Nim.')
  check(refined.result?.status === 'ok', `/refine succeeded (${JSON.stringify(refined.result)})`)
  const output = refined.result?.output || ''
  const changesetMatch = output.match(/rollback ([0-9a-f-]{36})/)
  check(Boolean(changesetMatch), `/refine reports an undoable changeset id (${output.slice(0, 200)})`)

  const baseline = (await visible(sid)) - 1
  const status1 = await slash(sid, '/refine status')
  check(/entries visible/.test(status1.result?.output || ''), `/refine status lists entries (${status1.result?.output?.slice(0, 80)})`)

  // 2. /refine rollback restores the prior state (the entry disappears).
  if (changesetMatch) {
    const rolled = await slash(sid, `/refine rollback ${changesetMatch[1]}`)
    check(/Rolled back/.test(rolled.result?.output || ''), `/refine rollback restored the prior state (${rolled.result?.output})`)
    const after = await visible(sid)
    check(after === baseline, `entry is gone after rollback (${after} visible, baseline ${baseline})`)
  }

  // 3. A local entry never leaks into another session; --global does.
  const other = (await rpc('session.create', { cwd: home })).result.session_id
  const otherBefore = await visible(other)
  await refineWithEvidence(sid, 'For quick scripts, use Zig.', 'For quick scripts, use Zig.')
  const otherAfter = await visible(other)
  check(otherAfter === otherBefore, `a local /refine entry does not leak into another session (${otherBefore} -> ${otherAfter})`)

  await refineWithEvidence(sid, 'Answer in one short paragraph.', '--global answer in one short paragraph')
  check((await visible(other)) > otherBefore, `a --global /refine entry is visible from another session`)

  // 4. The native Learning RPCs list what /refine created.
  const frames = await rpc('learning.frames', {})
  check(Array.isArray(frames.result?.frames) && frames.result.count > 0, `learning.frames lists harness entries (count ${frames.result?.count})`)
} catch (err) {
  failures++
  console.error('FAIL', err.message)
} finally {
  ws.close()
  engine.kill('SIGTERM')
  if (failures) console.error(stderr.split('\n').filter(l => /refine|harness|sovereign:/.test(l)).slice(-20).join('\n'))
  fs.rmSync(home, { recursive: true, force: true })
}
console.log(failures ? `${failures} failure(s)` : 'all refine checks passed')
process.exit(failures ? 1 : 0)
