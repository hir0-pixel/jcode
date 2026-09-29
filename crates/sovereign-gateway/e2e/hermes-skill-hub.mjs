#!/usr/bin/env node
// Prove Hermes hub install/removal is reflected by an already-running engine.
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const engineBin = process.env.SOVEREIGN_BIN || path.resolve('target/release/sovereign')
const hermesRoot = process.env.HERMES_REPO || path.resolve('../hermes-agent')
const python = process.env.HERMES_PYTHON || path.join(hermesRoot, '.venv/bin/python')
const model = process.env.E2E_MODEL || 'sovereign/bench-hermes-64k:latest'
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-skill-hub-'))
const home = path.join(root, 'home')
const hermesHome = path.join(root, '.hermes')
const jcodeHome = path.join(root, '.jcode')
for (const dir of [home, hermesHome, jcodeHome]) fs.mkdirSync(dir, { recursive: true })
const token = crypto.randomBytes(24).toString('hex')
fs.writeFileSync(path.join(jcodeHome, 'config.toml'),
  `[provider]\ndefault_provider = "local"\n\n[providers.local]\ntype = "openai-compatible"\nbase_url = "http://127.0.0.1:11434/v1"\napi_key = "ollama"\nrequires_api_key = false\ndefault_model = "${model}"\n\n[[providers.local.models]]\nid = "${model}"\ncontext_window = 65536\n`)
const env = {
  ...process.env,
  HOME: home,
  HERMES_HOME: hermesHome,
  JCODE_HOME: jcodeHome,
  HERMES_DASHBOARD_SESSION_TOKEN: token,
  SOVEREIGN_ENGINE_URL: 'http://127.0.0.1:1',
  SOVEREIGN_PROVIDER: 'local',
  SOVEREIGN_MODEL: model,
  PYTHONDONTWRITEBYTECODE: '1',
}
const children = []
function start(command, args, cwd, childEnv) {
  const child = spawn(command, args, { cwd, env: childEnv, stdio: ['ignore', 'pipe', 'pipe'] })
  children.push(child)
  let output = ''
  child.stdout.on('data', chunk => { output += chunk })
  child.stderr.on('data', chunk => { output += chunk })
  return { child, output: () => output }
}
async function ready(proc, marker) {
  const deadline = Date.now() + 120_000
  while (Date.now() < deadline) {
    const match = proc.output().match(new RegExp(`${marker} port=(\\d+)`))
    if (match) return Number(match[1])
    if (proc.child.exitCode !== null) throw new Error(`${marker} exited: ${proc.output()}`)
    await new Promise(resolve => setTimeout(resolve, 100))
  }
  throw new Error(`timed out waiting for ${marker}: ${proc.output()}`)
}
function actionName(verb, key) {
  const slug = key.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '').slice(0, 48) || 'skill'
  const hash = crypto.createHash('sha1').update(key).digest('hex').slice(0, 8)
  return `skills-${verb}-${slug}-${hash}`
}
async function waitAction(base, name) {
  const deadline = Date.now() + 60_000
  while (Date.now() < deadline) {
    const response = await fetch(`${base}/api/actions/${name}/status`, {
      headers: { 'X-Hermes-Session-Token': token },
    })
    assert.equal(response.status, 200, `action status ${name}`)
    const status = await response.json()
    if (!status.running) {
      assert.equal(status.exit_code, 0, `action failed: ${JSON.stringify(status)}`)
      return status
    }
    await new Promise(resolve => setTimeout(resolve, 150))
  }
  throw new Error(`action ${name} did not finish`)
}
async function engineSkills(base) {
  const response = await fetch(`${base}/api/skills`, { headers: { Authorization: `Bearer ${token}` } })
  assert.equal(response.status, 200)
  return response.json()
}

try {
  const hermes = start(python, ['-m', 'hermes_cli.main', 'serve', '--host', '127.0.0.1', '--port', '0', '--skip-build'], hermesRoot, env)
  const hermesPort = await ready(hermes, 'HERMES_BACKEND_READY')
  const base = `http://127.0.0.1:${hermesPort}`
  const identifier = 'official/autonomous-ai-agents/agent-merge-conflict-arbiter'
  const name = 'agent-merge-conflict-arbiter'
  const installedFile = path.join(jcodeHome, 'skills', 'autonomous-ai-agents', name, 'SKILL.md')
  let response = await fetch(`${base}/api/skills/hub/install`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ identifier }),
  })
  assert.equal(response.status, 200, await response.text())
  await waitAction(base, actionName('install', identifier))
  assert.ok(fs.existsSync(installedFile), 'hub install writes the shared JCODE_HOME skill')

  const engine = start(engineBin, ['--provider', 'openai-compatible', '--model', model, 'serve', '--host', '127.0.0.1', '--port', '0'], home, env)
  const enginePort = await ready(engine, 'HERMES_BACKEND_READY')
  let skills = await engineSkills(`http://127.0.0.1:${enginePort}`)
  assert.ok(skills.some(skill => skill.name === name), 'running engine sees installed hub skill')

  response = await fetch(`${base}/api/skills/hub/uninstall`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ name }),
  })
  assert.equal(response.status, 200, await response.text())
  const result = await waitAction(base, actionName('uninstall', name))
  assert.ok(result.lines.join('\n').includes(`Uninstalled '${name}'`), 'Hub confirms removal')
  assert.ok(!fs.existsSync(installedFile), 'hub uninstall removes the shared skill file')
  skills = await engineSkills(`http://127.0.0.1:${enginePort}`)
  assert.ok(!skills.some(skill => skill.name === name), 'running engine refreshes its registry after removal')
  console.log('PASS Hermes skills hub install and uninstall are visible to a running Rust engine')
} finally {
  for (const child of [...children].reverse()) if (child.exitCode === null) child.kill('SIGTERM')
  await Promise.all(children.map(child => new Promise(resolve => {
    if (child.exitCode !== null) return resolve()
    child.once('exit', resolve)
    setTimeout(() => { child.kill('SIGTERM'); resolve() }, 5000)
  })))
  fs.rmSync(root, { recursive: true, force: true })
}
