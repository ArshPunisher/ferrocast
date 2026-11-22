import assert from 'node:assert/strict'
import http from 'node:http'
import test from 'node:test'
import { FerrocastApiClient, FollowReactor, extractFollowEvent, liveWebSocketUrl, parseConfig } from './index.mjs'

test('parseConfig validates API key prefix and defaults', () => {
  assert.throws(() => parseConfig({}), /FERROCAST_API_KEY/)
  const cfg = parseConfig({ FERROCAST_API_KEY: 'fc_key_deadbeef' })
  assert.equal(cfg.baseUrl, 'http://127.0.0.1:47820')
  assert.equal(cfg.counterName, 'follows')
  assert.equal(cfg.sendChat, false)
})

test('liveWebSocketUrl encodes key and since cleanly', () => {
  const url = liveWebSocketUrl('http://127.0.0.1:47820', 'fc_key_abc', 42)
  assert.equal(url, 'ws://127.0.0.1:47820/api/v1/live?key=fc_key_abc&since=42')
})

test('extractFollowEvent filters non-follow frames', () => {
  assert.equal(extractFollowEvent({ type: 'status', data: {} }), null)
  assert.equal(
    extractFollowEvent({ type: 'event', data: { id: 'e1', kind: { type: 'chat_message' } } }),
    null,
  )
  const f = extractFollowEvent({
    type: 'event',
    seq: 9,
    data: {
      id: 'ev_follow_1',
      platform: 'twitch',
      actor: { name: 'AshaPlays' },
      kind: { type: 'follow' },
    },
  })
  assert.deepEqual(f, { id: 'ev_follow_1', seq: 9, platform: 'twitch', actorName: 'AshaPlays' })
})

test('FollowReactor reacts to live follow frames with Bearer auth and deduplicates', async () => {
  const seenRequests = []
  const logs = []
  const secretKey = `fc_key_${'12'.repeat(32)}`

  const server = http.createServer((req, res) => {
    let body = ''
    req.on('data', (c) => {
      body += c
    })
    req.on('end', () => {
      seenRequests.push({
        method: req.method,
        url: req.url,
        auth: req.headers.authorization,
        body: body ? JSON.parse(body) : null,
      })
      res.writeHead(200, { 'content-type': 'application/json', 'x-ratelimit-remaining': '118' })
      res.end(JSON.stringify({ name: 'follows', value: 7 }))
    })
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  const port = server.address().port

  try {
    const client = new FerrocastApiClient({ baseUrl: `http://127.0.0.1:${port}`, apiKey: secretKey })
    const reactor = new FollowReactor({
      client,
      counterName: 'follows',
      sendChat: true,
      log: {
        info: (m) => logs.push(m),
        warn: (m) => logs.push(m),
      },
    })

    await reactor.handleRawFrame(JSON.stringify({ v: 1, type: 'hello', data: { seq: 10 } }))
    assert.equal(reactor.lastSeq, 10)

    const followFrame = JSON.stringify({
      v: 1,
      seq: 11,
      type: 'event',
      data: { id: 'ev_11', platform: 'twitch', actor: { name: 'PixelFox' }, kind: { type: 'follow' } },
    })
    const reacted = await reactor.handleRawFrame(followFrame)
    assert.deepEqual(reacted, { kind: 'follow', actorName: 'PixelFox', platform: 'twitch', count: 7 })
    assert.equal(reactor.lastSeq, 11)

    // Duplicate replay must not double-react.
    const dup = await reactor.handleRawFrame(followFrame)
    assert.deepEqual(dup, { kind: 'duplicate', id: 'ev_11' })

    assert.equal(seenRequests.length, 2)
    assert.equal(seenRequests[0].url, '/api/v1/counters/follows/change')
    assert.equal(seenRequests[0].auth, `Bearer ${secretKey}`)
    assert.deepEqual(seenRequests[0].body, { op: 'add', amount: 1 })
    assert.equal(seenRequests[1].url, '/api/v1/chat/send')
    assert.equal(seenRequests[1].body.platforms, 'twitch')
    assert.match(seenRequests[1].body.text, /PixelFox/)
    assert.equal(logs.some((line) => line.includes(secretKey)), false)
  } finally {
    await new Promise((resolve) => server.close(resolve))
  }
})
