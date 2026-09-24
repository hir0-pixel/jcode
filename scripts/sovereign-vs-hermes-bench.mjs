#!/usr/bin/env node
/**
 * Fair token / RAM benchmark: stock Hermes vs Sovereign.
 *
 * Both backends serve the same desktop JSON-RPC protocol (`hermes serve`,
 * `sovereign serve`), so one WebSocket client drives both exactly like the
 * desktop does: session.create, prompt.submit per turn, approvals answered
 * "once". Every model call goes through scripts/sovereign-counting-proxy.mjs,
 * which logs one JSON line per call (tokens, cache hits, purpose). After each
 * session the backend stays up idle so background calls (titles, memory /
 * skill review) are counted too. Homes are throwaway; the user's real
 * ~/.hermes and ~/.jcode are never touched.
 *
 *   node scripts/sovereign-vs-hermes-bench.mjs            # full: 5 tasks x 3 runs
 *   BENCH_RUNS=1 BENCH_TASKS=plain BENCH_IDLE_MS=20000 node scripts/...  # smoke
 *
 * Output: docs/benchmark-runs/<stamp>/{calls.jsonl, turns.jsonl, rss.jsonl, sessions.jsonl}
 */
import { spawn, spawnSync, execFileSync } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const engineRoot = path.resolve(__dirname, '..')
const MODEL = process.env.BENCH_MODEL || 'sovereign/bench-hermes-64k:latest'
const BASE_MODEL = process.env.BENCH_BASE_MODEL || 'qwen3.8:27b'
const NUM_CTX = Number(process.env.BENCH_NUM_CTX || 65536)
const RUNS = Number(process.env.BENCH_RUNS || 3)
const IDLE_MS = Number(process.env.BENCH_IDLE_MS || 120_000)
const TURN_TIMEOUT_MS = Number(process.env.BENCH_TURN_TIMEOUT_MS || 20 * 60_000)
const PRODUCTS = (process.env.BENCH_PRODUCTS || 'hermes,sovereign').split(',')
const TASK_FILTER = process.env.BENCH_TASKS ? process.env.BENCH_TASKS.split(',') : null
const OLLAMA = process.env.BENCH_OLLAMA || 'http://127.0.0.1:11434'
const PROXY = process.env.BENCH_PROXY || '127.0.0.1:18080'
const HERMES_BIN = process.env.HERMES_BIN || path.join(os.homedir(), '.local/bin/hermes')
const SOVEREIGN_BIN = process.env.SOVEREIGN_BIN || path.join(engineRoot, 'target/release/sovereign')
const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19)
const outDir = process.env.BENCH_OUT || path.join(engineRoot, 'docs/benchmark-runs', stamp)
fs.mkdirSync(outDir, { recursive: true })
const log = (file, row) => fs.appendFileSync(path.join(outDir, file), JSON.stringify(row) + '\n')
const say = (...a) => console.error(new Date().toISOString().slice(11, 19), ...a)
const sleep = ms => new Promise(r => setTimeout(r, ms))

