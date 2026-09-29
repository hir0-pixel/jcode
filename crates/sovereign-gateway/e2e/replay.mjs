#!/usr/bin/env node
process.env.JCODE_RUNTIME_DIR ||= (await import('node:fs')).mkdtempSync('/tmp/sj-') // short private dir: never collide with a running engine
// Replay a completed local-model turn and verify its trace is complete and linked.
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const root = path.resolve(import.meta.dirname, '../../..')
const bin = process.env.SOVEREIGN_BIN || path.join(root, 'target/release/sovereign')
const model = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-replay-e2e-'))
const jcodeHome = path.join(home, '.jcode')
const token = crypto.randomBytes(24).toString('hex')
fs.mkdirSync(jcodeHome)
fs.writeFileSync(path.join(jcodeHome, 'config.toml'), `[provider]\ndefault_provider = "local"\n\n[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:11434/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${model}"\n\n[[providers.local.models]]\nid = "${model}"\ncontext_window = 65536\n`)
fs.writeFileSync(path.join(jcodeHome, 'observability.json'), JSON.stringify({ capture_content: true }))
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
let engine
let ws
let failures = 0
const check = (ok, message) => {
  console.log(ok ? 'ok  ' : 'FAIL', message)
  if (!ok) failures++
}

try {
  engine = spawn(bin, ['--provider', 'openai-compatible', '--model', model, 'serve', '--host', '127.0.0.1', '--port', '0'], {
    cwd: home,
    env: { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token },
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
    const body = await response.json()
    if (!response.ok) throw new Error(`${route}: ${response.status} ${JSON.stringify(body)}`)
    return body
  }
  ws = new WebSocket(`ws://127.0.0.1:${port}/api/ws?token=${token}`)
  const events = []
  const pending = new Map()
  let next = 0
  ws.addEventListener('message', message => {
    const frame = JSON.parse(String(message.data))
    if (frame.method === 'event') events.push(frame.params)
    else if (frame.id !== undefined && pending.has(frame.id)) { pending.get(frame.id)(frame); pending.delete(frame.id) }
    else if (frame.method === 'approval') ws.send(JSON.stringify({ jsonrpc: '2.0', id: frame.id, result: { choice: 'once' } }))
  })
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve); ws.addEventListener('error', reject) })
  const rpc = (method, params = {}) => {
    const id = `r${++next}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise(resolve => pending.set(id, resolve))
  }
  const turn = async (session, prompt) => {
    const from = events.length
    const submitted = await rpc('prompt.submit', { session_id: session, text: prompt })
    if (submitted.error) throw new Error(JSON.stringify(submitted.error))
    for (let i = 0; i < 3000; i++) {
      const done = events.slice(from).find(event => event.session_id === session && event.type === 'message.complete')
      if (done?.payload?.status === 'complete') return done
      if (done) throw new Error(`original turn ended ${done.payload?.status}`)
      await sleep(200)
    }
    throw new Error('original turn timed out')
  }

  const session = (await rpc('session.create', { cwd: home })).result.session_id
  await turn(session, 'Reply with only the word replay-ready.')
  const runsBefore = await api('/api/sovereign/observability/runs?limit=100')
  const original = runsBefore.runs.find(run => run.session_id === session)
  if (!original) throw new Error(`original run was not recorded: ${JSON.stringify(runsBefore)}`)
  const baselineIds = new Set(runsBefore.runs.map(run => run.id))

  const replay = await api('/api/sovereign/observability/replay', { method: 'POST', body: JSON.stringify({ run_id: original.id }) })
  const started = Date.now()
  let detail
  while (Date.now() - started < 600_000) {
    try { detail = await api(`/api/sovereign/observability/run?id=${encodeURIComponent(replay.run_id)}`) } catch {}
    if (detail?.run?.status === 'complete') break
    if (detail?.run?.status === 'failed') throw new Error(`replay failed: ${JSON.stringify(detail.run)}`)
    await sleep(250)
  }
  const runsAfter = await api('/api/sovereign/observability/runs?limit=100')
  const newRuns = runsAfter.runs.filter(run => !baselineIds.has(run.id))
  check(detail?.run?.status === 'complete', `replay finished (${detail?.run?.status})`)
  check(detail?.run?.replay_of === original.id && replay.replay_of === original.id, 'replay_of links to original run')
  check(detail?.spans?.some(span => span.kind === 'chat' || span.kind === 'tool_followup'), 'replay contains model-call spans')
  check(newRuns.length === 1 && newRuns[0].id === replay.run_id, `exactly one new run was recorded (${newRuns.length})`)
  if (failures) console.error(stderr)
} catch (error) {
  failures++
  console.error('FAIL', error.message)
} finally {
  ws?.close()
  engine?.kill('SIGTERM')
  engine?.stdout.destroy()
  engine?.stderr.destroy()
  if (!process.env.KEEP_REPLAY_HOME) fs.rmSync(home, { recursive: true, force: true })
  else console.log(`replay home: ${home}`)
}
console.log(failures ? `${failures} failure(s)` : 'all replay checks passed')
process.exit(failures ? 1 : 0)
