#!/usr/bin/env node
// Prime-learning "trap" check: a repo whose real test convention can only be
// discovered by failing first (`npm test` is a broken stub that names the
// real command, `./scripts/check.sh --fast`). Session A hits the trap and
// waits for a learning pass; session B, in a brand-new chat over a fresh copy
// of the same repo, is given the same instruction. If Prime learned the
// lesson, B should reach the working command with fewer failed tool calls
// than A, and should skip the stub outright most of the time.
//
// Provider-agnostic like scripts/sovereign-vs-hermes-bench.mjs: defaults to a
// local Ollama model, or point BENCH_BASE_URL/BENCH_API_KEY/BENCH_MODEL at any
// OpenAI-compatible endpoint (see docs/BENCHMARK.md).
//
//   node crates/sovereign-gateway/e2e/prime-trap.mjs                 # 5 reps
//   PRIME_TRAP_REPS=1 node crates/sovereign-gateway/e2e/prime-trap.mjs
//   node crates/sovereign-gateway/e2e/prime-trap.mjs --dry-run       # print config + repo, spawn nothing
//
//   BENCH_BASE_URL=https://api.example.com/v1 BENCH_API_KEY=sk-... BENCH_MODEL=my-model \
//   node crates/sovereign-gateway/e2e/prime-trap.mjs
import { spawn, spawnSync } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { resolvePrice, costOf } from '../../../scripts/lib/bench-prices.mjs'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const engineRoot = path.resolve(__dirname, '../../..')
const DRY_RUN = process.argv.includes('--dry-run')

const BIN = process.env.SOVEREIGN_BIN || path.join(engineRoot, 'target/release/sovereign')
const MODEL = process.env.E2E_MODEL || process.env.BENCH_MODEL || 'sovereign/bench-hermes-64k:latest'
const NUM_CTX = Number(process.env.BENCH_CONTEXT || 65536)
const OLLAMA = 'http://127.0.0.1:11434'
const BASE_URL = process.env.BENCH_BASE_URL || OLLAMA
const USING_OLLAMA = !process.env.BENCH_BASE_URL
const API_KEY = process.env.BENCH_API_KEY || 'ollama'
const baseUrlParsed = new URL(BASE_URL)
const UPSTREAM_ORIGIN = `${baseUrlParsed.protocol}//${baseUrlParsed.host}`
const PROXY_PATH = baseUrlParsed.pathname === '/' ? '/v1' : baseUrlParsed.pathname
const IDLE_MS = Number(process.env.SOVEREIGN_LEARN_IDLE_MS || 15_000)
const REPS = Number(process.env.PRIME_TRAP_REPS || 5)
const PRICE = resolvePrice(MODEL)
const sleep = ms => new Promise(r => setTimeout(r, ms))

const TASK_PROMPT = "Run this project's test suite and tell me whether it passes."

/**
 * The trap: `npm test` is a stub that fails and names the real command. This
 * mirrors scripts/sovereign-vs-hermes-bench.mjs's `makeTrapRepo`, kept
 * independent here so this e2e script has no runtime dependency on the bench
 * driver.
 */
function makeTrapRepo(dir) {
  fs.mkdirSync(path.join(dir, 'scripts'), { recursive: true })
  fs.writeFileSync(
    path.join(dir, 'package.json'),
    JSON.stringify(
      { name: 'trap-project', version: '1.0.0', scripts: { test: 'echo "npm test is a broken stub here - use ./scripts/check.sh --fast instead" && exit 1' } },
      null,
      2
    ) + '\n'
  )
  fs.writeFileSync(
    path.join(dir, 'scripts/check.sh'),
    ['#!/bin/sh', 'set -e', 'echo "checking..."', 'test -f package.json', 'echo "all checks passed"', ''].join('\n')
  )
  fs.chmodSync(path.join(dir, 'scripts/check.sh'), 0o755)
  fs.writeFileSync(path.join(dir, 'index.js'), 'module.exports.add = (a, b) => a + b\n')
  spawnSync('git', ['init', '-q'], { cwd: dir })
}

function resolvedConfig() {
  return {
    MODEL, NUM_CTX, REPS, IDLE_MS, BASE_URL, USING_OLLAMA, UPSTREAM_ORIGIN, PROXY_PATH,
    API_KEY: API_KEY === 'ollama' ? 'ollama' : '***',
    price: PRICE,
    task_prompt: TASK_PROMPT,
  }
}

if (DRY_RUN) {
  console.log(JSON.stringify(resolvedConfig(), null, 2))
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'prime-trap-dry-run-'))
  makeTrapRepo(tmp)
  console.log('\nTrap repo layout (' + tmp + '):')
  for (const f of ['package.json', 'scripts/check.sh', 'index.js']) console.log(`--- ${f} ---\n${fs.readFileSync(path.join(tmp, f), 'utf8')}`)
  fs.rmSync(tmp, { recursive: true, force: true })
  process.exit(0)
}

