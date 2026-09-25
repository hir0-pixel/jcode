/**
 * Shared token-usage parser for the benchmark's counting proxy.
 *
 * Understands three shapes, streaming or not:
 *   - OpenAI-compatible (Ollama, vLLM, SGLang, llama.cpp server, OpenAI itself):
 *     top-level or per-chunk `usage.prompt_tokens` / `usage.completion_tokens` /
 *     `usage.prompt_tokens_details.cached_tokens`. Streaming responses only
 *     carry `usage` on the final chunk when the request set
 *     `stream_options.include_usage` (the proxy injects that, see
 *     sovereign-counting-proxy.mjs).
 *   - Ollama's native `/api/chat` / `/api/generate`: `prompt_eval_count` /
 *     `eval_count` at the top level.
 *   - Anthropic Messages API: `usage.input_tokens` / `usage.output_tokens` /
 *     `usage.cache_read_input_tokens` / `usage.cache_creation_input_tokens`,
 *     either at the top level (non-streaming) or nested under
 *     `message.usage` (the `message_start` SSE event) with a later
 *     `message_delta` event carrying the final cumulative `output_tokens`.
 *
 * `prompt_tokens` in the returned shape always means "total input tokens
 * billed for this call", matching what a paid API's invoice would show:
 * for Anthropic that's input + cache_read + cache_creation, since Anthropic
 * bills all three (at different rates), not just the fresh `input_tokens`.
 */

function take(found, j) {
  const u = j.usage || {}

  // OpenAI-compatible.
  if (typeof u.prompt_tokens === 'number') found.prompt_tokens = u.prompt_tokens
  if (typeof u.completion_tokens === 'number') found.completion_tokens = u.completion_tokens
  const openaiCached = u.prompt_tokens_details?.cached_tokens
  if (typeof openaiCached === 'number') found.cached_tokens = openaiCached

  // Ollama native.
  if (typeof j.prompt_eval_count === 'number') found.prompt_tokens = j.prompt_eval_count
  if (typeof j.eval_count === 'number') found.completion_tokens = j.eval_count

  // Anthropic Messages API: top-level `usage` on a non-streaming response, or
  // `message.usage` on a `message_start` streaming event.
  takeAnthropic(found, u)
  if (j.message?.usage) takeAnthropic(found, j.message.usage)
}

function takeAnthropic(found, u) {
  const input = u.input_tokens
  const cacheRead = u.cache_read_input_tokens
  const cacheWrite = u.cache_creation_input_tokens
  if (typeof input === 'number' || typeof cacheRead === 'number' || typeof cacheWrite === 'number') {
    found.prompt_tokens = (input || 0) + (cacheRead || 0) + (cacheWrite || 0)
  }
  if (typeof cacheRead === 'number') found.cached_tokens = cacheRead
  if (typeof u.output_tokens === 'number') found.completion_tokens = u.output_tokens
}

/**
 * Parse token usage out of a full response body (Buffer or string), which may
 * be a single JSON object (non-streaming) or an SSE/NDJSON stream (each
 * `data: {...}` or bare JSON line inspected in order; later chunks overwrite
 * earlier ones field-by-field, so a streaming Anthropic response ends with
 * the `message_start` input counts plus the `message_delta` final output
 * count, and a streaming OpenAI-compatible response ends with whatever the
 * final `include_usage` chunk reported).
 */
export function usageFrom(buf) {
  const found = { prompt_tokens: null, cached_tokens: null, completion_tokens: null }
  const text = buf.toString('utf8')
  try {
    take(found, JSON.parse(text))
    return found
  } catch {
    // SSE or NDJSON stream; fall through to line-by-line parsing below.
  }
  for (const line of text.split('\n')) {
    const payload = line.startsWith('data:') ? line.slice(5).trim() : line.trim()
    if (!payload || payload === '[DONE]' || payload.startsWith('event:')) continue
    try {
      take(found, JSON.parse(payload))
    } catch {
      // partial/non-JSON line (e.g. an `event:` line already skipped above)
    }
  }
  return found
}
