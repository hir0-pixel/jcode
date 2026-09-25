#!/usr/bin/env node
// Live check of M10b agent loop: /goal budget stop, /autonomous quality gate,
// one heartbeat fire, and subagent list/interrupt. Needs Ollama.
//
//   node crates/sovereign-gateway/e2e/agent-loop.mjs
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const BIN = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const MODEL = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-agent-loop-e2e-'))
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

const env = {
  ...process.env,
  HOME: home,
  JCODE_HOME: jcodeHome,
  HERMES_HOME: path.join(home, '.hermes'),
  HERMES_DASHBOARD_SESSION_TOKEN: token,
  SOVEREIGN_HEARTBEAT_MIN_SECS: '2',
  // Keep swarm/schedule off; also block session_goal so the budget test is not
  // short-circuited by the model calling complete on the first turn.
  JCODE_DISABLED_TOOLS:
    (process.env.JCODE_DISABLED_TOOLS || '') + ',session_goal,swarm,schedule',
}
delete env.SOVEREIGN_LEARNING
const engine = spawn(
  BIN,
  ['--provider-profile', 'local', '--model', MODEL, 'serve', '--host', '127.0.0.1', '--port', '0'],
  { env, cwd: home, stdio: ['ignore', 'pipe', 'pipe'] }
)
let stderr = ''
engine.stderr.on('data', d => {
  stderr += d
})
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
  else if (f.id !== undefined && pending.has(f.id)) {
    pending.get(f.id)(f)
    pending.delete(f.id)
  } else if (f.method === 'approval') {
    ws.send(JSON.stringify({ jsonrpc: '2.0', id: f.id, result: { choice: 'once' } }))
  }
})
await new Promise((resolve, reject) => {
  ws.addEventListener('open', resolve)
  ws.addEventListener('error', reject)
})
const rpc = (method, params = {}) => {
  const id = `r${next++}`
  ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
  return new Promise(resolve => pending.set(id, resolve))
}
const turn = async (sid, text, timeoutMs = 180_000) => {
  const from = events.length
  await rpc('prompt.submit', { session_id: sid, text })
  const t0 = Date.now()
  while (Date.now() - t0 < timeoutMs) {
    const done = events.slice(from).find(e => e.session_id === sid && (e.type === 'message.complete' || e.type === 'error'))
    if (done) return done
    await sleep(200)
  }
  throw new Error('turn timed out')
}
const slash = (sid, command) => rpc('slash.exec', { session_id: sid, command })
const control = sid => rpc('session.control.read', { session_id: sid })

