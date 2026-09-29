#!/usr/bin/env node
// Cron end to end with no desktop: the engine's timer wakes an idle-stopped Hermes Python backend
// for one recurring job, the job's model turn runs on the engine (/api/agent/run), the run ledger
// and the observability ledger record it, the per-job toolset policy reaches the model request,
// and Python idle-stops afterwards. Needs local Ollama and the Hermes venv.
//
//   node crates/sovereign-gateway/e2e/cron-headless.mjs
import { spawn, execFileSync } from 'node:child_process'
import fs from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'
import { DatabaseSync } from 'node:sqlite'

const root = path.resolve(import.meta.dirname, '../../..')
const BIN = process.env.SOVEREIGN_BIN || path.join(root, 'target/release/sovereign')
const MODEL = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const HERMES_BIN = path.resolve(root, '../hermes-agent/.venv/bin/hermes')
const sleep = ms => new Promise(r => setTimeout(r, ms))
let failures = 0
const check = (cond, msg) => { console.log(cond ? 'ok  ' : 'FAIL', msg); if (!cond) failures++ }

const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-cron-headless-'))
const jcodeHome = path.join(home, '.jcode')
const hermesHome = path.join(home, '.hermes')
fs.mkdirSync(jcodeHome, { recursive: true })
fs.mkdirSync(hermesHome, { recursive: true })

// Records every model request's tool names, so the job's toolset policy is checked where it bites.
const modelCalls = []
const proxy = http.createServer((req, res) => {
  const chunks = []
  req.on('data', c => chunks.push(c))
  req.on('end', () => {
    const body = Buffer.concat(chunks)
    try {
      const j = JSON.parse(body)
      const last = [...(j.messages || [])].reverse().find(m => m.role === 'user')
      const defs = (j.tools || []).map(t => t.function || t)
      modelCalls.push({ tools: defs.map(t => t.name), catalog: JSON.stringify(defs.find(t => t.name === 'load_tools') || ''), user: JSON.stringify(last?.content ?? '') })
    } catch {}
    const up = http.request({ host: '127.0.0.1', port: 11434, path: req.url, method: req.method, headers: req.headers }, r => { res.writeHead(r.statusCode, r.headers); r.pipe(res) })
    up.on('error', () => res.destroy())
    up.end(body)
  })
})
await new Promise(r => proxy.listen(0, '127.0.0.1', r))
const proxyPort = proxy.address().port
fs.writeFileSync(
  path.join(jcodeHome, 'config.toml'),
  `[provider]\ndefault_provider = "local"\n\n[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:${proxyPort}/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${MODEL}"\n\n[[providers.local.models]]\nid = "${MODEL}"\ncontext_window = 65536\n`
)

// SOVEREIGN_HERMES_CMD is split on whitespace, so it needs a space-free path: a wrapper in the temp home.
const HERMES = path.join(home, 'hermes')
fs.writeFileSync(HERMES, `#!/bin/sh\nexec "${HERMES_BIN}" "$@"\n`, { mode: 0o755 })

const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^(HERMES_|JCODE_|SOVEREIGN_)|API_KEY|TOKEN|SECRET/.test(k)))
Object.assign(env, {
  HOME: home, JCODE_HOME: jcodeHome, HERMES_HOME: hermesHome,
  JCODE_RUNTIME_DIR: fs.mkdtempSync('/tmp/sj-'),
  SOVEREIGN_HERMES_CMD: HERMES,
  SOVEREIGN_FEATURE_IDLE_MS: '3000',
  SOVEREIGN_TRACE_FEATURE_ACTIVITY: '1',
})
const engine = spawn(BIN, ['--provider', 'openai-compatible', '--model', MODEL, 'serve', '--host', '127.0.0.1', '--port', '0'], { env, cwd: home, stdio: ['ignore', 'pipe', 'pipe'] })
const trace = [] // {t, line} from the engine's stderr
let stderr = ''
engine.stderr.on('data', d => {
  stderr += d
  for (const line of String(d).split('\n').filter(Boolean)) trace.push({ t: Date.now(), line })
})
const cleanup = () => { engine.kill('SIGTERM'); proxy.close(); proxy.closeAllConnections?.(); fs.rmSync(home, { recursive: true, force: true }); fs.rmSync(env.JCODE_RUNTIME_DIR, { recursive: true, force: true }) }

