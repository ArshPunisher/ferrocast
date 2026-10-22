# Ferrocast accounts (relay, 1.3)

The contract between the relay (`crates/relay/src/accounts/`) and the app, the web dashboard
and the Control Center. The relay side is built to this; screens are built against it.

Owner decisions this follows:

- The **app stays free and works without signing in**. Nothing that works today needs an account
  (app 1.2.0 keeps working exactly as before).
- The **web dashboard** (`https://ferrocast.clearcraftlabs.com/app/`, on the website) needs a sign-in. Accounts are free.
- **Pro belongs to an account** once one is signed in (install-only Pro keeps working).
- Sign-in is **Google** or an **email link**. No passwords.

## 0. Google OAuth client (for the owner)

| Setting | Value |
|---|---|
| Application type | Web application |
| Authorised redirect URI | `https://api.clearcraftlabs.com/login/google/callback` (the old `https://remote.clearcraftlabs.com/login/google/callback` stays listed until that host is deleted) |
| Authorised JavaScript origins | none needed |
| Scopes | `openid email profile` |
| Relay settings | `RELAY_GOOGLE_CLIENT_ID`, `RELAY_GOOGLE_CLIENT_SECRET` (`deploy/pro-keys.sh --google`) |

Both the web flow and the app flow sign in **in the browser**, on the website's sign-in page,
and share this one https callback on the API host. The app never registers a custom scheme at Google: the relay hands the
app a one-time code through a loopback redirect (§3), after Google is done.

Without `RELAY_GOOGLE_CLIENT_ID`/`_SECRET`, Google sign-in answers 503 `google_off` and the login
page hides the Google button; email sign-in keeps working.

## 1. Hosts, tokens, cookies

- **API host** `https://ferrocast-relay.clearcraftlabs.com` (`RELAY_PUBLIC_URL`): the app's calls.
- **Web host** `https://api.clearcraftlabs.com` (`RELAY_REMOTE_URL`): the sign-in API
  (`/v1/login*`), the Google callback, `/v1/account`, `/v1/cloud` and the web dashboard's `/r/…`
  API, with a cookie session. Accounts are off (404) without `RELAY_REMOTE_URL`.
- **Website** `https://ferrocast.clearcraftlabs.com` (`RELAY_WEB_URL`): serves the sign-in pages
  (`/login/`, `/login/link/`, `/login/done/`) and the dashboard (`/app/`) from Cloudflare Pages.
  It calls the web host with `credentials: 'include'`; CORS allows this one origin only, and the
  cookies count as same-site because both hosts are under clearcraftlabs.com.
- **App session token**: `fca_<session id>.<secret>`, sent as `Authorization: Bearer fca_…` on
  `/v1/account*`. Kept by the engine like the install secret (never shown, never in a URL).
- **Web session cookie**: `__Host-fc_session=<session id>.<secret>`; `HttpOnly; Secure;
  SameSite=Lax; Path=/`, `Max-Age` up to the session's expiry. Only accepted on the web host.
- **CSRF** (web only): every state-changing request with the cookie (POST, PUT, PATCH, DELETE)
  must send `X-CSRF-Token: <csrf_token>` (from `GET /v1/account`), and its `Origin`, if sent,
  must be the web origin. Missing or wrong → 403 `csrf`. Bearer (app) requests need no CSRF token.
- **Login browser cookie**: `__Host-fc_login` (HttpOnly, Secure, SameSite=Lax, 30 minutes) ties
  an email link, an email code and a Google round trip to the browser that started them.
- Every secret (session, email link, email code, OAuth state, app code) is random (≥ 128 bits,
  codes 40 bits with 5 tries) and stored only as a SHA-256 hash. Comparisons are constant time.
- Session lifetimes: **web** 30 days from the last use, at most 90 days in all; **app** 180 days
  from the last use. Use slides the expiry at most once a minute. Revoked = gone at once.

## 2. Errors

Always `{"error": {"code", "message", "hint"}}` with the right status; never a bare 500.

| Status | Code | When |
|---|---|---|
| 400 | `bad_json`, `invalid` | body isn't a JSON object / a field is wrong (`message` says which) |
| 401 | `signed_out` | no session, unknown, expired or revoked session (app: sign in again) |
| 401 | `unauthorized` | install `Authorization` missing/wrong (app/start, app/token) |
| 403 | `account_disabled` | the account was disabled by the owner (hint says to write to support) |
| 403 | `csrf` | cookie request without the right `X-CSRF-Token`, or from another origin |
| 403 | `login_required` | web dashboard `/r/…` call without a signed-in account (remote login mode `required`) |
| 403 | `wrong_account` | this phone/browser was paired by another account |
| 404 | `not_found` | no such session / PC / flow (also: accounts off on this relay) |
| 409 | `already_pro`, … | see each route |
| 410 | `flow_expired` | the app sign-in (10 minutes) or code (2 minutes) expired or was used |
| 415 | `unsupported_type` | body without `Content-Type: application/json` |
| 422 | `invalid_email`, `invalid_redirect`, `invalid_challenge` | see §3, §4 |
| 429 | `rate_limited` | too many sign-in emails / tries; `Retry-After` header |
| 502 | `google_unavailable` | Google didn't answer or refused the code |
| 503 | `google_off` | Google sign-in isn't set up on this relay (email still works) |
| 503 | `mail_off` | email sign-in can't send (no `BREVO_API_KEY`) |
| 503 | `storage` | database busy; try again |

