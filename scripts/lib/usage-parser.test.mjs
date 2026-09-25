#!/usr/bin/env node
// Fixture-based unit tests for usage-parser.mjs. No network, no model.
//
//   node scripts/lib/usage-parser.test.mjs
import test from 'node:test'
import assert from 'node:assert/strict'
import { usageFrom } from './usage-parser.mjs'

test('OpenAI non-streaming: prompt_tokens + cached_tokens + completion_tokens', () => {
  const body = JSON.stringify({
    choices: [{ message: { content: 'hi' } }],
    usage: { prompt_tokens: 120, completion_tokens: 30, prompt_tokens_details: { cached_tokens: 96 } },
  })
  assert.deepEqual(usageFrom(Buffer.from(body)), { prompt_tokens: 120, cached_tokens: 96, completion_tokens: 30 })
})

test('OpenAI streaming with include_usage: only the final chunk carries usage', () => {
  const chunks = [
    { choices: [{ delta: { content: 'h' } }] },
    { choices: [{ delta: { content: 'i' } }] },
    { choices: [], usage: { prompt_tokens: 200, completion_tokens: 5, prompt_tokens_details: { cached_tokens: 150 } } },
  ]
  const sse = chunks.map(c => `data: ${JSON.stringify(c)}`).join('\n\n') + '\ndata: [DONE]\n'
  assert.deepEqual(usageFrom(Buffer.from(sse)), { prompt_tokens: 200, cached_tokens: 150, completion_tokens: 5 })
})

test('Ollama native /api/chat: prompt_eval_count / eval_count', () => {
  const body = JSON.stringify({ message: { content: 'hi' }, prompt_eval_count: 8596, eval_count: 12, done: true })
  assert.deepEqual(usageFrom(Buffer.from(body)), { prompt_tokens: 8596, cached_tokens: null, completion_tokens: 12 })
})

test('Ollama native streaming NDJSON: final line has the counts', () => {
  const lines = [
    { message: { content: 'h' }, done: false },
    { message: { content: 'i' }, done: false },
    { message: { content: '' }, done: true, prompt_eval_count: 500, eval_count: 20 },
  ]
  const ndjson = lines.map(l => JSON.stringify(l)).join('\n')
  assert.deepEqual(usageFrom(Buffer.from(ndjson)), { prompt_tokens: 500, cached_tokens: null, completion_tokens: 20 })
})

test('Anthropic non-streaming: input/output/cache_read billed as total prompt_tokens', () => {
  const body = JSON.stringify({
    type: 'message',
    usage: { input_tokens: 40, cache_read_input_tokens: 900, cache_creation_input_tokens: 0, output_tokens: 60 },
  })
  assert.deepEqual(usageFrom(Buffer.from(body)), { prompt_tokens: 940, cached_tokens: 900, completion_tokens: 60 })
})

test('Anthropic streaming: message_start gives input+cache, message_delta gives final output', () => {
  const events = [
    { type: 'message_start', message: { usage: { input_tokens: 40, cache_read_input_tokens: 900, cache_creation_input_tokens: 0, output_tokens: 1 } } },
    { type: 'content_block_delta', delta: { text: 'hi' } },
    { type: 'message_delta', delta: {}, usage: { output_tokens: 60 } },
    { type: 'message_stop' },
  ]
  const sse = events.map(e => `event: ${e.type}\ndata: ${JSON.stringify(e)}`).join('\n\n')
  assert.deepEqual(usageFrom(Buffer.from(sse)), { prompt_tokens: 940, cached_tokens: 900, completion_tokens: 60 })
})

test('no usage anywhere: all fields stay null', () => {
  assert.deepEqual(usageFrom(Buffer.from(JSON.stringify({ choices: [{ message: { content: 'hi' } }] }))), {
    prompt_tokens: null,
    cached_tokens: null,
    completion_tokens: null,
  })
})

test('malformed body does not throw', () => {
  assert.deepEqual(usageFrom(Buffer.from('not json at all')), { prompt_tokens: null, cached_tokens: null, completion_tokens: null })
})
