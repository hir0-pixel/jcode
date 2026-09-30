#!/usr/bin/env node
/**
 * Aider-polyglot-exercises driver: stock Hermes vs Akira (sovereign) vs Prime, same model.
 *
 * For each exercise in scripts/bench/polyglot-exercises.json (ALL 225 Aider polyglot
 * exercises: cpp, go, java, javascript, python, rust; `--set 40` selects the old
 * fixed-seed 40-exercise python/rust/cpp subset in polyglot-exercises-40.json):
 *
 *   1. Copy the exercise to a throwaway sandbox (JS gets a pre-warmed node_modules).
 *   2. Give the agent the instructions + current stub file(s), mirroring
 *      Aider's benchmark prompt ("implement the stubs, don't touch the tests").
 *   3. Restore every non-solution file from the pristine exercise (tests, build files),
 *      un-skip the tests (Aider's harness does the same), then run the language's OWN
 *      test command independently (never via the agent): pass@1.
 *   4. On failure, ONE retry turn carrying the test output; re-run tests: pass@2.
 *
 * All arms mirror scripts/bench/abeval.py's hardened setup:
 *   - every model call goes through the shared counting proxy
 *     (scripts/sovereign-counting-proxy.mjs): tokens, dummy cost
 *     (BENCH_PRICE_IN/_CACHED/_OUT), model calls;
 *   - process-tree RSS (peak/mean MiB) sampled per exercise; proxy and Ollama
 *     are excluded because the walk only goes down from the arm's own process;
 *   - one persistent private home per arm-run (learning can carry across
 *     exercises), same fixed exercise order;
 *   hermes     `hermes_cli.main chat`, private HERMES_HOME, pristine upstream
 *              source when HERMES_STOCK_SRC is set.
 *   sovereign  Akira: one long-lived `sovereign serve` with private HOME,
 *              JCODE_HOME, HERMES_HOME and a short JCODE_RUNTIME_DIR,
 *              `--provider openai-compatible` + generated jcode config.
 *   prime      Prime Agent `--mode rpc` (one process per turn, skills and
 *              extensions on), short /tmp/pd-* daemon socket dir, the renamed
 *              `prime-agent` daemon counted then reaped.
 *
 * Usage:
 *   node scripts/bench/polyglot.mjs --dry-run
 *   node scripts/bench/polyglot.mjs prewarm [--lang l]   # download toolchain deps once (untimed)
 *   node scripts/bench/polyglot.mjs verify [--lang l] [--only name]  # stub must fail, reference must pass; no model
 *   node scripts/bench/polyglot.mjs run --arm hermes|sovereign|prime [--lang l] [--only name,...] [--set 40|full]
 *   node scripts/bench/polyglot.mjs report
 *
 * Environment: BENCH_BASE_URL, BENCH_MODEL, BENCH_API_KEY, BENCH_CONTEXT,
 * BENCH_PROXY, BENCH_OUT, BENCH_TURN_TIMEOUT_MS, BENCH_PRICE_*, HERMES_VENV_PY,
 * HERMES_STOCK_SRC, SOVEREIGN_BIN, PRIME_CLI, PRIME_AGENT_KERNEL_VENV, JAVA_HOME (default
 * Homebrew openjdk@21), BENCH_CACHE (shared gradle/npm/warm-marker cache).
 *
 * Codex subscription mode (BENCH_PROVIDER=codex; `hermes` and `prime` arms only, local mode untouched):
 *   no proxy, no API key. Each agent invocation (= one turn attempt) gets a FRESH private home holding a
 *   COPY of benchmarks/codex-auth/<harness>/auth.json (0700/0600); after the turn a refreshed login is written
 *   back to the master (refresh tokens rotate) and the copy is deleted. Tokens, model calls and tool calls
 *   (by name) are read from each harness's own session logs (Prime session jsonl, Hermes `sessions export`)
 *   with one definition for both: prompt_tokens = input + cache_read + cache_write, cached_tokens = cache_read,
 *   completion_tokens = output. Subscription throttling is detected, recorded per exercise and retried with
 *   backoff from the pre-turn sandbox snapshot (never scored as a failure); a login error aborts the run.
 *   Env: BENCH_PROVIDER=codex BENCH_MODEL=gpt-6-luna BENCH_TURN_TIMEOUT_MS BENCH_OUT, PRIME_CLI,
 *   PRIME_AGENT_KERNEL_VENV, HERMES_STOCK_SRC, HERMES_VENV_PY, BENCH_AUTH_ROOT.
 *   Writes <OUT>/status-<arm>.json per exercise and merges them into <OUT>/status.json.
 *
 * Never starts Ollama or any model server itself.
 */
import { spawn, spawnSync, execFileSync } from 'node:child_process'
import crypto from 'node:crypto'
import http from 'node:http'
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

const CODEX = process.env.BENCH_PROVIDER === 'codex'
const cfg = {
  CODEX,
  MODEL: process.env.BENCH_MODEL || (CODEX ? 'gpt-6-luna' : 'sovereign/bench-hermes-64k:latest'),
  AUTH_ROOT: process.env.BENCH_AUTH_ROOT || path.join(engineRoot, '..', 'benchmarks', 'codex-auth'),
  NUM_CTX: Number(process.env.BENCH_CONTEXT || process.env.BENCH_NUM_CTX || 65536),
  API_KEY: process.env.BENCH_API_KEY || 'ollama',
  BASE_URL: process.env.BENCH_BASE_URL || 'http://127.0.0.1:11434',
  PROXY: process.env.BENCH_PROXY || '127.0.0.1:18080',
  TURN_TIMEOUT_MS: Number(process.env.BENCH_TURN_TIMEOUT_MS || 10 * 60_000),
  OUT: process.env.BENCH_OUT || path.join(engineRoot, 'bench-results', 'polyglot'),
  HERMES_VENV_PY: process.env.HERMES_VENV_PY || path.join(engineRoot, '..', 'hermes-agent', '.venv', 'bin', 'python3'),
  SOVEREIGN_BIN: process.env.SOVEREIGN_BIN || path.join(engineRoot, 'target', 'release', 'sovereign'),
  HERMES_STOCK_SRC: process.env.HERMES_STOCK_SRC || '', // pristine upstream Hermes source (PYTHONPATH); unset = the fork
  PRIME_CLI: process.env.PRIME_CLI || '/tmp/prime-agent/packages/coding-agent/dist/bundle/cli.js',
  PRIME_KERNEL_VENV: process.env.PRIME_AGENT_KERNEL_VENV || '/tmp/prime-agent/kernel-venv',
  JAVA_HOME: process.env.BENCH_JAVA_HOME || '/opt/homebrew/opt/openjdk@21/libexec/openjdk.jdk/Contents/Home',
  CACHE: process.env.BENCH_CACHE || path.join(os.homedir(), '.cache', 'sovereign-bench'),
  PRICE_TABLE: process.env.SOVEREIGN_PRICE_TABLE || path.join(engineRoot, 'scripts', 'sovereign-prices.json'),
}
const baseUrlParsed = new URL(cfg.BASE_URL)
cfg.UPSTREAM_ORIGIN = `${baseUrlParsed.protocol}//${baseUrlParsed.host}`
cfg.PROXY_PATH = baseUrlParsed.pathname === '/' ? '/v1' : baseUrlParsed.pathname

const sleep = ms => new Promise(r => setTimeout(r, ms))
const say = (...a) => console.error(new Date().toISOString().slice(11, 19), ...a)

// ---------------------------------------------------------------- selection
const SET = flag('set', 'full')
const selection = JSON.parse(fs.readFileSync(path.join(__dirname, SET === '40' ? 'polyglot-exercises-40.json' : SET === 'topup' ? 'polyglot-exercises-topup.json' : 'polyglot-exercises.json'), 'utf8'))
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

