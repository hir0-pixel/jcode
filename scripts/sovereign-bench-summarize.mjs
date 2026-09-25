#!/usr/bin/env node
/**
 * Summarise a sovereign-vs-hermes-bench run directory into Markdown tables.
 *
 *   node scripts/sovereign-bench-summarize.mjs docs/benchmark-runs/<stamp> > summary.md
 */
import fs from 'node:fs'
import path from 'node:path'
import { resolvePrice } from './lib/bench-prices.mjs'

const dir = process.argv[2]
if (!dir) {
  console.error('usage: sovereign-bench-summarize.mjs <run dir>')
  process.exit(1)
}
const read = f => (fs.existsSync(path.join(dir, f)) ? fs.readFileSync(path.join(dir, f), 'utf8').trim().split('\n').filter(Boolean).map(l => JSON.parse(l)) : [])
const calls = read('calls.jsonl')
const turns = read('turns.jsonl')
const rss = read('rss.jsonl')
const sessions = read('sessions.jsonl')
const config = JSON.parse(fs.readFileSync(path.join(dir, 'config.json'), 'utf8'))

const products = config.PRODUCTS
const tasks = config.tasks
const median = xs => {
  const s = [...xs].sort((a, b) => a - b)
  return s.length ? (s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2) : NaN
}
const fmt = n => (Number.isFinite(n) ? Math.round(n).toLocaleString('en-US') : '-')
const spread = xs => (xs.length ? `${fmt(median(xs))} (${fmt(Math.min(...xs))}-${fmt(Math.max(...xs))})` : '-')
const usd = n => (Number.isFinite(n) ? `$${n.toFixed(4)}` : '-')
const pct = (a, b) => (a && b ? `${Math.round(((b - a) / a) * 100)}%` : '-')
const parse = tag => {
  const [product, task, run] = tag.split('|')
  return { product, task, run }
}
const modelCalls = calls.filter(c => c.purpose !== 'warm-up')

// config.price is what the bench run itself resolved (BENCH_PRICE_IN/_CACHED/_OUT
// env override, or the shared scripts/sovereign-prices.json / SOVEREIGN_PRICE_TABLE
// table - the same file the engine reads for its own accounting, see
// crates/sovereign-gateway/src/observability.rs). Older run directories predate
// that field, so fall back to resolving it fresh for config.MODEL.
const PRICE = config.price || resolvePrice(config.MODEL)
if (!PRICE) throw new Error(`No benchmark price for ${config.MODEL}`)
const costOf = c => {
  const prompt = c.prompt_tokens || 0
  const cached = Math.min(c.cached_tokens || 0, prompt)
  return ((prompt - cached) * PRICE.input + cached * PRICE.cached + (c.completion_tokens || 0) * PRICE.output) / 1e6
}

// Per (product, task, run) totals.
const per = {}
for (const c of modelCalls) {
  const { product, task, run } = parse(c.tag)
  const key = `${product}|${task}|${run}`
  per[key] ||= { calls: 0, prompt: 0, processed: 0, completion: 0, unknown: 0, cost: 0 }
  const p = per[key]
  p.cost += costOf(c)
  p.calls += 1
  if (c.prompt_tokens == null) p.unknown += 1
  p.prompt += c.prompt_tokens || 0
  p.processed += (c.prompt_tokens || 0) - (c.cached_tokens || 0)
  p.completion += c.completion_tokens || 0
}
for (const t of turns) {
  const key = `${t.product}|${t.task}|run${t.run}`
  per[key] ||= { calls: 0, prompt: 0, processed: 0, completion: 0, unknown: 0 }
  per[key].wall = (per[key].wall || 0) + t.ms
  per[key].failed = (per[key].failed || 0) + (t.ok ? 0 : 1)
}
const series = (product, task, field) =>
  Object.entries(per)
    .filter(([k]) => k.startsWith(`${product}|${task}|`))
    .map(([, v]) => v[field] ?? 0)

const out = []
out.push(`Model \`${config.MODEL}\` (num_ctx ${config.NUM_CTX}), ${config.RUNS} runs, idle window ${config.IDLE_MS / 1000}s after each session. Values are median (min-max) per task across runs.`)
out.push('')
out.push('"Prompt tokens" is the full input sent (what a paid API bills). "Processed" subtracts tokens served from Ollama\'s KV cache (what the local machine computes).')
out.push('')
for (const [field, title] of [
  ['calls', 'Model calls'],
  ['prompt', 'Prompt tokens'],
  ['processed', 'Processed prompt tokens'],
  ['completion', 'Completion tokens'],
  ['wall', 'Wall time of turns (ms)'],
]) {
  out.push(`### ${title}`)
  out.push('')
  out.push(`| Task | ${products.join(' | ')} | Sovereign vs Hermes |`)
  out.push(`|---|${products.map(() => '---:').join('|')}|---:|`)
  const totals = Object.fromEntries(products.map(p => [p, 0]))
  for (const task of tasks) {
    const cells = products.map(p => spread(series(p, task, field)))
    const h = median(series('hermes', task, field))
    const s = median(series('sovereign', task, field))
    for (const p of products) totals[p] += median(series(p, task, field)) || 0
    out.push(`| ${task} | ${cells.join(' | ')} | ${pct(h, s)} |`)
  }
  out.push(`| **all tasks (sum of medians)** | ${products.map(p => `**${fmt(totals[p])}**`).join(' | ')} | **${pct(totals.hermes, totals.sovereign)}** |`)
  out.push('')
}

