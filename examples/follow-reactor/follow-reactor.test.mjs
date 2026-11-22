import assert from 'node:assert/strict'
import http from 'node:http'
import test from 'node:test'
import {
  FerrocastApiClient,
  FollowReactor,
  SerialQueue,
  extractFollowEvent,
  liveWebSocketUrl,
  parseConfig,
} from './index.mjs'

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

const KEY = `fc_key_${'ab'.repeat(32)}`

test('plain http is for this machine only, everything else needs https', () => {
  for (const ok of [
    'http://127.0.0.1:47820',
    'http://localhost:47820/',
    'http://[::1]:47820',
    'http://127.0.0.7:1',
    'https://example.com',
    'https://192.168.1.20:47820',
  ]) {
    assert.doesNotThrow(() => parseConfig({ FERROCAST_API_KEY: KEY, FERROCAST_URL: ok }), ok)
  }
  for (const bad of [
    'http://192.168.1.20:47820',
    'http://example.com',
    'http://127.0.0.1.evil.example',
    'http://localhost.evil.example',
    'ftp://127.0.0.1',
    'ws://127.0.0.1:47820',
    '127.0.0.1:47820',
    'http://user:pw@127.0.0.1:1',
    'not a url',
  ]) {
    assert.throws(() => parseConfig({ FERROCAST_API_KEY: KEY, FERROCAST_URL: bad }), /FERROCAST_URL/, bad)
  }
  try {
    parseConfig({ FERROCAST_API_KEY: KEY, FERROCAST_URL: 'http://10.0.0.5' })
  } catch (err) {
    assert.equal(err.message.includes(KEY), false)
  }
  // The live socket follows the same scheme: ws on this machine, wss elsewhere.
  assert.equal(liveWebSocketUrl('https://example.com', KEY).startsWith('wss://example.com/'), true)
})

test('a redirect is refused and the key is never sent to the other address', async () => {
  const seen = []
  const other = http.createServer((req, res) => {
    seen.push(req.headers.authorization)
    res.writeHead(200)
    res.end('{}')
  })
  await new Promise((resolve) => other.listen(0, '127.0.0.1', resolve))
  const first = http.createServer((req, res) => {
    res.writeHead(302, { location: `http://127.0.0.1:${other.address().port}/steal` })
    res.end()
  })
  await new Promise((resolve) => first.listen(0, '127.0.0.1', resolve))
  try {
    const client = new FerrocastApiClient({ baseUrl: `http://127.0.0.1:${first.address().port}`, apiKey: KEY })
    await assert.rejects(client.getStatus(), (err) => {
      assert.equal(err.code, 'redirect_refused')
      assert.equal(err.message.includes(KEY), false)
      return true
    })
    assert.deepEqual(seen, [])
  } finally {
    await new Promise((resolve) => first.close(resolve))
    await new Promise((resolve) => other.close(resolve))
  }
})

const followFrame = (seq, id = `ev_${seq}`) =>
  JSON.stringify({
    v: 1,
    seq,
    type: 'event',
    data: { id, platform: 'twitch', actor: { name: 'Fox' }, kind: { type: 'follow' } },
  })

function stubClient(bumps) {
  const calls = { bump: 0, chat: 0 }
  return {
    calls,
    getStatus: async () => ({ data: {} }),
    bumpCounter: async () => {
      calls.bump++
      const next = bumps.shift()
      if (next instanceof Error) throw next
      return { data: { value: next ?? calls.bump } }
    },
    sendChat: async () => {
      calls.chat++
    },
  }
}

const quiet = { info() {}, warn() {}, error() {} }
const httpError = (status, retryAfter = null) => Object.assign(new Error(`HTTP ${status}`), { status, retryAfter })

test('a follow is checkpointed only after its counter update succeeded; a 429 waits and tries again', async () => {
  const client = stubClient([httpError(429, '3'), 8])
  const waits = []
  const reactor = new FollowReactor({ client, log: quiet, sleep: async (ms) => waits.push(ms) })
  await reactor.handleRawFrame(JSON.stringify({ v: 1, type: 'hello', data: { seq: 10 } }))
  const out = await reactor.handleRawFrame(followFrame(11))
  assert.equal(out.kind, 'follow')
  assert.deepEqual(waits, [3000], 'Retry-After was honoured')
  assert.equal(client.calls.bump, 2)
  assert.equal(reactor.lastSeq, 11)
  // Now it is done: a replay is a duplicate.
  assert.equal((await reactor.handleRawFrame(followFrame(11))).kind, 'duplicate')
})

