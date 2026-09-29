#!/usr/bin/env node
/**
 * Counting proxy in front of Ollama, a hosted OpenAI/Anthropic-compatible
 * API, or a self-hosted OpenAI-compatible server (vLLM/SGLang/llama.cpp).
 *
 * Keeps the running totals file (SOVEREIGN_PROXY_STATS) and, when
 * SOVEREIGN_PROXY_CALLS is set, appends one JSON line per model call with
 * token usage, cache hits, tool-schema size and a purpose label, so every
 * call can be attributed (main turn, tool follow-up, title, review, ...).
 *
 * Streaming /v1 requests get `stream_options.include_usage = true` when the
 * client did not ask for it, because Ollama (and most OpenAI-compatible
 * servers) only reports usage on streams that request it. Usage parsing
 * (OpenAI/Ollama/Anthropic shapes, streaming or not) lives in
 * scripts/lib/usage-parser.mjs, unit-tested separately.
 *
 * The client's own `Authorization` header (and everything else) is forwarded
 * upstream unmodified via `req.headers`, so pointing SOVEREIGN_PROXY_UPSTREAM
 * at a real `https://` API works as long as the backend under test was
 * configured with the real API key (BENCH_API_KEY) - the proxy never needs
 * to know the key itself, it just relays it.
 *
 *   SOVEREIGN_PROXY_UPSTREAM=http://127.0.0.1:11434 \
 *   SOVEREIGN_PROXY_LISTEN=127.0.0.1:18080 \
 *   SOVEREIGN_PROXY_STATS=/tmp/proxy-stats.json \
 *   SOVEREIGN_PROXY_CALLS=/tmp/calls.jsonl SOVEREIGN_PROXY_TAG=sovereign \
 *   node scripts/sovereign-counting-proxy.mjs
 *
 *   # Cloud/self-hosted endpoint (note https -> the http.request call below
 *   # switches transport based on SOVEREIGN_PROXY_UPSTREAM's protocol):
 *   SOVEREIGN_PROXY_UPSTREAM=https://api.example.com node scripts/sovereign-counting-proxy.mjs
 */
import http from 'node:http'
import https from 'node:https'
import { URL } from 'node:url'
import fs from 'node:fs'
import crypto from 'node:crypto'
import { usageFrom } from './lib/usage-parser.mjs'

const listen = process.env.SOVEREIGN_PROXY_LISTEN || '127.0.0.1:18080'
const upstream = process.env.SOVEREIGN_PROXY_UPSTREAM || 'http://127.0.0.1:11434'
const statsPath = process.env.SOVEREIGN_PROXY_STATS || ''
const callsPath = process.env.SOVEREIGN_PROXY_CALLS || ''
let tag = process.env.SOVEREIGN_PROXY_TAG || ''
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
  cached_tokens: 0,
  completion_tokens: 0,
  eval_count: 0,
  prompt_eval_count: 0,
  bytes_in: 0,
  bytes_out: 0,
  paths: {},
}

function classify(pathname) {
  if (pathname.includes('/chat/completions')) return 'chat_completions'
  if (pathname.endsWith('/api/chat')) return 'api_chat'
  if (pathname.endsWith('/api/generate')) return 'api_generate'
  return 'other'
}

const textOf = content =>
  typeof content === 'string'
    ? content
    : Array.isArray(content)
      ? content.map(part => (typeof part === 'string' ? part : part?.text || '')).join('')
      : ''

/** Purpose of a model call, read from its request body. */
function purposeOf(body) {
  const messages = Array.isArray(body.messages) ? body.messages : []
  const all = messages.map(m => textOf(m.content)).join('\n')
  const last = messages[messages.length - 1] || {}
  const lastText = textOf(last.content)
  if (body.prompt !== undefined && !messages.length) return 'warm-up'
  if (all.includes('You name chat sessions')) return 'title'
  if (all.includes('automatic /refine review gate') || all.includes("maintain an agent's Continual Harness") || all.includes('You learn durable lessons')) return 'learning pass'
  if (/Review the conversation above/.test(lastText)) return 'memory/skill review'
  if (all.includes('[CONTEXT SUMMARY]') || /summari[sz]e (the|this) (conversation|transcript)/i.test(lastText)) {
    return 'compression'
  }
  if (/extract (durable )?memor|memories from (this|the) (transcript|conversation)/i.test(all)) return 'memory extraction'
  if (last.role === 'tool' || (last.role === 'user' && Array.isArray(last.content) && last.content.some(p => p?.type === 'tool_result'))) {
    return 'tool follow-up'
  }
  if (last.role === 'user') return 'main turn'
  return 'other'
}

