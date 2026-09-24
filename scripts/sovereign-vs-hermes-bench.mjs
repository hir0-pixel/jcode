#!/usr/bin/env node
/**
 * Run one identical prompt through stock Hermes and Sovereign, both via the
 * counting proxy. Writes docs/benchmark-runs/<stamp>/{hermes,sovereign,summary}.json
 *
 * Prereq: Ollama up with qwen3.8:27b (or SOVEREIGN_BENCH_MODEL).
 * Stock Hermes refuses windows < 64k — uses a dedicated 64k alias.
 * Sovereign uses the product default 32k alias.
 */
import { spawn, spawnSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import os from 'node:os'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const engineRoot = path.resolve(__dirname, '..')
const baseModel = process.env.SOVEREIGN_BENCH_MODEL || 'qwen3.8:27b'
const alias =
  process.env.SOVEREIGN_BENCH_ALIAS ||
  `sovereign/${baseModel.replace(':', '-')}:latest`
const hermesAlias =
  process.env.SOVEREIGN_BENCH_HERMES_ALIAS || 'sovereign/bench-hermes-64k:latest'
const hermesNumCtx = Number(process.env.SOVEREIGN_BENCH_HERMES_NUM_CTX || 65536)
const prompt = process.env.SOVEREIGN_BENCH_PROMPT || 'Reply with exactly one word: pong'
const proxyListen = process.env.SOVEREIGN_PROXY_LISTEN || '127.0.0.1:18080'
const upstream = process.env.SOVEREIGN_PROXY_UPSTREAM || 'http://127.0.0.1:11434'
const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19)
const outDir = process.env.SOVEREIGN_BENCH_OUT || path.join(engineRoot, 'docs/benchmark-runs', stamp)
fs.mkdirSync(outDir, { recursive: true })

const hermesBin = process.env.HERMES_BIN || 'hermes'
const sovereignBin =
  process.env.SOVEREIGN_BIN || path.join(engineRoot, 'target/release/sovereign')

function sleep(ms) {
  return new Promise(r => setTimeout(r, ms))
}

function curlJson(url, body, timeout = 120_000) {
  return spawnSync(
    'curl',
    ['-s', url, '-d', JSON.stringify(body)],
    { encoding: 'utf8', timeout }
  )
}

function startProxy(statsFile) {
  const script = path.join(__dirname, 'sovereign-counting-proxy.mjs')
  return spawn(process.execPath, [script], {
    env: {
      ...process.env,
      SOVEREIGN_PROXY_LISTEN: proxyListen,
      SOVEREIGN_PROXY_UPSTREAM: upstream,
      SOVEREIGN_PROXY_STATS: statsFile,
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  })
}

function readStats(p) {
  return JSON.parse(fs.readFileSync(p, 'utf8'))
}

function ensureAlias(name, numCtx, unloadOther) {
  console.error(`Ensuring alias ${name} num_ctx=${numCtx}…`)
  if (unloadOther) {
    curlJson(`${upstream}/api/generate`, { model: unloadOther, keep_alive: 0 }, 60_000)
  }
  curlJson(`${upstream}/api/create`, {
    model: name,
    from: baseModel,
    stream: false,
    parameters: { num_ctx: numCtx },
  })
  const warm = curlJson(
    `${upstream}/api/generate`,
    { model: name, prompt: '.', stream: false, keep_alive: '10m' },
    600_000
  )
  console.error(`  warm bytes=${(warm.stdout || '').length}`)
}

async function runHermes(statsFile) {
  ensureAlias(hermesAlias, hermesNumCtx, alias)
  const proxy = startProxy(statsFile)
  await sleep(500)
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'bench-hermes-'))
  const hermesHome = path.join(home, 'hermes')
  fs.mkdirSync(hermesHome, { recursive: true })
  fs.writeFileSync(
    path.join(hermesHome, 'config.yaml'),
    [
      'model:',
      `  default: "${hermesAlias}"`,
      '  provider: custom',
      `  base_url: "http://${proxyListen}/v1"`,
      '  api_key: "ollama"',
      `  ollama_num_ctx: ${hermesNumCtx}`,
      `  context_length: ${hermesNumCtx}`,
      'terminal:',
      '  backend: local',
      '',
    ].join('\n')
  )
  fs.writeFileSync(path.join(hermesHome, '.env'), 'OPENAI_API_KEY=ollama\n')

  const started = Date.now()
  const result = spawnSync(
    hermesBin,
    ['-z', prompt, '--provider', 'custom', '-m', hermesAlias, '--yolo'],
    {
      env: {
        ...process.env,
        HOME: home,
        HERMES_HOME: hermesHome,
        OPENAI_API_KEY: 'ollama',
      },
      encoding: 'utf8',
      timeout: 600_000,
      maxBuffer: 8 * 1024 * 1024,
    }
  )
  const elapsed_ms = Date.now() - started
  await sleep(400)
  proxy.kill('SIGTERM')
  await sleep(200)
  return {
    kind: 'stock-hermes',
    ok: result.status === 0,
    status: result.status,
    elapsed_ms,
    stdout: (result.stdout || '').slice(0, 8000),
    stderr: (result.stderr || '').slice(0, 8000),
    stats: fs.existsSync(statsFile) ? readStats(statsFile) : null,
  }
}