// ---------------------------------------------------------------- tasks
const TASKS = [
  {
    id: 'plain',
    sessions: [['What is the capital of Australia? Answer in one sentence.', 'Roughly how many people live there? One sentence.']],
  },
  {
    id: 'read-file',
    sessions: [[
      "Read notes.txt in the current directory and tell me how many lines mention a deadline.",
      'Which of those deadlines is the earliest? Answer briefly.',
    ]],
  },
  {
    id: 'edit-code',
    sessions: [[
      'In app.py, rename the function compute_total to calculate_total everywhere in the file.',
      'Now add a one-line docstring to calculate_total that says what it returns.',
      'Reply with only the def line of calculate_total as it is now in app.py.',
    ]],
    check: dir => {
      const src = fs.readFileSync(path.join(dir, 'app.py'), 'utf8')
      return !/compute_total/.test(src) && /def calculate_total/.test(src) && /"""|'''/.test(src)
    },
  },
  {
    id: 'shell',
    sessions: [[
      'Run a shell command to list the files in the data directory with their sizes, then tell me which file is the largest.',
      'How many files were in that directory in total?',
    ]],
  },
  {
    id: 'memory',
    // Two separate sessions: the preference must survive into the second.
    sessions: [
      ['For future sessions: my preferred language for quick scripts is Nim. Please remember that.'],
      ['Which programming language should you use when you write a quick script for me? Answer with just the language name.'],
    ],
    checkReply: text => /\bnim\b/i.test(text),
  },
  {
    // Hermes reviews memory every 10 user turns and skills every 10 tool
    // steps by default (agent/agent_init.py), so short sessions never show
    // that cost. This one crosses the 10-turn mark.
    id: 'long-session',
    sessions: [[
      'Read notes.txt and list the tasks in it, one per line.',
      'Which of those tasks has no deadline?',
      'I usually work on billing first. Which task is that?',
      'What is 17 times 23? Just the number.',
      'Read app.py and tell me what compute_total returns, in one sentence.',
      'Is the tax applied before or after rounding? One sentence.',
      'How many functions are defined in app.py? Just the number.',
      'Name the file in the data directory that is a CSV. Just the name.',
      'Summarise our conversation so far in two sentences.',
      'Which deadline in notes.txt is in April? Just the task.',
      'Thanks. Reply with just the word done.',
    ]],
  },
].filter(t => !TASK_FILTER || TASK_FILTER.includes(t.id))

function makeRepo(dir) {
  fs.mkdirSync(path.join(dir, 'data'), { recursive: true })
  fs.writeFileSync(
    path.join(dir, 'notes.txt'),
    [
      'Team notes',
      'Ship the billing export - deadline 14 March',
      'Refactor the login page when there is time',
      'Security review deadline 2 March',
      'Lunch order on Friday',
      'Final deadline for the annual report: 30 April',
      'Remember to water the plants',
    ].join('\n') + '\n'
  )
  fs.writeFileSync(
    path.join(dir, 'app.py'),
    [
      'def compute_total(prices, tax_rate):',
      '    subtotal = sum(prices)',
      '    return round(subtotal * (1 + tax_rate), 2)',
      '',
      '',
      'def invoice(prices):',
      '    total = compute_total(prices, 0.2)',
      '    return f"Total: {total}"',
      '',
      '',
      'if __name__ == "__main__":',
      '    print(compute_total([1.0, 2.5], 0.1))',
      '    print(invoice([3.0]))',
      '',
    ].join('\n')
  )
  fs.writeFileSync(path.join(dir, 'data/small.csv'), 'a,b\n1,2\n')
  fs.writeFileSync(path.join(dir, 'data/medium.log'), 'x'.repeat(4_000))
  fs.writeFileSync(path.join(dir, 'data/large.bin'), Buffer.alloc(64_000, 7))
  spawnSync('git', ['init', '-q'], { cwd: dir })
}

// ---------------------------------------------------------------- processes
function descendants(rootPid) {
  const rows = execFileSync('ps', ['-axo', 'pid=,ppid=,rss=,comm='], { encoding: 'utf8' })
    .trim()
    .split('\n')
    .map(l => l.trim().split(/\s+/))
    .map(([pid, ppid, rss, ...comm]) => ({ pid: +pid, ppid: +ppid, rss: +rss, comm: comm.join(' ') }))
  const keep = new Set([rootPid])
  let grew = true
  while (grew) {
    grew = false
    for (const r of rows) if (keep.has(r.ppid) && !keep.has(r.pid)) { keep.add(r.pid); grew = true }
  }
  const tree = rows.filter(r => keep.has(r.pid))
  const ollama = rows.filter(r => /ollama|llama-server/i.test(r.comm) && !keep.has(r.pid))
  return {
    backend_kb: tree.reduce((s, r) => s + r.rss, 0),
    backend_procs: tree.map(r => path.basename(r.comm)),
    python_kb: tree.filter(r => /python/i.test(r.comm)).reduce((s, r) => s + r.rss, 0),
    ollama_kb: ollama.reduce((s, r) => s + r.rss, 0),
  }
}

async function startBackend(product, home, token) {
  const env = { ...process.env }
  for (const k of Object.keys(env)) if (/API_KEY|TOKEN|SECRET|^HERMES_|^JCODE_|^SOVEREIGN_/.test(k)) delete env[k]
  Object.assign(env, { HOME: home, HERMES_DASHBOARD_SESSION_TOKEN: token })
  let cmd, args
  if (product === 'hermes') {
    const hermesHome = path.join(home, '.hermes')
    fs.mkdirSync(hermesHome, { recursive: true })
    fs.writeFileSync(
      path.join(hermesHome, 'config.yaml'),
      [
        'model:',
        `  default: "${MODEL}"`,
        '  provider: custom',
        `  base_url: "http://${PROXY}/v1"`,
        '  api_key: "ollama"',
        `  context_length: ${NUM_CTX}`,
        `  ollama_num_ctx: ${NUM_CTX}`,
        'terminal:',
        '  backend: local',
        '',
      ].join('\n')
    )
    fs.writeFileSync(path.join(hermesHome, '.env'), 'OPENAI_API_KEY=ollama\n')
    Object.assign(env, { HERMES_HOME: hermesHome, OPENAI_API_KEY: 'ollama' })
    cmd = HERMES_BIN
    args = ['serve', '--host', '127.0.0.1', '--port', '0', '--skip-build']
  } else {
    const jcodeHome = path.join(home, '.jcode')
    fs.mkdirSync(jcodeHome, { recursive: true })
    fs.writeFileSync(
      path.join(jcodeHome, 'config.toml'),
      [
        '[providers.bench]',
        'type = "openai-compatible"',
        `base_url = "http://${PROXY}/v1"`,
        'api_key = "ollama"',
        'requires_api_key = false',
        `default_model = "${MODEL}"`,
        '',
        '[[providers.bench.models]]',
        `id = "${MODEL}"`,
        `context_window = ${NUM_CTX}`,
        '',
      ].join('\n')
    )
    Object.assign(env, { JCODE_HOME: jcodeHome })
    cmd = SOVEREIGN_BIN
    args = ['--provider-profile', 'bench', '--model', MODEL, 'serve', '--host', '127.0.0.1', '--port', '0']
  }
  const child = spawn(cmd, args, { env, cwd: home, stdio: ['ignore', 'pipe', 'pipe'], detached: true })
  const errLog = fs.createWriteStream(path.join(outDir, `${product}-${path.basename(home)}.stderr.log`))
  child.stderr.pipe(errLog)
  const port = await new Promise((resolve, reject) => {
    let buf = ''
    const timer = setTimeout(() => reject(new Error(`${product}: no READY line in 180s`)), 180_000)
    child.stdout.on('data', d => {
      buf += d
      const m = buf.match(/HERMES_BACKEND_READY port=(\d+)/)
      if (m) { clearTimeout(timer); resolve(Number(m[1])) }
    })
    child.on('exit', code => { clearTimeout(timer); reject(new Error(`${product} exited ${code}`)) })
  })
  child.stdout.resume()
  return { child, port }
}

function stopBackend(child) {
  try { process.kill(-child.pid, 'SIGTERM') } catch {}
  return new Promise(resolve => {
    const t = setTimeout(() => { try { process.kill(-child.pid, 'SIGKILL') } catch {} ; resolve() }, 10_000)
    child.on('exit', () => { clearTimeout(t); resolve() })
  })
}

// ---------------------------------------------------------------- client
async function connect(port, token) {
  const ws = new WebSocket(`ws://127.0.0.1:${port}/api/ws?token=${token}`)
  const events = []
  const serverRequests = []
  const pending = new Map()
  let nextId = 1
  ws.addEventListener('message', msg => {
    const frame = JSON.parse(String(msg.data))
    if (frame.method === 'event') events.push({ at: Date.now(), ...frame.params })
    else if (frame.id !== undefined && pending.has(frame.id)) { pending.get(frame.id)(frame); pending.delete(frame.id) }
    else if (frame.method && frame.id !== undefined) {
      serverRequests.push(frame.method)
      const result =
        frame.method === 'approval' ? { choice: 'once' } : frame.method === 'clarify' ? { answer: '' } : { value: null }
      ws.send(JSON.stringify({ jsonrpc: '2.0', id: frame.id, result }))
    }
  })
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true })
    ws.addEventListener('error', () => reject(new Error('websocket failed')), { once: true })
  })
  const rpc = (method, params = {}) => {
    const id = `b${nextId++}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error(`${method} timed out`)), 180_000)
      pending.set(id, f => { clearTimeout(t); resolve(f) })
    })
  }
  return { ws, events, serverRequests, rpc }
}

const replyText = payload =>
  typeof payload?.text === 'string' ? payload.text : typeof payload?.content === 'string' ? payload.content : JSON.stringify(payload ?? '')

async function runTurn(client, sid, text) {
  const from = client.events.length
  const started = Date.now()
  const f = await client.rpc('prompt.submit', { session_id: sid, text })
  if (f.error) return { ok: false, error: f.error.message, ms: Date.now() - started }
  while (Date.now() - started < TURN_TIMEOUT_MS) {
    const done = client.events.slice(from).find(e => e.session_id === sid && (e.type === 'message.complete' || e.type === 'error'))
    if (done) {
      return {
        ok: done.type === 'message.complete',
        ms: Date.now() - started,
        reply: done.type === 'message.complete' ? replyText(done.payload).slice(0, 600) : null,
        error: done.type === 'error' ? replyText(done.payload).slice(0, 300) : null,
      }
    }
    await sleep(250)
  }
  return { ok: false, error: 'turn timeout', ms: Date.now() - started }
}

// ---------------------------------------------------------------- main
async function tagProxy(tag) {
  await fetch(`http://${PROXY}/__tag?tag=${encodeURIComponent(tag)}`)
}

async function runTask(product, task, run) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), `bench-${product}-${task.id}-`))
  const repo = path.join(home, 'repo')
  fs.mkdirSync(repo)
  makeRepo(repo)
  const token = crypto.randomBytes(24).toString('hex')
  const label = `${product}|${task.id}|run${run}`
  await tagProxy(`${label}|startup`)
  const t0 = Date.now()
  const { child, port } = await startBackend(product, home, token)
  const readyMs = Date.now() - t0
  await sleep(3000)
  log('rss.jsonl', { label, phase: 'idle-before-chat', ...descendants(child.pid) })
  let peak = { backend_kb: 0, ollama_kb: 0, python_kb: 0 }
  const sampler = setInterval(() => {
    try {
      const s = descendants(child.pid)
      peak = { backend_kb: Math.max(peak.backend_kb, s.backend_kb), ollama_kb: Math.max(peak.ollama_kb, s.ollama_kb), python_kb: Math.max(peak.python_kb, s.python_kb) }
    } catch {}
  }, 2000)
  const client = await connect(port, token)
  let lastReply = ''
  let turnsOk = true
  try {
    for (const [si, turns] of task.sessions.entries()) {
      const created = await client.rpc('session.create', { cwd: repo })
      if (created.error) throw new Error(`session.create: ${created.error.message}`)
      const sid = created.result.session_id
      for (const [ti, text] of turns.entries()) {
        await tagProxy(`${label}|s${si + 1}t${ti + 1}`)
        say(label, `session ${si + 1} turn ${ti + 1}...`)
        const r = await runTurn(client, sid, text)
        turnsOk &&= r.ok
        lastReply = r.reply || ''
        log('turns.jsonl', { label, product, task: task.id, run, session: si + 1, turn: ti + 1, ...r })
        say(label, `  -> ${r.ok ? 'ok' : 'FAIL ' + r.error} ${(r.ms / 1000).toFixed(1)}s`)
      }
      // Background work (titles, memory/skill review) happens after the turn.
      await tagProxy(`${label}|s${si + 1}idle`)
      await sleep(IDLE_MS)
      log('rss.jsonl', { label, phase: `idle-after-session-${si + 1}`, ...descendants(child.pid) })
    }
  } finally {
    clearInterval(sampler)
    log('rss.jsonl', { label, phase: 'peak', ...peak })
    client.ws.close()
    await tagProxy(`${label}|shutdown`)
    await stopBackend(child)
  }
  const result = {
    label, product, task: task.id, run, ready_ms: readyMs, turns_ok: turnsOk,
    check: task.check ? task.check(repo) : null,
    check_reply: task.checkReply ? task.checkReply(lastReply) : null,
    last_reply: lastReply.slice(0, 300),
    server_requests: client.serverRequests,
    memory_files: listMemory(home),
  }
  log('sessions.jsonl', result)
  fs.rmSync(home, { recursive: true, force: true })
  return result
}

function listMemory(home) {
  const found = []
  const walk = dir => {
    for (const e of fs.existsSync(dir) ? fs.readdirSync(dir, { withFileTypes: true }) : []) {
      const p = path.join(dir, e.name)
      if (e.isDirectory()) walk(p)
      else if (/memor|USER\.md/i.test(p)) {
        const text = fs.readFileSync(p, 'utf8')
        found.push({ file: path.relative(home, p), mentions_nim: /\bnim\b/i.test(text), bytes: text.length })
      }
    }
  }
  walk(path.join(home, '.hermes'))
  walk(path.join(home, '.jcode'))
  return found
}

async function main() {
  const tags = await fetch(`${OLLAMA}/api/tags`).then(r => r.json()).catch(() => null)
  if (!tags) throw new Error(`Ollama not reachable at ${OLLAMA}`)
  if (!tags.models.some(m => m.name === MODEL)) {
    say(`creating ${MODEL} (num_ctx ${NUM_CTX}) from ${BASE_MODEL}`)
    await fetch(`${OLLAMA}/api/create`, { method: 'POST', body: JSON.stringify({ model: MODEL, from: BASE_MODEL, parameters: { num_ctx: NUM_CTX }, stream: false }) })
  }
  // Load once at the benchmark context so neither product pays the load.
  await fetch(`${OLLAMA}/api/generate`, { method: 'POST', body: JSON.stringify({ model: MODEL, prompt: '.', stream: false, keep_alive: -1 }) })
  const proxy = spawn(process.execPath, [path.join(__dirname, 'sovereign-counting-proxy.mjs')], {
    env: {
      ...process.env,
      SOVEREIGN_PROXY_LISTEN: PROXY,
      SOVEREIGN_PROXY_UPSTREAM: OLLAMA,
      SOVEREIGN_PROXY_STATS: path.join(outDir, 'proxy-stats.json'),
      SOVEREIGN_PROXY_CALLS: path.join(outDir, 'calls.jsonl'),
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  })
  await sleep(800)
  fs.writeFileSync(path.join(outDir, 'config.json'), JSON.stringify({ MODEL, BASE_MODEL, NUM_CTX, RUNS, IDLE_MS, PRODUCTS, tasks: TASKS.map(t => t.id), HERMES_BIN, SOVEREIGN_BIN, started: new Date().toISOString() }, null, 2))
  try {
    for (let run = 1; run <= RUNS; run++) {
      // Alternate which product goes first so drift doesn't favour one side.
      const order = run % 2 ? PRODUCTS : [...PRODUCTS].reverse()
      for (const task of TASKS) {
        for (const product of order) {
          try {
            await runTask(product, task, run)
          } catch (err) {
            say('ERROR', product, task.id, run, err.message)
            log('sessions.jsonl', { label: `${product}|${task.id}|run${run}`, product, task: task.id, run, error: err.message })
          }
        }
      }
    }
  } finally {
    proxy.kill('SIGTERM')
    await fetch(`${OLLAMA}/api/generate`, { method: 'POST', body: JSON.stringify({ model: MODEL, keep_alive: 0 }) }).catch(() => {})
  }
  say('done ->', outDir)
}

main().catch(err => {
  console.error(err)
  process.exit(1)
})