const median = xs => {
  const s = [...xs].sort((a, b) => a - b)
  if (!s.length) return NaN
  const mid = Math.floor(s.length / 2)
  return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2
}

let failures = 0
const check = (cond, msg) => {
  console.log(cond ? 'ok  ' : 'FAIL', msg)
  if (!cond) failures++
  return cond
}

/** One engine + one counting proxy for the lifetime of a single repetition. */
async function withEngine(fn) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-prime-trap-'))
  const jcodeHome = path.join(home, '.jcode')
  fs.mkdirSync(jcodeHome, { recursive: true })
  fs.writeFileSync(
    path.join(jcodeHome, 'config.toml'),
    `[providers.bench]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:18099${PROXY_PATH}"\napi_key = "${API_KEY}"\nrequires_api_key = false\ndefault_model = "${MODEL}"\n\n[[providers.bench.models]]\nid = "${MODEL}"\ncontext_window = ${NUM_CTX}\n`
  )
  const callsFile = path.join(home, 'calls.jsonl')
  const proxy = spawn(process.execPath, [path.join(engineRoot, 'scripts/sovereign-counting-proxy.mjs')], {
    env: { ...process.env, SOVEREIGN_PROXY_LISTEN: '127.0.0.1:18099', SOVEREIGN_PROXY_UPSTREAM: UPSTREAM_ORIGIN, SOVEREIGN_PROXY_CALLS: callsFile },
    stdio: ['ignore', 'ignore', 'pipe'],
  })
  let proxyErr = ''
  proxy.stderr.on('data', d => { proxyErr += d })
  for (let i = 0; i < 100 && !proxyErr.includes('listening on'); i++) await sleep(100)
  if (!proxyErr.includes('listening on')) { proxy.kill('SIGTERM'); throw new Error(`proxy did not start: ${proxyErr}`) }

  const token = crypto.randomBytes(24).toString('hex')
  const env = { ...process.env, HOME: home, JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token, SOVEREIGN_LEARN_IDLE_MS: String(IDLE_MS) }
  delete env.SOVEREIGN_LEARNING // let the default (local-idle) switch learning on for a loopback model
  const engine = spawn(BIN, ['--provider-profile', 'bench', '--model', MODEL, 'serve', '--host', '127.0.0.1', '--port', '0'], { env, cwd: home, stdio: ['ignore', 'pipe', 'pipe'] })
  let stderr = ''
  engine.stderr.on('data', d => { stderr += d })
  try {
    const port = await new Promise((resolve, reject) => {
      let buf = ''
      const timer = setTimeout(() => reject(new Error(`engine did not start: ${stderr}`)), 180_000)
      engine.stdout.on('data', d => {
        buf += d
        const m = buf.match(/HERMES_BACKEND_READY port=(\d+)/)
        if (m) { clearTimeout(timer); resolve(Number(m[1])) }
      })
      engine.on('exit', code => { clearTimeout(timer); reject(new Error(`engine exited ${code}: ${stderr}`)) })
    })
    return await fn({ home, jcodeHome, port, token })
  } finally {
    engine.kill('SIGTERM')
    proxy.kill('SIGTERM')
  }
}

function wsClient(port, token) {
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
  const rpc = (method, params = {}) => {
    const id = `t${next++}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise(resolve => pending.set(id, resolve))
  }
  const turn = async (sid, text, timeoutMs = 600_000) => {
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
  const ready = new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true })
    ws.addEventListener('error', () => reject(new Error('websocket failed')), { once: true })
  })
  return { ws, events, rpc, turn, ready }
}

/** Metrics for one session, from the ledger (see observability.rs's `list`/`detail`):
 * top-level runs are turns, `chat` spans are model calls, `execute_tool`
 * spans are tool calls (a non-'complete' status or an `error` field means the
 * tool call failed - in this repo's case, almost always the `npm test` stub). */
async function sessionMetrics(port, token, sid) {
  const api = async p => {
    const r = await fetch(`http://127.0.0.1:${port}${p}`, { headers: { authorization: `Bearer ${token}` } })
    return r.json()
  }
  const runs = ((await api('/api/sovereign/observability/runs?limit=500')).runs || []).filter(r => r.session_id === sid)
  const totals = { model_calls: 0, tool_calls: 0, failed_tool_calls: 0, prompt_tokens: 0, completion_tokens: 0, cached_tokens: 0, hit_trap: false }
  for (const r of runs) {
    totals.prompt_tokens += r.input_tokens || 0
    totals.completion_tokens += r.output_tokens || 0
    totals.cached_tokens += r.cache_read_tokens || 0
    const detail = await api(`/api/sovereign/observability/run?id=${encodeURIComponent(r.id)}`)
    for (const s of detail.spans || []) {
      if (s.kind === 'chat') totals.model_calls += 1
      if (s.kind === 'execute_tool') {
        totals.tool_calls += 1
        const failed = s.status === 'error' || Boolean(s.error)
        if (failed) {
          totals.failed_tool_calls += 1
          if (/broken stub|npm test/i.test(`${s.name || ''} ${s.error || ''}`)) totals.hit_trap = true
        }
      }
    }
  }
  return totals
}