// ---------------------------------------------------------------- toolchain env, sandbox setup, test runner
/** Toolchain env shared by the harness's own test runs and every agent arm (real JDK, brew tools, shared caches). */
const toolEnv = () => ({
  JAVA_HOME: cfg.JAVA_HOME,
  GRADLE_USER_HOME: path.join(cfg.CACHE, 'gradle'),
  npm_config_cache: path.join(cfg.CACHE, 'npm'),
  PATH: [`${cfg.JAVA_HOME}/bin`, '/opt/homebrew/bin', process.env.PATH].join(':'),
})
const sh = (bin, argv, opts = {}) => {
  const r = spawnSync(bin, argv, { encoding: 'utf8', env: { ...process.env, ...toolEnv(), CI: '1' }, maxBuffer: 64 << 20, ...opts })
  return { pass: r.status === 0, output: `${r.stdout || ''}\n${r.stderr || ''}`.trim() }
}
const sha = x => crypto.createHash('sha1').update(x).digest('hex').slice(0, 16)
const SKIP_DIRS = new Set(['target', 'node_modules', 'build', '.gradle'])

/** Aider's harness runs every test; the exercises ship with skips (@Disabled, xit/xtest, rust #[ignore]). */
function unskip(lang, text) {
  if (lang === 'java') return text.replace(/^[ \t]*@Disabled(\([^)]*\))?[ \t]*\r?\n/gm, '')
  if (lang === 'javascript') return text.replace(/\bx(it|test|describe)\(/g, '$1(').replace(/\b(it|test|describe)\.skip\(/g, '$1(')
  return text
}

function walk(dir, rel = '') {
  return fs.readdirSync(path.join(dir, rel), { withFileTypes: true }).flatMap(e => {
    if (SKIP_DIRS.has(e.name) || e.name === 'Cargo.lock') return []
    const r = path.join(rel, e.name)
    return e.isDirectory() ? walk(dir, r) : [r]
  })
}

/** Put back every non-solution file (tests, build files, wrappers) from the pristine exercise, un-skipped.
 *  Returns the files the agent had changed (tampering). */
function restoreTests(lang, dir, name) {
  const orig = exDir(lang, name)
  const { solution, test } = solutionAndTestFiles(orig)
  const tampered = []
  for (const rel of walk(orig)) {
    if (solution.includes(rel)) continue
    const o = fs.readFileSync(path.join(orig, rel))
    const want = test.includes(rel) ? Buffer.from(unskip(lang, o.toString('utf8'))) : o
    const dst = path.join(dir, rel)
    const have = fs.existsSync(dst) ? fs.readFileSync(dst) : null
    if (have && have.equals(want)) continue
    if (have && !have.equals(o)) tampered.push(rel)
    fs.mkdirSync(path.dirname(dst), { recursive: true })
    fs.writeFileSync(dst, want)
    if (fs.statSync(path.join(orig, rel)).mode & 0o111) fs.chmodSync(dst, 0o755)
  }
  return tampered
}

/** Untimed per-exercise setup inside the sandbox (network allowed on a cold cache). JS: node_modules from a
 *  shared template keyed by the devDependencies hash (APFS clone), so no npm install ever lands in a timed turn. */
function setupSandbox(lang, dir) {
  if (lang !== 'javascript') return
  const pkg = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'))
  const tpl = path.join(cfg.CACHE, 'nm', sha(JSON.stringify(pkg.devDependencies || {})), 'node_modules')
  if (!fs.existsSync(tpl)) {
    const r = sh('npm', ['install', '--no-audit', '--no-fund'], { cwd: dir, timeout: 600_000 })
    if (!r.pass) throw new Error(`npm install failed in ${dir}\n${r.output.slice(-800)}`)
    fs.mkdirSync(path.dirname(tpl), { recursive: true })
    spawnSync('cp', ['-cR', path.join(dir, 'node_modules'), tpl])
  } else if (!fs.existsSync(path.join(dir, 'node_modules'))) {
    spawnSync('cp', ['-cR', tpl, path.join(dir, 'node_modules')])
  }
}

/** Download deps once so no timed test run needs the network. Idempotent via marker files. */
function prewarm(exercises) {
  const marks = path.join(cfg.CACHE, 'warm')
  fs.mkdirSync(marks, { recursive: true })
  for (const { lang, name } of exercises) {
    const src = exDir(lang, name)
    let key = null, argv = null, bin = null
    if (lang === 'java') { key = 'java-' + sha(fs.readFileSync(path.join(src, 'build.gradle')) + fs.readFileSync(path.join(src, 'gradle/wrapper/gradle-wrapper.properties'))); bin = './gradlew'; argv = ['test', '--no-daemon', '--console=plain'] }
    else if (lang === 'rust') { key = `rust2-${name}`; bin = 'cargo'; argv = ['fetch'] }
    else if (lang === 'javascript') key = 'js-' + sha(JSON.stringify(JSON.parse(fs.readFileSync(path.join(src, 'package.json'), 'utf8')).devDependencies || {}))
    else continue
    const mark = path.join(marks, key)
    if (fs.existsSync(mark)) continue
    say('prewarm', lang, name)
    const box = shortDir('pw-')
    fs.cpSync(src, box, { recursive: true, filter: s => !SKIP_DIRS.has(path.basename(s)) })
    if (lang === 'javascript') setupSandbox(lang, box)
    else {
      if (lang === 'rust' && fs.existsSync(path.join(box, '.meta', 'Cargo-example.toml'))) { // also cache the reference's crates
        const first = sh(bin, argv, { cwd: box, timeout: 900_000 })
        if (!first.pass) throw new Error(`prewarm rust/${name} failed\n${first.output.slice(-800)}`)
        fs.copyFileSync(path.join(box, '.meta', 'Cargo-example.toml'), path.join(box, 'Cargo.toml'))
        fs.rmSync(path.join(box, 'Cargo.lock'), { force: true })
      }
      const r = sh(bin, argv, { cwd: box, timeout: 900_000 }) // java: stub tests fail, that is fine, deps are resolved
      if (!r.pass && lang !== 'java') throw new Error(`prewarm ${lang}/${name} failed\n${r.output.slice(-800)}`)
      if (lang === 'java' && !/BUILD (SUCCESSFUL|FAILED)|tests completed/.test(r.output)) throw new Error(`prewarm java/${name} failed\n${r.output.slice(-800)}`)
    }
    fs.rmSync(box, { recursive: true, force: true })
    fs.writeFileSync(mark, new Date().toISOString())
  }
}

/** The language's own test command, as Aider's benchmark.py runs it (all tests, deps offline). Runs in the sandbox. */
function runTests(lang, dir, name) {
  const tampered = restoreTests(lang, dir, name)
  const { test } = solutionAndTestFiles(dir)
  let r
  if (lang === 'python') r = sh('python3', ['-m', 'pytest', '-q', '-p', 'no:cacheprovider', ...test], { cwd: dir, timeout: 120_000 })
  else if (lang === 'rust') r = sh('cargo', ['test', '--offline', '--', '--include-ignored'], { cwd: dir, timeout: 300_000 })
  else if (lang === 'go') r = sh('go', ['test', './...'], { cwd: dir, timeout: 300_000 })
  else if (lang === 'javascript') r = sh('npm', ['test'], { cwd: dir, timeout: 300_000 })
  else if (lang === 'java') r = sh('./gradlew', ['test', '--offline', '--no-daemon', '--console=plain'], { cwd: dir, timeout: 600_000 })
  else if (lang === 'cpp') {
    fs.rmSync(path.join(dir, 'build'), { recursive: true, force: true })
    r = sh('sh', ['-c', 'cmake -S . -B build -G "Unix Makefiles" -DEXERCISM_RUN_ALL_TESTS=1 && cmake --build build -j 4'], { cwd: dir, timeout: 300_000 })
  } else throw new Error(`no test runner for language: ${lang}`)
  return { ...r, tampered }
}

/** Copy the exercise's reference solution (files.example) over the stub (files.solution). */
function applyReference(lang, dir, name) {
  const { solution } = solutionAndTestFiles(dir)
  const meta = JSON.parse(fs.readFileSync(path.join(exDir(lang, name), '.meta', 'config.json'), 'utf8'))
  const exs = meta.files.example || []
  const bn = f => path.basename(f)
  const dest = new Map() // basename match wins over extension match (java: extra example classes)
  for (const ex of exs) { const to = solution.find(f => bn(f) === bn(ex)); if (to) dest.set(ex, to) }
  for (const ex of exs) {
    if (dest.has(ex)) continue
    const taken = new Set(dest.values())
    dest.set(ex, solution.find(f => !taken.has(f) && path.extname(f) === path.extname(ex)) || path.join(path.dirname(solution[0]), bn(ex)))
  }
  for (const [ex, to] of dest) fs.copyFileSync(path.join(exDir(lang, name), ex), path.join(dir, to))
  const cargoEx = path.join(exDir(lang, name), '.meta', 'Cargo-example.toml') // rust: the reference's dependencies
  if (lang === 'rust' && fs.existsSync(cargoEx)) fs.copyFileSync(cargoEx, path.join(dir, 'Cargo.toml'))
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

// ---------------------------------------------------------------- process-tree RSS sampling (port of abeval.py)
function psSnapshot() {
  try {
    const out = execFileSync('ps', ['-ww', '-o', 'pid=,ppid=,rss=,command=', '-A'], { encoding: 'utf8', timeout: 5000, maxBuffer: 64 << 20 })
    const table = new Map()
    for (const line of out.split('\n')) {
      const m = line.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s*(.*)$/)
      if (m) table.set(Number(m[1]), { ppid: Number(m[2]), rss: Number(m[3]), cmd: m[4] })
    }
    return table
  } catch { return new Map() }
}
function descendants(root, table) {
  if (!table.has(root)) return new Set()
  const kids = new Map()
  for (const [pid, { ppid }] of table) kids.set(ppid, [...(kids.get(ppid) || []), pid])
  const seen = new Set([root])
  for (let frontier = [root]; frontier.length;) {
    frontier = frontier.flatMap(p => kids.get(p) || []).filter(c => !seen.has(c) && seen.add(c))
  }
  return seen
}
/** Summed RSS (MiB) of root's tree (walks DOWN only, so proxy and Ollama are excluded by construction),
 *  plus detached helpers whose command line satisfies `marker(pid, cmd)`. */
function treeRssMib(root, table, marker) {
  const pids = descendants(root, table)
  if (marker) for (const [pid, { cmd }] of table) if (marker(pid, cmd)) descendants(pid, table).forEach(p => pids.add(p))
  let kib = 0
  for (const p of pids) kib += table.get(p).rss
  return kib / 1024
}
/** Kill anything an arm left running with its cwd inside this exercise's sandbox (e.g. a model-written
 *  test that never exits), so it cannot hold memory or inflate later exercises' RSS. Same for every arm. */
function reapStragglers(dir) {
  try {
    const out = execFileSync('lsof', ['-a', '-d', 'cwd', '-Fpn'], { encoding: 'utf8', timeout: 10000, maxBuffer: 64 << 20 })
    let pid = null
    for (const line of out.split('\n')) {
      if (line[0] === 'p') pid = Number(line.slice(1))
      else if (line[0] === 'n' && pid && pid !== process.pid && line.slice(1).startsWith(dir)) { try { process.kill(pid, 'SIGKILL') } catch {} }
    }
  } catch {}
}
function startSampler(root, marker) {
  const samples = []
  const tick = () => samples.push(treeRssMib(root, psSnapshot(), marker))
  tick()
  const t = setInterval(tick, 200)
  return () => { clearInterval(t); return samples }
}
const namedPids = (table, name) => new Set([...table].filter(([, v]) => v.cmd.trim().startsWith(name)).map(([p]) => p))

/** Async spawn with RSS sampling. daemonName: a detached helper that renames itself (Prime's
 *  `prime-agent` daemon) is counted while the run lasts and reaped afterwards. */
async function runWithRss(bin, argv, { cwd, env, timeoutMs, input, marker, daemonName }) {
  const before = daemonName ? namedPids(psSnapshot(), daemonName) : null
  const mark = daemonName ? (pid, cmd) => !before.has(pid) && cmd.trim().startsWith(daemonName) : marker
  const child = spawn(bin, argv, { cwd, env, stdio: ['pipe', 'pipe', 'pipe'] })
  const stop = startSampler(child.pid, mark)
  let out = '', err = '', timedOut = false
  child.stdout.on('data', d => { out += d })
  child.stderr.on('data', d => { if (err.length < (4 << 20)) err += d })
  child.stdin.on('error', () => {})
  child.stdin.end(input ?? '')
  const timer = setTimeout(() => { timedOut = true; child.kill('SIGKILL') }, timeoutMs)
  const code = await new Promise(res => child.on('close', c => res(c ?? -9)))
  clearTimeout(timer)
  const samples = stop()
  if (daemonName) for (const pid of [...namedPids(psSnapshot(), daemonName)].filter(p => !before.has(p))) { try { process.kill(pid, 'SIGTERM') } catch {} }
  return { code, out, err, timedOut, samples }
}

// ---------------------------------------------------------------- shared env / bench config
/** Strip credentials from the inherited env; `strict` also drops engine-specific vars. */
function cleanEnv(home, strict = true) {
  const env = { ...process.env }
  for (const k of Object.keys(env)) {
    if (/API_KEY|TOKEN|SECRET/.test(k) || (strict && /^(HERMES_|JCODE_|SOVEREIGN_)/.test(k))) delete env[k]
  }
  return Object.assign(env, toolEnv(), { HOME: home })
}
const shortDir = prefix => fs.mkdtempSync(path.join('/tmp', prefix))
const proxyUrl = () => `http://${cfg.PROXY}${cfg.PROXY_PATH}`

// ---------------------------------------------------------------- hermes arm (persistent HERMES_HOME, one-shot CLI turns)
function ensureHermesHome(homeDir) {
  const hermesHome = path.join(homeDir, '.hermes')
  fs.mkdirSync(hermesHome, { recursive: true })
  fs.writeFileSync(
    path.join(hermesHome, 'config.yaml'),
    [
      'model:', `  default: "${cfg.MODEL}"`, '  provider: custom', `  base_url: "${proxyUrl()}"`,
      `  api_key: "${cfg.API_KEY}"`, `  context_length: ${cfg.NUM_CTX}`, `  ollama_num_ctx: ${cfg.NUM_CTX}`,
      'terminal:', '  backend: local', '',
    ].join('\n')
  )
  fs.writeFileSync(path.join(hermesHome, '.env'), `OPENAI_API_KEY=${cfg.API_KEY}\n`)
  return hermesHome
}

async function runHermesTurn(hermesHome, dir, prompt) {
  const homeDir = path.dirname(hermesHome)
  const env = Object.assign(cleanEnv(homeDir), { HERMES_HOME: hermesHome, OPENAI_API_KEY: cfg.API_KEY })
  // Pristine upstream Hermes ahead of the fork's editable install, as abeval's hermes arm.
  if (cfg.HERMES_STOCK_SRC) env.PYTHONPATH = cfg.HERMES_STOCK_SRC
  const t0 = Date.now()
  const r = await runWithRss(
    cfg.HERMES_VENV_PY,
    ['-m', 'hermes_cli.main', 'chat', '--query', prompt, '--quiet', '--max-turns', '30', '--accept-hooks', '--model', cfg.MODEL],
    { cwd: dir, env, timeoutMs: cfg.TURN_TIMEOUT_MS, marker: (_p, cmd) => cmd.includes(homeDir) }
  )
  return { ok: r.code === 0, wall_ms: Date.now() - t0, samples: r.samples }
}

// ---------------------------------------------------------------- prime arm (one `--mode rpc` process per turn, persistent home)
function ensurePrimeHome(homeDir) {
  const agentDir = path.join(homeDir, '.prime', 'agent')
  fs.mkdirSync(agentDir, { recursive: true })
  fs.writeFileSync(
    path.join(agentDir, 'models.json'),
    JSON.stringify({
      providers: { bench: { baseUrl: proxyUrl(), api: 'openai-completions', apiKey: cfg.API_KEY, models: [{ id: cfg.MODEL, contextWindow: cfg.NUM_CTX, maxTokens: 4096 }] } },
    })
  )
  return agentDir
}

async function runPrimeTurn(agentDir, dir, prompt) {
  const homeDir = path.resolve(agentDir, '..', '..')
  const env = Object.assign(cleanEnv(homeDir, false), { PRIME_AGENT_CODING_AGENT_DIR: agentDir, PRIME_AGENT_KERNEL_VENV: cfg.PRIME_KERNEL_VENV })
  const sockDir = shortDir('pd-') // AF_UNIX paths are capped at 104 bytes on macOS
  const t0 = Date.now()
  const r = await runWithRss(
    'node',
    [cfg.PRIME_CLI, '--mode', 'rpc', '--provider', 'bench', '--model', `bench/${cfg.MODEL}`, '--cwd', dir, '--daemon-socket', `${sockDir}/d.sock`, '--offline'],
    { cwd: dir, env, timeoutMs: cfg.TURN_TIMEOUT_MS, input: JSON.stringify({ type: 'prompt', message: prompt }) + '\n', daemonName: 'prime-agent' }
  )
  fs.rmSync(sockDir, { recursive: true, force: true })
  let tool_calls = 0, tool_errors = 0
  for (const line of r.out.split('\n')) {
    try {
      const ev = JSON.parse(line)
      if (ev.type === 'tool_execution_end') { tool_calls++; tool_errors += ev.isError ? 1 : 0 }
    } catch {}
  }
  return { ok: r.code === 0, wall_ms: Date.now() - t0, samples: r.samples, tool_calls, tool_errors }
}

// ---------------------------------------------------------------- sovereign arm (Akira: one persistent engine for the whole run)
async function startSovereign(homeDir, token) {
  const jcodeHome = path.join(homeDir, '.jcode')
  const hermesHome = path.join(homeDir, '.hermes')
  fs.mkdirSync(jcodeHome, { recursive: true })
  fs.mkdirSync(hermesHome, { recursive: true })
  fs.writeFileSync(
    path.join(jcodeHome, 'config.toml'),
    [
      '[provider]', 'default_provider = "bench"', '',
      '[providers.bench]', 'type = "openai-compatible"', `base_url = "${proxyUrl()}"`, `api_key = "${cfg.API_KEY}"`,
      'requires_api_key = false', `default_model = "${cfg.MODEL}"`, '',
      '[[providers.bench.models]]', `id = "${cfg.MODEL}"`, `context_window = ${cfg.NUM_CTX}`, '',
    ].join('\n')
  )
  // Private HERMES_HOME (else the engine reads the real ~/.hermes) and a short private runtime dir
  // (the default is shared across engines; AF_UNIX paths are capped at 104 bytes on macOS).
  const env = Object.assign(cleanEnv(homeDir), {
    JCODE_HOME: jcodeHome, HERMES_HOME: hermesHome, JCODE_RUNTIME_DIR: shortDir('jr-'),
    HERMES_DASHBOARD_SESSION_TOKEN: token, SOVEREIGN_PRICE_TABLE: cfg.PRICE_TABLE,
  })
  const child = spawn(cfg.SOVEREIGN_BIN, ['--provider', 'openai-compatible', '--model', cfg.MODEL, 'serve', '--host', '127.0.0.1', '--port', '0'], {
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
  return { child, port, jcodeHome, homeDir, runtimeDir: env.JCODE_RUNTIME_DIR }
}

function stopSovereign(sv) {
  try { process.kill(-sv.child.pid, 'SIGTERM') } catch {}
  return new Promise(resolve => {
    const t = setTimeout(() => { try { process.kill(-sv.child.pid, 'SIGKILL') } catch {} ; resolve() }, 10_000)
    sv.child.on('exit', () => { clearTimeout(t); resolve() })
  }).then(() => fs.rmSync(sv.runtimeDir, { recursive: true, force: true }))
}

// fetch() (undici) aborts after 300 s with no response headers, and /api/agent/run only
// answers when the turn ends; use node:http with the benchmark's own turn limit instead.
function postJson(port, pathName, token, body, timeoutMs) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port, path: pathName, method: 'POST',
      headers: { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body), authorization: `Bearer ${token}` } },
    res => {
      let data = ''
      res.setEncoding('utf8')
      res.on('data', chunk => { data += chunk })
      res.on('end', () => { try { resolve(JSON.parse(data)) } catch (err) { reject(err) } })
    })
    req.setTimeout(timeoutMs, () => req.destroy(new Error(`no reply within ${timeoutMs} ms`)))
    req.on('error', reject)
    req.end(body)
  })
}

