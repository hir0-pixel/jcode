#!/usr/bin/env node
/**
 * Counting proxy in front of Ollama (or any OpenAI-compat upstream).
 * Tallies chat/completions + native /api/chat|/api/generate calls and tokens.
 *
 *   SOVEREIGN_PROXY_UPSTREAM=http://127.0.0.1:11434 \
 *   SOVEREIGN_PROXY_LISTEN=127.0.0.1:18080 \
 *   SOVEREIGN_PROXY_STATS=/tmp/proxy-stats.json \
 *   node scripts/sovereign-counting-proxy.mjs
 */
import http from 'node:http'
import { URL } from 'node:url'
import fs from 'node:fs'

const listen = process.env.SOVEREIGN_PROXY_LISTEN || '127.0.0.1:18080'
const upstream = process.env.SOVEREIGN_PROXY_UPSTREAM || 'http://127.0.0.1:11434'
const statsPath = process.env.SOVEREIGN_PROXY_STATS || ''
const [host, portStr] = listen.split(':')
const port = Number(portStr || 18080)
const up = new URL(upstream)

const stats = {
  started_at: new Date().toISOString(),
  upstream,
  listen,
  requests: 0,
  chat_completions: 0,
  api_chat: 0,
  api_generate: 0,
  other: 0,
  prompt_tokens: 0,
  completion_tokens: 0,
  eval_count: 0,
  prompt_eval_count: 0,
  bytes_in: 0,
  bytes_out: 0,
  paths: {},
}

function bumpPath(p) {
  stats.paths[p] = (stats.paths[p] || 0) + 1
}

function classify(pathname) {
  if (pathname.includes('/chat/completions')) {
    stats.chat_completions += 1
    return 'chat_completions'
  }
  if (pathname === '/api/chat' || pathname.endsWith('/api/chat')) {
    stats.api_chat += 1
    return 'api_chat'
  }
  if (pathname === '/api/generate' || pathname.endsWith('/api/generate')) {
    stats.api_generate += 1
    return 'api_generate'
  }
  stats.other += 1
  return 'other'
}

function harvestUsage(buf, kind) {
  const text = buf.toString('utf8')
  // Non-stream OpenAI JSON
  try {
    const j = JSON.parse(text)
    const u = j.usage || {}
    if (typeof u.prompt_tokens === 'number') stats.prompt_tokens += u.prompt_tokens
    if (typeof u.completion_tokens === 'number') stats.completion_tokens += u.completion_tokens
    if (typeof j.prompt_eval_count === 'number') stats.prompt_eval_count += j.prompt_eval_count
    if (typeof j.eval_count === 'number') stats.eval_count += j.eval_count
    return
  } catch {
    // SSE / NDJSON stream: scan lines
  }
  for (const line of text.split('\n')) {
    const payload = line.startsWith('data:') ? line.slice(5).trim() : line.trim()
    if (!payload || payload === '[DONE]') continue
    try {
      const j = JSON.parse(payload)
      const u = j.usage || {}
      if (typeof u.prompt_tokens === 'number') stats.prompt_tokens += u.prompt_tokens
      if (typeof u.completion_tokens === 'number') stats.completion_tokens += u.completion_tokens
      if (typeof j.prompt_eval_count === 'number') stats.prompt_eval_count += j.prompt_eval_count
      if (typeof j.eval_count === 'number') stats.eval_count += j.eval_count
      // Ollama stream final
      if (j.done && kind === 'api_chat') {
        if (typeof j.prompt_eval_count === 'number') {
          /* already added */
        }
      }
    } catch {
      /* ignore */
    }
  }
}

function writeStats() {
  if (!statsPath) return
  stats.updated_at = new Date().toISOString()
  fs.writeFileSync(statsPath, JSON.stringify(stats, null, 2))
}

const server = http.createServer((req, res) => {
  stats.requests += 1
  const u = new URL(req.url || '/', `http://${host}:${port}`)
  bumpPath(u.pathname)
  const kind = classify(u.pathname)

  const chunks = []
  req.on('data', c => {
    chunks.push(c)
    stats.bytes_in += c.length
  })
  req.on('end', () => {
    const body = Buffer.concat(chunks)
    const headers = { ...req.headers, host: up.host }
    delete headers['content-length']

    const upReq = http.request(
      {
        protocol: up.protocol,
        hostname: up.hostname,
        port: up.port || 80,
        path: u.pathname + u.search,
        method: req.method,
        headers,
      },
      upRes => {
        const out = []
        upRes.on('data', c => {
          out.push(c)
          stats.bytes_out += c.length
          res.write(c)
        })
        upRes.on('end', () => {
          const buf = Buffer.concat(out)
          if (kind === 'chat_completions' || kind === 'api_chat' || kind === 'api_generate') {
            harvestUsage(buf, kind)
          }
          writeStats()
          res.end()
        })
        res.writeHead(upRes.statusCode || 502, upRes.headers)
      }
    )
    upReq.on('error', err => {
      res.writeHead(502, { 'content-type': 'text/plain' })
      res.end(String(err))
      writeStats()
    })
    if (body.length) upReq.write(body)
    upReq.end()
  })
})

server.listen(port, host, () => {
  console.error(`sovereign-counting-proxy listening on http://${host}:${port} → ${upstream}`)
  writeStats()
})

process.on('SIGINT', () => {
  writeStats()
  process.exit(0)
})
process.on('SIGTERM', () => {
  writeStats()
  process.exit(0)
})
