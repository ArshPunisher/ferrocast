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

- `FERROCAST_URL` (default `http://127.0.0.1:47820`)
- `FERROCAST_SEND_CHAT` (`1` to greet new followers in connected chats; default `0` only bumps counter)
- `FERROCAST_COUNTER` (counter name to increment; default `follows`)

## Test

```sh
node --test follow-reactor.test.mjs
```
