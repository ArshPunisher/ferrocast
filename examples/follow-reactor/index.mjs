import { pathToFileURL } from 'node:url'

const DEFAULT_BASE_URL = 'http://127.0.0.1:47820'

/**
 * The API address, or an Error. The key travels in every request (and in the live socket's address),
 * so plain http is for this machine only (127.x, ::1, localhost); anything else must be https.
 */
export function checkBaseUrl(raw) {
  let u
  try {
    u = new URL(String(raw).trim())
  } catch {
    throw new Error('FERROCAST_URL is not a valid address.')
  }
  if (u.username || u.password) {
    throw new Error('FERROCAST_URL cannot carry a user name or password.')
  }
  const host = u.hostname.replace(/^\[|\]$/g, '')
  const loopback = host === 'localhost' || host === '::1' || /^127(\.\d{1,3}){3}$/.test(host)
  if (u.protocol === 'https:' && host) return String(raw).trim()
  if (u.protocol === 'http:' && loopback) return String(raw).trim()
  if (u.protocol === 'http:') {
    throw new Error(
      'FERROCAST_URL is plain http to another machine: the API key would cross the network in the clear. Use https, or an address on this machine (127.0.0.1).',
    )
  }
  throw new Error('FERROCAST_URL must start with https:// (or http:// for this machine).')
}

export function parseConfig(env = process.env) {
  const apiKey = (env.FERROCAST_API_KEY ?? '').trim()
  if (!apiKey.startsWith('fc_key_')) {
    throw new Error('Set FERROCAST_API_KEY to a local Ferrocast API key (fc_key_...).')
  }
  const baseUrl = checkBaseUrl(env.FERROCAST_URL ?? DEFAULT_BASE_URL).replace(/\/+$/, '')
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
      // Never followed: a redirect would carry the Authorization header on to wherever it points.
      redirect: 'manual',
      headers: {
        Authorization: `Bearer ${this.#apiKey}`,
        ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    })
    if (res.status >= 300 && res.status < 400) {
      const err = new Error(
        `${method} ${relPath} was redirected; the API key is never sent to another address. Set FERROCAST_URL to the address Ferrocast answers on directly.`,
      )
      err.code = 'redirect_refused'
      err.status = res.status
      err.fatal = true
      throw err
    }
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

/** Seconds to wait from a Retry-After header (a number, or a date), 1 to 60; 2 when it says nothing. */
export function retryDelayMs(retryAfter) {
  if (retryAfter === null || retryAfter === undefined || retryAfter === '') return 2000
  const secs = Number(retryAfter)
  const wait = Number.isFinite(secs) ? secs : (Date.parse(String(retryAfter)) - Date.now()) / 1000
  if (!Number.isFinite(wait)) return 2000
  return Math.min(60, Math.max(1, Math.ceil(wait))) * 1000
}

/**
 * One reaction at a time, in order, with a limit on how many wait. When it is full the caller
 * stops listening and reconnects from its checkpoint, so nothing is lost and nothing piles up.
 */
export class SerialQueue {
  #limit
  #jobs = []
  #running = null

  constructor({ limit = 100 } = {}) {
    this.#limit = limit
  }

  /** false when full (the job was not taken). */
  push(job) {
    if (this.#jobs.length >= this.#limit) return false
    this.#jobs.push(job)
    this.#running ??= this.#drain()
    return true
  }

  /** Drops what has not started → how many. The one running finishes. */
  clear() {
    const n = this.#jobs.length
    this.#jobs = []
    return n
  }

  async idle() {
    await this.#running
  }

  async #drain() {
    await Promise.resolve()
    while (this.#jobs.length > 0) {
      const job = this.#jobs.shift()
      try {
        await job()
      } catch {
        // A job reports its own trouble.
      }
    }
    this.#running = null
  }
}

export class FollowReactor {
  #client
  #counterName
  #sendChat
  #seen = new Set()
  #checkpoint = null
  #log
  #sleep
  #maxRetries

  constructor({
    client,
    counterName = 'follows',
    sendChat = false,
    log = console,
    sleep = (ms) => new Promise((r) => setTimeout(r, ms)),
    maxRetries = 5,
  }) {
    this.#client = client
    this.#counterName = counterName
    this.#sendChat = sendChat
    this.#log = log
    this.#sleep = sleep
    this.#maxRetries = maxRetries
  }

