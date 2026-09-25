/**
 * Shared price lookup for the benchmark scripts and for the engine itself.
 *
 * `scripts/sovereign-prices.json` is the same file the engine reads at
 * runtime when `SOVEREIGN_PRICE_TABLE` points to it (see `cost_usd` in
 * crates/sovereign-gateway/src/observability.rs), so a model added here is
 * priced the same way in both the benchmark's own dummy-cost math and in the
 * engine's own /api/analytics/usage totals.
 *
 * Priority: BENCH_PRICE_IN/_CACHED/_OUT env overrides (set BENCH_PRICE_IN and
 * BENCH_PRICE_OUT together; BENCH_PRICE_CACHED defaults to BENCH_PRICE_IN,
 * i.e. no cache discount) > SOVEREIGN_PRICE_TABLE file, if set > the bundled
 * scripts/sovereign-prices.json fallback table.
 */
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const DEFAULT_TABLE = path.join(__dirname, '..', 'sovereign-prices.json')

export function resolvePrice(model, env = process.env) {
  const { BENCH_PRICE_IN, BENCH_PRICE_CACHED, BENCH_PRICE_OUT, SOVEREIGN_PRICE_TABLE } = env
  if (BENCH_PRICE_IN && BENCH_PRICE_OUT) {
    return {
      input: Number(BENCH_PRICE_IN),
      cached: Number(BENCH_PRICE_CACHED ?? BENCH_PRICE_IN),
      output: Number(BENCH_PRICE_OUT),
      source: 'env (BENCH_PRICE_IN/_CACHED/_OUT)',
    }
  }
  for (const file of [SOVEREIGN_PRICE_TABLE, DEFAULT_TABLE].filter(Boolean)) {
    try {
      const table = JSON.parse(fs.readFileSync(file, 'utf8'))
      if (table[model]) return { ...table[model], source: file }
    } catch {
      // missing/unreadable/invalid file: try the next candidate
    }
  }
  return null
}

/** Dummy USD cost for one call's usage numbers, or null with no known price. */
export function costOf({ prompt_tokens = 0, cached_tokens = 0, completion_tokens = 0 }, price) {
  if (!price) return null
  const prompt = prompt_tokens || 0
  const cached = Math.min(cached_tokens || 0, prompt)
  return ((prompt - cached) * price.input + cached * price.cached + (completion_tokens || 0) * price.output) / 1e6
}
