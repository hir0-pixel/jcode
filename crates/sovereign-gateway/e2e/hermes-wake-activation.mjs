#!/usr/bin/env node
process.env.JCODE_RUNTIME_DIR ||= (await import('node:fs')).mkdtempSync('/tmp/sj-') // short private dir: never collide with a running engine
import { spawn, execFileSync } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'sovereign-wake-e2e-'))
const home = path.join(root, 'home')
const hermesHome = path.join(root, '.hermes')
const jcodeHome = path.join(root, '.jcode')
for (const directory of [home, hermesHome, jcodeHome]) fs.mkdirSync(directory, { recursive: true })
const token = crypto.randomBytes(24).toString('hex')
const env = { ...process.env, HOME: home, HERMES_HOME: hermesHome, JCODE_HOME: jcodeHome,
  HERMES_DASHBOARD_SESSION_TOKEN: token, PYTHONDONTWRITEBYTECODE: '1',
  HERMES_DISABLE_LAZY_INSTALLS: '1' }
const hermesRoot = process.env.HERMES_REPO || path.resolve('../hermes-agent')
const python = process.env.HERMES_PYTHON || path.join(hermesRoot, '.venv/bin/python')
const audioBase = path.join(root, 'hey-hermes')
const wavPath = `${audioBase}.wav`
const config = `security:\n  allow_lazy_installs: false\nwake_word:\n  enabled: true\n  surface: gui\n  capture: client\n  provider: openwakeword\n  phrase: hey hermes\n  sensitivity: 0.2\n  confirmation_frames: 1\n  start_new_session: false\n  openwakeword:\n    inference_framework: tflite\n`
fs.writeFileSync(path.join(hermesHome, 'config.yaml'), config)
fs.writeFileSync(`${audioBase}.txt`, 'Hey Hermes')
let child
let ws
try {
  execFileSync('/usr/bin/say', ['-o', `${audioBase}.aiff`, '-f', `${audioBase}.txt`])
  execFileSync('/usr/bin/afconvert', ['-f', 'WAVE', '-d', 'LEI16@16000', '-c', '1', `${audioBase}.aiff`, wavPath])
  const wav = fs.readFileSync(wavPath)
  const pcm = wav.subarray(44)
  if (wav.subarray(0, 4).toString() !== 'RIFF' || pcm.length < 2560) throw new Error('say produced no usable wake sample')

  child = spawn(python, ['-m', 'hermes_cli.main', 'serve', '--host', '127.0.0.1', '--port', '0', '--skip-build'], {
    cwd: hermesRoot, env, stdio: ['ignore', 'pipe', 'pipe'], detached: process.platform !== 'win32',
  })
  let output = ''
  child.stdout.on('data', chunk => { output += chunk })
  child.stderr.on('data', chunk => { output += chunk })
  const deadline = Date.now() + 60_000
  let port
  while (Date.now() < deadline) {
    port = Number(output.match(/HERMES_BACKEND_READY port=(\d+)/)?.[1])
    if (port) break
    if (child.exitCode !== null) throw new Error(`Hermes serve exited: ${output}`)
    await new Promise(resolve => setTimeout(resolve, 50))
  }
  if (!port) throw new Error(`Hermes serve did not become ready: ${output}`)

  ws = new WebSocket(`ws://127.0.0.1:${port}/api/ws?token=${token}`)
  const pending = new Map()
  let next = 1
  let detected
  let resolveDetected
  const detection = new Promise(resolve => { resolveDetected = resolve })
  ws.addEventListener('message', message => {
    const frame = JSON.parse(String(message.data))
    if (frame.id !== undefined && pending.has(frame.id)) {
      pending.get(frame.id)(frame)
      pending.delete(frame.id)
    }
    if (frame.method === 'event' && frame.params?.type === 'wake.detected') {
      detected = frame.params.payload
      resolveDetected(frame.params.payload)
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
  const started = await rpc('wake.start', { surface: 'gui', client_capture: true, session_id: 'wake-activation-e2e' })
  if (!started.result?.started || started.result.capture !== 'client') {
    throw new Error(`real wake detector did not start in client capture mode: ${JSON.stringify(started)}`)
  }
  for (let offset = 0; offset < pcm.length && !detected; offset += 2560) {
    const chunk = pcm.subarray(offset, Math.min(offset + 2560, pcm.length))
    const fed = await rpc('wake.feed', { pcm: chunk.toString('base64'), sample_rate: 16000 })
    if (fed.error || fed.result?.fed !== true) throw new Error(`wake.feed rejected PCM: ${JSON.stringify(fed)}`)
    await Promise.race([detection, new Promise(resolve => setTimeout(resolve, 70))])
  }
  if (!detected) {
    await Promise.race([detection, new Promise(resolve => setTimeout(resolve, 5000))])
  }
  if (!detected || detected.phrase?.toLowerCase() !== 'hey hermes' || detected.start_new_session !== false) {
    throw new Error(`the owning client did not receive wake.detected for the real phrase: ${JSON.stringify(detected)}`)
  }
  const stopped = await rpc('wake.stop', {})
  if (stopped.error || stopped.result?.stopped !== true) throw new Error(`wake listener did not stop: ${JSON.stringify(stopped)}`)
  console.log('PASS real openWakeWord/TFLite detected “Hey Hermes” from local PCM and emitted wake.detected to the owning client')
} finally {
  ws?.close()
  if (child && child.exitCode === null) {
    try { process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGTERM') } catch {}
    await Promise.race([new Promise(resolve => child.once('exit', resolve)), new Promise(resolve => setTimeout(resolve, 5000))])
    if (child.exitCode === null) {
      try { process.kill(process.platform === 'win32' ? child.pid : -child.pid, 'SIGKILL') } catch {}
    }
  }
  fs.rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 })
}
