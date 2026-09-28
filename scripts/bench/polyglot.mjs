#!/usr/bin/env node
/**
 * Aider-polyglot-exercises driver: stock Hermes vs Sovereign, same model.
 *
 * For each exercise in scripts/bench/polyglot-exercises.json (a fixed-seed
 * selection of 40 exercises across python/rust/cpp - see
 * select_polyglot_exercises.py for why java/javascript/go are excluded):
 *
 *   1. Copy the exercise to a throwaway sandbox.
 *   2. Give the agent the instructions + current stub file(s), mirroring
 *      Aider's own benchmark prompt style ("implement the stubs, don't touch
 *      the tests"). One turn.
 *   3. Run the language's OWN test command independently (never through the
 *      agent) and record pass/fail.
 *   4. On failure, ONE retry: a second turn carrying the test output,
 *      identical in shape for both arms. Re-run the tests independently.
 *
 * Both arms:
 *   - go through the shared counting proxy (scripts/sovereign-counting-proxy.mjs)
 *     for token/cache/cost/wall metrics, exactly like scripts/bench/abeval.py
 *     and scripts/sovereign-vs-hermes-bench.mjs.
 *   - run every exercise through ONE persistent engine home for the whole
 *     arm-run (HERMES_HOME for hermes, JCODE_HOME + one long-lived `sovereign
 *     serve` process for sovereign), so Prime/memory learning can carry over
 *     between exercises, same as a real user's session.
 *   - run the exercises in the SAME fixed order (the order they appear in
 *     polyglot-exercises.json).
 *
 * Usage:
 *   node scripts/bench/polyglot.mjs --dry-run
 *   node scripts/bench/polyglot.mjs run --arm hermes
 *   node scripts/bench/polyglot.mjs run --arm sovereign
 *   node scripts/bench/polyglot.mjs report
 *
 * Environment: same BENCH_* variables as scripts/bench/abeval.py and
 * scripts/sovereign-vs-hermes-bench.mjs (BENCH_BASE_URL, BENCH_MODEL,
 * BENCH_API_KEY, BENCH_CONTEXT, BENCH_PROXY, BENCH_OUT, BENCH_TURN_TIMEOUT_MS,
 * HERMES_VENV_PY, SOVEREIGN_BIN).
 *
 * This script only BUILDS/verifies via --dry-run; it never starts Ollama or
 * any model server itself.
 */
import { spawn, spawnSync, execFileSync } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { resolvePrice, costOf } from '../lib/bench-prices.mjs'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const engineRoot = path.resolve(__dirname, '..', '..')
const polyglotRoot = path.resolve(engineRoot, '..', 'benchmarks', 'polyglot-benchmark')
const DRY_RUN = process.argv.includes('--dry-run')
const args = process.argv.slice(2)
const cmd = args.find(a => !a.startsWith('--')) || null
const flag = (name, def) => {
  const i = args.indexOf(`--${name}`)
  return i === -1 ? def : args[i + 1]
}

const cfg = {
  MODEL: process.env.BENCH_MODEL || 'sovereign/bench-hermes-64k:latest',
  NUM_CTX: Number(process.env.BENCH_CONTEXT || process.env.BENCH_NUM_CTX || 65536),
  API_KEY: process.env.BENCH_API_KEY || 'ollama',
  BASE_URL: process.env.BENCH_BASE_URL || 'http://127.0.0.1:11434',
  PROXY: process.env.BENCH_PROXY || '127.0.0.1:18080',
  TURN_TIMEOUT_MS: Number(process.env.BENCH_TURN_TIMEOUT_MS || 10 * 60_000),
  OUT: process.env.BENCH_OUT || path.join(engineRoot, 'bench-results', 'polyglot'),
  HERMES_VENV_PY: process.env.HERMES_VENV_PY || path.join(engineRoot, '..', 'hermes-agent', '.venv', 'bin', 'python3'),
  SOVEREIGN_BIN: process.env.SOVEREIGN_BIN || path.join(engineRoot, 'target', 'release', 'sovereign'),
  PRICE_TABLE: process.env.SOVEREIGN_PRICE_TABLE || path.join(engineRoot, 'scripts', 'sovereign-prices.json'),
}
const baseUrlParsed = new URL(cfg.BASE_URL)
cfg.UPSTREAM_ORIGIN = `${baseUrlParsed.protocol}//${baseUrlParsed.host}`
cfg.PROXY_PATH = baseUrlParsed.pathname === '/' ? '/v1' : baseUrlParsed.pathname

