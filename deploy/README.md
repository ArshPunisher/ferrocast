# Deploying the relay

The relay (`crates/relay`) runs on the Oracle ARM server in Docker at
`/opt/ferrocast-relay`, reached only through its own Cloudflare Tunnel
(https://ferrocast-relay.clearcraftlabs.com). No source code lives on the server.

```
push to main ─▶ 🧱 Build relay: tests + image health check ─▶ ghcr.io/arshpunisher/ferrocast-relay:1.0.YYMMDD-sha
                                                             (not live)
Run workflow ─▶ 🚀 Deploy relay ─SSH─▶ /opt/ferrocast-relay/deploy.sh on the server:
                 pull ▸ switch ▸ health check ▸ (previous version back if unhealthy)
```

## Releasing

**Actions → 🚀 Deploy relay → Run workflow.** Leave the version blank for the latest
successful build, or enter an older one (`1.0.261018-ab12cd3`) to roll back. The run's
summary says what went live, or that the previous version was put back.

## Files

| File | Where it runs | What it does |
|---|---|---|
| `relay.Dockerfile` | build | The relay image |
| `docker-compose.yml` | server | Relay + `cloudflared`; the relay's image comes from `RELAY_IMAGE`/`RELAY_TAG` |
| `relay-server-deploy.sh` | server (`/opt/ferrocast-relay/deploy.sh`) | Pull, switch, health check, rollback; the only thing the deploy key can run |
| `deploy-relay.sh` | your Mac | First install, and changing config or secrets (`../secrets/relay.env`). Once GitHub deploys, it keeps the live version and never swaps in a local build |
| `relay-ci-setup.sh` | your Mac, once | Lets GitHub deploy (below), and installs `razorpay-keys.sh` and `vault-key.sh` |
| `razorpay-keys.sh` | server (`/opt/ferrocast-relay/razorpay-keys.sh`) | Ferrocast Pro's Razorpay keys, plans, webhook secret, and the `PRO_SALES_OPEN` switch (below) |
| `tests/razorpay-keys.test.sh` | CI, your Mac | Tests `razorpay-keys.sh` against a fake Razorpay |
| `vault-key.sh` | server (`/opt/ferrocast-relay/vault-key.sh`) | Makes the relay's vault key (`RELAY_VAULT_KEY`) once, without ever showing it (below) |
| `tests/vault-key.test.sh` | CI, your Mac | Tests `vault-key.sh` |

## One-time setup for GitHub deploys

Needs the server's `deploy` user (made for TransformX: in the `docker` group and logged in
to ghcr.io with a `read:packages` token, see `transformx-infra/DEPLOY.md` §2 and §6) and
the GitHub CLI logged in on your Mac. Then, from this repo:

```bash
deploy/relay-ci-setup.sh          # host from RELAY_HOST in ../secrets/relay.env
```

It makes a deploy-only key (`~/.ssh/ferrocast-relay-deploy`), installs the server script,
locks the key to it in `~deploy/.ssh/authorized_keys` (separate from TransformX's key),
hands the relay files to `deploy`, saves `RELAY_DEPLOY_HOST`, `_PORT`, `_SSH_KEY` and
`_KNOWN_HOSTS` as secrets of the repo's `relay` environment (usable from `main` only; the
host key comes from your own verified `~/.ssh/known_hosts`), and checks that the key can
reach the script and nothing else. Safe to run again.

## Ferrocast Pro: Razorpay keys

Pro uses the Razorpay account's existing key pair (the same one as TransformX and Hookline;
**never regenerate it**, that would break them). First add `https://ferrocast.clearcraftlabs.com`
as an approved website in Razorpay (Account & Settings → Websites & API keys → Add additional
website/app). Then deploy a relay that has billing, run `deploy/relay-ci-setup.sh` once more (it
installs the script), and on the server:

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/razorpay-keys.sh'
```

It asks for the Key ID and the Key Secret (hidden), checks Razorpay accepts them, backs up
`relay.env` to `state/env-backup-…`, saves the keys, creates the two plans (₹199/month,
₹1,999/year) once per mode, makes a webhook secret, restarts the relay, and prints what to add
in Razorpay → Webhooks: URL `https://ferrocast-relay.clearcraftlabs.com/billing/razorpay/webhook`,
the secret, and the `subscription.*` and `refund.*` events. Start with test-mode keys
(`rzp_test_…`); run it again with live keys later (new plans and a new webhook in live mode).

Pro is not sold until you say so; the relay answers checkout with 409 "Pro isn't on sale yet":

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/razorpay-keys.sh --status'        # no secrets shown
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/razorpay-keys.sh --sales open'    # after version 1
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/razorpay-keys.sh --sales closed'
```

`deploy-relay.sh` keeps these settings when it rewrites `relay.env`.

## Ferrocast Pro: the vault key

Pro keeps a few secrets on the relay (platform sign-ins for the cloud bot, a mail address),
always encrypted with the relay's vault key. Make it once, on the server (after
`deploy/relay-ci-setup.sh` has installed the script):

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/vault-key.sh'           # makes it, restarts the relay
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/vault-key.sh --status'  # set or missing, never the key
```

