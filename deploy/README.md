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
| `relay-ci-setup.sh` | your Mac, once | Lets GitHub deploy (below), and installs `razorpay-keys.sh`, `vault-key.sh`, `pro-keys.sh`, `pro-grant.sh`, `bot-check.sh`, `relay-backup.sh` and `control-center-key.sh` |
| `razorpay-keys.sh` | server (`/opt/ferrocast-relay/razorpay-keys.sh`) | Ferrocast Pro's Razorpay keys, plans, webhook secret, and the `PRO_SALES_OPEN` switch (below) |
| `tests/razorpay-keys.test.sh` | CI, your Mac | Tests `razorpay-keys.sh` against a fake Razorpay |
| `vault-key.sh` | server (`/opt/ferrocast-relay/vault-key.sh`) | Makes the relay's vault key (`RELAY_VAULT_KEY`) once, without ever showing it (below) |
| `tests/vault-key.test.sh` | CI, your Mac | Tests `vault-key.sh` |
| `pro-keys.sh` | server (`/opt/ferrocast-relay/pro-keys.sh`) | Types Pro's service keys (Google voices, the Discord bot, the weekly email's Brevo key, the cloud bot's Twitch app and Google client, custom tip addresses' Cloudflare token and zone, Google sign-in for accounts with `--google`) with typing hidden, never showing them (below) |
| `tests/pro-keys.test.sh` | CI, your Mac | Tests `pro-keys.sh` |
| `pro-grant.sh` | server (`/opt/ferrocast-relay/pro-grant.sh`) | The owner's test Pro for one install, without Razorpay (below) |
| `tests/pro-grant.test.sh` | CI, your Mac | Tests `pro-grant.sh` |
| `bot-check.sh` | server (`/opt/ferrocast-relay/bot-check.sh`) | Read-only look at the Twitch cloud bot: conduit, shard, EventSub, channel rows (below) |
| `tests/bot-check.test.sh` | CI, your Mac | Tests `bot-check.sh` |
| `control-center-key.sh` | server (`/opt/ferrocast-relay/control-center-key.sh`) | The token the Control Center reads `/admin/v1/stats` with, in `relay.env` and TransformX's `.env.prod`, never shown (below) |
| `tests/control-center-key.test.sh` | CI, your Mac | Tests `control-center-key.sh` |

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
type it on the server, so it never passes through this Mac (typing is hidden; it backs up
relay.env and restarts the relay):

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --tts'
```

`TTS_GLOBAL_MONTHLY_CHARS` (default 800000) is still set by hand in relay.env if it should differ.

The relay checks two caps before it ever calls Google: 20,000 characters a month per
subscription, and `TTS_GLOBAL_MONTHLY_CHARS` (default 800,000) for everyone together, never
more than Google's free 1,000,000. Once that's reached the voices "rest" (503) until the 1st
and Ferrocast reads with each PC's own voice. Without the key, `features.tts` is false and the
dashboard doesn't offer the voices. `deploy-relay.sh` keeps both lines when it rewrites
`relay.env`.

## Ferrocast Pro: smart features (free tier only)

Smart moderation, smart replies and the stream recap (docs/API.md §9.10) are **off until a key
is set**. They use a language model's **free tier**, with hard daily caps on the relay, so
nothing can be billed:

- **Cloudflare Workers AI** (recommended). In the Cloudflare dashboard, on an account that is on
  the **Workers Free** plan (then Workers AI stops at its free 10,000 Neurons a day instead of
  billing): copy the **Account ID** from the account's home page, and make an API token: My
  Profile → API Tokens → Create Token → Custom token → permission *Account → Workers AI → Read*,
  that account only. Cloudflare doesn't train on what it's sent.
- **Google Gemini** (only if not Cloudflare): [aistudio.google.com](https://aistudio.google.com)
  → Get API key, in a Google project **without billing** (a project with billing, like the one
  for voices, is charged). On the free tier Google may use prompts to improve its products, so
  viewers' chat goes to Google; prefer Cloudflare.

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --ai'
```