const sleep = ms => new Promise(r => setTimeout(r, ms))
const say = (...a) => console.error(new Date().toISOString().slice(11, 19), ...a)

// ---------------------------------------------------------------- selection
const selection = JSON.parse(fs.readFileSync(path.join(__dirname, 'polyglot-exercises.json'), 'utf8'))
/** Fixed, deterministic order: languages as listed, exercises alphabetically within each. */
const EXERCISES = selection.languages.flatMap(lang => (selection.exercises[lang] || []).map(name => ({ lang, name })))

function exDir(lang, name) {
  return path.join(polyglotRoot, lang, 'exercises', 'practice', name)
}

// ---------------------------------------------------------------- prompt building (Aider benchmark style)
function readInstructions(dir) {
  const docs = path.join(dir, '.docs')
  let text = ''
  for (const f of ['instructions.md', 'instructions.append.md']) {
    const p = path.join(docs, f)
    if (fs.existsSync(p)) text += fs.readFileSync(p, 'utf8') + '\n'
  }
  return text.trim()
}

function solutionAndTestFiles(dir) {
  const cfgPath = path.join(dir, '.meta', 'config.json')
  const meta = JSON.parse(fs.readFileSync(cfgPath, 'utf8'))
  return { solution: meta.files?.solution || [], test: meta.files?.test || [] }
}

function buildPrompt(dir, retry) {
  const instructions = readInstructions(dir)
  const { solution, test } = solutionAndTestFiles(dir)
  const files = solution
    .map(rel => `File: ${rel}\n\`\`\`\n${fs.readFileSync(path.join(dir, rel), 'utf8')}\n\`\`\``)
    .join('\n\n')
  let prompt =
    `Below are the instructions for a coding exercise, followed by the current (stub) ` +
    `contents of the file(s) you must implement. Implement the solution IN PLACE in ` +
    `exactly these file(s); do not create new files. Do NOT modify the test file(s): ` +
    `${test.join(', ')}.\n\nInstructions:\n${instructions}\n\n${files}`
  if (retry) {
    prompt += `\n\nYour previous attempt failed the test suite. Test output:\n\`\`\`\n${retry.slice(0, 4000)}\n\`\`\`\nFix the implementation in the same file(s) so the tests pass.`
  }
  return prompt
}