test('while a follow is not done its place and id are not recorded', async () => {
  const client = stubClient([httpError(429, '1'), httpError(429, '1'), httpError(429, '1')])
  const reactor = new FollowReactor({ client, log: quiet, sleep: async () => {}, maxRetries: 2 })
  await reactor.handleRawFrame(JSON.stringify({ v: 1, type: 'hello', data: { seq: 10 } }))
  await assert.rejects(reactor.handleRawFrame(followFrame(11)), /429/)
  assert.equal(reactor.lastSeq, 10, 'a reconnect asks for 11 again')
  // The same frame comes again and is worked on, not skipped as a duplicate.
  const again = await reactor.handleRawFrame(followFrame(11))
  assert.equal(again.kind, 'follow')
  assert.equal(reactor.lastSeq, 11)
})

test('a lost answer is not repeated blindly, and a revoked key stops everything', async () => {
  const lost = stubClient([new TypeError('fetch failed')])
  const logs = []
  const reactor = new FollowReactor({ client: lost, log: { ...quiet, warn: (m) => logs.push(m) }, sleep: async () => {} })
  const out = await reactor.handleRawFrame(followFrame(5))
  assert.equal(out.kind, 'unknown')
  assert.equal(lost.calls.bump, 1, 'the add is not sent again: it may already have counted')
  assert.equal(reactor.lastSeq, 5)
  assert.equal(logs.some((m) => /may or may not/.test(m)), true)
  assert.equal((await reactor.handleRawFrame(followFrame(5))).kind, 'duplicate')

  const revoked = stubClient([httpError(401)])
  const r2 = new FollowReactor({ client: revoked, log: quiet, sleep: async () => {} })
  await assert.rejects(r2.handleRawFrame(followFrame(6)), (err) => err.fatal === true)
  assert.equal(r2.lastSeq, null)
})

test('a chat welcome that fails does not count the follow twice', async () => {
  const client = stubClient([4])
  client.sendChat = async () => {
    throw httpError(500)
  }
  const reactor = new FollowReactor({ client, log: quiet, sendChat: true, sleep: async () => {} })
  assert.equal((await reactor.handleRawFrame(followFrame(7))).kind, 'follow')
  assert.equal(client.calls.bump, 1)
  assert.equal(reactor.lastSeq, 7)
})

test('a reconnect hello does not move the checkpoint back or forward past unfinished work', async () => {
  const reactor = new FollowReactor({ client: stubClient([1]), log: quiet, sleep: async () => {} })
  await reactor.handleRawFrame(followFrame(20))
  await reactor.handleRawFrame(JSON.stringify({ v: 1, type: 'hello', data: { seq: 50 } }))
  assert.equal(reactor.lastSeq, 20)
})

test('the queue runs one reaction at a time, in order, and is bounded', async () => {
  const order = []
  let running = 0
  let most = 0
  const q = new SerialQueue({ limit: 3 })
  const gate = []
  const job = (n) => async () => {
    running++
    most = Math.max(most, running)
    await new Promise((r) => gate.push(r))
    order.push(n)
    running--
  }
  assert.equal(q.push(job(1)), true)
  assert.equal(q.push(job(2)), true)
  assert.equal(q.push(job(3)), true)
  await new Promise((r) => setImmediate(r))
  // One is running, two wait: room for one more, then it is full.
  assert.equal(q.push(job(4)), true)
  assert.equal(q.push(job(5)), false, 'full: the caller reconnects from its checkpoint')
  while (order.length < 4) {
    gate.shift()?.()
    await new Promise((r) => setImmediate(r))
  }
  assert.deepEqual(order, [1, 2, 3, 4])
  assert.equal(most, 1)
})

test('closing the queue drops what has not started and lets the running one finish', async () => {
  const done = []
  const q = new SerialQueue({ limit: 10 })
  let release
  q.push(async () => {
    await new Promise((r) => (release = r))
    done.push('a')
  })
  q.push(async () => done.push('b'))
  q.push(async () => done.push('c'))
  await new Promise((r) => setImmediate(r))
  assert.equal(q.clear(), 2)
  release()
  await q.idle()
  assert.deepEqual(done, ['a'])
})