out.push(`### Dummy API cost per task (USD; input $${PRICE.input}/M, cached input $${PRICE.cached}/M, output $${PRICE.output}/M)`)
out.push('')
out.push('Ollama treated as a paid API. Cached input uses Ollama\'s KV-cache hits as a stand-in for provider prompt caching, whose rules differ (minimum prefix, time-to-live, explicit breakpoints on some providers).')
out.push('')
out.push(`| Task | ${products.join(' | ')} | Sovereign vs Hermes |`)
out.push(`|---|${products.map(() => '---:').join('|')}|---:|`)
const costTotals = Object.fromEntries(products.map(p => [p, 0]))
for (const task of tasks) {
  const med = p => median(series(p, task, 'cost'))
  for (const p of products) costTotals[p] += med(p) || 0
  out.push(`| ${task} | ${products.map(p => usd(med(p))).join(' | ')} | ${pct(med('hermes'), med('sovereign'))} |`)
}
out.push(`| **all tasks** | ${products.map(p => `**${usd(costTotals[p])}**`).join(' | ')} | **${pct(costTotals.hermes, costTotals.sovereign)}** |`)
out.push('')
out.push(`Same prices with no prompt caching at all (every input token at $${PRICE.input}/M):`)
out.push('')
out.push(`| ${products.map(p => `${p}`).join(' | ')} | Sovereign vs Hermes |`)
out.push(`|${products.map(() => '---:').join('|')}|---:|`)
const uncached = Object.fromEntries(products.map(p => [p, tasks.reduce((sum, task) => sum + (median(series(p, task, 'prompt')) * PRICE.input + median(series(p, task, 'completion')) * PRICE.output) / 1e6, 0)]))
out.push(`| ${products.map(p => usd(uncached[p])).join(' | ')} | ${pct(uncached.hermes, uncached.sovereign)} |`)
out.push('')

out.push('### Calls by purpose (all runs)')
out.push('')
const purposes = [...new Set(modelCalls.map(c => c.purpose))].sort()
out.push(`| Purpose | ${products.map(p => `${p} calls | ${p} prompt tokens | ${p} dummy cost`).join(' | ')} |`)
out.push(`|---|${products.map(() => '---:|---:|---:').join('|')}|`)
for (const purpose of purposes) {
  const cells = products.map(p => {
    const cs = modelCalls.filter(c => c.purpose === purpose && c.tag.startsWith(`${p}|`))
    return `${cs.length} | ${fmt(cs.reduce((s, c) => s + (c.prompt_tokens || 0), 0))} | ${usd(cs.reduce((s, c) => s + costOf(c), 0))}`
  })
  out.push(`| ${purpose} | ${cells.join(' | ')} |`)
}
out.push('')

out.push('### Fixed prefix per main-turn call')
out.push('')
out.push('| Product | Tools | Tool schema (est. tokens) | System prompt (bytes) | First-turn prompt tokens |')
out.push('|---|---:|---:|---:|---:|')
for (const p of products) {
  const first = modelCalls.filter(c => c.tag.startsWith(`${p}|`) && c.purpose === 'main turn' && /\|s1t1$/.test(c.tag))
  out.push(`| ${p} | ${fmt(median(first.map(c => c.tools)))} | ${fmt(median(first.map(c => c.tool_schema_tokens_est)))} | ${fmt(median(first.map(c => c.system_bytes)))} | ${fmt(median(first.map(c => c.prompt_tokens || 0)))} |`)
}
out.push('')

out.push('### RAM (MB, median across all sessions)')
out.push('')
out.push('| Product | Backend idle before chat | Backend after session | Backend peak | of which Python (peak) | Ollama peak |')
out.push('|---|---:|---:|---:|---:|---:|')
for (const p of products) {
  const rows = rss.filter(r => r.label.startsWith(`${p}|`))
  const mb = (phase, field) => median(rows.filter(r => phase(r.phase)).map(r => r[field] / 1024))
  out.push(`| ${p} | ${fmt(mb(x => x === 'idle-before-chat', 'backend_kb'))} | ${fmt(mb(x => x.startsWith('idle-after'), 'backend_kb'))} | ${fmt(mb(x => x === 'peak', 'backend_kb'))} | ${fmt(mb(x => x === 'peak', 'python_kb'))} | ${fmt(mb(x => x === 'peak', 'ollama_kb'))} |`)
}
out.push('')

out.push('### Correctness')
out.push('')
out.push('| Product | Failed turns | edit-code file check passed | memory task recalled preference | memory written to disk |')
out.push('|---|---:|---:|---:|---:|')
for (const p of products) {
  const ss = sessions.filter(s => s.product === p)
  const failed = turns.filter(t => t.product === p && !t.ok).length
  const edit = ss.filter(s => s.task === 'edit-code')
  const mem = ss.filter(s => s.task === 'memory')
  const written = mem.filter(s => (s.memory_files || []).some(f => f.mentions_nim)).length
  out.push(`| ${p} | ${failed} | ${edit.filter(s => s.check).length}/${edit.length} | ${mem.filter(s => s.check_reply).length}/${mem.length} | ${written}/${mem.length} |`)
}
const errors = sessions.filter(s => s.error)
if (errors.length) {
  out.push('')
  out.push('Errors: ' + errors.map(e => `${e.label}: ${e.error}`).join('; '))
}
const unknown = modelCalls.filter(c => c.prompt_tokens == null).length
out.push('')
out.push(`Calls with unknown token counts: ${unknown} of ${modelCalls.length}.`)
console.log(out.join('\n'))