It asks for `CLOUDFLARE_AI_ACCOUNT_ID` (shown as typed), `CLOUDFLARE_AI_TOKEN` and, optionally,
`GEMINI_API_KEY` (Enter skips; Cloudflare wins while both of its settings are there). Caps:
`SMART_DAILY_CALLS` (default 140 model calls a day relay-wide, never above 1,000; 140 keeps
Cloudflare under its free 10,000 Neurons even in the worst case), plus per subscription a day 30
moderation checks, 20 replies and 3 recaps; set `SMART_DAILY_CALLS` (and `SMART_MODEL` /
`SMART_GEMINI_MODEL`) by hand in relay.env only if needed. When the day's calls or the free
allowance are used up the features rest until 00:00 UTC and the app says so; everything else
keeps working. Check with `pro-keys.sh --status`; the Control Center shows calls used today
(`usage.smart`). `deploy-relay.sh` keeps all of these.

## Ferrocast Pro: dashboard from anywhere

Phones open the dashboard at `https://remote.clearcraftlabs.com/app/` and its API at `/r/…` on
the same host (API.md §9.5). To switch it on:

1. Cloudflare → Networking → Tunnels → ferrocast-relay → Public hostnames: add
   `remote.clearcraftlabs.com` → `http://ferrocast-relay:8787` (like `tip.clearcraftlabs.com`).
2. In `../ferrocast-web`: `pnpm build:embed` (writes `dist/dashboard` and `dist/overlays`).
3. `deploy/deploy-relay.sh`: writes `RELAY_REMOTE_URL`, `RELAY_DASHBOARD_DIR=/dashboard/current`
   and `RELAY_OVERLAYS_DIR=/overlays/current` (cloud mode's overlay pages, swapped the same way
   into `/opt/ferrocast-relay/overlays`),
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

Then type the four values on the server, so they never pass through this Mac:

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --discord'
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --status'   # set or missing, never the keys
```

With all four set, `features.discord` is true and Settings › Discord shows "Ferrocast bot".
Missing or partial, the relay logs which and the bot stays off; the free webhook posts keep
working. `deploy-relay.sh` keeps the lines when it rewrites `relay.env`.

## Ferrocast Pro: the weekly email (Brevo)

The sending domain `mail.clearcraftlabs.com` is already authenticated in Brevo (SPF, DKIM,
DMARC). In Brevo: **SMTP & API → API keys → Generate a new API key** (it starts `xkeysib-`;
the SMTP key that starts `xsmtpsib-` doesn't work with the API). Type it on the server:

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --mail'
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --status'
```