async function runSovereignTurn(sv, token, dir, prompt, title) {
  const stop = startSampler(sv.child.pid, (_p, cmd) => cmd.includes(sv.homeDir))
  const t0 = Date.now()
  const body = JSON.stringify({ prompt, cwd: dir, title, timeout_s: Math.round(cfg.TURN_TIMEOUT_MS / 1000) })
  const res = await postJson(sv.port, '/api/agent/run', token, body, cfg.TURN_TIMEOUT_MS + 60_000)
    .catch(err => ({ ok: false, error: String(err) }))
  const wall_ms = Date.now() - t0
  return { ok: Boolean(res.ok), wall_ms, samples: stop(), session_id: res.session_id, ...sovereignToolMetrics(sv.jcodeHome, res.session_id) }
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

// ---------------------------------------------------------------- run (local/proxy mode)
/** Sandbox lives at <parent>/<exercise-name> (cmake names the project after the directory); returns that path. */
function copySandbox(lang, name, parent) {
  fs.rmSync(parent, { recursive: true, force: true })
  const dest = path.join(parent, name)
  fs.cpSync(exDir(lang, name), dest, { recursive: true, filter: src => !SKIP_DIRS.has(path.basename(src)) && path.basename(src) !== 'Cargo.lock' })
  setupSandbox(lang, dest)
  return dest
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

const selected = (lang, name, only, langFilter) => (!langFilter || langFilter === lang) && (!only || only.includes(name) || only.includes(lang))
const ARMS = ['hermes', 'sovereign', 'prime']
const sum = xs => xs.reduce((s, x) => s + (x || 0), 0)
const addNullable = (a, b) => (a === null || b === null || a === undefined || b === undefined ? null : a + b)

async function runArm(arm, only, langFilter) {
  const outDir = path.join(cfg.OUT, 'results', arm)
  fs.mkdirSync(outDir, { recursive: true })
  const metaPath = path.join(outDir, 'meta.jsonl')
  const done = loadDone(metaPath)
  const homeDir = path.join(cfg.OUT, 'homes', arm) // persistent for the whole arm, across resumes too
  fs.mkdirSync(homeDir, { recursive: true })
  prewarm(EXERCISES.filter(e => selected(e.lang, e.name, only, langFilter)))
  const proxy = startProxy(cfg.OUT)
  await sleep(800)

  let handle, sovereign, token
  if (arm === 'hermes') handle = ensureHermesHome(homeDir)
  else if (arm === 'prime') {
    if (!fs.existsSync(cfg.PRIME_CLI)) throw new Error(`Prime CLI missing at ${cfg.PRIME_CLI}`)
    handle = ensurePrimeHome(homeDir)
  } else {
    token = crypto.randomBytes(24).toString('hex')
    sovereign = await startSovereign(homeDir, token)
  }
  const turnFn = (dir, prompt, title) =>
    arm === 'hermes' ? runHermesTurn(handle, dir, prompt)
    : arm === 'prime' ? runPrimeTurn(handle, dir, prompt)
    : runSovereignTurn(sovereign, token, dir, prompt, title)

  try {
    for (const { lang, name } of EXERCISES) {
      if (!selected(lang, name, only, langFilter)) continue
      const run_id = `${lang}-${name}`
      if (done.has(run_id)) continue
      const parent = path.join(cfg.OUT, 'runs', arm, run_id)
      const sandbox = copySandbox(lang, name, parent)
      const tag = `${arm}|${run_id}`
      await tagProxy(tag)
      say(arm, run_id, 'turn 1...')

      const turns = [await turnFn(sandbox, buildPrompt(sandbox, null), run_id)]
      const test1 = runTests(lang, sandbox, name)
      let test = test1
      if (!test1.pass) {
        say(arm, run_id, 'turn 2 (retry with test output)...')
        turns.push(await turnFn(sandbox, buildPrompt(sandbox, test1.output), `${run_id}-retry`))
        test = runTests(lang, sandbox, name)
      }

      const samples = turns.flatMap(t => t.samples)
      const pm = proxyMetrics(callsFor(proxy.callsPath, tag))
      const toolSum = k => turns.reduce((s, t) => addNullable(s, t[k] ?? null), 0)
      const rec = {
        run_id, lang, name, arm, retries: turns.length - 1,
        pass1: test1.pass, pass: test.pass, tests_tampered: [...new Set([...test1.tampered, ...(test.tampered || [])])], agent_ok: turns.every(t => t.ok),
        ...pm,
        tool_calls: arm === 'hermes' ? null : toolSum('tool_calls'),
        tool_errors: arm === 'hermes' ? null : toolSum('tool_errors'),
        wall_ms: sum(turns.map(t => t.wall_ms)),
        rss_peak_mib: samples.length ? Math.round(Math.max(...samples) * 10) / 10 : null,
        rss_mean_mib: samples.length ? Math.round((sum(samples) / samples.length) * 10) / 10 : null,
      }
      fs.appendFileSync(metaPath, JSON.stringify(rec) + '\n')
      say(arm, run_id, `pass@1=${rec.pass1} pass@2=${rec.pass} calls=${pm.model_calls} ${rec.wall_ms}ms rss=${rec.rss_peak_mib}/${rec.rss_mean_mib}MiB`)
      reapStragglers(parent)
      fs.rmSync(parent, { recursive: true, force: true })
    }
  } finally {
    proxy.proc.kill('SIGTERM')
    if (sovereign) await stopSovereign(sovereign)
  }
}

// ---------------------------------------------------------------- codex subscription mode (hermes + prime only)
const THROTTLE_RE = /rate[ _-]?limit|too many requests|\b429\b|usage[ _-]?limit|quota|at capacity|overloaded|try again (in|later)|throttl|resets? (in|at)/i
const LOGIN_RE = /not (logged|signed) in|login (is )?(required|expired)|log ?in again|re-?log|re-?authenticat|unauthori[sz]ed|invalid_grant|refresh[ _-]token|token (is |has )?(expired|invalid|revoked)|\b401\b|authentication (failed|error)|no (codex )?credentials/i
const THROTTLE_MAX = Number(process.env.BENCH_THROTTLE_MAX || 8)
const THROTTLE_BASE_MS = Number(process.env.BENCH_THROTTLE_BASE_MS || 60_000)
// Estimate only: gpt-6-luna per-million-token rates exactly as Prime Agent v0.9.8's own model catalog priced the
// turns in its session logs (verified: 5293 in / 36 out -> $0.00697). The subscription itself has no per-token bill.
const CODEX_EST = { input: 1.25, cached: 0.125, output: 10 }
const CODEX_EST_BASIS = 'ESTIMATE: gpt-6-luna list rates 1.25 in / 0.125 cached / 10 out USD per Mtok as logged by Prime v0.9.8 catalog; subscription has no per-token bill'

class BenchAbort extends Error {}

const fileSha = f => crypto.createHash('sha256').update(fs.readFileSync(f)).digest('hex')
const masterAuth = arm => path.join(cfg.AUTH_ROOT, arm === 'prime' ? 'prime' : 'hermes', 'auth.json')

/** Copy the harness login into a private 0700 dir; returns the master's sha so a rotation can be detected. */
function putLogin(arm, dir) {
  const master = masterAuth(arm)
  if (!fs.existsSync(master)) throw new BenchAbort(`LOGIN: missing login file for ${arm}`)
  fs.mkdirSync(dir, { recursive: true })
  fs.chmodSync(dir, 0o700)
  const dst = path.join(dir, 'auth.json')
  fs.copyFileSync(master, dst)
  fs.chmodSync(dst, 0o600)
  return fileSha(master)
}
/** Refresh tokens rotate: write a changed login back to the master (atomic), then delete the copy. Never logged. */
function pullLoginBack(arm, dir, origSha) {
  const f = path.join(dir, 'auth.json')
  let refreshed = false
  try {
    if (fs.existsSync(f) && fs.statSync(f).size > 0 && fileSha(f) !== origSha) {
      JSON.parse(fs.readFileSync(f, 'utf8'))
      const master = masterAuth(arm)
      const staged = path.join(path.dirname(master), '.auth.json.bench-new')
      fs.copyFileSync(f, staged)
      fs.chmodSync(staged, 0o600)
      fs.renameSync(staged, master)
      refreshed = true
    }
  } catch {}
  try { fs.rmSync(f, { force: true }) } catch {}
  return refreshed
}

const readJsonl = f => fs.readFileSync(f, 'utf8').split('\n').filter(Boolean).flatMap(l => { try { return [JSON.parse(l)] } catch { return [] } })
function walkFiles(dir, ext) {
  if (!fs.existsSync(dir)) return []
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap(e => {
    const p = path.join(dir, e.name)
    return e.isDirectory() ? walkFiles(p, ext) : e.name.endsWith(ext) ? [p] : []
  })
}
const tally = (obj, k) => { obj[k] = (obj[k] || 0) + 1 }

/** Prime: every session jsonl (sub-sessions included). One assistant message with usage = one model call. */
function parsePrimeSessions(sessDir) {
  const m = { model_calls: 0, input: 0, cache_read: 0, cache_write: 0, output: 0, tool_names: {}, tool_errors: 0, models: new Set(), errors: [], lastStop: null }
  for (const f of walkFiles(sessDir, '.jsonl')) {
    for (const d of readJsonl(f)) {
      if (d.type === 'model_change' && d.modelId) m.models.add(d.modelId)
      const msg = d.type === 'message' ? d.message : null
      if (!msg) continue
      if (msg.role === 'toolResult' && msg.isError) m.tool_errors++
      if (msg.role !== 'assistant') continue
      if (msg.model) m.models.add(msg.model)
      const u = msg.usage
      if (u) { m.model_calls++; m.input += u.input || 0; m.cache_read += u.cacheRead || 0; m.cache_write += u.cacheWrite || 0; m.output += u.output || 0 }
      for (const c of msg.content || []) if (c && c.type === 'toolCall') tally(m.tool_names, c.name || '?')
      m.lastStop = msg.stopReason || null
      if (msg.stopReason === 'error' || msg.errorMessage) m.errors.push(String(msg.errorMessage || 'error'))
    }
  }
  return m
}
/** Hermes: `sessions export` rows (parent + subagent sessions; one row per session, counters already summed). */
function parseHermesExport(file) {
  const m = { model_calls: 0, input: 0, cache_read: 0, cache_write: 0, output: 0, reasoning: 0, tool_names: {}, tool_errors: null, models: new Set(), errors: [], lastStop: null }
  if (!fs.existsSync(file)) return m
  for (const s of readJsonl(file)) {
    m.model_calls += s.api_call_count || 0
    m.input += s.input_tokens || 0
    m.cache_read += s.cache_read_tokens || 0
    m.cache_write += s.cache_write_tokens || 0
    m.output += s.output_tokens || 0
    m.reasoning += s.reasoning_tokens || 0
    if (s.model) m.models.add(s.model)
    for (const msg of s.messages || []) for (const tc of msg.tool_calls || []) tally(m.tool_names, (tc.function || {}).name || '?')
  }
  return m
}

function copyLogs(src, dst) {
  if (!fs.existsSync(src)) return
  try {
    fs.cpSync(src, dst, {
      recursive: true,
      filter: s => {
        const b = path.basename(s)
        if (b === 'auth.json' || b.startsWith('.auth')) return false
        const st = fs.lstatSync(s)
        return (st.isFile() && st.size < 50 << 20) || st.isDirectory()
      },
    })
  } catch (e) { say('copyLogs warning', path.basename(src), String(e.message).slice(0, 120)) }
}

/** Shared tail of one attempt: metrics + throttle/login classification + raw-log copy. */
function finishAttempt(arm, r, m, errText, logDir, extra) {
  const failed = (r.code !== 0 && !r.timedOut) || m.model_calls === 0
  const hits = errText.split('\n').filter(l => THROTTLE_RE.test(l)).length
  const errChannel = failed || m.lastStop === 'error' ? `${errText}\n${m.errors.join('\n')}` : ''
  const throttled = !r.timedOut && THROTTLE_RE.test(errChannel)
  const login_error = !throttled && !r.timedOut && LOGIN_RE.test(errChannel)
  const msg = (errChannel.split('\n').find(l => THROTTLE_RE.test(l) || LOGIN_RE.test(l)) || '').slice(0, 200)
  return {
    ok: r.code === 0 && !r.timedOut, code: r.code, timedOut: r.timedOut, wall_ms: r.wall_ms, samples: r.samples,
    model_calls: m.model_calls, input_total: m.input + m.cache_read + m.cache_write, cache_read: m.cache_read, cache_write: m.cache_write,
    output: m.output, tool_names: m.tool_names, tool_errors: m.tool_errors, models: [...m.models],
    throttled, login_error, err_msg: msg, log_hits: hits, log_dir: logDir, ...extra,
  }
}

async function codexHermesAttempt(sandbox, prompt, home, logDir) {
  const hh = path.join(home, 'hermes')
  const orig = putLogin('hermes', hh)
  const env = Object.assign(cleanEnv(home), { HERMES_HOME: hh, TERMINAL_ENV: 'local', PYTHONDONTWRITEBYTECODE: '1', PYTHONPATH: cfg.HERMES_STOCK_SRC, ...cargoEnv() })
  fs.mkdirSync(logDir, { recursive: true })
  let r, refreshed = false
  try {
    const t0 = Date.now()
    r = await runWithRss(cfg.HERMES_VENV_PY,
      ['-m', 'hermes_cli.main', '--yolo', 'chat', '-Q', '--provider', 'openai-codex', '-m', cfg.MODEL, '-q', prompt],
      { cwd: sandbox, env, timeoutMs: cfg.TURN_TIMEOUT_MS, marker: (_p, cmd) => cmd.includes(home) })
    r.wall_ms = Date.now() - t0
    spawnSync(cfg.HERMES_VENV_PY, ['-m', 'hermes_cli.main', 'sessions', 'export', path.join(logDir, 'hermes-sessions.jsonl')], { env, cwd: sandbox, timeout: 120_000, stdio: 'ignore' })
  } finally { refreshed = pullLoginBack('hermes', hh, orig) }
  fs.writeFileSync(path.join(logDir, 'hermes-stdout.txt'), r.out)
  fs.writeFileSync(path.join(logDir, 'hermes-stderr.txt'), r.err)
  copyLogs(path.join(hh, 'logs'), path.join(logDir, 'hermes-logs'))
  const m = parseHermesExport(path.join(logDir, 'hermes-sessions.jsonl'))
  const logText = ['agent.log', 'errors.log'].map(f => { try { return fs.readFileSync(path.join(hh, 'logs', f), 'utf8') } catch { return '' } }).join('\n')
  const errText = `${r.err}\n${(r.code !== 0 || m.model_calls === 0) ? r.out.slice(-4000) : ''}\n${logText.split('\n').filter(l => /rate|429|limit|quota|401|unauthor/i.test(l)).join('\n')}`
  return finishAttempt('hermes', r, m, errText, logDir, { login_refreshed: refreshed })
}

async function codexPrimeAttempt(sandbox, prompt, home, logDir) {
  const agentDir = path.join(home, 'prime-agent')
  const sessDir = path.join(home, 'prime-sessions')
  const orig = putLogin('prime', agentDir)
  const env = Object.assign(cleanEnv(home, false), { PRIME_AGENT_CODING_AGENT_DIR: agentDir, PRIME_AGENT_KERNEL_VENV: cfg.PRIME_KERNEL_VENV, PYTHONDONTWRITEBYTECODE: '1', ...cargoEnv() })
  const sockDir = shortDir('pd-') // AF_UNIX paths are capped at 104 bytes on macOS
  fs.mkdirSync(logDir, { recursive: true })
  let r, refreshed = false
  try {
    const t0 = Date.now()
    r = await runWithRss('node',
      [cfg.PRIME_CLI, '--provider', 'openai-codex', '--model', cfg.MODEL, '--offline', '--mode', 'json', '--session-dir', sessDir, '--daemon-socket', `${sockDir}/d.sock`, '-p', prompt],
      { cwd: sandbox, env, timeoutMs: cfg.TURN_TIMEOUT_MS, daemonName: 'prime-agent' })
    r.wall_ms = Date.now() - t0
  } finally { refreshed = pullLoginBack('prime', agentDir, orig); fs.rmSync(sockDir, { recursive: true, force: true }) }
  fs.writeFileSync(path.join(logDir, 'prime-events.jsonl'), r.out)
  fs.writeFileSync(path.join(logDir, 'prime-stderr.txt'), r.err)
  copyLogs(sessDir, path.join(logDir, 'prime-sessions'))
  const m = parsePrimeSessions(sessDir)
  // events the harness itself printed about errors / auto-retries (internal retries that later succeeded still count as hits)
  const evText = r.out.split('\n').filter(l => /auto_retry|errorMessage|"error"/.test(l)).join('\n')
  return finishAttempt('prime', r, m, `${r.err}\n${evText}`, logDir, { login_refreshed: refreshed })
}

/** The agent runs with a private HOME; keep the real cargo registry reachable so its own `cargo test` works (same for both arms). */
function cargoEnv() {
  const c = path.join(os.homedir(), '.cargo')
  return fs.existsSync(c) ? { CARGO_HOME: c } : {}
}

/** One turn with throttle handling: snapshot the sandbox, run, and on a subscription throttle restore the
 *  snapshot, back off, and run the same turn again. Only the successful attempt's metrics are returned. */
async function codexTurn(arm, sandbox, prompt, label, ctx) {
  const snap = path.join(path.dirname(sandbox), `.snap-${label}`)
  fs.rmSync(snap, { recursive: true, force: true })
  spawnSync('cp', ['-cR', sandbox, snap])
  const events = []
  let waited = 0
  try {
    for (let attempt = 0; ; attempt++) {
      const home = path.join(cfg.OUT, 'homes', arm, `${ctx.run_id}.${label}.a${attempt}`)
      fs.rmSync(home, { recursive: true, force: true })
      fs.mkdirSync(home, { recursive: true, mode: 0o700 })
      const logDir = path.join(cfg.OUT, 'results', arm, 'logs', ctx.run_id, `${label}.a${attempt}`)
      let t
      try { t = await (arm === 'hermes' ? codexHermesAttempt : codexPrimeAttempt)(sandbox, prompt, home, logDir) }
      finally { fs.rmSync(home, { recursive: true, force: true }) }
      if (t.login_error) throw new BenchAbort(`LOGIN: ${arm} login rejected (${t.err_msg.replace(/\S{30,}/g, '<redacted>')}). Re-login required; run aborted, exercise ${ctx.run_id} not recorded.`)
      if (!t.throttled) return { ...t, throttle_events: events, throttle_wait_ms: waited, attempts: attempt + 1 }
      events.push({ attempt, at: new Date().toISOString(), msg: t.err_msg.replace(/\S{30,}/g, '<redacted>') })
      say(arm, ctx.run_id, label, `THROTTLED (attempt ${attempt + 1}): ${t.err_msg.slice(0, 100)}`)
      if (attempt + 1 >= THROTTLE_MAX) throw new BenchAbort(`THROTTLE: ${arm} still rate-limited after ${THROTTLE_MAX} attempts on ${ctx.run_id}; exercise not recorded, rerun later to resume.`)
      const wait = Math.min(15 * 60_000, THROTTLE_BASE_MS * 2 ** attempt)
      ctx.onWait?.(wait, events.length)
      await sleep(wait)
      waited += wait
      fs.rmSync(sandbox, { recursive: true, force: true })
      spawnSync('cp', ['-cR', snap, sandbox])
    }
  } finally { fs.rmSync(snap, { recursive: true, force: true }) }
}

function writeStatus(arm, patch) {
  const f = path.join(cfg.OUT, `status-${arm}.json`)
  let cur = {}
  try { cur = JSON.parse(fs.readFileSync(f, 'utf8')) } catch {}
  cur = { ...cur, ...patch, arm, updated: new Date().toISOString() }
  fs.writeFileSync(f + '.tmp', JSON.stringify(cur, null, 2))
  fs.renameSync(f + '.tmp', f)
  const merged = {}
  for (const a of ['prime', 'hermes']) { try { merged[a] = JSON.parse(fs.readFileSync(path.join(cfg.OUT, `status-${a}.json`), 'utf8')) } catch {} }
  try { // best effort: the other arm's process may be merging at the same time
    const tmp = path.join(cfg.OUT, `status.json.${process.pid}.tmp`)
    fs.writeFileSync(tmp, JSON.stringify(merged, null, 2))
    fs.renameSync(tmp, path.join(cfg.OUT, 'status.json'))
  } catch {}
  return cur
}

async function runArmCodex(arm, only, langFilter) {
  if (arm !== 'hermes' && arm !== 'prime') throw new Error('codex mode supports the hermes and prime arms only')
  if (!cfg.HERMES_STOCK_SRC && arm === 'hermes') throw new Error('codex mode needs HERMES_STOCK_SRC (pristine Hermes source)')
  if (arm === 'prime' && !fs.existsSync(cfg.PRIME_CLI)) throw new Error(`Prime CLI missing at ${cfg.PRIME_CLI}`)
  const outDir = path.join(cfg.OUT, 'results', arm)
  fs.mkdirSync(outDir, { recursive: true })
  const metaPath = path.join(outDir, 'meta.jsonl')
  const done = loadDone(metaPath)
  const list = EXERCISES.filter(e => selected(e.lang, e.name, only, langFilter))
  prewarm(list)
  let n_done = list.filter(e => done.has(`${e.lang}-${e.name}`)).length, n_pass1 = 0, n_pass = 0
  for (const l of fs.existsSync(metaPath) ? readJsonl(metaPath) : []) { n_pass1 += (l.pass1 ? 1 : 0); n_pass += (l.pass ? 1 : 0) }
  const st = patch => writeStatus(arm, patch)
  st({ state: 'running', total: list.length, done: n_done, pass1: n_pass1, pass2: n_pass, started: new Date().toISOString(), model: cfg.MODEL, error: null })
  try {
    for (const { lang, name } of list) {
      const run_id = `${lang}-${name}`
      if (done.has(run_id)) continue
      const parent = path.join(cfg.OUT, 'runs', arm, run_id)
      const sandbox = copySandbox(lang, name, parent)
      st({ current: run_id, phase: 'turn1', throttle_wait: null })
      const ctx = { run_id, onWait: (ms, k) => st({ phase: `throttled (wait ${Math.round(ms / 1000)}s, event ${k})` }) }
      say(arm, run_id, 'turn 1...')
      const turns = [await codexTurn(arm, sandbox, buildPrompt(sandbox, null), 'turn1', ctx)]
      st({ phase: 'test1' })
      const test1 = runTests(lang, sandbox, name)
      let test = test1
      if (!test1.pass) {
        say(arm, run_id, 'turn 2 (retry with test output)...')
        st({ phase: 'turn2' })
        turns.push(await codexTurn(arm, sandbox, buildPrompt(sandbox, test1.output), 'turn2', ctx))
        st({ phase: 'test2' })
        test = runTests(lang, sandbox, name)
      }
      const samples = turns.flatMap(t => t.samples)
      const tn = {}
      for (const t of turns) for (const [k, v] of Object.entries(t.tool_names)) tn[k] = (tn[k] || 0) + v
      const prompt_tokens = sum(turns.map(t => t.input_total)), cached_tokens = sum(turns.map(t => t.cache_read)), completion_tokens = sum(turns.map(t => t.output))
      const rec = {
        run_id, lang, name, arm, provider: 'openai-codex', model: cfg.MODEL, models_seen: [...new Set(turns.flatMap(t => t.models))], retries: turns.length - 1,
        pass1: test1.pass, pass: test.pass, tests_tampered: [...new Set([...test1.tampered, ...(test.tampered || [])])], agent_ok: turns.every(t => t.ok),
        model_calls: sum(turns.map(t => t.model_calls)), prompt_tokens, cached_tokens, completion_tokens, cache_write_tokens: sum(turns.map(t => t.cache_write)),
        cost_usd: null,
        cost_est_usd: Math.round(((prompt_tokens - cached_tokens) * CODEX_EST.input + cached_tokens * CODEX_EST.cached + completion_tokens * CODEX_EST.output)) / 1e6,
        cost_est_basis: CODEX_EST_BASIS,
        tool_calls: sum(Object.values(tn)), tool_errors: turns.reduce((s, t) => addNullable(s, t.tool_errors), 0), tool_names: tn,
        wall_ms: sum(turns.map(t => t.wall_ms)),
        turns_timed_out: turns.filter(t => t.timedOut).length,
        throttle_events: turns.flatMap(t => t.throttle_events), throttle_wait_ms: sum(turns.map(t => t.throttle_wait_ms)), throttle_log_hits: sum(turns.map(t => t.log_hits)),
        login_refreshed: turns.some(t => t.login_refreshed),
        rss_peak_mib: samples.length ? Math.round(Math.max(...samples) * 10) / 10 : null,
        rss_mean_mib: samples.length ? Math.round((sum(samples) / samples.length) * 10) / 10 : null,
        log_dir: path.relative(cfg.OUT, path.join(outDir, 'logs', run_id)),
      }
      fs.appendFileSync(metaPath, JSON.stringify(rec) + '\n')
      say(arm, run_id, `pass@1=${rec.pass1} pass@2=${rec.pass} calls=${rec.model_calls} tools=${rec.tool_calls} tok=${prompt_tokens}/${cached_tokens}/${completion_tokens} throttle=${rec.throttle_events.length} ${rec.wall_ms}ms`)
      n_done++; n_pass1 += rec.pass1 ? 1 : 0; n_pass += rec.pass ? 1 : 0
      st({ done: n_done, pass1: n_pass1, pass2: n_pass, last: { run_id, pass1: rec.pass1, pass: rec.pass, wall_ms: rec.wall_ms, model_calls: rec.model_calls, tool_calls: rec.tool_calls, throttle_events: rec.throttle_events.length }, phase: 'idle' })
      reapStragglers(parent)
      fs.rmSync(parent, { recursive: true, force: true })
    }
    st({ state: 'finished', current: null, phase: null })
  } catch (e) {
    st({ state: e instanceof BenchAbort ? 'aborted' : 'crashed', error: String(e.message).slice(0, 400) })
    throw e
  }
}

// ---------------------------------------------------------------- report
function summarize(rows) {
  const n = rows.length
  const pct = k => `${((100 * rows.filter(r => r[k]).length) / n).toFixed(0)}% (${rows.filter(r => r[k]).length}/${n})`
  const cost = rows.every(r => r.cost_usd !== null && r.cost_usd !== undefined) ? sum(rows.map(r => r.cost_usd)).toFixed(4) : 'n/a'
  const rss = rows.filter(r => r.rss_peak_mib !== null && r.rss_peak_mib !== undefined)
  return {
    n, pass1: pct('pass1'), pass2: pct('pass'),
    calls: sum(rows.map(r => r.model_calls)), prompt: sum(rows.map(r => r.prompt_tokens)),
    cached: sum(rows.map(r => r.cached_tokens)), completion: sum(rows.map(r => r.completion_tokens)),
    cost, wall_s: (sum(rows.map(r => r.wall_ms)) / 1000).toFixed(0),
    rss_peak: rss.length ? Math.max(...rss.map(r => r.rss_peak_mib)).toFixed(0) : 'n/a',
    rss_mean: rss.length ? (sum(rss.map(r => r.rss_mean_mib)) / rss.length).toFixed(0) : 'n/a',
    // Turns that did not finish cleanly (cut at the turn limit, or the agent errored).
    cut: rows.filter(r => r.agent_ok === false).length,
  }
}

// Report names: the Factr-I arm keeps its on-disk id `sovereign` (resume depends on it).
const ARM_LABEL = { sovereign: 'Factr-I', prime: 'Prime', hermes: 'Hermes (stock)' }

function report() {
  const rowsByArm = {}
  for (const arm of ARMS) {
    const metaPath = path.join(cfg.OUT, 'results', arm, 'meta.jsonl')
    rowsByArm[arm] = fs.existsSync(metaPath)
      ? fs.readFileSync(metaPath, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l)).map(r => ({ pass1: r.pass, ...r }))
      : []
  }
  const head = '| arm | n | pass@1 | pass@2 | model_calls | prompt_tok | cached_tok | completion_tok | cost_usd | wall_s | rss_peak_MiB | rss_mean_MiB | turns_cut |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|'
  const line = (label, s) => `| ${label} | ${s.n} | ${s.pass1} | ${s.pass2} | ${s.calls} | ${s.prompt} | ${s.cached} | ${s.completion} | ${s.cost} | ${s.wall_s} | ${s.rss_peak} | ${s.rss_mean} | ${s.cut} |`
  console.log('Summary (totals over all exercises; rss_peak = max, rss_mean = mean of per-exercise means)\n')
  console.log(head)
  for (const arm of ARMS) if (rowsByArm[arm].length) console.log(line(ARM_LABEL[arm] || arm, summarize(rowsByArm[arm])))
  console.log('\nPer language\n')
  console.log(head.replace('| arm |', '| lang / arm |'))
  for (const lang of selection.languages) {
    for (const arm of ARMS) {
      const rows = rowsByArm[arm].filter(r => r.lang === lang)
      if (rows.length) console.log(line(`${lang} / ${ARM_LABEL[arm] || arm}`, summarize(rows)))
    }
  }
}

// ---------------------------------------------------------------- verify (no model): stub fails, reference passes
function verify(only, langFilter) {
  const list = EXERCISES.filter(e => selected(e.lang, e.name, only, langFilter))
  prewarm(list)
  fs.mkdirSync(cfg.OUT, { recursive: true })
  let bad = 0
  for (const { lang, name } of list) {
    const parent = path.join(cfg.OUT, 'verify', `${lang}-${name}`)
    const box = copySandbox(lang, name, parent)
    const stub = runTests(lang, box, name)
    applyReference(lang, box, name)
    const ref = runTests(lang, box, name)
    const ok = ref.pass // stub already passing = refactoring exercise (go counter/ledger/markdown, java ledger/tree-building, js ledger): kept, flagged
    if (!ok) bad++
    const rec = { lang, name, stub_fails: !stub.pass, reference_passes: ref.pass, ok }
    fs.appendFileSync(path.join(cfg.OUT, 'verify.jsonl'), JSON.stringify(rec) + '\n')
    say(ok ? (stub.pass ? 'OK*' : 'OK ') : 'BAD', `${lang}/${name}`, `stub_fails=${!stub.pass} reference_passes=${ref.pass}`)
    if (!ok) say((ref.pass ? stub.output : ref.output).slice(-1200))
    else fs.rmSync(parent, { recursive: true, force: true })
  }
  say(`verify: ${list.length - bad}/${list.length} ok`)
  if (bad) process.exitCode = 1
}

// ---------------------------------------------------------------- main
function resolvedConfig() {
  return {
    ...cfg,
    exercise_count: EXERCISES.length,
    order: EXERCISES.map(e => `${e.lang}/${e.name}`),
    arms: ARMS,
    hermes_source: cfg.HERMES_STOCK_SRC || 'fork (HERMES_STOCK_SRC unset)',
    prime_cli_present: fs.existsSync(cfg.PRIME_CLI),
    toolchains: Object.fromEntries(
      ['python3', 'cargo', 'g++', 'clang++', 'cmake', 'go', 'java', 'node', 'npm'].map(t => [t, spawnSync(t, [t === 'java' ? '-version' : t === 'go' ? 'version' : '--version'], { stdio: 'ignore', env: { ...process.env, ...toolEnv() } }).status === 0])
    ),
  }
}

async function main() {
  if (DRY_RUN || !cmd) {
    console.log(JSON.stringify(resolvedConfig(), null, 2))
    return
  }
  const langFilter = flag('lang', null)
  const only = flag('only', null)?.split(',') || null
  if (langFilter && !selection.languages.includes(langFilter)) throw new Error(`--lang must be one of ${selection.languages}`)
  if (['run', 'verify', 'prewarm'].includes(cmd) && !EXERCISES.some(e => selected(e.lang, e.name, only, langFilter))) throw new Error('--lang/--only matched no exercise')
  if (cmd === 'prewarm') {
    prewarm(EXERCISES.filter(e => selected(e.lang, e.name, only, langFilter)))
    return
  }
  if (cmd === 'verify') return verify(only, langFilter)
  if (cmd === 'run') {
    const arm = flag('arm', null)
    if (!ARMS.includes(arm)) throw new Error(`usage: polyglot.mjs run --arm ${ARMS.join('|')} [--lang l] [--only name1,...]`)
    fs.mkdirSync(cfg.OUT, { recursive: true })
    if (cfg.CODEX) {
      fs.writeFileSync(path.join(cfg.OUT, `config-${arm}.json`), JSON.stringify({ ...resolvedConfig(), started: new Date().toISOString() }, null, 2))
      await runArmCodex(arm, only, langFilter)
      return
    }
    fs.writeFileSync(path.join(cfg.OUT, 'config.json'), JSON.stringify({ ...resolvedConfig(), started: new Date().toISOString() }, null, 2))
    await runArm(arm, only, langFilter)
    return
  }
  if (cmd === 'report') {
    report()
    return
  }
  console.log(`usage: polyglot.mjs [--dry-run] | prewarm | verify | run --arm <${ARMS.join('|')}> [--lang l] [--only a,b] [--set 40|full] | report`)
  process.exitCode = 2
}

main().catch(err => {
  console.error(err)
  process.exit(1)
})
