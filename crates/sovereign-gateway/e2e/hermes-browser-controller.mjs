#!/usr/bin/env node
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import http from 'node:http'
import os from 'node:os'
import path from 'node:path'

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-browser-e2e-'))
const home = path.join(root, 'home')
const hermesHome = path.join(root, '.hermes')
const jcodeHome = path.join(root, '.jcode')
for (const directory of [home, hermesHome, jcodeHome]) fs.mkdirSync(directory, { recursive: true })
const model = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
fs.writeFileSync(path.join(jcodeHome, 'config.toml'),
  `[provider]\ndefault_provider = "local"\n\n[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:11434/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${model}"\nsupports_reasoning_effort = true\n\n[[providers.local.models]]\nid = "${model}"\ncontext_window = 65536\n`)
const token = crypto.randomBytes(24).toString('hex')
const env = { ...process.env, HOME: home, HERMES_HOME: hermesHome, JCODE_HOME: jcodeHome,
  HERMES_DASHBOARD_SESSION_TOKEN: token, PYTHONDONTWRITEBYTECODE: '1',
  SOVEREIGN_HERMES_PYTHON: process.env.HERMES_PYTHON || path.resolve('../hermes-agent/.venv/bin/python'),
  SOVEREIGN_HERMES_PYTHONPATH: path.resolve('../hermes-agent'),
  SOVEREIGN_HERMES_TOOLS_DIR: path.resolve('../hermes-agent/tools') }
const chromium = path.join(os.homedir(), 'Library/Caches/ms-playwright/chromium-1243/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing')
const engineBin = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const hermesRoot = process.env.HERMES_REPO || path.resolve('../hermes-agent')
let engine
let browser
let ws
let fixtureServer

