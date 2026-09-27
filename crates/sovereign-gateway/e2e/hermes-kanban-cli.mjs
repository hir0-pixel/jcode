#!/usr/bin/env node
// Exercise the desktop's Hermes-owned Kanban store through its real CLI.
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const hermesRoot = process.env.HERMES_REPO || path.resolve('../hermes-agent')
const python = process.env.HERMES_PYTHON || path.join(hermesRoot, '.venv/bin/python')
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'akira-kanban-e2e-'))
const env = {
  ...process.env,
  HOME: path.join(root, 'home'),
  HERMES_HOME: path.join(root, '.hermes'),
  JCODE_HOME: path.join(root, '.jcode'),
  PYTHONDONTWRITEBYTECODE: '1',
}
for (const name of ['HOME', 'HERMES_HOME', 'JCODE_HOME']) fs.mkdirSync(env[name], { recursive: true })

function hermes(...args) {
  const result = spawnSync(python, ['-m', 'hermes_cli.main', 'kanban', ...args], {
    cwd: hermesRoot,
    env,
    encoding: 'utf8',
    timeout: 30_000,
  })
  if (result.error || result.status !== 0) {
    throw new Error(`hermes kanban ${args.join(' ')} failed: ${result.error || result.stderr || result.stdout}`)
  }
  return result.stdout.trim()
}

try {
  hermes('init')
  hermes('boards', 'create', 'akira-feature-e2e', '--name', 'Akira Feature E2E', '--switch')
  const created = JSON.parse(hermes('create', 'Kanban CLI feature check', '--body', 'Persist and complete this task.', '--json'))
  assert.ok(created.id, 'CLI should return the created task id')
  assert.equal(created.title, 'Kanban CLI feature check')
  hermes('claim', created.id)
  hermes('complete', created.id, '--result', 'CLI e2e completed')
  const rows = JSON.parse(hermes('list', '--json'))
  const saved = rows.find(task => task.id === created.id)
  assert.ok(saved, 'completed task should remain in the board')
  assert.equal(saved.status, 'done')
  assert.equal(saved.result, 'CLI e2e completed')
  console.log('PASS Hermes Kanban CLI created a board, claimed a task, completed it, and read persisted state')
} finally {
  fs.rmSync(root, { recursive: true, force: true })
}