Login **pages** (§4) show the same problems as a page with a "Try again" link, with the same status.

## 3. The app's sign-in (engine)

Loopback redirect with PKCE (RFC 8252), on Mac, Windows and Linux alike, no deep link:

1. The engine makes `code_verifier` (43–128 chars, `[A-Za-z0-9-._~]`) and
   `code_challenge = BASE64URL(SHA256(verifier))`, plus a random `state`.
2. `POST /v1/account/app/start` (install `Authorization: Bearer <install>.<secret>`)

   ```json
   { "redirect_uri": "http://127.0.0.1:7878/auth/callback", "code_challenge": "…",
     "code_challenge_method": "S256", "state": "…", "label": "Studio PC" }
   ```

   `redirect_uri` must be `http://127.0.0.1:<port>/…` or `http://localhost:<port>/…` (an
   explicit port; no `[::1]`), no userinfo, no fragment, ≤ 512 chars; else 422
   `invalid_redirect`. `state` 8–128 URL-safe chars. `label` optional, ≤ 40 chars (default
   "Ferrocast app"). 10 starts an hour per install.
   → 201 `{ "flow_id": "af_…", "url": "https://ferrocast.clearcraftlabs.com/login/?flow=af_…", "expires_at": ms }`
3. The engine opens `url` in the default browser. The person signs in (§4).
4. The relay redirects the browser (303) to `redirect_uri?code=<one-time code>&state=<state>`
   (or `?error=access_denied&state=…` if they cancel at Google). The engine's local server
   checks `state` and shows "You're signed in, you can close this tab".
5. `POST /v1/account/app/token` (same install `Authorization`)
   `{ "code": "…", "code_verifier": "…" }` → 201

   ```json
   { "session_token": "fca_se_….…", "session": { "id": "se_…", "expires_at": ms },
     "account": { …same as GET /v1/account… } }
   ```

   The code works once, for 2 minutes, only for the install that started the flow and only with
   the verifier (else 410 `flow_expired` / 422 `invalid_challenge`, same answer for every miss).

Signing in links the install to the account (§6). Signing out (`POST /v1/account/logout`) or a
revoked session unlinks it. The engine should call `GET /v1/account` at start and every few hours
(keeps the session alive and Pro current).

## 4. Login pages

With `RELAY_WEB_URL` set (production since 22 Oct) the pages are the website's
(`{RELAY_WEB_URL}/login/`, `/login/link/#t=…`, `/login/done/`) and drive the same steps through
the JSON API on the web host: `GET /v1/login` → `{csrf, google, app, signed_in, next}`, then
`POST /v1/login/email`, `/code`, `/link`, `/google` with `X-CSRF-Token`. The email link points at
`{RELAY_WEB_URL}/login/link/`. Without it the relay serves the pages itself (no JavaScript), as
below; the rules are the same either way.


- `GET /login?next=/app/` (web) or `GET /login?flow=af_…` (app): the page with "Continue with
  Google" (hidden when Google is off) and "Email me a sign-in link". `next` must be a path on the
  web host starting with `/app` (anything else → `/app/`).