async function runSovereign(statsFile) {
  ensureAlias(alias, 32768, hermesAlias)
  const proxy = startProxy(statsFile)
  await sleep(500)
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'bench-sov-'))
  const jcodeHome = path.join(home, '.jcode')
  fs.mkdirSync(jcodeHome, { recursive: true })

  const loginEnv = {
    ...process.env,
    HOME: home,
    JCODE_HOME: jcodeHome,
    JCODE_OPENAI_COMPAT_API_BASE: `http://${proxyListen}/v1`,
    JCODE_OPENAI_COMPAT_API_KEY: 'ollama',
  }
  const login = spawnSync(
    sovereignBin,
    [
      'login',
      'openai-compatible',
      '--api-key',
      'ollama',
      '--api-base',
      `http://${proxyListen}/v1`,
    ],
    { env: loginEnv, encoding: 'utf8', timeout: 60_000 }
  )

  const started = Date.now()
  const result = spawnSync(
    sovereignBin,
    ['-p', 'openai-compatible', '-m', alias, 'run', '--json', prompt],
    {
      env: {
        ...loginEnv,
        SOVEREIGN_OLLAMA_NUM_CTX: '32768',
      },
      encoding: 'utf8',
      timeout: 600_000,
      maxBuffer: 8 * 1024 * 1024,
    }
  )
  const elapsed_ms = Date.now() - started
  await sleep(500)
  proxy.kill('SIGTERM')
  await sleep(200)
  return {
    kind: 'sovereign',
    ok: result.status === 0,
    status: result.status,
    elapsed_ms,
    stdout: (result.stdout || '').slice(0, 8000),
    stderr: (result.stderr || '').slice(0, 8000),
    login_status: login.status,
    login_stderr: (login.stderr || '').slice(0, 2000),
    stats: fs.existsSync(statsFile) ? readStats(statsFile) : null,
  }
}

function summarize(hermes, sovereign) {
  const h = hermes.stats || {}
  const s = sovereign.stats || {}
  const hCalls = (h.chat_completions || 0) + (h.api_chat || 0)
  const sCalls = (s.chat_completions || 0) + (s.api_chat || 0)
  return {
    stamp,
    baseModel,
    alias,
    hermesAlias,
    hermesNumCtx,
    prompt,
    note:
      'Counting proxy on OpenAI-compat /v1. Hermes requires ≥64k (MINIMUM_CONTEXT_LENGTH) so it uses a dedicated 64k alias; Sovereign uses the product default 32k alias.',
    hermes: {
      ok: hermes.ok,
      elapsed_ms: hermes.elapsed_ms,
      model_http_calls: hCalls,
      chat_completions: h.chat_completions || 0,
      prompt_tokens: h.prompt_tokens || h.prompt_eval_count || 0,
      completion_tokens: h.completion_tokens || h.eval_count || 0,
      paths: h.paths || {},
    },
    sovereign: {
      ok: sovereign.ok,
      elapsed_ms: sovereign.elapsed_ms,
      model_http_calls: sCalls,
      chat_completions: s.chat_completions || 0,
      prompt_tokens: s.prompt_tokens || s.prompt_eval_count || 0,
      completion_tokens: s.completion_tokens || s.eval_count || 0,
      paths: s.paths || {},
    },
    delta: {
      model_http_calls: hCalls - sCalls,
      prompt_tokens: (h.prompt_tokens || 0) - (s.prompt_tokens || 0),
      elapsed_ms: hermes.elapsed_ms - sovereign.elapsed_ms,
    },
  }
}

const hermesStats = path.join(outDir, 'hermes-proxy-stats.json')
const sovStats = path.join(outDir, 'sovereign-proxy-stats.json')

console.error('Checking Ollama…')
if (spawnSync('curl', ['-s', `${upstream}/api/tags`], { encoding: 'utf8' }).status !== 0) {
  console.error('Ollama not reachable at', upstream)
  process.exit(1)
}

console.error('=== stock Hermes ===')
const hermes = await runHermes(hermesStats)
fs.writeFileSync(path.join(outDir, 'hermes.json'), JSON.stringify(hermes, null, 2))
console.error('hermes ok=', hermes.ok, 'calls=', hermes.stats?.chat_completions, 'tokens=', hermes.stats?.prompt_tokens)

console.error('=== Sovereign ===')
const sovereign = await runSovereign(sovStats)
fs.writeFileSync(path.join(outDir, 'sovereign.json'), JSON.stringify(sovereign, null, 2))
console.error('sovereign ok=', sovereign.ok, 'calls=', sovereign.stats?.chat_completions, 'tokens=', sovereign.stats?.prompt_tokens)

const summary = summarize(hermes, sovereign)
fs.writeFileSync(path.join(outDir, 'summary.json'), JSON.stringify(summary, null, 2))
console.log(JSON.stringify(summary, null, 2))
console.error('Wrote', outDir)
