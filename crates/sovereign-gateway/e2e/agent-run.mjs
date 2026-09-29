#!/usr/bin/env node
process.env.JCODE_RUNTIME_DIR ||= (await import('node:fs')).mkdtempSync('/tmp/sj-') // short private dir: never collide with a running engine
// Live check of #M4 `/api/agent/run`: a headless prompt runs to completion and
// returns text, its session never shows up in session.list, and a shell
// command needing approval is denied outright — no approval frame reaches a
// connected desktop client, and no file gets written. Needs Ollama.
//
//   node crates/sovereign-gateway/e2e/agent-run.mjs
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const BIN = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const MODEL = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-agent-run-e2e-'))
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

const env = { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token }
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

// A connected "desktop" client: any approval frame arriving here for a
// headless run would be exactly the bug this test guards against.
const approvals = []
const ws = new WebSocket(`ws://127.0.0.1:${port}/api/ws?token=${token}`)
ws.addEventListener('message', m => {
  const f = JSON.parse(String(m.data))
  if (f.method === 'approval') approvals.push(f)
})
await new Promise((resolve, reject) => { ws.addEventListener('open', resolve); ws.addEventListener('error', reject) })

const api = async (route, opts = {}) => {
  const res = await fetch(`http://127.0.0.1:${port}${route}`, {
    ...opts,
    headers: { Authorization: `Bearer ${token}`, ...(opts.body ? { 'Content-Type': 'application/json' } : {}), ...opts.headers },
  })
  return { status: res.status, body: await res.json() }
}

try {
  // 1. A plain prompt runs to completion and returns non-empty text.
  const { status, body } = await api('/api/agent/run', {
    method: 'POST',
    body: JSON.stringify({ prompt: 'What is 2 plus 2? Reply with just the number.', cwd: home, timeout_s: 300 }),
  })
  check(status === 200, `agent.run responds 200 (${status})`)
  check(body.ok === true, `agent.run reports ok (${JSON.stringify(body)})`)
  check(typeof body.text === 'string' && body.text.trim().length > 0, `agent.run returns non-empty text (${JSON.stringify(body.text)})`)
  check(typeof body.session_id === 'string' && body.session_id.length > 0, 'agent.run reports a session_id')

  // 2. That session never shows up in session.list (hidden the same way an
  // archived chat is).
  const list = await api('/api/sessions?limit=1000')
  check(!list.body.sessions?.some(s => s.id === body.session_id), 'the agent.run session is absent from session.list')

  // 3. A prompt that tries a risky shell command (a recursive delete, which
  // jcode's own risk classifier always sends to approval) is denied outright:
  // no blocking approval prompt reaches the connected desktop client, and the file
  // this would have destroyed survives.
  const marker = path.join(home, 'agent-run-marker.txt')
  fs.writeFileSync(marker, 'do not delete me')
  const before = approvals.length
  const denied = await api('/api/agent/run', {
    method: 'POST',
    body: JSON.stringify({
      prompt: `Use your shell/bash tool to run exactly this command: rm -rf ${marker}`,
      cwd: home,
      timeout_s: 300,
    }),
  })
  check(denied.status === 200, `denied run still responds 200 (${denied.status})`)
  check(fs.existsSync(marker), 'the shell command was denied: the file was not deleted')
  // A denied unattended command is parked for a late answer: the desktop may be shown it, but nothing waits on it.
  check(approvals.slice(before).every(a => a.params?.unattended === true), `no blocking approval frame reached the desktop client (${approvals.length - before} parked frame(s))`)
} catch (err) {
  failures++
  console.error('FAIL', err.message)
} finally {
  ws.close()
  engine.kill('SIGTERM')
  if (failures) console.error(stderr.split('\n').filter(l => /sovereign:/.test(l)).slice(-10).join('\n'))
  fs.rmSync(home, { recursive: true, force: true })
}
console.log(failures ? `${failures} failure(s)` : 'all agent.run checks passed')
process.exit(failures ? 1 : 0)
