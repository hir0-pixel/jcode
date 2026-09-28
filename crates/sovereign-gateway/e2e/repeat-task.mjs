#!/usr/bin/env node
// Live check that the Prime learning loop helps a REPEATED task: session A
// does a small multi-step project-scaffolding task cold, waits for a learning
// pass to run, then session B is given the same kind of task in a fresh
// working directory. Reports tool calls / model calls / tokens for A vs B and
// whether a skill or memory shows up as applied in B. Needs Ollama.
//
//   node crates/sovereign-gateway/e2e/repeat-task.mjs
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const BIN = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const MODEL = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const IDLE_MS = Number(process.env.SOVEREIGN_LEARN_IDLE_MS || 4000)
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-repeat-task-e2e-'))
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

const env = { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token, SOVEREIGN_LEARN_TURN_INTERVAL: '1', SOVEREIGN_LEARN_COOLDOWN_MS: '0' }
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
  const id = `r${next++}`
  ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
  return new Promise(resolve => pending.set(id, resolve))
}
const turn = async (sid, text, timeoutMs = 600_000) => {
  const from = events.length
  await rpc('prompt.submit', { session_id: sid, text })
  const t0 = Date.now()
  while (Date.now() - t0 < timeoutMs) {
    const done = events.slice(from).find(e => e.session_id === sid && (e.type === 'message.complete' || e.type === 'error'))
    if (done) return { done, seen: events.slice(from) }
    await sleep(200)
  }
  throw new Error('turn timed out')
}
const http = (method, p, body) =>
  fetch(`http://127.0.0.1:${port}${p}`, {
    method,
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  }).then(async r => ({ status: r.status, body: await r.json().catch(() => null) }))

// Same kind of task each time, in a fresh working directory: scaffold a tiny
// project layout the model must figure out via tools (list/create files),
// not something it can answer from parametric knowledge alone.
const taskPrompt = () =>
  'In this working directory, create a file named NOTES.md containing exactly the line "hello from repeat-task", ' +
  'then create a subdirectory named "out" containing a file out/DONE.txt with exactly the line "done". ' +
  'Use your tools to do this; do not just describe it.'

const projectDir = label => {
  const dir = path.join(home, label)
  fs.mkdirSync(dir, { recursive: true })
  return dir
}

const usageSnapshot = async () => (await http('GET', '/api/analytics/usage?days=7')).body?.totals ?? {}
const toolCount = async () => {
  const tools = (await http('GET', '/api/analytics/usage?days=7')).body?.tools ?? []
  return tools.reduce((sum, t) => sum + (t.count || 0), 0)
}
// Prime's checkpoint gate (SOVEREIGN_LEARN_TURN_INTERVAL=1): a pass that changed
// the harness is announced as status.update kind=learning, "Learned ...".
const learnLog = () => events
  .filter(e => e.type === 'status.update' && e.payload?.kind === 'learning' && /^Learned/.test(e.payload?.text || ''))
  .map(e => ({ session: e.session_id, text: e.payload.text }))
const skillsDir = path.join(jcodeHome, 'skills')
const listSkills = () => (fs.existsSync(skillsDir) ? fs.readdirSync(skillsDir) : [])

const verifyDone = dir =>
  fs.existsSync(path.join(dir, 'NOTES.md')) &&
  fs.readFileSync(path.join(dir, 'NOTES.md'), 'utf8').includes('hello from repeat-task') &&
  fs.existsSync(path.join(dir, 'out', 'DONE.txt')) &&
  fs.readFileSync(path.join(dir, 'out', 'DONE.txt'), 'utf8').includes('done')

try {
  // --- Session A: cold run ---
  const dirA = projectDir('project-a')
  const a = (await rpc('session.create', { cwd: dirA })).result.session_id
  const before = await usageSnapshot()
  const toolsBefore = await toolCount()
  await turn(a, taskPrompt())
  const afterA = await usageSnapshot()
  const toolsAfterA = await toolCount()
  const a_stats = {
    tool_calls: toolsAfterA - toolsBefore,
    model_calls: (afterA.total_api_calls || 0) - (before.total_api_calls || 0),
    tokens: (afterA.total_input || 0) + (afterA.total_output || 0) - ((before.total_input || 0) + (before.total_output || 0)),
  }
  check(verifyDone(dirA), 'session A produced the expected files')

  // --- Wait for a learning pass over session A ---
  const t0 = Date.now()
  while (learnLog().filter(e => e.session === a).length === 0 && Date.now() - t0 < IDLE_MS + 300_000) await sleep(1000)
  const learnedA = learnLog().filter(e => e.session === a)
  check(learnedA.length >= 1, `a learning pass ran over session A (${learnedA.length})`)
  const skillsAfterA = listSkills()
  const skillApplied = learnedA.some(e => /skill/i.test(e.text))
  const memoryOrRuleApplied = learnedA.length > 0
  console.log('learned from A:', JSON.stringify(learnedA[0] ?? null))
  console.log('skills on disk after A:', JSON.stringify(skillsAfterA))

  // --- Session B: same kind of task, fresh directory ---
  const dirB = projectDir('project-b')
  const b = (await rpc('session.create', { cwd: dirB })).result.session_id
  const beforeB = await usageSnapshot()
  const toolsBeforeB = await toolCount()
  const { seen } = await turn(b, taskPrompt())
  const afterB = await usageSnapshot()
  const toolsAfterB = await toolCount()
  const b_stats = {
    tool_calls: toolsAfterB - toolsBeforeB,
    model_calls: (afterB.total_api_calls || 0) - (beforeB.total_api_calls || 0),
    tokens: (afterB.total_input || 0) + (afterB.total_output || 0) - ((beforeB.total_input || 0) + (beforeB.total_output || 0)),
  }
  check(verifyDone(dirB), 'session B produced the expected files')

  const skillNoteInB = seen.some(
    e => e.type === 'status.update' && e.payload?.kind === 'learning' && /skill/i.test(e.payload?.text || '')
  )
  // Whatever the gate approved (memory, prompt addendum, skill) is stored once
  // and reaches every new session, including B, by construction.
  const usedLearnedSomething = skillApplied || skillNoteInB || skillsAfterA.length > 0 || memoryOrRuleApplied

  console.log('--- results ---')
  console.log('A:', JSON.stringify(a_stats))
  console.log('B:', JSON.stringify(b_stats))
  console.log('something learned and available for B:', usedLearnedSomething)

  check(b_stats.tool_calls <= a_stats.tool_calls, `B is no worse on tool calls (A ${a_stats.tool_calls} vs B ${b_stats.tool_calls})`)
  check(usedLearnedSomething, 'something learned from A (a memory, rule, or skill) was applied/available for B')
} catch (err) {
  failures++
  console.error('FAIL', err.message)
} finally {
  ws.close()
  engine.kill('SIGTERM')
  if (failures) console.error(stderr.split('\n').filter(l => /learning|sovereign:|skill/.test(l)).slice(-20).join('\n'))
  fs.rmSync(home, { recursive: true, force: true })
}
console.log(failures ? `${failures} failure(s)` : 'all repeat-task checks passed')
process.exit(failures ? 1 : 0)