// ---------------------------------------------------------------- per-language test runner
function runTests(lang, dir) {
  const { test } = solutionAndTestFiles(dir)
  if (lang === 'python') {
    const modules = test.map(f => f.replace(/\.py$/, '').replace(/\//g, '.'))
    const r = spawnSync('python3', ['-m', 'unittest', ...modules, '-v'], { cwd: dir, encoding: 'utf8', timeout: 120_000 })
    return { pass: r.status === 0, output: `${r.stdout || ''}\n${r.stderr || ''}`.trim() }
  }
  if (lang === 'rust') {
    const r = spawnSync('cargo', ['test'], { cwd: dir, encoding: 'utf8', timeout: 300_000 })
    return { pass: r.status === 0, output: `${r.stdout || ''}\n${r.stderr || ''}`.trim() }
  }
  if (lang === 'cpp') {
    const { solution } = solutionAndTestFiles(dir)
    const cppSolution = solution.filter(f => f.endsWith('.cpp'))
    const bin = path.join(os.tmpdir(), `polyglot-cpp-${crypto.randomBytes(6).toString('hex')}`)
    const compile = spawnSync(
      'g++',
      ['-std=c++17', '-I', path.join(dir, 'test'), '-o', bin, ...test, ...cppSolution, path.join(dir, 'test', 'tests-main.cpp')],
      { cwd: dir, encoding: 'utf8', timeout: 120_000 }
    )
    if (compile.status !== 0) return { pass: false, output: `${compile.stdout || ''}\n${compile.stderr || ''}`.trim() }
    const run = spawnSync(bin, [], { encoding: 'utf8', timeout: 60_000 })
    fs.rmSync(bin, { force: true })
    return { pass: run.status === 0, output: `${run.stdout || ''}\n${run.stderr || ''}`.trim() }
  }
  throw new Error(`no test runner for language: ${lang}`)
}

// ---------------------------------------------------------------- counting proxy
function startProxy(outDir) {
  const proc = spawn(
    process.execPath,
    [path.join(__dirname, '..', 'sovereign-counting-proxy.mjs')],
    {
      env: {
        ...process.env,
        SOVEREIGN_PROXY_LISTEN: cfg.PROXY,
        SOVEREIGN_PROXY_UPSTREAM: cfg.UPSTREAM_ORIGIN,
        SOVEREIGN_PROXY_STATS: path.join(outDir, 'proxy-stats.json'),
        SOVEREIGN_PROXY_CALLS: path.join(outDir, 'calls.jsonl'),
      },
      stdio: ['ignore', 'ignore', 'inherit'],
    }
  )
  return { proc, callsPath: path.join(outDir, 'calls.jsonl') }
}
async function tagProxy(tag) {
  await fetch(`http://${cfg.PROXY}/__tag?tag=${encodeURIComponent(tag)}`).catch(() => {})
}
function callsFor(callsPath, tag) {
  if (!fs.existsSync(callsPath)) return []
  return fs
    .readFileSync(callsPath, 'utf8')
    .split('\n')
    .filter(Boolean)
    .map(l => { try { return JSON.parse(l) } catch { return null } })
    .filter(r => r && r.tag === tag)
}
function proxyMetrics(rows) {
  const price = resolvePrice(cfg.MODEL, process.env)
  const prompt_tokens = rows.reduce((s, r) => s + (r.prompt_tokens || 0), 0)
  const cached_tokens = rows.reduce((s, r) => s + (r.cached_tokens || 0), 0)
  const completion_tokens = rows.reduce((s, r) => s + (r.completion_tokens || 0), 0)
  return {
    model_calls: rows.length,
    prompt_tokens,
    cached_tokens,
    completion_tokens,
    cost_usd: costOf({ prompt_tokens, cached_tokens, completion_tokens }, price),
  }
}

// ---------------------------------------------------------------- hermes arm (persistent HERMES_HOME, one-shot CLI turns)
function ensureHermesHome(homeDir) {
  const hermesHome = path.join(homeDir, '.hermes')
  fs.mkdirSync(hermesHome, { recursive: true })
  fs.writeFileSync(
    path.join(hermesHome, 'config.yaml'),
    [
      'model:',
      `  default: "${cfg.MODEL}"`,
      '  provider: custom',
      `  base_url: "http://${cfg.PROXY}${cfg.PROXY_PATH}"`,
      `  api_key: "${cfg.API_KEY}"`,
      `  context_length: ${cfg.NUM_CTX}`,
      `  ollama_num_ctx: ${cfg.NUM_CTX}`,
      'terminal:',
      '  backend: local',
      '',
    ].join('\n')
  )
  fs.writeFileSync(path.join(hermesHome, '.env'), `OPENAI_API_KEY=${cfg.API_KEY}\n`)
  return hermesHome
}

function cleanEnv(home) {
  const env = { ...process.env }
  for (const k of Object.keys(env)) if (/API_KEY|TOKEN|SECRET|^HERMES_|^JCODE_|^SOVEREIGN_/.test(k)) delete env[k]
  return Object.assign(env, { HOME: home })
}

function runHermesTurn(hermesHome, dir, prompt) {
  const env = cleanEnv(path.dirname(hermesHome))
  Object.assign(env, { HERMES_HOME: hermesHome, OPENAI_API_KEY: cfg.API_KEY })
  const t0 = Date.now()
  const r = spawnSync(
    cfg.HERMES_VENV_PY,
    ['-m', 'hermes_cli.main', 'chat', '--query', prompt, '--quiet', '--max-turns', '30', '--accept-hooks', '--model', cfg.MODEL],
    { cwd: dir, env, encoding: 'utf8', timeout: cfg.TURN_TIMEOUT_MS }
  )
  return { ok: r.status === 0, wall_ms: Date.now() - t0, text: (r.stdout || '').trim() }
}

// ---------------------------------------------------------------- sovereign arm (one persistent engine for the whole run)
async function startSovereign(homeDir, token) {
  const jcodeHome = path.join(homeDir, '.jcode')
  fs.mkdirSync(jcodeHome, { recursive: true })
  fs.writeFileSync(
    path.join(jcodeHome, 'config.toml'),
    [
      '[providers.bench]',
      'type = "openai-compatible"',
      `base_url = "http://${cfg.PROXY}${cfg.PROXY_PATH}"`,
      `api_key = "${cfg.API_KEY}"`,
      'requires_api_key = false',
      `default_model = "${cfg.MODEL}"`,
      '',
      '[[providers.bench.models]]',
      `id = "${cfg.MODEL}"`,
      `context_window = ${cfg.NUM_CTX}`,
      '',
    ].join('\n')
  )
  const env = cleanEnv(homeDir)
  Object.assign(env, { JCODE_HOME: jcodeHome, HERMES_DASHBOARD_SESSION_TOKEN: token, SOVEREIGN_PRICE_TABLE: cfg.PRICE_TABLE })
  const child = spawn(cfg.SOVEREIGN_BIN, ['--provider-profile', 'bench', '--model', cfg.MODEL, 'serve', '--host', '127.0.0.1', '--port', '0'], {
    env, cwd: homeDir, stdio: ['ignore', 'pipe', 'ignore'], detached: true,
  })
  const port = await new Promise((resolve, reject) => {
    let buf = ''
    const timer = setTimeout(() => reject(new Error('sovereign: no READY line in 180s')), 180_000)
    child.stdout.on('data', d => {
      buf += d
      const m = buf.match(/HERMES_BACKEND_READY port=(\d+)/)
      if (m) { clearTimeout(timer); resolve(Number(m[1])) }
    })
    child.on('exit', code => { clearTimeout(timer); reject(new Error(`sovereign exited ${code}`)) })
  })
  child.stdout.resume()
  return { child, port, jcodeHome }
}

function stopSovereign(child) {
  try { process.kill(-child.pid, 'SIGTERM') } catch {}
  return new Promise(resolve => {
    const t = setTimeout(() => { try { process.kill(-child.pid, 'SIGKILL') } catch {} ; resolve() }, 10_000)
    child.on('exit', () => { clearTimeout(t); resolve() })
  })
}

async function runSovereignTurn(port, token, dir, prompt, title) {
  const t0 = Date.now()
  const res = await fetch(`http://127.0.0.1:${port}/api/agent/run`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${token}` },
    body: JSON.stringify({ prompt, cwd: dir, title, timeout_s: Math.round(cfg.TURN_TIMEOUT_MS / 1000) }),
  }).then(r => r.json()).catch(err => ({ ok: false, error: String(err) }))
  return { ok: Boolean(res.ok), wall_ms: Date.now() - t0, text: res.text || '', session_id: res.session_id }
}

function sovereignToolMetrics(jcodeHome, sessionId) {
  if (!sessionId) return { tool_calls: null, tool_errors: null }
  const dbPath = path.join(jcodeHome, 'sovereign.db')
  if (!fs.existsSync(dbPath)) return { tool_calls: null, tool_errors: null }
  try {
    const sql =
      `SELECT COUNT(*), SUM(CASE WHEN s.status='error' OR s.error IS NOT NULL THEN 1 ELSE 0 END) FROM spans s ` +
      `JOIN fact_turn r ON r.id = s.root_id WHERE r.session_id='${sessionId.replace(/'/g, "''")}' AND s.kind='execute_tool';`
    const out = execFileSync('sqlite3', ['-readonly', dbPath, sql], { encoding: 'utf8' }).trim()
    const [calls, errs] = out.split('|')
    return { tool_calls: Number(calls || 0), tool_errors: Number(errs || 0) }
  } catch {
    return { tool_calls: null, tool_errors: null } // sqlite3 CLI unavailable or DB not yet flushed
  }
}