  /** Where to resume from: the last event whose reaction is done (null before the first). */
  get lastSeq() {
    return this.#checkpoint
  }

  #done(seq) {
    if (typeof seq === 'number' && (this.#checkpoint === null || seq > this.#checkpoint)) {
      this.#checkpoint = seq
    }
  }

  #remember(id) {
    if (!id) return
    this.#seen.add(id)
    if (this.#seen.size > 1000) {
      this.#seen.delete(this.#seen.values().next().value)
    }
  }

  async handleRawFrame(rawText) {
    let frame
    try {
      frame = JSON.parse(rawText)
    } catch {
      return null
    }
    if (frame.type === 'hello' && typeof frame.data?.seq === 'number') {
      // Only a first connection starts here: after a reconnect the checkpoint is what was done.
      if (this.#checkpoint === null) this.#checkpoint = frame.data.seq
      return { kind: 'hello', seq: this.#checkpoint }
    }
    if (frame.type === 'resync') {
      this.#log.warn(`[resync] missed=${frame.data?.missed ?? 0}; refreshing status`)
      await this.#client.getStatus()
      return { kind: 'resync' }
    }
    const follow = extractFollowEvent(frame)
    if (!follow) {
      this.#done(frame.seq)
      return null
    }
    if (follow.id && this.#seen.has(follow.id)) {
      this.#done(follow.seq)
      return { kind: 'duplicate', id: follow.id }
    }

    const res = await this.#bump(follow)
    // Counted (or, after a lost answer, left): now it is done, and only now.
    this.#remember(follow.id)
    this.#done(follow.seq)
    if (res === null) {
      return { kind: 'unknown', id: follow.id }
    }
    const count = res.data?.value
    this.#log.info(
      `[follow] ${follow.actorName} on ${follow.platform} -> ${this.#counterName}=${count ?? 'updated'}`,
    )
    if (this.#sendChat && ['twitch', 'youtube', 'kick'].includes(follow.platform)) {
      try {
        await this.#client.sendChat(follow.platform, `Welcome to the stream, ${follow.actorName}!`)
      } catch (err) {
        // The follow is counted; the greeting is not worth counting it twice.
        this.#log.warn(`[chat] the welcome was not sent: ${err.message}`)
      }
    }
    return { kind: 'follow', actorName: follow.actorName, platform: follow.platform, count }
  }

  /**
   * Adds one to the counter. A definite "no" (429, other 4xx) is waited out and tried again, a few
   * times; a revoked key stops everything; an answer that never came (network, 5xx) is not
   * repeated, since the add may already have counted: null.
   */
  async #bump(follow) {
    for (let attempt = 0; ; attempt++) {
      try {
        return await this.#client.bumpCounter(this.#counterName, 1)
      } catch (err) {
        const status = err.status
        if (status === 401 || status === 403 || err.fatal) {
          err.fatal = true
          throw err
        }
        const definite = typeof status === 'number' && status >= 400 && status < 500
        if (!definite) {
          this.#log.warn(
            `[follow] ${follow.actorName}: no answer to the counter update (${err.message}); it may or may not have counted, so it is not sent again`,
          )
          return null
        }
        if (attempt >= this.#maxRetries) throw err
        const wait = retryDelayMs(err.retryAfter)
        this.#log.warn(`[follow] counter update refused (${status}); trying again in ${wait / 1000}s`)
        await this.#sleep(wait)
      }
    }
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
    // One reaction at a time, in order. If they pile up (a replay burst), the socket is closed and
    // opened again from the last finished event: nothing is lost and no burst goes out at once.
    const queue = new SerialQueue({ limit: 200 })
    await new Promise((resolve) => {
      const ws = new globalThis.WebSocket(url)
      activeWs = ws
      ws.addEventListener('message', (ev) => {
        const raw = String(ev.data)
        const taken = queue.push(async () => {
          try {
            await reactor.handleRawFrame(raw)
          } catch (err) {
            console.error(`Reaction failed: ${err.message}`)
            if (err.fatal) {
              stopped = true
              ws.close()
            }
          }
        })
        if (!taken) {
          console.warn('Falling behind: reconnecting from the last finished event.')
          queue.clear()
          ws.close()
        }
      })
      ws.addEventListener('error', () => {
        // Closed handler triggers reconnect.
      })
      ws.addEventListener('close', () => {
        // What has not started is dropped: it comes again from the checkpoint.
        queue.clear()
        resolve()
      })
    })
    await queue.idle()
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
