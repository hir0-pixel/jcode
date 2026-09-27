#!/usr/bin/env node
// Live API check for Activity run history and the learning data behind the star map.
// Uses one local model turn; the star-map fixture is inserted into the isolated DB.
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

const root = path.resolve(import.meta.dirname, '../../..')
const bin = process.env.SOVEREIGN_BIN || path.join(root, 'target/release/sovereign')
const model = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-activity-learning-'))
const jcodeHome = path.join(home, '.jcode')
const runtimeDir = path.join(os.tmpdir(), `sov-act-${process.pid}-${crypto.randomBytes(3).toString('hex')}`)
const token = crypto.randomBytes(24).toString('hex')
fs.mkdirSync(jcodeHome)
fs.mkdirSync(runtimeDir)
fs.writeFileSync(path.join(jcodeHome, 'config.toml'), `[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:11434/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${model}"\n\n[[providers.local.models]]\nid = "${model}"\ncontext_window = 65536\n`)

let engine
let ws
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const check = (condition, message) => { if (!condition) throw new Error(message) }
try {
  engine = spawn(bin, ['--provider-profile', 'local', '--model', model, 'serve', '--host', '127.0.0.1', '--port', '0'], {
    cwd: home,
    env: { ...process.env, HOME: home, HERMES_HOME: path.join(home, '.hermes'), JCODE_HOME: jcodeHome, JCODE_RUNTIME_DIR: runtimeDir, HERMES_DASHBOARD_SESSION_TOKEN: token },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  let stderr = ''
  engine.stderr.on('data', chunk => { stderr += chunk })
  const port = await new Promise((resolve, reject) => {
    let out = ''
    const timer = setTimeout(() => reject(new Error(`engine did not start: ${stderr}`)), 120_000)
    engine.stdout.on('data', chunk => {
      out += chunk
      const ready = out.match(/HERMES_BACKEND_READY port=(\d+)/)
      if (ready) { clearTimeout(timer); resolve(Number(ready[1])) }
    })
    engine.once('exit', code => { clearTimeout(timer); reject(new Error(`engine exited ${code}: ${stderr}`)) })
  })
  const base = `http://127.0.0.1:${port}`
  const headers = { Authorization: `Bearer ${token}`, 'content-type': 'application/json' }
  const api = async (route, options = {}) => {
    const response = await fetch(`${base}${route}`, { ...options, headers })
    const body = await response.json()
    if (!response.ok) throw new Error(`${options.method || 'GET'} ${route}: ${response.status} ${JSON.stringify(body)}`)
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
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve, { once: true }); ws.addEventListener('error', reject, { once: true }) })
  const rpc = (method, params = {}) => {
    const id = `al${++next}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise(resolve => pending.set(id, resolve))
  }
  const created = await rpc('session.create', { cwd: home })
  const session = created.result?.session_id
  check(session, `session.create failed: ${JSON.stringify(created)}`)
  const from = events.length
  const submitted = await rpc('prompt.submit', { session_id: session, text: 'Reply with only the word activity-ready.' })
  check(!submitted.error, `prompt.submit failed: ${JSON.stringify(submitted.error)}`)
  let completed
  const turnDeadline = Date.now() + 600_000
  while (Date.now() < turnDeadline && !completed) {
    completed = events.slice(from).find(event => event.session_id === session && ['message.complete', 'error'].includes(event.type))
    if (!completed) await sleep(200)
  }
  check(completed?.type === 'message.complete', `chat turn failed: ${JSON.stringify(completed)}\n${stderr}`)

  let run
  const runDeadline = Date.now() + 10_000
  while (Date.now() < runDeadline && !run) {
    const listing = await api('/api/sovereign/observability/runs?limit=100')
    run = listing.runs?.find(item => item.session_id === session)
    if (!run) await sleep(100)
  }
  const completeDeadline = Date.now() + 10_000
  while (run && run.status === 'running' && Date.now() < completeDeadline) {
    await sleep(100)
    const listing = await api('/api/sovereign/observability/runs?limit=100')
    run = listing.runs?.find(item => item.id === run.id) || run
  }
  check(run?.status === 'complete', `Activity run list omitted the completed session: ${JSON.stringify(run)}`)
  const detail = await api(`/api/sovereign/observability/run?id=${encodeURIComponent(run.id)}`)
  check(detail.run?.id === run.id && detail.spans?.some(span => span.kind === 'chat'), 'Activity detail did not contain its chat span')
  const approvals = await api('/api/sovereign/observability/approvals?limit=20')
  check(Array.isArray(approvals.approvals), 'Activity approvals endpoint did not return an array')

  // Seed a scoped, isolated fixture using the same schema as EntryStore. The public API
  // below is what the desktop star map uses for listing, inspection, editing, and removal.
  let graph = await api('/api/learning/graph')
  const db = new DatabaseSync(path.join(jcodeHome, 'sovereign.db'))
  const id = crypto.randomUUID()
  const now = Date.now()
  const seq = db.prepare('SELECT COALESCE(MAX(seq), 0) + 1 AS next FROM harness_entries').get().next
  db.prepare(`INSERT INTO harness_entries
    (id, kind, title, content, path, scope, session, reference, arguments, metadata, source, created_at_ms, updated_at_ms, version, seq)
    VALUES (?, 'skill', ?, ?, '', 'global', NULL, '{}', '{}', '{}', 'activity-learning-e2e', ?, ?, 1, ?)`).run(
    id, 'Activity e2e fixture', 'Keep the fixture until the star-map deletion check.', now, now, seq,
  )
  db.close()

  graph = await api('/api/learning/graph')
  check(graph.nodes?.some(node => node.id === id && node.kind === 'skill'), 'star-map graph omitted the seeded learning node')
  const nodeRoute = `/api/learning/node?id=${encodeURIComponent(id)}`
  const node = await api(nodeRoute)
  check(node.ok && node.kind === 'skill' && node.content.includes('fixture'), 'star-map node detail did not return its content')
  const edited = await api('/api/learning/node', { method: 'PUT', body: JSON.stringify({ id, content: 'Edited through the star-map API.' }) })
  check(edited.ok, 'star-map edit did not succeed')
  check((await api(nodeRoute)).content === 'Edited through the star-map API.', 'star-map edit was not persisted')
  const deleted = await api('/api/learning/node', { method: 'DELETE', body: JSON.stringify({ id }) })
  check(deleted.ok, 'star-map delete did not succeed')
  graph = await api('/api/learning/graph')
  check(!graph.nodes?.some(item => item.id === id), 'deleted node remained in the star-map graph')
  const missing = await fetch(`${base}${nodeRoute}`, { headers })
  check(missing.status === 404, `deleted star-map node returned ${missing.status}, expected 404`)
  console.log('PASS Activity lists a completed chat run, its chat span, and approvals')
  console.log('PASS star-map graph, node detail, edit, and delete APIs')
} finally {
  ws?.close()
  engine?.kill('SIGTERM')
  engine?.stdout.destroy()
  engine?.stderr.destroy()
  fs.rmSync(runtimeDir, { recursive: true, force: true })
  if (!process.env.KEEP_ACTIVITY_LEARNING_HOME) fs.rmSync(home, { recursive: true, force: true })
  else console.log(`activity-learning home: ${home}`)
}