// ---------------------------------------------------------------- run
function copySandbox(lang, name, dest) {
  fs.rmSync(dest, { recursive: true, force: true })
  fs.cpSync(exDir(lang, name), dest, { recursive: true })
}

function loadDone(metaPath) {
  const done = new Set()
  if (fs.existsSync(metaPath)) {
    for (const line of fs.readFileSync(metaPath, 'utf8').split('\n')) {
      if (!line) continue
      try { done.add(JSON.parse(line).run_id) } catch {}
    }
  }
  return done
}

async function runArm(arm, only) {
  const outDir = path.join(cfg.OUT, 'results', arm)
  fs.mkdirSync(outDir, { recursive: true })
  const metaPath = path.join(outDir, 'meta.jsonl')
  const done = loadDone(metaPath)
  const homeDir = path.join(cfg.OUT, 'homes', arm) // persistent for the whole arm, across resumes too
  fs.mkdirSync(homeDir, { recursive: true })
  const proxy = startProxy(cfg.OUT)
  await sleep(800)

  let hermesHome, sovereign, token
  if (arm === 'hermes') {
    hermesHome = ensureHermesHome(homeDir)
  } else {
    token = crypto.randomBytes(24).toString('hex')
    sovereign = await startSovereign(homeDir, token)
  }

  try {
    for (const { lang, name } of EXERCISES) {
      if (only && !only.includes(name) && !only.includes(lang)) continue
      const run_id = `${lang}-${name}`
      if (done.has(run_id)) continue
      const sandbox = path.join(cfg.OUT, 'runs', arm, run_id)
      copySandbox(lang, name, sandbox)
      const tag = `${arm}|${run_id}`
      await tagProxy(tag)
      say(arm, run_id, 'turn 1...')

      let prompt = buildPrompt(sandbox, null)
      let turn =
        arm === 'hermes'
          ? runHermesTurn(hermesHome, sandbox, prompt)
          : await runSovereignTurn(sovereign.port, token, sandbox, prompt, run_id)
      let test = runTests(lang, sandbox)
      let retries = 0
      if (!test.pass) {
        retries = 1
        say(arm, run_id, 'turn 2 (retry with test output)...')
        prompt = buildPrompt(sandbox, test.output)
        turn =
          arm === 'hermes'
            ? runHermesTurn(hermesHome, sandbox, prompt)
            : await runSovereignTurn(sovereign.port, token, sandbox, prompt, `${run_id}-retry`)
        test = runTests(lang, sandbox)
      }

      const rows = callsFor(proxy.callsPath, tag)
      const pm = proxyMetrics(rows)
      const toolMetrics = arm === 'sovereign' ? sovereignToolMetrics(sovereign.jcodeHome, turn.session_id) : { tool_calls: null, tool_errors: null }
      const rec = {
        run_id, lang, name, arm, retries,
        pass: test.pass, agent_ok: turn.ok,
        ...pm, ...toolMetrics,
        wall_ms: turn.wall_ms,
      }
      fs.appendFileSync(metaPath, JSON.stringify(rec) + '\n')
      say(arm, run_id, `pass=${rec.pass} retries=${retries} calls=${pm.model_calls} ${rec.wall_ms}ms`)
      fs.rmSync(sandbox, { recursive: true, force: true })
    }
  } finally {
    proxy.proc.kill('SIGTERM')
    if (sovereign) await stopSovereign(sovereign.child)
  }
}

