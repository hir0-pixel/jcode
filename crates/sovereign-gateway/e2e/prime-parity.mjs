#!/usr/bin/env node
// Live smoke for the Prime REPL bridge and installed Python-package skills.
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const bin = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const model = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-prime-parity-'))
const jcode = path.join(home, '.jcode')
fs.writeFileSync(path.join(home, 'prime-input.txt'), 'prime-parity chunk probe')
fs.mkdirSync(path.join(jcode, 'skills', 'parity_probe', 'src', 'parity_probe'), { recursive: true })
fs.writeFileSync(path.join(jcode, 'skills', 'parity_probe', 'SKILL.md'), '---\nname: parity-probe\ndescription: e2e test package\n---\n')
fs.writeFileSync(path.join(jcode, 'skills', 'parity_probe', 'src', 'parity_probe', '__init__.py'), 'def marker():\n    return "prime-parity-import-ok"\n')
fs.writeFileSync(path.join(jcode, 'config.toml'), `[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:11434/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${model}"\n\n[[providers.local.models]]\nid = "${model}"\ncontext_window = 65536\n`)
const token = crypto.randomBytes(24).toString('hex')
const env = { ...process.env, HOME: home, JCODE_HOME: jcode, HERMES_HOME: path.join(home, '.hermes'), SOVEREIGN_HERMES_PYTHON: process.env.SOVEREIGN_HERMES_PYTHON || path.resolve('../hermes-agent/.venv/bin/python'), HERMES_DASHBOARD_SESSION_TOKEN: token }
delete env.SOVEREIGN_LEARNING
const engine = spawn(bin, ['--provider', 'ollama', '--model', model, 'serve', '--host', '127.0.0.1', '--port', '0'], { cwd: home, env, stdio: ['ignore', 'pipe', 'pipe'] })
let stderr = ''
engine.stderr.on('data', chunk => { stderr += chunk })
let ws
try {
  let out = ''
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`engine startup timeout\n${stderr}`)), 120_000)
    engine.stdout.on('data', chunk => {
      out += chunk
      const match = out.match(/HERMES_BACKEND_READY port=(\d+)/)
      if (match) { clearTimeout(timer); resolve(Number(match[1])) }
    })
    engine.once('exit', code => { clearTimeout(timer); reject(new Error(`engine exited ${code}\n${stderr}`)) })
  })
  const wsUrl = `ws://127.0.0.1:${port}/api/ws?token=${token}`
  ws = new WebSocket(wsUrl)
  const events = []
  const pending = new Map()
  let id = 0
  ws.addEventListener('message', event => {
    const frame = JSON.parse(String(event.data))
    if (frame.method === 'event') events.push(frame.params)
    else if (frame.id !== undefined && pending.has(frame.id)) { pending.get(frame.id)(frame); pending.delete(frame.id) }
    else if (frame.method === 'approval') ws.send(JSON.stringify({ jsonrpc: '2.0', id: frame.id, result: { choice: 'once' } }))
  })
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve); ws.addEventListener('error', reject) })
  const rpc = (method, params = {}) => {
    const requestId = `p${++id}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id: requestId, method, params }))
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`${method} timed out`)), 120_000)
      pending.set(requestId, frame => { clearTimeout(timer); resolve(frame) })
    })
  }
  const session = (await rpc('session.create', { cwd: home })).result?.session_id
  if (!session) throw new Error('session.create returned no id')
  await rpc('slash.exec', { session_id: session, command: '/heartbeat every 1h keep the session alive' })
  const runRepl = async code => {
    const from = events.length
    const submitted = await rpc('prompt.submit', { session_id: session, text: `Use the repl tool once and run this Python code exactly. Do not use any other tool.\n\n\`\`\`python\n${code}\n\`\`\`` })
    if (submitted.error) throw new Error(`prompt.submit failed: ${JSON.stringify(submitted.error)}`)
    const deadline = Date.now() + 180_000
    while (Date.now() < deadline && !events.slice(from).some(e => e.session_id === session && ['message.complete', 'error'].includes(e.type))) await new Promise(resolve => setTimeout(resolve, 200))
    const complete = events.slice(from).find(e => e.session_id === session && e.type === 'tool.complete' && e.payload.name === 'repl')
    if (!complete) throw new Error(`model did not complete a REPL call: ${JSON.stringify(events.slice(from).map(e => e.type + ':' + (e.payload?.text || ''))).slice(0, 1200)}`)
    return complete.payload.result_text || ''
  }
  const dataResult = await runRepl(String.raw`import json,re,parity_probe
value=int(re.search(r"(\d+)","v=42").group(1))
chunk=await load("prime-input.txt")
answer=await llm_query("Reply exactly CHUNK_OK: "+chunk)
print(json.dumps({"marker":parity_probe.marker(),"value":value,"chunk":chunk,"llm":answer}))`)
  if (!dataResult.includes('prime-parity-import-ok') || !dataResult.includes('prime-parity chunk probe') || !dataResult.includes('CHUNK_OK')) throw new Error(`REPL imports/load/llm_query failed: ${dataResult}`)
  console.log('ok CPython imports, package use, load, and llm_query')
  const goalResult = await runRepl(String.raw`import json,goal
trial=await goal.create("exercise Python goal package",token_budget=50000)
finished=await goal.complete()
active=await goal.create("keep the long task alive until I reconnect")
assert trial["goal"]["token_budget"]==50000 and finished["goal"]["status"]=="done"
print(json.dumps({"goal":active["goal"]["objective"],"remaining":active["remaining_tokens"]}))`)
  if (!goalResult.includes('keep the long task alive until I reconnect')) throw new Error(`goal skill failed: ${goalResult}`)
  console.log('ok Python goal get/create/complete, including token budget')
  const heartbeatResult = await runRepl(String.raw`import json,rlm_heartbeat
h=await rlm_heartbeat.create("keep checking the long task",interval="1h",label="reattach",delivery_mode="follow_up")
items=await rlm_heartbeat.list()
assert any(x["id"]==h["id"] and x["label"]=="reattach" for x in items["heartbeats"])
print(json.dumps(h))`)
  if (!heartbeatResult.includes('reattach')) throw new Error(`RLM heartbeat skill failed: ${heartbeatResult}`)
  console.log('ok Python heartbeat create/list')
  const refineResult = await runRepl(String.raw`import json,refine
r=await refine.run("preserve local test evidence")
s=await refine.status()
assert r["scheduled"] and s["pending"]
print(json.dumps({"refine_pending":s["pending"]}))`)
  if (!refineResult.includes('"refine_pending": true')) throw new Error(`refine skill failed: ${refineResult}`)
  console.log('ok Python refine scheduling/status')
  const agentResult = await runRepl(String.raw`import json,agent_observe,agent_message
child=await spawn_subagent("Review ten source files and return ten concise findings. Continue until done.","parity-child")
roster=await agent_observe.list_agents()
observed=await agent_observe.get_agent(child["session_id"])
receipt=await agent_message.send("parity message",receiver_role="child",receiver_name="parity-child")
wait=await await_subagent(child["session_id"],timeout=1)
assert observed["agent"]["sessionId"]==child["session_id"]
print(json.dumps({"child":child,"observed":observed["agent"]["sessionId"],"roster":len(roster["agents"]),"receipt":receipt,"await":wait}))`)
  if (!agentResult.includes('"deliveryStatus": "queued"') || !agentResult.includes('"status": "running"') || !agentResult.includes('"observed":')) throw new Error(`agent messaging/spawn/await failed: ${agentResult}`)
  console.log('ok agent observe/message and subagent spawn/await')
  const compactResult = await runRepl(String.raw`import json,compact
result=await compact.run("keep the selected model and task objective")
assert result["scheduled"]
print(json.dumps(result))`)
  if (!compactResult.includes('"phase": "end_of_turn"')) throw new Error(`compact skill failed: ${compactResult}`)
  console.log('ok compact scheduling')
  ws.close()
  await new Promise(resolve => setTimeout(resolve, 300))
  ws = new WebSocket(wsUrl)
  const pendingResume = new Map()
  ws.addEventListener('message', event => {
    const frame = JSON.parse(String(event.data))
    if (frame.id !== undefined && pendingResume.has(frame.id)) { pendingResume.get(frame.id)(frame); pendingResume.delete(frame.id) }
    else if (frame.method === 'approval') ws.send(JSON.stringify({ jsonrpc: '2.0', id: frame.id, result: { choice: 'once' } }))
  })
  await new Promise((resolve, reject) => { ws.addEventListener('open', resolve); ws.addEventListener('error', reject) })
  const reconnectRpc = (method, params = {}) => {
    const requestId = `r${++id}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id: requestId, method, params }))
    return new Promise(resolve => pendingResume.set(requestId, resolve))
  }
  await reconnectRpc('session.resume', { session_id: session, omit_messages: true })
  const state = (await reconnectRpc('session.control.read', { session_id: session })).result?.control
  const swarm = (await reconnectRpc('subagent.list', { session_id: session })).result
  const persisted = state?.goal?.status === 'active' && state?.heartbeat?.status === 'active'
  const childVisible = (swarm?.subagents || []).some(item =>
    item.subagent_id === 'parity-child' && item.status === 'running'
  )
  console.log(`${persisted && childVisible ? 'ok' : 'FAIL'} goal, heartbeat, and spawned agent survive desktop disconnect/reattach`)
  if (!persisted || !childVisible) { console.error(JSON.stringify({ state, swarm }, null, 2)); process.exitCode = 1 }
} catch (error) {
  console.error(error.stack || error)
  process.exitCode = 1
} finally {
  ws?.close()
  engine.kill('SIGTERM')
  if (engine.exitCode === null && engine.signalCode === null) {
    await new Promise(resolve => {
      engine.once('exit', resolve)
      setTimeout(resolve, 5000).unref()
    })
  }
  for (let attempt = 0; attempt < 5; attempt++) {
    try { fs.rmSync(home, { recursive: true, force: true }); break }
    catch (error) { if (attempt === 4 || error.code !== 'ENOTEMPTY') throw error; await new Promise(resolve => setTimeout(resolve, 100)) }
  }
}
