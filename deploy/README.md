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
| `relay-ci-setup.sh` | your Mac, once | Lets GitHub deploy (below), and installs `razorpay-keys.sh` |
| `razorpay-keys.sh` | server (`/opt/ferrocast-relay/razorpay-keys.sh`) | Ferrocast Pro's Razorpay keys, plans, webhook secret, and the `PRO_SALES_OPEN` switch (below) |
| `tests/razorpay-keys.test.sh` | CI, your Mac | Tests `razorpay-keys.sh` against a fake Razorpay |

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
