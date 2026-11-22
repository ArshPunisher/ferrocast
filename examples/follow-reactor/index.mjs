import { pathToFileURL } from 'node:url'

const DEFAULT_BASE_URL = 'http://127.0.0.1:47820'

export function parseConfig(env = process.env) {
  const apiKey = (env.FERROCAST_API_KEY ?? '').trim()
  if (!apiKey.startsWith('fc_key_')) {
    throw new Error('Set FERROCAST_API_KEY to a local Ferrocast API key (fc_key_...).')
  }
  const baseUrl = (env.FERROCAST_URL ?? DEFAULT_BASE_URL).replace(/\/+$/, '')
  return {
    apiKey,
    baseUrl,
    counterName: (env.FERROCAST_COUNTER ?? 'follows').trim() || 'follows',
    sendChat: env.FERROCAST_SEND_CHAT === '1',
  }
}

export function liveWebSocketUrl(baseUrl, apiKey, since = null) {
  const wsBase = baseUrl.replace(/^http:\/\//i, 'ws://').replace(/^https:\/\//i, 'wss://')
  const params = new URLSearchParams({ key: apiKey })
  if (since !== null && since !== undefined) {
    params.set('since', String(since))
  }
  return `${wsBase}/api/v1/live?${params.toString()}`
}

export function extractFollowEvent(frame) {
  if (!frame || frame.type !== 'event' || typeof frame.data !== 'object' || frame.data === null) {
    return null
  }
  const ev = frame.data
  if (ev.kind?.type !== 'follow') {
    return null
  }
  return {
    id: String(ev.id ?? ''),
    seq: typeof frame.seq === 'number' ? frame.seq : typeof ev.seq === 'number' ? ev.seq : null,
    platform: String(ev.platform ?? 'unknown'),
    actorName: String(ev.actor?.name ?? 'someone'),
  }
}

export class FerrocastApiClient {
  #baseUrl
  #apiKey
  #fetchImpl

  constructor({ baseUrl, apiKey, fetchImpl = globalThis.fetch }) {
    this.#baseUrl = baseUrl
    this.#apiKey = apiKey
    this.#fetchImpl = fetchImpl
  }

  async requestJson(method, relPath, body = undefined) {
    const res = await this.#fetchImpl(`${this.#baseUrl}/api/v1${relPath}`, {
      method,
      headers: {
        Authorization: `Bearer ${this.#apiKey}`,
        ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    })
    const remaining = res.headers.get('x-ratelimit-remaining')
    const text = await res.text()
    let data = null
    if (text) {
      try {
        data = JSON.parse(text)
      } catch {
        data = text
      }
    }
    if (!res.ok) {
      const msg = data?.error?.message ?? `HTTP ${res.status}`
      const err = new Error(`${method} ${relPath} failed (${res.status}): ${msg}`)
      err.status = res.status
      err.retryAfter = res.headers.get('retry-after')
      throw err
    }
    return { status: res.status, remaining, data }
  }

  getStatus() {
    return this.requestJson('GET', '/status')
  }

  bumpCounter(counterName, amount = 1) {
    const encoded = encodeURIComponent(counterName)
    return this.requestJson('POST', `/counters/${encoded}/change`, { op: 'add', amount })
  }

  sendChat(platform, text) {
    return this.requestJson('POST', '/chat/send', { platforms: platform, text })
  }
}

export class FollowReactor {
  #client
  #counterName
  #sendChat
  #seen = new Set()
  #lastSeq = null
  #log

  constructor({ client, counterName = 'follows', sendChat = false, log = console }) {
    this.#client = client
    this.#counterName = counterName
    this.#sendChat = sendChat
    this.#log = log
  }

  get lastSeq() {
    return this.#lastSeq
  }

  async handleRawFrame(rawText) {
    let frame
    try {
      frame = JSON.parse(rawText)
    } catch {
      return null
    }
    if (frame.type === 'hello' && typeof frame.data?.seq === 'number') {
      this.#lastSeq = frame.data.seq
      return { kind: 'hello', seq: this.#lastSeq }
    }
    if (frame.type === 'resync') {
      this.#log.warn(`[resync] missed=${frame.data?.missed ?? 0}; refreshing status`)
      await this.#client.getStatus()
      return { kind: 'resync' }
    }
    if (typeof frame.seq === 'number') {
      this.#lastSeq = frame.seq
    }
    const follow = extractFollowEvent(frame)
    if (!follow) {
      return null
    }
    if (follow.id && this.#seen.has(follow.id)) {
      return { kind: 'duplicate', id: follow.id }
    }
    if (follow.id) {
      this.#seen.add(follow.id)
      if (this.#seen.size > 1000) {
        const first = this.#seen.values().next().value
        this.#seen.delete(first)
      }
    }

    const res = await this.#client.bumpCounter(this.#counterName, 1)
    const count = res.data?.value
    this.#log.info(
      `[follow] ${follow.actorName} on ${follow.platform} -> ${this.#counterName}=${count ?? 'updated'}`,
    )
    if (this.#sendChat && ['twitch', 'youtube', 'kick'].includes(follow.platform)) {
      await this.#client.sendChat(follow.platform, `Welcome to the stream, ${follow.actorName}!`)
    }
    return { kind: 'follow', actorName: follow.actorName, platform: follow.platform, count }
  }
}

async function runCli() {
  const cfg = parseConfig()
  const client = new FerrocastApiClient({ baseUrl: cfg.baseUrl, apiKey: cfg.apiKey })
  const status = await client.getStatus()
  console.info(
    `Connected to Ferrocast v${status.data?.version ?? '?'} at ${cfg.baseUrl} (rate limit remaining: ${status.remaining ?? '?'})`,
  )

  const reactor = new FollowReactor({
    client,
    counterName: cfg.counterName,
    sendChat: cfg.sendChat,
  })

  let stopped = false
  let activeWs = null
  const stop = () => {
    stopped = true
    activeWs?.close()
  }
  process.on('SIGINT', stop)
  process.on('SIGTERM', stop)

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
  while (!stopped) {
    const url = liveWebSocketUrl(cfg.baseUrl, cfg.apiKey, reactor.lastSeq)
    await new Promise((resolve) => {
      const ws = new globalThis.WebSocket(url)
      activeWs = ws
      ws.addEventListener('message', async (ev) => {
        try {
          await reactor.handleRawFrame(String(ev.data))
        } catch (err) {
          console.error(`Reaction failed: ${err.message}`)
        }
      })
      ws.addEventListener('error', () => {
        // Closed handler triggers reconnect.
      })
      ws.addEventListener('close', resolve)
    })
    if (!stopped) {
      await sleep(2000)
    }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  runCli().catch((err) => {
    console.error(err.message)
    process.exit(1)
  })
}