// Descendants of the engine that are the Hermes backend.
const pythonUp = () => {
  const rows = execFileSync('/bin/ps', ['-axo', 'pid=,ppid=,command=', '-ww'], { encoding: 'utf8' }).trim().split('\n')
    .map(l => l.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/)).filter(Boolean).map(m => ({ pid: +m[1], ppid: +m[2], cmd: m[3] }))
  const owned = new Set([engine.pid])
  for (let grew = true; grew;) { grew = false; for (const r of rows) if (owned.has(r.ppid) && !owned.has(r.pid)) { owned.add(r.pid); grew = true } }
  return rows.some(r => owned.has(r.pid) && r.pid !== engine.pid && /hermes/.test(r.cmd))
}

try {
  const port = await new Promise((resolve, reject) => {
    let buf = ''
    engine.stdout.on('data', d => { buf += d; const m = buf.match(/HERMES_BACKEND_READY port=(\d+)/); if (m) resolve(Number(m[1])) })
    engine.on('exit', code => reject(new Error(`engine exited ${code}\n${stderr}`)))
    setTimeout(() => reject(new Error('engine did not start')), 120_000)
  })
  const token = fs.readFileSync(path.join(jcodeHome, 'sovereign-gateway.token'), 'utf8').trim()
  const api = async (route, opts = {}) => {
    const res = await fetch(`http://127.0.0.1:${port}${route}`, { ...opts, headers: { Authorization: `Bearer ${token}`, ...(opts.body ? { 'Content-Type': 'application/json' } : {}) } })
    const text = await res.text()
    let body; try { body = JSON.parse(text) } catch { body = text }
    return { status: res.status, body }
  }
  const jobsFile = path.join(hermesHome, 'cron', 'jobs.json')
  const readJob = id => { try { const raw = JSON.parse(fs.readFileSync(jobsFile, 'utf8')); return (Array.isArray(raw) ? raw : raw.jobs).find(j => j.id === id) } catch { return undefined } }

  // Baseline: a plain agent run offers the self-scheduling and shell tools, so their absence below means something.
  const base = await api('/api/agent/run', { method: 'POST', body: JSON.stringify({ prompt: 'Reply with the word ready.', cwd: home, timeout_s: 300 }) })
  check(base.status === 200 && base.body.ok, `baseline agent run ok (${JSON.stringify(base.body).slice(0, 120)})`)
  const baseTools = modelCalls.at(-1)?.tools || []
  console.log('baseline tools:', baseTools.join(','))
  const baseCatalog = modelCalls.at(-1)?.catalog || ''
  check(['bash', 'read', 'load_tools'].every(t => baseTools.includes(t)) && /heartbeat/.test(baseCatalog) && /session_goal/.test(baseCatalog), 'baseline offers bash, read and (via load_tools) heartbeat/session_goal')

  // Create the recurring job through Hermes's own cron API, via the engine's proxy.
  const marker = `cron-marker-${Math.random().toString(36).slice(2, 10)}`
  const name = `headless-${marker}`
  const created = await api('/api/cron/jobs', {
    method: 'POST',
    body: JSON.stringify({ name, schedule: 'every 1m', deliver: 'local', enabled_toolsets: ['file'], prompt: `Reply with exactly this text and nothing else: ${marker}` }),
  })
  check(created.status === 200 && created.body.id, `job created (${created.status} ${JSON.stringify(created.body).slice(0, 160)})`)
  const id = created.body.id
  const dueAt = Date.parse(created.body.next_run_at)
  check(dueAt - Date.now() > 20_000 && dueAt - Date.now() < 90_000, `next_run_at about 60 s ahead (${Math.round((dueAt - Date.now()) / 1000)} s)`)
  const created_at = Date.now()

  // Python idle-stops before the job is due, so the tick must be started by the engine's timer.
  let stopped = false
  while (Date.now() < dueAt - 8_000) { if (!pythonUp()) { stopped = true; break } await sleep(500) }
  check(stopped, `Python idle-stopped before the job was due (${Math.round((Date.now() - created_at) / 1000)} s after create)`)
  const startsBefore = trace.filter(e => e.line.includes('backend-start-or-reuse')).length

  // Wait (up to 5 min) for the run to finish; note whether Python was seen up during the tick.
  let sawPython = false, job
  const deadline = Math.max(Date.now(), dueAt) + 300_000 - 60_000
  while (Date.now() < deadline) {
    sawPython ||= pythonUp()
    job = readJob(id)
    if (job?.last_status) break
    await sleep(500)
  }
  check(job?.last_status === 'ok', `job last_status ok (${job?.last_status} ${job?.last_error || ''})`)
  check(sawPython, 'Python was running during the tick')
  const starts = trace.filter(e => e.line.includes('backend-start-or-reuse'))
  check(starts.length > startsBefore && starts.some(e => e.t >= dueAt - 5_000), 'the timer started Python for the tick at the due time (feature_activity trace)')
  check(!/cron tick failed/.test(stderr), 'no "cron tick failed" from the engine timer')
  check(Math.abs(Date.parse(job?.last_run_at) - dueAt) < 90_000, `last_run_at within 90 s of next_run_at (${job?.last_run_at})`)

  // The model turn went to the engine as a cron run, and its reply carries the marker.
  const dbFile = ['sovereign.db', 'observability.sqlite3'].map(f => path.join(jcodeHome, f)).find(f => fs.existsSync(f))
  let row
  for (let i = 0; i < 50 && !row; i++) {
    const db = new DatabaseSync(dbFile, { readOnly: true })
    row = db.prepare("SELECT id, kind, title, status, outcome FROM fact_turn WHERE kind='cron' AND title=?").get(name)
    db.close()
    if (!row) await sleep(200)
  }
  check(row && row.status !== 'error' && row.status !== 'spawned', `observability fact_turn row kind=cron for the job (${JSON.stringify(row)})`)
  const outDir = path.join(hermesHome, 'cron', 'output')
  const docs = fs.existsSync(outDir) ? fs.readdirSync(outDir, { recursive: true }).map(f => path.join(outDir, String(f))).filter(f => fs.statSync(f).isFile()) : []
  const reply = docs.map(f => fs.readFileSync(f, 'utf8').split('## Response')[1] || '').join('\n')
  check(reply.includes(marker), `the reply carries the marker (${JSON.stringify(reply.trim().slice(0, 100))})`)

  // Per-job toolset policy: enabled_toolsets ["file"] plus Hermes's cron denylist (cronjob, messaging, clarify).
  const call = modelCalls.find(c => c.user.includes(marker))
  const tools = call?.tools || []
  check(tools.includes('read') && !tools.includes('bash'), `job allowlist applied: has read, no bash (${tools.join(',')})`)
  check(!/heartbeat|session_goal|send_message/.test(call?.catalog || '') && !tools.some(t => /heartbeat|session_goal|send_message/.test(t)), 'scheduling and messaging tools absent from the cron session')

  // Hermes's run ledger, then remove the job so it does not fire again.
  const runs = await api(`/api/cron/jobs/${id}/runs`)
  check(runs.status === 200 && JSON.stringify(runs.body).includes('ok') && !/"error"/.test(JSON.stringify(runs.body).replace(/"error":null/g, '')), `run ledger lists the run (${JSON.stringify(runs.body).slice(0, 200)})`)
  const del = await api(`/api/cron/jobs/${id}`, { method: 'DELETE' })
  check(del.status === 200, `job deleted (${del.status})`)

  // Idle stop after the lease is released (SOVEREIGN_FEATURE_IDLE_MS=3000).
  const idleFrom = Date.now()
  let idle = false
  while (Date.now() < idleFrom + 30_000) { if (!pythonUp()) { idle = true; break } await sleep(500) }
  check(idle, `Python idle-stopped after the tick (${Math.round((Date.now() - idleFrom) / 1000)} s)`)
  check(trace.some(e => e.line.includes('lease-released')) && trace.some(e => e.line.includes('feature_idle decision=stop')), 'lease released and idle stop decided (trace)')
} catch (err) {
  failures++
  console.error('FAIL', err.stack || err.message, err.cause || '')
} finally {
  if (failures) try { console.error(fs.readFileSync(path.join(jcodeHome, 'logs/hermes-backend.log'), 'utf8').split('\n').slice(-25).join('\n')) } catch {}
  if (failures) console.error(stderr.split('\n').slice(-25).join('\n'))
  cleanup()
}
console.log(failures ? `${failures} failure(s)` : 'all cron-headless checks passed')
process.exit(failures ? 1 : 0)