It makes 32 random bytes on the server, saves them in `relay.env` (backed up first), never
prints them, and restarts the relay. Running it again keeps the key: a new key couldn't open
what the old one sealed, so it never changes. Until it's set, Pro features that keep secrets
answer 503 `vault_unavailable` and everything else works. `deploy-relay.sh` keeps it when it
rewrites `relay.env`. Keep the server's `state/env-backup-*` files as safe as `relay.env`.

## Ferrocast Pro: voices (Google Cloud TTS)

In the Google Cloud console (a project with billing, which the free tier needs): enable the
Cloud Text-to-Speech API, make an API key, and restrict it to that API only (APIs & Services ›
Credentials › the key › API restrictions). Also set a budget alert of ₹1 on the project. Then
add it on the server by hand, so it never passes through this Mac:

```bash
ssh -t <server> 'sudo -u deploy sh -c "cp /opt/ferrocast-relay/relay.env /opt/ferrocast-relay/state/env-backup-tts && \
  nano /opt/ferrocast-relay/relay.env"'      # add GOOGLE_TTS_KEY=AIza… (and TTS_GLOBAL_MONTHLY_CHARS if not 800000)
ssh <server> 'cd /opt/ferrocast-relay && sudo docker compose up -d'
```

The relay checks two caps before it ever calls Google: 20,000 characters a month per
subscription, and `TTS_GLOBAL_MONTHLY_CHARS` (default 800,000) for everyone together, never
more than Google's free 1,000,000. Once that's reached the voices "rest" (503) until the 1st
and Ferrocast reads with each PC's own voice. Without the key, `features.tts` is false and the
dashboard doesn't offer the voices. `deploy-relay.sh` keeps both lines when it rewrites
`relay.env`.

## Ferrocast Pro: dashboard from anywhere

Phones open the dashboard at `https://remote.clearcraftlabs.com/app/` and its API at `/r/…` on
the same host (API.md §9.5). To switch it on:

1. Cloudflare → Networking → Tunnels → ferrocast-relay → Public hostnames: add
   `remote.clearcraftlabs.com` → `http://ferrocast-relay:8787` (like `tip.clearcraftlabs.com`).
2. In `../ferrocast-web`: `pnpm build:embed` (writes `dist/dashboard`).
3. `deploy/deploy-relay.sh`: writes `RELAY_REMOTE_URL` and `RELAY_DASHBOARD_DIR=/dashboard/current`,
   and copies the build to `/opt/ferrocast-relay/dashboard/releases/<date>`, then switches the
   `current` link inside to it with one rename (`deploy/dashboard-swap.sh`). The folder itself
   is mounted read-only and never renamed: a Docker bind mount follows the folder, so swapping
   the folder would leave the relay serving the old copy until a restart. The live release and
   the one before stay; to roll back by hand, point `current` at the older one
   (`sudo ln -sfn releases/<date> current.next && sudo mv -fT current.next current` inside the
   folder). Run it again after each dashboard release so phones get the same version as the
   app. Test: `deploy/tests/dashboard-swap.test.sh`.

Phone tokens and pairing codes are stored as hashes keyed with `RELAY_REMOTE_PEPPER` if it's set
on the server (any long random text, added by hand, kept by `deploy-relay.sh`), else with a key
derived from `RELAY_VAULT_KEY`. Changing either signs every phone out.

## Ferrocast Pro: the Discord bot

The app "Ferrocast" (id 1561463863197835294) at discord.com/developers. In the portal:

1. **OAuth2 → Redirects**: add `https://ferrocast-relay.clearcraftlabs.com/discord/callback`
   (one address for both the bot install and viewers linking their accounts).
2. **General Information → Interactions Endpoint URL**:
   `https://ferrocast-relay.clearcraftlabs.com/discord/interactions`. Save it only after the
   relay runs with the keys below: Discord checks the address with a signed PING at once.
3. **Bot**: Public Bot on (streamers add it to their own servers), no privileged intents
   (the relay uses REST only, no gateway). Reset Token to get `DISCORD_BOT_TOKEN`.
4. **Installation**: Install Link "None" (the relay makes its own invite with the state it
   checks), Guild Install only.

Then add the four values on the server by hand, so they never pass through this Mac:

```bash
ssh -t <server> 'sudo -u deploy sh -c "cp /opt/ferrocast-relay/relay.env /opt/ferrocast-relay/state/env-backup-discord && \
  nano /opt/ferrocast-relay/relay.env"'   # DISCORD_APP_ID=…, DISCORD_PUBLIC_KEY=…, DISCORD_CLIENT_SECRET=…, DISCORD_BOT_TOKEN=…
ssh <server> 'cd /opt/ferrocast-relay && sudo docker compose up -d'
```

With all four set, `features.discord` is true and Settings › Discord shows "Ferrocast bot".
Missing or partial, the relay logs which and the bot stays off; the free webhook posts keep
working. `deploy-relay.sh` keeps the lines when it rewrites `relay.env`.