- Email: the person types an address → one email with a **link** and an **8-character code**
  (15 minutes, single use). The page then asks for the code. Opening the link or typing the code
  only works **in the browser that asked** (the `__Host-fc_login` cookie), so a link someone else
  requested for your address can't sign them in. Opening the link shows a "Sign in" button
  (a GET never uses the link up, so mail scanners can't spend it). 5 wrong codes end that link.
- Limits: 3 emails per address per 15 minutes and 10 per day; 20 per IP address per hour.
  The answer is the same whether or not an account exists.
- Google: authorization code + PKCE (S256), `state` and `nonce`; the `id_token` is verified with
  Google's JWKS (RS256, `kid`, `iss`, `aud` = client id, `exp`, `iat`, `nonce`), and
  `email_verified` must be true.
- Done: web flow → sets `__Host-fc_session` and goes to `next`; app flow → the loopback redirect
  (§3). Disabled accounts get a 403 page.
- `POST /logout` (form, CSRF token field) or `POST /v1/account/logout` ends a web session.

Accounts: one per verified email. Google and email sign-in with the same address reach the same
account (Google's identity is added to it).

## 5. Account API (app: Bearer `fca_…`; web: the cookie + CSRF)

Paths are the same on both hosts (`/v1/account…`); cookies are only read on the web host.

### `GET /v1/account`

```json
{
  "account": { "id": "ac_…", "email": "a@b.c", "name": "Arsh", "avatar_url": "https://…" | null,
               "created_at": ms, "providers": ["google", "email"] },
  "plan": "free" | "pro",
  "pro": { "active": true, "source": "subscription" | "grant" | null,
           "plan": "monthly" | "yearly" | "comp" | null, "status": "active" | null,
           "subscription_id": "sub_…" | null, "current_end": ms | null, "access_until": ms | null,
           "renews": true, "cancel_at_period_end": false },
  "session": { "id": "se_…", "kind": "app" | "web", "label": "…", "expires_at": ms },
  "pcs": 1,
  "csrf_token": "…"   // web sessions only
}
```

`pro` is the account's best subscription or grant (the one granting Pro now, else the latest).
An install's own Pro (bought before signing in, not matched by email) isn't shown here; the app
keeps showing it from `/v1/pro/status` as today.

### `POST /v1/account/logout` `{ "everywhere": false }`

204. Ends this session (and with `everywhere: true`, every session of the account, app and web).
Web: also clears the cookie.

### `GET /v1/account/sessions`

`{ "sessions": [ { "id", "kind", "label", "created_at", "last_used_at", "expires_at", "current" } ] }`
(active ones, newest use first; at most 100).

### `DELETE /v1/account/sessions/{id}`

204. Revokes one of this account's sessions (404 for anyone else's). An app session's PC loses
the account's Pro at once.

## 6. Pro and accounts

- A subscription or grant can **belong to one account** (`account_subs`).
- **Checkout while signed in** attaches the new subscription to the account: the app's
  `POST /v1/pro/checkout` (install auth, as today) attaches it when the install is signed in.
- **Web checkout** `POST /r/v1/pro/checkout {plan, code?}` (cookie + CSRF): the account's verified
  email is used, the subscription belongs to the account and isn't on any PC yet. Same answers
  and errors as `/v1/pro/checkout`.
- **Existing subscriptions**: at every sign-in, Razorpay subscriptions whose checkout email hash
  (HMAC with `PRO_EMAIL_KEY`) equals the verified email's hash, and that don't belong to an account
  yet, are attached to it. Comp grants (no email) are never matched.
- **Pro for an install** = its own subscription/grant (as in 1.2.0) **or** its signed-in account's.
  The install's own one wins while it grants Pro. Account Pro reaches the account's 3 most
  recently signed-in PCs (`MAX_INSTALLS`); Pro data (quotas, history, backups…) stays keyed by
  the subscription, so every PC of the account shares it.
- Never signing in changes nothing.

## 7. Web dashboard (`/r/…` on the web host)

Remote login mode `RELAY_REMOTE_LOGIN` = `required` (default) | `optional` (rollout escape hatch).

- `/r/v1/pair` needs a signed-in account (`required`: else 403 `login_required`). The new
  device is bound to the account.
- Every device call (`/r/v1/me`, `/r/v1/ticket`, `/r/v1/forget`, `/r/api/v1/*`) also needs the
  cookie (`required`). A device paired before 1.3 is bound to the first account that uses it;
  a device bound to another account → 403 `wrong_account`. Device tokens keep working as before.
- **Free accounts** get the dashboard: a PC signed in to an account may open the tunnel and be
  paired without Pro. Offline features (cloud bot, history) stay Pro.
- `GET /r/v1/pcs` → `{ "pcs": [ { "id": "pc_…", "label", "pc_online": bool, "pro": bool,
  "last_seen_at": ms } ] }`: the account's signed-in PCs.
- `POST /r/v1/pcs/{id}/connect` (CSRF) → 201 `{ token, device_id, role: "owner", name, expires_at }`
  (same as `/r/v1/pair`): this browser becomes an owner device of that PC, no code needed. The
  relay also queues `remote_wanted` on the PC's link so the engine opens its tunnel
  (engine: on that item, set "Dashboard anywhere" wanted, even without Pro when signed in).

## 8. What the relay keeps (privacy)

Per account: email address (lower-cased, verified), name and picture URL from Google (if used),
Google's account id (`sub`), created / last-seen times, disabled flag. Per session: a hash of the
token, kind, a label (the app's PC name or "Chrome on macOS"-style from the User-Agent), which
install (app), times. Sign-in links/codes/OAuth states: hashes only, deleted within a day.
No passwords, no IP addresses stored (IPs are only counted in memory for rate limits).
Deleting an account (owner, on request) removes all of the above; subscriptions stay for billing.

## 9. Admin API

See `docs/ADMIN-API.md` § Accounts.