// ---------------------------------------------------------------- report
function median(nums) {
  const xs = nums.filter(n => n !== null && n !== undefined).sort((a, b) => a - b)
  if (!xs.length) return null
  const mid = Math.floor(xs.length / 2)
  return xs.length % 2 ? xs[mid] : (xs[mid - 1] + xs[mid]) / 2
}

function report() {
  const arms = ['hermes', 'sovereign']
  const rowsByArm = {}
  for (const arm of arms) {
    const metaPath = path.join(cfg.OUT, 'results', arm, 'meta.jsonl')
    rowsByArm[arm] = fs.existsSync(metaPath)
      ? fs.readFileSync(metaPath, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l))
      : []
  }
  console.log('| lang | arm | n | pass% | model_calls | prompt_tok | cached_tok | completion_tok | cost_usd | tool_calls | tool_errors | wall_ms |')
  console.log('|---|---|---|---|---|---|---|---|---|---|---|---|')
  for (const lang of selection.languages) {
    for (const arm of arms) {
      const rows = rowsByArm[arm].filter(r => r.lang === lang)
      if (!rows.length) continue
      const n = rows.length
      const passPct = (100 * rows.filter(r => r.pass).length) / n
      const cost = median(rows.map(r => r.cost_usd).filter(c => c !== null && c !== undefined))
      console.log(
        `| ${lang} | ${arm} | ${n} | ${passPct.toFixed(0)}% | ${median(rows.map(r => r.model_calls))} | ` +
        `${median(rows.map(r => r.prompt_tokens))} | ${median(rows.map(r => r.cached_tokens))} | ` +
        `${median(rows.map(r => r.completion_tokens))} | ${cost === null ? 'n/a' : cost.toFixed(4)} | ` +
        `${median(rows.map(r => r.tool_calls)) ?? 'n/a'} | ${median(rows.map(r => r.tool_errors)) ?? 'n/a'} | ${median(rows.map(r => r.wall_ms))} |`
      )
    }
  }
  console.log('|---|---|---|---|---|---|---|---|---|---|---|---|')
  for (const arm of arms) {
    const rows = rowsByArm[arm]
    if (!rows.length) continue
    const n = rows.length
    const passPct = (100 * rows.filter(r => r.pass).length) / n
    console.log(`| TOTAL | ${arm} | ${n} | ${passPct.toFixed(0)}% | ${median(rows.map(r => r.model_calls))} | | | | | | | ${median(rows.map(r => r.wall_ms))} |`)
  }
}

// ---------------------------------------------------------------- main
function resolvedConfig() {
  return {
    ...cfg,
    exercise_count: EXERCISES.length,
    order: EXERCISES.map(e => `${e.lang}/${e.name}`),
    arms: ['hermes', 'sovereign'],
  }
}

async function main() {
  if (DRY_RUN || !cmd) {
    console.log(JSON.stringify(resolvedConfig(), null, 2))
    return
  }
  if (cmd === 'run') {
    const arm = flag('arm', null)
    if (!['hermes', 'sovereign'].includes(arm)) throw new Error('usage: polyglot.mjs run --arm hermes|sovereign [--only name1,name2]')
    const only = flag('only', null)?.split(',') || null
    fs.mkdirSync(cfg.OUT, { recursive: true })
    fs.writeFileSync(path.join(cfg.OUT, 'config.json'), JSON.stringify({ ...resolvedConfig(), started: new Date().toISOString() }, null, 2))
    await runArm(arm, only)
    return
  }
  if (cmd === 'report') {
    report()
    return
  }
  console.log('usage: polyglot.mjs [--dry-run] | run --arm <hermes|sovereign> [--only a,b] | report')
  process.exitCode = 2
}

main().catch(err => {
  console.error(err)
  process.exit(1)
})