async function runRepetition(n) {
  return withEngine(async ({ jcodeHome, port, token }) => {
    const client = wsClient(port, token)
    await client.ready
    const results = { a: null, b: null }
    try {
      // --- Session A: cold, in a fresh trap repo ---
      const dirA = fs.mkdtempSync(path.join(os.tmpdir(), `prime-trap-a-${n}-`))
      makeTrapRepo(dirA)
      const a = (await client.rpc('session.create', { cwd: dirA })).result.session_id
      const replyA = await client.turn(a, TASK_PROMPT)
      results.a = { ...(await sessionMetrics(port, token, a)), reply: String(replyA.payload?.text ?? replyA.payload?.content ?? '').slice(0, 200) }
      results.a.cost = costOf(results.a, PRICE)

      // --- Wait for a learning pass over session A ---
      const logFile = path.join(jcodeHome, 'harness', 'log.jsonl')
      const t0 = Date.now()
      const learnLog = () => (fs.existsSync(logFile) ? fs.readFileSync(logFile, 'utf8').trim().split('\n').filter(Boolean).map(l => JSON.parse(l)) : [])
      while (!learnLog().some(e => e.op === 'learn' && e.session === a) && Date.now() - t0 < IDLE_MS + 120_000) await sleep(1000)

      // --- Session B: same instruction, brand-new chat, fresh trap repo ---
      const dirB = fs.mkdtempSync(path.join(os.tmpdir(), `prime-trap-b-${n}-`))
      makeTrapRepo(dirB)
      const b = (await client.rpc('session.create', { cwd: dirB })).result.session_id
      const replyB = await client.turn(b, TASK_PROMPT)
      results.b = { ...(await sessionMetrics(port, token, b)), reply: String(replyB.payload?.text ?? replyB.payload?.content ?? '').slice(0, 200) }
      results.b.cost = costOf(results.b, PRICE)

      fs.rmSync(dirA, { recursive: true, force: true })
      fs.rmSync(dirB, { recursive: true, force: true })
    } finally {
      client.ws.close()
    }
    return results
  })
}

async function main() {
  const reps = []
  for (let n = 1; n <= REPS; n++) {
    console.log(`--- repetition ${n}/${REPS} ---`)
    try {
      reps.push(await runRepetition(n))
    } catch (err) {
      console.error('FAIL repetition', n, err.message)
      failures++
    }
  }
  console.log('\n--- per-repetition results ---')
  for (const [i, r] of reps.entries()) console.log(i + 1, JSON.stringify(r))

  const field = (side, key) => reps.map(r => r[side]?.[key] ?? NaN).filter(Number.isFinite)
  const medA = field('a', 'failed_tool_calls')
  const medB = field('b', 'failed_tool_calls')
  const avoided = reps.filter(r => r.b && !r.b.hit_trap).length

  console.log('\n--- summary ---')
  console.log(`A median failed tool calls: ${median(medA)} (n=${medA.length})`)
  console.log(`B median failed tool calls: ${median(medB)} (n=${medB.length})`)
  console.log(`B avoided the trap in ${avoided}/${reps.length} runs`)
  console.log(`A median model calls: ${median(field('a', 'model_calls'))}, B: ${median(field('b', 'model_calls'))}`)
  console.log(`A median tool calls: ${median(field('a', 'tool_calls'))}, B: ${median(field('b', 'tool_calls'))}`)
  console.log(`A median prompt tokens: ${median(field('a', 'prompt_tokens'))}, B: ${median(field('b', 'prompt_tokens'))}`)
  console.log(`A median dummy cost: ${median(field('a', 'cost'))}, B: ${median(field('b', 'cost'))}`)

  check(reps.length === REPS, `all ${REPS} repetitions completed without error`)
  check(median(medB) < median(medA), `B's median failed tool calls (${median(medB)}) is below A's (${median(medA)})`)
  check(avoided >= 3, `B avoided the trap in at least 3 of ${reps.length} runs (got ${avoided})`)

  console.log(failures ? `\n${failures} failure(s)` : '\nall prime-trap checks passed')
  process.exit(failures ? 1 : 0)
}

main().catch(err => {
  console.error(err)
  process.exit(1)
})