function writeStats() {
  if (!statsPath) return
  stats.updated_at = new Date().toISOString()
  fs.writeFileSync(statsPath, JSON.stringify(stats, null, 2))
}

const server = http.createServer((req, res) => {
  const u = new URL(req.url || '/', `http://${host}:${port}`)
  // Control endpoint: relabel the calls that follow (one proxy, many runs).
  if (u.pathname === '/__tag') {
    tag = u.searchParams.get('tag') || ''
    res.end('ok')
    return
  }
  stats.requests += 1
  stats.paths[u.pathname] = (stats.paths[u.pathname] || 0) + 1
  const kind = classify(u.pathname)
  if (kind === 'other') stats.other += 1
  else stats[kind] += 1

  const chunks = []
  req.on('data', c => {
    chunks.push(c)
    stats.bytes_in += c.length
  })
  req.on('end', () => {
    let body = Buffer.concat(chunks)
    let parsed = null
    let injectedUsage = false
    if (kind !== 'other' && body.length) {
      try {
        parsed = JSON.parse(body.toString('utf8'))
        if (kind === 'chat_completions' && parsed.stream && !parsed.stream_options?.include_usage) {
          parsed.stream_options = { ...(parsed.stream_options || {}), include_usage: true }
          injectedUsage = true
          body = Buffer.from(JSON.stringify(parsed))
        }
      } catch {
        parsed = null
      }
    }
    const headers = { ...req.headers, host: up.host }
    delete headers['content-length']
    if (body.length) headers['content-length'] = String(body.length)
    const started = Date.now()

    // https upstream (a hosted API) needs the https module, not http; the
    // Authorization header above is forwarded either way since it just came
    // from req.headers.
    const transport = up.protocol === 'https:' ? https : http
    const upReq = transport.request(
      { protocol: up.protocol, hostname: up.hostname, port: up.port || (up.protocol === 'https:' ? 443 : 80), path: u.pathname + u.search, method: req.method, headers },
      upRes => {
        const out = []
        res.writeHead(upRes.statusCode || 502, upRes.headers)
        upRes.on('data', c => {
          out.push(c)
          stats.bytes_out += c.length
          res.write(c)
        })
        upRes.on('end', () => {
          res.end()
          if (kind === 'other') {
            writeStats()
            return
          }
          const usage = usageFrom(Buffer.concat(out))
          stats.prompt_tokens += usage.prompt_tokens || 0
          stats.cached_tokens += usage.cached_tokens || 0
          stats.completion_tokens += usage.completion_tokens || 0
          writeStats()
          if (!callsPath) return
          const messages = Array.isArray(parsed?.messages) ? parsed.messages : []
          const system = messages.filter(m => m.role === 'system').map(m => textOf(m.content)).join('\n')
          const tools = Array.isArray(parsed?.tools) ? parsed.tools : []
          const toolsJson = JSON.stringify(tools)
          const lastUser = [...messages].reverse().find(m => m.role === 'user')
          fs.appendFileSync(
            callsPath,
            JSON.stringify({
              at: new Date(started).toISOString(),
              tag,
              endpoint: u.pathname,
              status: upRes.statusCode,
              model: parsed?.model ?? null,
              stream: Boolean(parsed?.stream),
              injected_usage: injectedUsage,
              purpose: parsed ? purposeOf(parsed) : 'unparsed',
              messages: messages.length,
              tools: tools.length,
              tool_schema_bytes: toolsJson.length,
              tool_schema_tokens_est: Math.round(toolsJson.length / 4),
              system_bytes: system.length,
              system_sha256: crypto.createHash('sha256').update(system).digest('hex').slice(0, 16),
              prompt_tokens: usage.prompt_tokens,
              cached_tokens: usage.cached_tokens,
              completion_tokens: usage.completion_tokens,
              duration_ms: Date.now() - started,
              last_user_head: lastUser ? textOf(lastUser.content).slice(0, 160) : null,
            }) + '\n'
          )
        })
      }
    )
    upReq.on('error', err => {
      if (!res.headersSent) res.writeHead(502, { 'content-type': 'text/plain' })
      res.end(String(err))
      writeStats()
    })
    if (body.length) upReq.write(body)
    upReq.end()
  })
})

server.listen(port, host, () => {
  console.error(`sovereign-counting-proxy listening on http://${host}:${port} -> ${upstream}`)
  writeStats()
})

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    writeStats()
    process.exit(0)
  })
}