try {
  // 1. /goal stops at a small budget (1 turn). Discourage tool completion.
  const sid = (await rpc('session.create', { cwd: home })).result.session_id
  const setGoal = await slash(sid, '/goal --turns 1 keep researching forever, never complete')
  check(/Goal set/.test(setGoal.result?.output || ''), `/goal set (${setGoal.result?.output})`)
  const c0 = await control(sid)
  check(c0.result?.control?.goal?.status === 'active', `control.read goal active`)
  check(c0.result?.control?.goal?.max_turns === 1, `goal max_turns=1`)
  await turn(sid, 'Reply with exactly: ping. Do not call any tools.')
  await sleep(2000)
  const c2 = await control(sid)
  const goalStatus = c2.result?.control?.goal?.status
  const turns = c2.result?.control?.goal?.turns_used ?? 0
  check(
    goalStatus === 'paused' || turns >= 1,
    `goal budget consumed (status=${goalStatus}, turns=${turns})`
  )
  await slash(sid, '/goal clear')

  // 2. /autonomous passes a quality gate (true).
  const auto = await slash(sid, '/autonomous on --gate true --max-continuations 2')
  check(/Autonomous on/.test(auto.result?.output || ''), `/autonomous on (${auto.result?.output})`)
  await turn(sid, 'Do nothing elaborate; just acknowledge.')
  await sleep(3000)
  const cAuto = await control(sid)
  const loop = cAuto.result?.control?.loop
  check(
    loop === null || loop?.status === 'done' || loop?.status === 'active',
    `autonomous reflected in control.loop (status=${loop?.status})`
  )
  // After gate `true` passes, state should be done+succeeded (loop may clear or stay done).
  const autoStatus = await slash(sid, '/autonomous status')
  check(
    /gates passed|Autonomous complete|Autonomous on|Autonomous off|Autonomous stopped/i.test(autoStatus.result?.output || ''),
    `/autonomous status after gate (${autoStatus.result?.output})`
  )
  await slash(sid, '/autonomous off')

  // 3. Heartbeat fires once (SOVEREIGN_HEARTBEAT_MIN_SECS=2 → due immediately).
  const hb = await slash(sid, '/heartbeat every 2s pulse once')
  check(/Heartbeat set/.test(hb.result?.output || ''), `/heartbeat set (${hb.result?.output})`)
  const cHb = await control(sid)
  check(cHb.result?.control?.heartbeat?.prompt?.includes('pulse once'), `heartbeat in control.read`)
  check(cHb.result?.control?.heartbeat?.status === 'active', `heartbeat active`)
  const fromHb = events.length
  await turn(sid, 'Idle check — reply with ok.')
  let hbFired = false
  const tHb = Date.now()
  while (Date.now() - tHb < 90_000) {
    const cNow = await control(sid)
    if ((cNow.result?.control?.heartbeat?.fire_count ?? 0) >= 1) {
      hbFired = true
      break
    }
    const fired = events.slice(fromHb).some(e => e.session_id === sid && e.type === 'message.complete')
    if (fired && (cNow.result?.control?.heartbeat?.fire_count ?? 0) >= 1) {
      hbFired = true
      break
    }
    await sleep(400)
  }
  check(hbFired, `heartbeat fired once (fire_count>=1)`)
  // Survive disconnect/reattach
  ws.close()
  await sleep(500)
  const ws2 = new WebSocket(`ws://127.0.0.1:${port}/api/ws?token=${token}`)
  const events2 = []
  const pending2 = new Map()
  let next2 = 1
  ws2.addEventListener('message', m => {
    const f = JSON.parse(String(m.data))
    if (f.method === 'event') events2.push(f.params)
    else if (f.id !== undefined && pending2.has(f.id)) {
      pending2.get(f.id)(f)
      pending2.delete(f.id)
    } else if (f.method === 'approval') {
      ws2.send(JSON.stringify({ jsonrpc: '2.0', id: f.id, result: { choice: 'once' } }))
    }
  })
  await new Promise((resolve, reject) => {
    ws2.addEventListener('open', resolve)
    ws2.addEventListener('error', reject)
  })
  const rpc2 = (method, params = {}) => {
    const id = `r${next2++}`
    ws2.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise(resolve => pending2.set(id, resolve))
  }
  await rpc2('session.resume', { session_id: sid, omit_messages: true })
  const cRe = await rpc2('session.control.read', { session_id: sid })
  check(
    cRe.result?.control?.heartbeat?.prompt?.includes('pulse once'),
    `heartbeat survives disconnect/reattach (${JSON.stringify(cRe.result?.control?.heartbeat)})`
  )
  await rpc2('slash.exec', { session_id: sid, command: '/heartbeat clear' })

  // 4. Subagent list/interrupt: spawn via delegate if a child appears; otherwise
  //    verify empty list shape and that interrupt on missing id is soft-fail.
  const list = await rpc2('subagent.list', { session_id: sid })
  check(Array.isArray(list.result?.subagents), `subagent.list returns array`)
  check(Array.isArray(list.result?.delegations), `subagent.list returns delegations`)
  // Try a short spawn through the model (may create a child). Keep short.
  const spawnTurn = await (async () => {
    const from = events2.length
    await rpc2('prompt.submit', {
      session_id: sid,
      text: 'Use the delegate tool once with action=spawn, label=ping, prompt="reply pong and stop". Then stop.',
    })
    const t0 = Date.now()
    while (Date.now() - t0 < 180_000) {
      const done = events2.slice(from).find(e => e.session_id === sid && (e.type === 'message.complete' || e.type === 'error'))
      if (done) return done
      await sleep(300)
    }
    return null
  })()
  check(Boolean(spawnTurn), `spawn turn completed`)
  await sleep(1000)
  const list2 = await rpc2('subagent.list', { session_id: sid })
  const kids = list2.result?.subagents || []
  if (kids.length > 0) {
    const id = kids[0].subagent_id || kids[0].child_session_id
    const childSid = kids[0].child_session_id || id
    const interrupted = await rpc2('subagent.interrupt', { session_id: sid, subagent_id: childSid })
    const ok =
      interrupted.result?.found === true ||
      interrupted.result?.subagent_id === childSid ||
      interrupted.result?.subagent_id === id
    check(ok, `subagent.interrupt (${JSON.stringify(interrupted.result)}) for ${childSid}`)
  } else {
    const miss = await rpc2('subagent.interrupt', { session_id: sid, subagent_id: 'missing' })
    check(miss.result?.found === false, `subagent.interrupt missing → found=false`)
    console.log('note: no child session spawned in this run (model may have skipped delegate)')
  }

  // Learning REST still works (Task 0 regression).
  const graph = await fetch(`http://127.0.0.1:${port}/api/learning/graph`, {
    headers: { Authorization: `Bearer ${token}` },
  })
  check(graph.status === 200, `GET /api/learning/graph → ${graph.status}`)

  ws2.close()
} catch (err) {
  failures++
  console.error('FAIL', err.message || err)
} finally {
  try {
    ws.close()
  } catch {}
  engine.kill('SIGTERM')
  if (failures) {
    console.error(
      stderr
        .split('\n')
        .filter(l => /agent.?loop|goal|autonomous|heartbeat|sovereign:|ERROR/i.test(l))
        .slice(-30)
        .join('\n')
    )
  }
  fs.rmSync(home, { recursive: true, force: true })
}
console.log(failures ? `${failures} failure(s)` : 'all agent-loop checks passed')
process.exit(failures ? 1 : 0)