const port = async () => {
  const server = http.createServer()
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  const value = server.address().port
  await new Promise(resolve => server.close(resolve))
  return value
}
const startFixture = async () => {
  const marker = `browser-route-proof-${crypto.randomBytes(8).toString('hex')}`
  fixtureServer = http.createServer((_req, res) => {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' })
    res.end(`<main><h1>Hermes local browser fixture</h1><p>${marker}</p></main>`)
  })
  await new Promise((resolve, reject) => {
    fixtureServer.once('error', reject)
    fixtureServer.listen(0, '127.0.0.1', resolve)
  })
  return { marker, url: `http://127.0.0.1:${fixtureServer.address().port}/` }
}
const waitPort = async (proc, marker) => {
  const until = Date.now() + 60_000
  while (Date.now() < until) {
    const match = proc.output.match(new RegExp(`${marker} port=(\\d+)`))
    if (match) return Number(match[1])
    if (proc.child.exitCode !== null) throw new Error(`${marker} exited: ${proc.output}`)
    await new Promise(resolve => setTimeout(resolve, 50))
  }
  throw new Error(`timed out waiting for ${marker}: ${proc.output}`)
}
try {
  if (!fs.existsSync(chromium)) throw new Error(`cached Chromium is absent: ${chromium}`)
  const fixture = await startFixture()
  fs.writeFileSync(path.join(hermesHome, 'config.yaml'), 'security:\n  allow_private_urls: true\n')
  env.AGENT_BROWSER_EXECUTABLE_PATH = chromium
  const browserPort = await port()
  const browserUrl = `http://127.0.0.1:${browserPort}`
  browser = spawn(chromium, [
    '--headless=new', '--no-first-run', '--no-default-browser-check', '--disable-gpu',
    '--no-sandbox', '--remote-debugging-address=127.0.0.1', `--remote-debugging-port=${browserPort}`,
    `--user-data-dir=${path.join(root, 'chromium-profile')}`, 'about:blank',
  ], { env, stdio: 'ignore', detached: process.platform !== 'win32' })
  const cdpDeadline = Date.now() + 20_000
  while (Date.now() < cdpDeadline) {
    try {
      const response = await fetch(`${browserUrl}/json/version`)
      if (response.ok) break
    } catch {}
    if (browser.exitCode !== null) throw new Error(`Chromium exited with ${browser.exitCode}`)
    await new Promise(resolve => setTimeout(resolve, 100))
  }
  const cdp = await fetch(`${browserUrl}/json/version`)
  if (!cdp.ok) throw new Error(`Chromium CDP did not start: ${cdp.status}`)
  const child = spawn(engineBin, ['--provider', 'openai-compatible', '--model', model,
    'serve', '--host', '127.0.0.1', '--port', '0'], {
    cwd: home, env, stdio: ['ignore', 'pipe', 'pipe'], detached: process.platform !== 'win32',
  })
  engine = { child, output: '' }
  child.stdout.on('data', chunk => { engine.output += chunk })
  child.stderr.on('data', chunk => { engine.output += chunk })
  const enginePort = await waitPort(engine, 'HERMES_BACKEND_READY')
  ws = new WebSocket(`ws://127.0.0.1:${enginePort}/api/ws?token=${token}`)
  const pending = new Map()
  let next = 0
  ws.addEventListener('message', message => {
    const frame = JSON.parse(String(message.data))
    if (frame.id !== undefined && pending.has(frame.id)) {
      pending.get(frame.id)(frame)
      pending.delete(frame.id)
    }
  })
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true })
    ws.addEventListener('error', reject, { once: true })
  })
  const rpc = (method, params) => {
    const id = `browser-${++next}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise(resolve => pending.set(id, resolve))
  }
  const connected = await rpc('browser.manage', { action: 'connect', url: browserUrl })
  if (connected.error || connected.result?.connected !== true || connected.result?.url !== browserUrl) {
    throw new Error(`Hermes browser controller did not connect to Chromium: ${JSON.stringify(connected)}`)
  }
  const status = await rpc('browser.manage', { action: 'status' })
  if (status.error || status.result?.connected !== true || status.result?.url !== browserUrl) {
    throw new Error(`Hermes browser controller status disagrees with connected CDP: ${JSON.stringify(status)}`)
  }
  const disconnected = await rpc('browser.manage', { action: 'disconnect' })
  if (disconnected.error || disconnected.result?.connected !== false) {
    throw new Error(`Hermes browser controller did not release CDP: ${JSON.stringify(disconnected)}`)
  }
  const engineBase = `http://127.0.0.1:${enginePort}`
  const browserAction = async (action, params) => {
    const response = await fetch(`${engineBase}/api/browser/act?feature=1`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'x-hermes-session-token': token },
      body: JSON.stringify({ action, task_id: 'browser-route-e2e', params }),
    })
    return { status: response.status, body: await response.json() }
  }
  const navigation = await browserAction('navigate', { url: fixture.url })
  const snapshot = await browserAction('snapshot', { full: true })
  if (navigation.status !== 200 || !navigation.body.success || navigation.body.url !== fixture.url ||
      !JSON.stringify(navigation.body).includes('Hermes local browser fixture')) {
    throw new Error(`Hermes browser route did not navigate to the local fixture: ${JSON.stringify(navigation)}`)
  }
  if (snapshot.status !== 200 || !snapshot.body.success || !JSON.stringify(snapshot.body).includes(fixture.marker)) {
    throw new Error(`Hermes browser route snapshot did not contain the local fixture marker: ${JSON.stringify(snapshot)}`)
  }
  console.log('PASS browser controller connected, returned CDP status, disconnected, and navigated/read a local page through /api/browser/act')
} finally {
  ws?.close()
  if (fixtureServer) await new Promise(resolve => fixtureServer.close(resolve))
  for (const child of [engine?.child, browser]) {
    if (child && child.exitCode === null) {
      try { process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGTERM') } catch {}
      await Promise.race([
        new Promise(resolve => child.once('exit', resolve)),
        new Promise(resolve => setTimeout(resolve, 5000)),
      ])
      if (child.exitCode === null) {
        try { process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGKILL') } catch {}
        await new Promise(resolve => child.once('exit', resolve))
      }
    }
  }
  fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
}
