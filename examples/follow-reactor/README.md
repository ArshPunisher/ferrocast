# Ferrocast Follow Reactor (Node.js)

Zero-dependency Node.js 22+ example that connects to Ferrocast's `/api/v1/live` WebSocket using a scoped local API key, listens for `follow` events with lossless `?since=` reconnects, bumps a `follows` chat counter, and optionally posts a welcome message or triggers a celebratory alert.

## Required key scopes

Create a local API key in Ferrocast (**Settings > Advanced > Local API keys**) with:

- `read_only` (connect to `/api/v1/live` and inspect `/api/v1/status`)
- `chat` (bump the `follows` counter and optionally send a chat welcome)

## Run

```sh
export FERROCAST_API_KEY="fc_key_..."
node index.mjs
```

Optional environment variables:

- `FERROCAST_URL` (default `http://127.0.0.1:47820`). The key is sent in every request and in the live socket's address, so plain `http://` is accepted for this machine only (127.x, `::1`, `localhost`); any other address must be `https://` (the socket then uses `wss://`). Redirects are never followed, so the key can't be sent on to another address.
- `FERROCAST_SEND_CHAT` (`1` to greet new followers in connected chats; default `0` only bumps counter)
- `FERROCAST_COUNTER` (counter name to increment; default `follows`)

## Test

```sh
node --test follow-reactor.test.mjs
```

## How it keeps its place

- It resumes from the last event whose reaction is **done** (`?since=`), not the last one that arrived.
- A counter update that gets a `429` (or another definite "no") is waited out (`Retry-After`, at most 60 s) and tried again, a few times; the follow is only remembered once it counted.
- If the answer never came (a network error, a 5xx) the add is **not** sent again, since it may already have counted: it is logged as "may or may not have counted" and passed. The counter's add is not idempotent, so a blind repeat could count a follow twice.
- Reactions run one at a time, in order. If more than 200 wait (a replay burst), the socket is closed and opened again from the checkpoint; what had not started is dropped and comes again.
- A revoked key (401/403) stops the program.
