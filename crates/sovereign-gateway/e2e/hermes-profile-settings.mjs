#!/usr/bin/env node
process.env.JCODE_RUNTIME_DIR ||= (await import('node:fs')).mkdtempSync('/tmp/sj-') // short private dir: never collide with a running engine
// Proves --profile settings selected by the Hermes desktop configure Rust chat.
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const bin = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const model = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-hermes-profile-'))
const hermesRoot = path.join(root, '.hermes')
const profileHome = path.join(hermesRoot, 'profiles', 'research')
const home = path.join(root, 'home')
const jcodeHome = path.join(root, '.jcode')
const runtime = fs.mkdtempSync(path.join('/tmp', 'svr-'))
for (const dir of [hermesRoot, profileHome, home, jcodeHome]) fs.mkdirSync(dir, { recursive: true })
fs.writeFileSync(path.join(profileHome, 'config.yaml'), `model:\n  provider: ollama\n  default: ${model}\n`)
fs.writeFileSync(path.join(profileHome, 'SOUL.md'), 'Always include PROFILE_SOUL_APPLIED in your reply.')
const token = crypto.randomBytes(24).toString('hex')
const env = { ...process.env, HOME: home, HERMES_HOME: hermesRoot, JCODE_HOME: jcodeHome, XDG_RUNTIME_DIR: runtime,
  HERMES_DASHBOARD_SESSION_TOKEN: token, PYTHONDONTWRITEBYTECODE: '1' }
delete env.SOVEREIGN_PROVIDER
delete env.SOVEREIGN_MODEL
const child = spawn(bin, ['--profile', 'research', 'serve', '--host', '127.0.0.1', '--port', '0'],
  { cwd: home, env, stdio: ['ignore', 'pipe', 'pipe'] })
let output = ''
child.stdout.on('data', chunk => { output += chunk })
child.stderr.on('data', chunk => { output += chunk })
let ws
try {
  const until = Date.now() + 120_000
  let port
  while (Date.now() < until) {
    port = Number(output.match(/HERMES_BACKEND_READY port=(\d+)/)?.[1]) || 0
    if (port) break
    if (child.exitCode !== null) throw new Error(`engine exited: ${output}`)
    await new Promise(resolve => setTimeout(resolve, 100))
  }
  if (!port) throw new Error(`timed out waiting for engine: ${output}`)
  const base = `http://127.0.0.1:${port}`
  const headers = { Authorization: `Bearer ${token}` }
  const active = await fetch(`${base}/api/profiles/active`, { headers }).then(r => r.json())
  if (active.active !== 'research') throw new Error(`selected profile not reported: ${JSON.stringify(active)}`)
  ws = new WebSocket(`ws://127.0.0.1:${port}/api/ws?token=${token}`)
  const events = []
  const pending = new Map()
  let next = 0
  ws.addEventListener('message', event => {
    const frame = JSON.parse(String(event.data))
    if (frame.method === 'event') events.push(frame.params)
    else if (frame.id !== undefined && pending.has(frame.id)) {
      pending.get(frame.id)(frame)
      pending.delete(frame.id)
    }
  })
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true })
    ws.addEventListener('error', reject, { once: true })
  })
  const rpc = (method, params = {}) => {
    const id = `p${++next}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise(resolve => pending.set(id, resolve))
  }
  const created = await rpc('session.create', { cwd: home })
  const sid = created.result?.session_id
  if (!sid) throw new Error(`session.create failed: ${JSON.stringify(created)}`)
  const info = created.result.info
  if (info.model !== model || info.provider !== 'ollama') {
    throw new Error(`profile model/provider did not reach the session: ${JSON.stringify(info)}`)
  }
  const from = events.length
  const submitted = await rpc('prompt.submit', { session_id: sid, text: 'Say a short greeting.' })
  if (submitted.error) throw new Error(`prompt.submit failed: ${JSON.stringify(submitted)}`)
  const deadline = Date.now() + 180_000
  while (Date.now() < deadline && !events.slice(from).some(e => e.session_id === sid && ['message.complete', 'error'].includes(e.type))) {
    await new Promise(resolve => setTimeout(resolve, 150))
  }
  const history = await rpc('session.history', { session_id: sid })
  const transcript = JSON.stringify(history.result?.messages || [])
  if (!events.slice(from).some(e => e.session_id === sid && e.type === 'message.complete') ||
    !transcript.includes('PROFILE_SOUL_APPLIED')) {
    throw new Error(`profile prompt was not applied: ${transcript}\n${output}`)
  }
  console.log('PASS Hermes --profile selection sets the Rust chat model, provider, and SOUL prompt')
} finally {
  if (ws && ws.readyState < WebSocket.CLOSING) ws.close()
  if (child.exitCode === null) child.kill('SIGTERM')
  await new Promise(resolve => {
    if (child.exitCode !== null) return resolve()
    child.once('exit', resolve)
    setTimeout(() => { child.kill('SIGKILL'); resolve() }, 5000)
  })
  fs.rmSync(runtime, { recursive: true, force: true })
  fs.rmSync(root, { recursive: true, force: true })
}
