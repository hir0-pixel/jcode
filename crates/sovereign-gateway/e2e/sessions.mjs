#!/usr/bin/env node
// Live check that every chat-bound session method is answered by the engine
// (never forwarded to Hermes's Python backend) and really changes the
// engine's store. Needs Ollama with the model below.
//
//   node crates/sovereign-gateway/e2e/sessions.mjs
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import { execFileSync } from 'node:child_process'
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
const waitTurn = async (sid, from) => {
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
  const firstFrom = events.length
  await rpc('prompt.submit', { session_id: a, text: 'Use the bash tool to run `sleep 3`, then reply with exactly the word ONE.' })
  let active
  for (let i = 0; i < 100; i++) {
    active = (await rpc('session.active_list', { current_session_id: a })).result
    if (active?.sessions?.some(s => s.id === a && s.current)) break
    await new Promise(r => setTimeout(r, 150))
  }
  check(active?.sessions?.some(s => s.id === a && s.current), `session.active_list reads live engine sessions (${JSON.stringify(active)})`)
  await waitTurn(a, firstFrom)
  await turn(a, 'Reply with exactly the word TWO.')
  const h0 = await history(a)
  check(h0.filter(m => m.role === 'user').length === 2, `two user turns stored (${h0.length} messages)`)

  const status = (await rpc('session.status', { session_id: a })).result
  check(status?.output?.includes(a), 'session.status reports the session')
  const context = (await rpc('session.context_breakdown', { session_id: a })).result
  check(context?.context_estimated === true && context.context_used >= 0, 'session.context_breakdown reports an explicit engine transcript estimate')
  const replay = (await rpc('session.events.since', { session_id: a, last_seen: 0 })).result
  check(typeof replay?.latest_seq === 'number' && replay.epoch, 'session.events.since returns a cursor and stable gateway epoch')
  const replayStats = (await rpc('session.events.stats', {})).result
  check(typeof replayStats?.sessions === 'number' && typeof replayStats?.events === 'number', 'session.events.stats reports the replay ledger')
  const expectedCwd = fs.realpathSync(home)
  const changedCwd = (await rpc('session.cwd.set', { session_id: a, cwd: home })).result
  check(changedCwd?.cwd === expectedCwd, `session.cwd.set persists the session working directory (${JSON.stringify(changedCwd)})`)
  const moved = (await rpc('session.workspace.move', { session_key: a, cwd: home })).result
  check(moved?.cwd === expectedCwd, `session.workspace.move updates the engine session cwd (${JSON.stringify(moved)})`)
  const foreign = (await rpc('session.foreign.list', { source: 'claude', limit: 10 })).result
  check(Array.isArray(foreign?.sessions), 'session.foreign.list uses the engine importer')

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

  // Undo after a tool turn: history carries tool rows that jcode's rewind
  // does not count, so positions must be taken among user/assistant only.
  const t = (await rpc('session.create', { cwd: home })).result.session_id
  await turn(t, 'Use the bash tool to run exactly: echo tool-ran . Then reply with exactly the word DONE.')
  await turn(t, 'Reply with exactly the word FOUR.')
  const tu = (await rpc('session.undo', { session_id: t })).result
  const th = await history(t)
  check(!JSON.stringify(th).includes('FOUR') && th.filter(m => m.role === 'user').length === 1, `undo after a tool turn removes only the last turn (removed ${tu?.removed}, ${th.length} left)`)

  // REST surface the desktop sidebar uses (/api/sessions/*).
  const http = (method, p, body, withToken = true) =>
    fetch(`http://127.0.0.1:${port}${p}`, {
      method,
      headers: { ...(withToken ? { authorization: `Bearer ${token}` } : {}), 'content-type': 'application/json' },
      body: body ? JSON.stringify(body) : undefined,
    }).then(async r => ({ status: r.status, body: await r.json().catch(() => null) }))
  await turn(a, 'Reply with exactly the word THREE.')
  const msgs = await http('GET', `/api/sessions/${a}/messages`)
  check(msgs.status === 200 && JSON.stringify(msgs.body.messages).includes('THREE'), 'REST transcript includes the newest turn (snapshot + journal)')
  const page = await http('GET', `/api/sessions/${a}/messages?limit=1`)
  check(page.body?.messages?.length === 1 && JSON.stringify(page.body.messages).includes('THREE'), 'REST latest page of 1 is the newest message')
  const renamed = await http('PATCH', `/api/sessions/${a}`, { title: 'Renamed via REST' })
  check(renamed.status === 200, 'REST rename accepted')
  const got = await http('GET', `/api/sessions/${a}`)
  check(got.body?.title === 'Renamed via REST', `REST get shows the new title (${got.body?.title})`)
  const exported = await http('GET', `/api/sessions/${a}/export`)
  const imported = await http('POST', '/api/sessions/import', { sessions: [exported.body] })
  check(exported.status === 200 && imported.status === 200 && imported.body?.skipped === 1,
    'REST export envelope round-trips through the engine importer')
  const found = await http('GET', `/api/sessions/search?q=three`)
  check(found.body?.results?.some(r => r.session_id === a), 'REST search finds the session by message text')
  const sessionRoutes = [
    ['GET', '/api/sessions/stats'],
    ['GET', '/api/sessions/empty/count'],
    ['DELETE', '/api/sessions/empty'],
    ['POST', '/api/sessions/bulk-delete', { ids: [] }],
    ['POST', '/api/sessions/import', { sessions: [] }],
    ['POST', '/api/sessions/prune', { dry_run: true }],
    ['GET', `/api/sessions/${a}/latest-descendant`],
    ['GET', `/api/sessions/${a}/export`],
  ]
  const curl = (method, route, body, withToken) => {
    const args = ['-sS', '-X', method, '-H', 'content-type: application/json', '-w', '\n%{http_code}']
    if (withToken) args.push('-H', `authorization: Bearer ${token}`)
    if (body) args.push('--data-binary', JSON.stringify(body))
    args.push(`http://127.0.0.1:${port}${route}`)
    const result = execFileSync('curl', args, { encoding: 'utf8' })
    const split = result.lastIndexOf('\n')
    return { status: Number(result.slice(split + 1)), body: JSON.parse(result.slice(0, split) || 'null') }
  }
  for (const [method, route, body] of sessionRoutes) {
    const missingToken = curl(method, route, body, false)
    const authenticated = curl(method, route, body, true)
    check(missingToken.status === 401, `${method} ${route} rejects a missing token`)
    check(authenticated.status >= 200 && authenticated.status < 300, `${method} ${route} is served with a token (${authenticated.status})`)
  }
  await http('PATCH', `/api/sessions/${a}`, { archived: true })
  check(!(await listIds()).includes(a), 'REST archive hides the session')
  await http('PATCH', `/api/sessions/${a}`, { archived: false })
  check((await listIds()).includes(a), 'REST unarchive restores it')
  const gone = await http('DELETE', `/api/sessions/${a}`)
  check(gone.status === 200 && !(await listIds()).includes(a), 'REST delete removes the session')
  const usage = await http('GET', `/api/analytics/usage?days=7`)
  check(usage.status === 200 && usage.body?.totals?.total_sessions >= 1 && usage.body?.totals?.total_api_calls >= 1, `usage chart comes from the engine ledger (${usage.body?.totals?.total_api_calls} calls)`)
  const insights = (await rpc('insights.get', { days: 7 })).result
  check(insights?.sessions >= 1 && insights?.messages >= 2, `insights.get counts engine chats (${insights?.sessions} sessions, ${insights?.messages} messages)`)
  const bars = (await rpc('usage.bars', {})).result
  check(bars?.available === false, 'usage.bars answers unavailable without Python')
  const handoff = await rpc('handoff.request', { session_id: a, platform: 'telegram' })
  check(Boolean(handoff.error), 'handoff is refused until messaging runs on the engine')
  check(!/forward RPC (session|insights|usage)\./.test(stderr), 'no chat-bound method was forwarded to Python')
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
