#!/usr/bin/env node
import { spawn } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-audio-e2e-'))
const home = path.join(root, 'home')
const hermesHome = path.join(root, '.hermes')
const jcodeHome = path.join(root, '.jcode')
for (const directory of [home, hermesHome, jcodeHome]) fs.mkdirSync(directory, { recursive: true })
const token = crypto.randomBytes(24).toString('hex')
const env = { ...process.env, HOME: home, HERMES_HOME: hermesHome, JCODE_HOME: jcodeHome,
  HERMES_DASHBOARD_SESSION_TOKEN: token, PYTHONDONTWRITEBYTECODE: '1', PORCUPINE_ACCESS_KEY: '' }
const hermesRoot = process.env.HERMES_REPO || path.resolve('../hermes-agent')
const python = process.env.HERMES_PYTHON || path.join(hermesRoot, '.venv/bin/python')
const command = '/usr/bin/say -o {output_path}.aiff -f {input_path} && /usr/bin/afconvert {output_path}.aiff -o {output_path} -f WAVE -d LEI16 && /bin/rm {output_path}.aiff'
fs.writeFileSync(path.join(hermesHome, 'config.yaml'), `security:\n  allow_lazy_installs: false\n# Porcupine with no access key is unavailable on any host; openWakeWord is installed now and would open a real mic.\nwake_word:\n  provider: porcupine\ntts:\n  provider: local-say\n  providers:\n    local-say:\n      type: command\n      format: wav\n      command: "${command}"\n`)
let child
try {
  child = spawn(python, ['-m', 'hermes_cli.main', 'serve', '--host', '127.0.0.1', '--port', '0', '--skip-build'], {
    cwd: hermesRoot, env: { ...env, HERMES_DISABLE_LAZY_INSTALLS: '1' }, stdio: ['ignore', 'pipe', 'pipe'], detached: process.platform !== 'win32',
  })
  let output = ''
  child.stdout.on('data', chunk => { output += chunk })
  child.stderr.on('data', chunk => { output += chunk })
  const deadline = Date.now() + 60_000
  let serverPort
  while (Date.now() < deadline) {
    serverPort = Number(output.match(/HERMES_BACKEND_READY port=(\d+)/)?.[1])
    if (serverPort) break
    if (child.exitCode !== null) throw new Error(`Hermes serve exited: ${output}`)
    await new Promise(resolve => setTimeout(resolve, 50))
  }
  if (!serverPort) throw new Error(`Hermes serve did not become ready: ${output}`)
  const base = `http://127.0.0.1:${serverPort}`
  const response = await fetch(`${base}/api/audio/speak`, {
    method: 'POST', headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
    body: JSON.stringify({ text: 'Akira local speech output check.' }),
  })
  if (!response.ok) throw new Error(`local speech synthesis returned ${response.status}: ${await response.text()}`)
  const audio = await response.json()
  const wav = Buffer.from(audio.data_url?.split(',')[1] || '', 'base64')
  if (audio.mime_type !== 'audio/wav' || audio.provider !== 'local-say' ||
      wav.subarray(0, 4).toString() !== 'RIFF' || wav.length < 2048) {
    throw new Error(`local TTS returned no usable WAV audio: ${JSON.stringify({ ...audio, data_url: undefined, bytes: wav.length })}`)
  }
  for (const active of [true, false]) {
    const lease = await fetch(`${base}/api/audio/tts-lease`, {
      method: 'POST', headers: { 'content-type': 'application/json', 'X-Hermes-Session-Token': token },
      body: JSON.stringify({ lease: 'desktop:voice-e2e', active }),
    })
    if (!lease.ok || (await lease.json()).active !== active) throw new Error(`desktop TTS lease ${active} failed`)
  }
  const ws = new WebSocket(`ws://127.0.0.1:${serverPort}/api/ws?token=${token}`)
  const pending = new Map()
  let next = 1
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
    const id = `wake-${next++}`
    ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }))
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timed out`)) }, 10_000)
      pending.set(id, frame => { clearTimeout(timer); resolve(frame) })
    })
  }
  const wakeStatus = await rpc('wake.status', { surface: 'gui', client_capture: true })
  const wakeStart = await rpc('wake.start', { surface: 'gui', client_capture: true, persist: true })
  if (!wakeStatus.result || wakeStatus.result.available !== false ||
      !wakeStart.result || wakeStart.result.started !== false || wakeStart.result.reason !== 'unavailable') {
    throw new Error(`wake detector should report unavailable without installing dependencies: ${JSON.stringify({ wakeStatus, wakeStart })}`)
  }
  const afterWake = await fetch(`${base}/api/config`, { headers: { 'X-Hermes-Session-Token': token } })
  const savedConfig = await afterWake.json()
  if (savedConfig.config?.wake_word?.enabled === true) throw new Error('refused wake start persisted wake_word.enabled=true')
  ws.close()
  console.log(`PASS Hermes voice synthesized ${wav.length} bytes of local WAV and released its TTS lease; wake status/start safely reported unavailable without installing dependencies`)
} finally {
  if (child && child.exitCode === null) {
    try { process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGTERM') } catch {}
    await Promise.race([new Promise(resolve => {
      if (child.exitCode !== null) return resolve()
      child.once('exit', resolve)
    }), new Promise(resolve => setTimeout(resolve, 5000))])
    if (child.exitCode === null) {
      try { process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGKILL') } catch {}
      await Promise.race([new Promise(resolve => {
        if (child.exitCode !== null) return resolve()
        child.once('exit', resolve)
      }), new Promise(resolve => setTimeout(resolve, 1000))])
    }
  }
  fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
}