With `BREVO_API_KEY` and the vault key set, `features.weekly_email` is true and `#/history`
shows the Weekly email card. Optional, by hand in `relay.env`: `MAIL_FROM` (default
`Ferrocast <reports@mail.clearcraftlabs.com>`) and `MAIL_TOKEN_KEY` (signs confirm and
unsubscribe links; default: derived from `RELAY_VAULT_KEY`; changing it breaks links in emails
already sent) and `MAIL_DAILY_BUDGET` (emails a UTC day, relay-wide, confirms and reports
together; default 270, under Brevo's free 300; raise it only with a bigger Brevo plan).
`deploy-relay.sh` keeps all four. Reports go out at 04:00 UTC (the relay checks hourly) on
each subscription's day; past the budget (or when Brevo says its limit is reached) the rest
wait for the next day, up to 2 days late, and confirm emails answer "Try again tomorrow."
Watch the count before many subscriptions (a paid plan or a second sender is a follow-up).

## Ferrocast Pro: the 24/7 cloud bot (Twitch, Kick and YouTube)

**Twitch** needs a second app, separate from the desktop's public one. At
[dev.twitch.tv/console](https://dev.twitch.tv/console) → Applications → Register Your Application:

| Field | Value |
|---|---|
| Name | `Ferrocast Cloud` (Twitch names are global; if it's taken, `Ferrocast Cloud Bot`) |
| OAuth Redirect URLs | `https://ferrocast-relay.clearcraftlabs.com/v1/bot/twitch/callback` |
| Category | Chat Bot |
| Client Type | Confidential |

Then Manage → copy the Client ID and press New Secret. The EventSub callback isn't typed in
the console: the relay makes one conduit and points its webhook shard at
`https://ferrocast-relay.clearcraftlabs.com/v1/bot/twitch/eventsub` itself (Twitch sends a
challenge there at once, which the relay answers). Make the EventSub secret with
`openssl rand -hex 32` and type all three on the server:

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --twitch'
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --status'
```

**Kick** uses the relay's existing Kick app and its redirect (`/v1/kick/callback`); nothing
new to register (the app already has the `chat:write` scope).

With the vault key and Twitch's three (or Kick's app) set, `features.cloud_bot` is true and
the Bot page shows "Keep the bot on when this PC is off". Missing or partial, the relay logs
which and that platform stays off; the bot on the PC keeps working. `deploy-relay.sh` keeps the
lines when it rewrites `relay.env`.

### YouTube (beta, M12)

YouTube needs its own Google OAuth client, separate from the desktop app's ("Desktop app"
type, public). Use the same Google Cloud project as the desktop app only if you accept that
both share its 10,000 units a day; a **separate project** ("Ferrocast Cloud") gives the cloud
bot its own 10,000 and is recommended. In [console.cloud.google.com](https://console.cloud.google.com):

1. Create (or pick) the project, then APIs & Services → Library → enable **YouTube Data API v3**.
2. APIs & Services → **OAuth consent screen** (Google Auth Platform → Branding / Audience /
   Data access): app name `Ferrocast Cloud`, support email, the app home page
   `https://ferrocast.clearcraftlabs.com`, privacy policy and terms links, authorised domain
   `clearcraftlabs.com`. User type **External**. Under Data access add exactly one scope:
   `https://www.googleapis.com/auth/youtube.force-ssl` (reading, posting and moderating live
   chat all need it; `youtube.readonly` adds nothing, so it isn't asked).
3. Credentials → Create credentials → **OAuth client ID** → type **Web application**, name
   `Ferrocast Cloud`, Authorised redirect URI
   `https://ferrocast-relay.clearcraftlabs.com/v1/bot/youtube/callback` (no JavaScript origins).
   Copy the Client ID (`…apps.googleusercontent.com`) and the Client secret (`GOCSPX-…`).

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --youtube'
```

**Verification.** `youtube.force-ssl` is a *sensitive* scope. While the consent screen is in
**Testing**, only the test users you list (at most **100** Google accounts) can sign in, they
see an "unverified app" warning, and their sign-ins **expire after 7 days** (the cloud bot then
shows "Google signed the cloud bot out of YouTube"). For everyone, press **Publish app** and
submit it for Google's verification (a privacy policy that lists what the relay keeps, a demo
video of the sign-in and the scope in use, domain ownership in Search Console); it is not a
restricted scope, so no paid security assessment. Separately, the 10,000 units a day can be
raised with the **YouTube API Services audit and quota extension** form once the beta is used.

Budget: `YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB` (default: the relay's budget) and `YOUTUBE_CLOUD_DAILY_UNITS`
(default 9,000, under Google's 10,000 so the relay's own checks keep a margin), counted per
Pacific day (YouTube's quota resets at midnight Pacific time); set them by hand in
`relay.env` only after a quota increase. `YOUTUBE_CLOUD_DESKTOP_CLIENT_ID` defaults to the
official desktop app's public client id (the PC proves its channel with a token of that app).
`YOUTUBE_CLOUD_TIMER_GAP_MIN` (default 15, 0 = off) spaces cloud timer posts on YouTube; of
each budget, replies, timers and moderation leave a reserve for reading (300 a subscription,
10% relay-wide) and live checks are capped (150 a subscription, 30% relay-wide).
Both id and secret, plus `RELAY_VAULT_KEY`, or YouTube stays off (`available.youtube` false).
**Fair share** (1.4): on a busy day each subscription's limit is cut to an even part of the
relay's budget among those who used YouTube that day (never below 600). **From the web** (1.4):
a Pro account can connect its channel on the web dashboard's Account page with no PC; it uses
the same client and the same redirect URI, so nothing new is registered in Google. For
verification (the 100 test users, the 7-day sign-ins) see docs/GOOGLE-VERIFICATION.md.
The relay calls `oauth2.googleapis.com`, `www.googleapis.com` and the gRPC
`youtube.googleapis.com:443` (outbound HTTPS).

## Ferrocast Pro: custom tip addresses (Cloudflare for SaaS)

A streamer types `tips.mychannel.com` on the Tips screen; the relay adds it as a **custom
hostname** on the `clearcraftlabs.com` zone (no `custom_metadata`, which is Enterprise only)
and shows them **two records** to add: a CNAME
(`tips` → `tip.clearcraftlabs.com`) and a TXT record `_ferrocast.tips` with a value only this
relay makes for that install and hostname (an HMAC keyed from `RELAY_VAULT_KEY`). Cloudflare
checks the CNAME, gets a certificate (HTTP validation, DV, TLS 1.2+) and sends the address's
traffic to the zone's fallback origin, the tip host, so the relay sees `Host: tips.mychannel.com`.
The relay reads the TXT record itself over DNS-over-HTTPS (`https://cloudflare-dns.com/dns-query`,
so the server needs outbound HTTPS to it) and answers with that streamer's tip page (`/`), goal
page (`/goal`) and images **only when Cloudflare says active and the TXT value matches**: a CNAME
someone left pointing at us can't be claimed by anyone else. The hourly sweep checks every
address again; one whose TXT record is gone three sweeps running stops (`proof_missing`) until
it's back. One address per subscription, 100 on the relay (Cloudflare for SaaS includes 100
custom hostnames free; only live addresses and unfinished ones younger than 7 days count), 10
changes a day. An address that isn't live 7 days after it was added (or last worked) is removed
from Cloudflare and the relay, and one that isn't live after 24 hours can be claimed by another
tip page.

Once, in the Cloudflare dashboard (clearcraftlabs.com):

1. **SSL/TLS → Custom Hostnames → Enable Cloudflare for SaaS** (free plan; it may ask for a
   payment method on file for hostnames past the free 100; the relay never makes more than 100).
2. **Fallback Origin**: `tip.clearcraftlabs.com`. It must be a **proxied** (orange cloud) DNS
   record in the zone; the tunnel's public hostname for `tip.clearcraftlabs.com` already is
   (a CNAME to the tunnel). Wait until the fallback origin shows **Active**.
3. **The tunnel must hand unknown hostnames to the relay.** Cloudflare sends a custom
   hostname's requests through the tunnel with the streamer's own `Host`, which no public
   hostname rule matches, so the tunnel's **catch-all rule** must be
   `http://ferrocast-relay:8787` (not the default `http_status:404`). The relay is safe to
   receive any host: Ferrocast's own hosts behave as before, a live address gets only its tip
   pages, anything else gets a 404 "not set up yet" page. If the dashboard doesn't let you edit
   the catch-all, set it with the API (`PUT /accounts/{account_id}/cfd_tunnel/{tunnel_id}/configurations`,
   keeping every existing rule and making the last one `{"service": "http://ferrocast-relay:8787"}`).
   Check it after the first address is live: `curl -sI https://tips.<test-domain>/` answers
   200 with the tip page's `content-security-policy`.
4. **API token**: My Profile → API Tokens → Create Token → Custom token. Permissions:
   **Zone → SSL and Certificates → Edit** (this covers custom hostnames; if the list also
   shows **Zone → Custom Hostnames → Edit**, add it too). Zone Resources: **Include → Specific
   zone → clearcraftlabs.com**. No account permissions, no IP filter needed (the relay calls
   from the server). Never use the Global API Key.
5. **Zone ID**: clearcraftlabs.com → Overview → API → Zone ID.
6. **The TXT proof needs `RELAY_VAULT_KEY`** (`vault-key.sh`, above): its values are keyed
   from it, so without it the feature stays off (the relay logs
   "RELAY_VAULT_KEY not set: custom tip addresses are off"). Changing the vault key changes
   every value: live addresses stop after three hourly checks until their streamers update the
   TXT record, so don't rotate it while addresses are live.
7. **An address Cloudflare already has** (left over after a crash, or set up elsewhere) is
   taken over only when its `_ferrocast` TXT record holds that streamer's value: the dashboard
   shows the record with the 409 and a Try again button. Nothing to do on the zone for this.
8. **Test it once** with a domain you own: add the address on Tips, add both records (CNAME
   **DNS only** if that domain is on Cloudflare, and the TXT record exactly as shown), wait for
   **Live**, then `curl -sI https://tips.<test-domain>/` answers 200. Remove the TXT record: the
   address keeps working through two hourly sweeps and stops at the third. `curl -sI -H 'Host:
   nobody.example.com' https://tip.clearcraftlabs.com/health` answers 404 (unknown hosts fail
   closed); `curl -s http://127.0.0.1:8787/health` on the server still answers 200.

Then type both on the server:

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --domains'
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --status'
```

With both set (and the vault key), `features.tip_domain` is true and Tips shows "Your own
address". A streamer on Cloudflare DNS must set their CNAME to **DNS only** (grey cloud). The
relay checks an address (Cloudflare and the TXT record) at most once a minute when the streamer
looks, and every address hourly; a lapsed subscription's address keeps working 7 days, then the
hourly sweep deletes it from Cloudflare and the relay. A wrong zone id, a refused token or
Cloudflare for SaaS switched off is logged once (`Cloudflare for SaaS isn't usable: …`) and
streamers see 503 `domains_unavailable`, never Cloudflare's words.
`deploy-relay.sh` keeps both lines when it rewrites `relay.env`.

## Checking the Twitch cloud bot

When the cloud bot doesn't answer, look from the server (read only, no tokens printed):

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/bot-check.sh'               # every bound channel
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/bot-check.sh <install-id>'  # one install
```

It shows the conduit (saved vs Twitch's), shard 0's status and callback (`enabled` at this
relay's `/v1/bot/twitch/eventsub` or chat never arrives), the EventSub subscriptions per
channel (chat, online, offline; MISSING ones named), and each channel's row (accounts,
ready/blocked, last error, the switch, Pro, config version, the desktop bot switch and the
commands with the switched-off ones marked). It's a separate process: who's on duty (the
in-memory lease) isn't visible; the Bot page shows that, and `docker logs` shows the relay's
"didn't answer a chat line" reasons (once per reason, channel and hour).

## Test Pro for your own install (no Razorpay)

To try Pro end to end on your own desktop, grant it to that install from the server:

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-grant.sh <install-id> 30'      # grant or extend (1-90 days, capped at 90 from now)
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-grant.sh --revoke <install-id>' # end it now
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-grant.sh --list'
```

It runs `ferrocast-relay pro-grant …` in the relay container on the live database (no HTTP
route; safe while the relay serves). The grant is a `comp_…` subscription with plan `comp`:
`/v1/pro/status` shows it as Pro, and nothing about it is ever sent to Razorpay (daily
re-check, refresh, cancel, manage and restore skip or refuse it with 409 `test_grant`). It's
refused for an install that still has a real subscription granting or renewing Pro. Revoking
keeps the row, ended now, so the usual 30-day lapse rules tidy its data. The install id is the
part before the dot of `relay-install` in the app's `secrets.json`
(macOS: `~/Library/Application Support/Ferrocast/secrets.json`; never share the part after the dot).

## The Control Center (ops.transformx.site)

The owner's admin panel shows Ferrocast's numbers (installs, Pro subscriptions and MRR, Pro
feature use, relay health, the last nightly backup) from `GET /admin/v1/stats`: aggregate
counts only, never an install id, email, handle or tip amount (docs/API.md §9.9). It answers
only inside the server, to a container on the relay's Docker network
(`ferrocast-relay_ferrocast`, which the Control Center API joins in TransformX's
`docker-compose.prod.yml`), with a bearer token:

```bash
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/control-center-key.sh'           # set it up (safe to run again)
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/control-center-key.sh --status'  # set, missing or different
ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/control-center-key.sh --rotate'  # a new token on both sides
```

It makes 64 random hex characters on the server, writes them as `RELAY_ADMIN_TOKEN` in
`relay.env` and as `FERROCAST_ADMIN_TOKEN` in `/opt/transformx/.env.prod` (both 0600, backed up
first), restarts the relay and the Control Center API only if their file changed, then checks
from inside the Control Center container that the relay answers 200. `deploy-relay.sh` keeps
`RELAY_ADMIN_TOKEN`. Without it the endpoint answers 503 `admin_off` and the panel says so.
The nightly backup writes its last good time into the relay's volume (`/data/backup-last-ok`),
which the panel shows.
