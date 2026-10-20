# Ferrocast relay: admin API for the Control Center

The relay (`crates/relay`) serves the Control Center's Ferrocast tabs: Users, Subscriptions,
Payments, Give Pro, Codes, Pricing, Sales. Shapes mirror Hookline's `/internal/admin` and the
Control Center's existing page types (`control-center-web/src/lib/api.ts`) wherever Ferrocast
has the same thing. Code: `crates/relay/src/admin.rs` (guard, stats) and
`crates/relay/src/admin_pro.rs` (everything below).

## Access (every route)

- Base: `http://ferrocast-relay:8787` from a container on the relay's Docker network only.
- `Authorization: Bearer <RELAY_ADMIN_TOKEN>` (same token as `GET /admin/v1/stats`).
- Every **write** (POST/PUT) also needs `X-Admin-Email: owner@example.com` (the admin making
  the change). It is lower-cased and kept in the relay's `admin_audit` table with the change.
- At most 60 admin requests a minute, relay-wide (reads and writes together).
- Responses carry `Cache-Control: no-store`.
- Times in responses are UTC RFC 3339 strings (`"2026-10-20T10:00:00Z"`) or `null`.
  Times in requests: RFC 3339 (`"2026-10-20T10:00:00.000Z"`, any offset), Unix ms, or for
  `until`/`valid_until` a date `"2026-10-31"` (valid through the end of that UTC day).
- Money is always **paise** (integer), currency `INR`. Plan `pro`; periods `monthly`, `yearly`.
  Sale/code keys are `pro_monthly`, `pro_yearly`.

### Errors

Always `{"error": {"code": "...", "message": "...", "hint": "..." | null}}`, never a bare 500.
(Unlike Hookline there is no `detail` field: read `error.message`.)

| Status | Codes | When |
|---|---|---|
| 400 | `admin_email_required`, `bad_json`, `bad_query` | missing/invalid `X-Admin-Email` on a write; body not a JSON object; bad query value |
| 401 | `unauthorized` | token missing or wrong (`WWW-Authenticate: Bearer`) |
| 403 | `internal_only` | came through Cloudflare / a proxy, or not from a private address |
| 404 | `not_found`, `unknown_install`, `unknown_price` | no such subscriber, grant, code, sale, install or price |
| 409 | `has_subscription`, `already_ended`, `code_taken`, `price_changed`, `sale_overlap`, `sale_over` | conflicts (see each route) |
| 415 | `unsupported_type` | body sent with a non-JSON `Content-Type` |
| 422 | `invalid_days`, `invalid_code`, `invalid_kind`, `invalid_percent`, `invalid_amount`, `invalid_dates`, `invalid_name`, `invalid_theme`, `unsupported_currency`, `unsupported`, `nothing_on_sale` | a field is wrong; `message` says which and the allowed range |
| 429 | `rate_limited` | over 60 a minute; `Retry-After` header |
| 502 | `razorpay_unavailable` | Razorpay refused or didn't answer while making a plan (nothing was saved) |
| 503 | `admin_off`, `storage` | `RELAY_ADMIN_TOKEN` unset/too short; database busy |

## Privacy

The Pro tabs below are about subscriptions: the relay keeps only a keyed hash of the checkout
email, so there `email` is always `null` (with `email_on_file: true|false`). Install ids are
shown masked (`"AbC1…"`); search accepts a full install id. A "user" there is one Pro
subscription (paid `sub_…` or free grant `comp_…`).

Since 1.3 people may also have an **account** (free, Google or email sign-in, docs/ACCOUNTS.md).
Accounts have a verified email, shown in the Accounts tab (§ Accounts at the end).

---

## Reads

### `GET /admin/v1/users?q=&source=&page=1&per_page=50`

Pro subscribers (unpaid checkouts left out). `q`: part of a subscription id, or a full install
id. `source`: `paid` | `grant` (optional). `per_page` 1..100.

```json
{
  "total": 2, "page": 1, "per_page": 50,
  "items": [
    {
      "id": "sub_Pq1x2y3z4a5b6c", "email": null, "email_on_file": true, "display_name": null,
      "auth_provider": "razorpay", "source": "paid", "plan": "pro", "pro": true,
      "period": "monthly", "status": "active", "amount": 13900, "currency": "INR",
      "current_end": "2026-11-20T10:00:00Z", "access_until": "2026-11-23T10:00:00Z",
      "cancel_at_period_end": false, "refunded_at": null,
      "created_at": "2026-10-20T10:00:00Z", "jobs": 0, "accounts": 1, "installs": 1
    },
    {
      "id": "comp_Xy12ab34cd56", "email": null, "email_on_file": false, "display_name": null,
      "auth_provider": "grant", "source": "grant", "plan": "pro", "pro": true,
      "period": null, "status": "active", "amount": null, "currency": "INR",
      "current_end": "2026-11-03T10:00:00Z", "access_until": "2026-11-03T10:00:00Z",
      "cancel_at_period_end": true, "refunded_at": null,
      "created_at": "2026-10-20T10:00:00Z", "jobs": 0, "accounts": 1, "installs": 1
    }
  ]
}
```

`plan` is `"pro"` when it gives Pro now, else `"free"`. `amount` is what the subscription is
charged each period (its checkout price; `null` for a grant).

### `GET /admin/v1/users/{id}`

One subscriber (`sub_…` or `comp_…`). 404 `not_found` if unknown.

```json
{
  "user": { "id": "sub_Pq1x2y3z4a5b6c", "email": null, "email_on_file": true, "display_name": null,
            "auth_provider": "razorpay", "created_at": "2026-10-20T10:00:00Z" },
  "subscription": { "id": "sub_Pq1x2y3z4a5b6c", "source": "paid", "pro": true, "period": "monthly",
                    "status": "active", "amount": 13900, "list_amount": 19900, "currency": "INR",
                    "code": "DIWALI30", "sale_id": null, "current_end": "...", "access_until": "...",
                    "cancel_at_period_end": false, "refunded_at": null, "created_at": "..." },
  "subscriptions": [ /* SubscriptionRow (below); [] for a grant */ ],
  "entitlements": [ /* Grant (below); [] for a paid subscription */ ],
  "payments": [ { "payment_id": "pay_...", "amount": 13900, "currency": "INR", "paid_at": "...",
                  "refunded": 0, "pending_refunds": 0 } ],
  "refunds": [ { "refund_id": "rfnd_...", "payment_id": "pay_...", "amount": 13900,
                 "status": "processed", "refunded_at": "..." } ],
  "installs": [ { "install": "AbC1…", "current": true, "linked_at": "...", "last_seen": "..." } ],
  "redemptions": [ { "code": "DIWALI30", "kind": "discount", "redeemed_at": "..." } ],
  "team": null
}
```

### `GET /admin/v1/subscriptions?status=`

Paid subscriptions only (grants are under `/grants`), newest first, at most 500. Without
`status`, unpaid checkouts (`created`) are left out. `status` must be one of Razorpay's:
`created authenticated active pending halted paused cancelled completed expired` (else 400).

```json
[
  { "gateway_subscription_id": "sub_Pq1x2y3z4a5b6c", "plan": "pro", "period": "monthly",
    "status": "active", "amount": 13900, "list_amount": 19900, "currency": "INR",
    "current_end": "2026-11-20T10:00:00Z", "access_until": "2026-11-23T10:00:00Z",
    "cancel_at_period_end": false, "refunded_at": null, "code": "DIWALI30", "sale_id": null,
    "installs": 1, "created_at": "2026-10-20T10:00:00Z" }
]
```

### `GET /admin/v1/payments?days=30&refunded_only=false`

Charges in the last `days` (1..3650) with their refunds. Same shape as TransformX's payments
page (`Payments`). `refunded` = processed refunds (paise); `pending_refunds` = count.

```json
{
  "payments": [
    { "payment_id": "pay_...", "gateway": "razorpay", "email": null,
      "subscription_id": "sub_...", "plan": "pro", "period": "monthly", "amount": 19900,
      "currency": "INR", "paid_at": "2026-10-20T10:00:00Z", "refunded": 0, "pending_refunds": 0,
      "last_refund_at": null, "plan_ended_at": null }
  ],
  "totals": [ { "currency": "INR", "payments": 1, "gross": 19900, "refunded": 0, "net": 19900 } ],
  "unmatched_refunds": []
}
```

Refunds are copied from Razorpay whenever the relay checks a payment's refunds (refund
webhooks and the daily re-check), so refunds made before this release don't show.

### `GET /admin/v1/grants`

Free Pro grants (`comp_…`), newest first.

```json
[
  { "id": "comp_Xy12ab34cd56", "plan": "pro", "source": "admin", "code": null,
    "created_by": "owner@example.com", "note": "Giveaway winner", "install": "AbC1…",
    "active": true, "starts_at": "2026-10-20T10:00:00Z", "ends_at": "2026-11-03T10:00:00Z",
    "revoked_at": null, "created_at": "2026-10-20T10:00:00Z",
    "history": [ { "kind": "grant", "source": "admin", "code": null, "actor": "owner@example.com",
                   "note": "Giveaway winner", "days": 14, "until": "2026-11-03T10:00:00Z",
                   "at": "2026-10-20T10:00:00Z" } ] }
]
```

`source`: `admin` (Control Center), `code` (redeemed in the app; `code` says which), `cli`
(`deploy/pro-grant.sh`), or `unknown` for grants made before this release.

### `GET /admin/v1/codes`

`Code[]` (Control Center type), newest first.

```json
[
  { "id": "FRIENDS-7K2M", "code": "FRIENDS-7K2M", "kind": "grant", "plan": "pro",
    "duration_days": 14, "percent_off": null, "applies_period": null, "max_redemptions": 50,
    "valid_until": "2026-12-31T23:59:59Z", "allowed_emails": null, "active": true,
    "note": "Discord friends", "created_by": "owner@example.com",
    "created_at": "2026-10-20T10:00:00Z", "used": 3 },
  { "id": "DIWALI30", "code": "DIWALI30", "kind": "discount", "plan": "pro",
    "duration_days": null, "percent_off": 30, "applies_period": "monthly",
    "max_redemptions": null, "valid_until": null, "allowed_emails": null, "active": true,
    "note": null, "created_by": "owner@example.com", "created_at": "...", "used": 1 }
]
```

`used`: grant codes = redemptions; discount codes = checkouts paid with it (an unpaid checkout
holds a use for two days, then gives it back).

### `GET /admin/v1/pricing`

```json
{
  "prices": [
    { "plan": "pro", "period": "monthly", "currency": "INR", "amount": 19900, "default": true,
      "gateway_ready": true, "changed_by": null, "changed_at": null, "sale_amount": 13900 },
    { "plan": "pro", "period": "yearly", "currency": "INR", "amount": 199900, "default": true,
      "gateway_ready": true, "changed_by": null, "changed_at": null, "sale_amount": null }
  ],
  "history": [ { "id": "price_...", "plan": "pro", "period": "monthly", "currency": "INR",
                 "amount": 24900, "active": false, "created_by": "owner@example.com",
                 "created_at": "...", "retired_at": "..." } ],
  "gateways": { "INR": true },
  "sales_open": false,
  "sales": [ /* Sale (below) */ ],
  "themes": ["violet", "sunset", "emerald", "midnight", "festive"],
  "now": "2026-10-20T10:00:00Z"
}
```

`amount` is the list price; `default: true` means never changed (₹199 / ₹1,999).
`gateway_ready`: a Razorpay plan exists for that amount (otherwise one is made at the next
checkout). `sale_amount`: today's price under the live sale, or `null`.

### `GET /admin/v1/audit?limit=100`

Every admin change, newest first (`limit` 1..500).

```json
[ { "id": 7, "at": "2026-10-20T10:00:00Z", "actor": "owner@example.com", "action": "price.set",
    "target": "pro_monthly", "detail": { "amount": 24900, "was": 19900 } } ]
```

Actions: `grant.create`, `grant.revoke`, `code.create`, `code.disable`, `price.set`,
`sale.create`, `sale.update`, `sale.stop`.

---

## Writes (all need `X-Admin-Email`)

### Give Pro: `POST /admin/v1/grants`

```json
{ "install_id": "AbC1dEf2gHi3jKl4", "days": 14, "note": "Giveaway winner" }
```

`days` 1..90 (a grant never reaches more than 90 days past now; granting again extends).
`plan`, if sent, must be `"pro"`. Hookline's `email` / `forever` are refused with 422
`unsupported` (no accounts, no forever grants). The install id is the part before the dot of
the app's relay-install secret.

→ 201 `{ "id": "comp_Xy12ab34cd56", "plan": "pro", "starts_at": "...", "ends_at": "...", "extended": false }`

Errors: 404 `unknown_install`; 409 `has_subscription` (that install pays for Pro; nothing
changed); 422 `invalid_days`.

### Revoke: `POST /admin/v1/grants/{id}/revoke` (alias `POST /admin/v1/entitlements/{id}/revoke`)

Ends a `comp_…` grant now. → 200 `{ "id": "comp_...", "revoked_at": "..." }`.
404 `not_found` (unknown, or a paid `sub_…`); 409 `already_ended`.

### Codes: `POST /admin/v1/codes`

```json
{ "kind": "grant", "days": 14, "code": "FRIENDS", "prefix": "", "max_uses": 50,
  "until": "2026-12-31", "note": "Discord friends" }
{ "kind": "discount", "percent_off": 30, "period": "monthly", "code": null, "prefix": "DIWALI-",
  "max_uses": 100, "until": null, "note": null }
```

- `kind`: `grant` (free Pro for `days` 1..90, redeemed in the app) or `discount`
  (`percent_off` 1..90 off the list price at checkout; `period` `monthly` | `yearly` | null
  for both). Hookline's `plan` (must be `"pro"`) and `valid_until` (same as `until`) are accepted.
- `code`: 4..32 letters, digits, dashes, stored upper-case. Empty → `prefix` + 8 random characters.
- `max_uses` ≥ 1 or null (unlimited). `until`: last valid day, or null.

→ 201 the `Code` object (as in GET). 409 `code_taken`; 422 for a bad field.

How a discount works: checkout makes the subscription on a Razorpay plan for the discounted
amount (made through Razorpay's Plans API, free, once per amount). **The buyer keeps that
price on every renewal** for the life of the subscription. If a live sale is cheaper than the
code, the sale price is used and the code isn't counted. `max_uses` for discount codes is
checked at checkout and can be exceeded by one under exactly simultaneous checkouts.

### Disable a code: `POST /admin/v1/codes/{code}/disable`

→ 200 `{ "code": "FRIENDS", "active": false }`. 404 `not_found`. Already-disabled is fine (200).

### Price: `PUT /admin/v1/prices/pro/{monthly|yearly}/INR`

```json
{ "amount": 24900, "expected_amount": 19900 }
```

`amount` 100..1,499,999 paise (under ₹15,000: RBI's no-OTP e-mandate limit).
`expected_amount` (optional) is the price the editor saw: 409 `price_changed` if it differs.
The relay first makes the Razorpay plan for the new amount (502 if Razorpay fails; nothing
saved). **Only new checkouts pay the new price; existing subscribers keep their plan.**
Same amount as now → 200, nothing changes. Other plan/period/currency → 404 `unknown_price`.

→ 200 `{ "id": "price_...", "plan": "pro", "period": "monthly", "currency": "INR", "amount": 24900,
"active": true, "gateway_ready": true, "created_by": "owner@example.com", "created_at": "...", "retired_at": null }`

### Sales: `POST /admin/v1/sales`, `PUT /admin/v1/sales/{id}`, `POST /admin/v1/sales/{id}/stop`

Body (the Control Center's `SaleBody`):

```json
{
  "name": "Diwali 2026", "headline": "30% off Pro for Diwali", "message": null,
  "discounts": { "pro_monthly": 30, "pro_yearly": 25 },
  "prices": { "pro_yearly": 149900 },
  "currencies": ["INR"],
  "starts_at": "2026-10-28T00:00:00.000Z", "ends_at": "2026-11-05T00:00:00.000Z",
  "show_countdown": true, "show_banner": false, "theme": "festive"
}
```

- `discounts`: percent 1..90 per key. Optional `prices`: a set price (paise) per key instead
  of a percent (wins over `discounts` for that key). Hookline's `percent_off` + `periods`
  also work. At least one plan must be on sale (422 `nothing_on_sale`).
- Percent prices round down to whole rupees (₹199 − 30% = ₹139).
- `starts_at` null = now; `ends_at` null = until stopped; `ends_at` must be after `starts_at`
  and in the future. `currencies` may only be `["INR"]`; `plans` (if sent) only `["pro"]`.
- Two sales can't overlap in time: 409 `sale_overlap`.
- Updating or stopping an ended/stopped sale: 409 `sale_over`.
- Razorpay plans for the sale prices are made on save (502 if Razorpay fails; nothing saved).
- **Buyers during the sale keep the sale price on renewals** (their subscription is on the
  sale's plan); when it ends, new checkouts go back to the list price.

→ 201 (create) / 200 (update, stop), a `Sale`:

```json
{
  "id": "sale_...", "name": "Diwali 2026", "headline": "30% off Pro for Diwali", "message": null,
  "percent_off": 30, "discounts": { "pro_monthly": 30, "pro_yearly": 25 },
  "prices": { "pro_yearly": 149900 }, "amounts": { "pro_monthly": 13900, "pro_yearly": 149900 },
  "plans": ["pro"], "periods": ["monthly", "yearly"], "currencies": ["INR"],
  "starts_at": "...", "ends_at": "...", "stopped_at": null,
  "show_countdown": true, "show_banner": false, "theme": "festive",
  "state": "scheduled", "created_by": "owner@example.com", "updated_by": "owner@example.com",
  "created_at": "...", "updated_at": "..."
}
```

`state`: `scheduled` | `live` | `ended` | `stopped`. `amounts`: today's sale price per key at
the current list prices.

---

## Public and app routes (not admin; for reference)

### `GET /v1/pro/prices` (no auth)

For the website. `Cache-Control: public, max-age=60`; CORS for
`https://ferrocast.clearcraftlabs.com` (and `localhost:4321` for the site's dev server);
120 a minute per address.

```json
{
  "prices": { "currency": "INR", "monthly": 13900, "yearly": 199900 },
  "list_prices": { "currency": "INR", "monthly": 19900, "yearly": 199900 },
  "sale": { "id": "sale_...", "name": "Diwali 2026", "headline": "30% off Pro for Diwali",
            "message": null, "ends_at": 1793750400000, "show_countdown": true,
            "show_banner": false, "theme": "festive", "percent_off": 30,
            "was": { "monthly": 19900 } },
  "sales_open": true
}
```

`prices` = what a buyer pays now. `sale` is `null` with no live sale. `was` lists only the
plans the sale lowers. `ends_at` is Unix ms (or null).

### `GET /v1/pro/status` (install auth)

Unchanged, plus: `prices` are now the effective prices (sale applied), and new
`list_prices` and `sale` (same shapes as above).

### `POST /v1/pro/redeem` `{ "code": "FRIENDS" }` (install auth)

10 an hour per install, 30 per address. A **grant** code gives (or extends) free Pro and
answers the Pro status plus
`"redeemed": { "code": "FRIENDS", "kind": "grant", "days": 14, "until": 1793750400000, "extended": false }`.
A **discount** code changes nothing and answers what it does, for the app to pass to checkout:

```json
{ "kind": "discount", "code": "DIWALI30", "percent_off": 30, "applies_period": "monthly",
  "valid_until": null, "currency": "INR",
  "prices": { "monthly": { "list": 19900, "amount": 13900, "code_applies": true },
              "yearly": { "list": 199900, "amount": 199900, "code_applies": false } } }
```

Errors: 404 `code_not_found` (unknown or disabled), 410 `code_expired`, 409 `code_used_up`,
409 `already_redeemed` (this install used it), 409 `already_pro` (this install pays for Pro),
422 `invalid_code`, 429 `rate_limited`.

### `POST /v1/pro/checkout` `{ "plan", "email", "code"? }`

Optional `code` (a discount code). Answers as before plus `list_amount`, `sale`, `code`,
`code_applied`. `amount` is what the buyer pays each period. Code errors as for redeem, plus
422 `grant_code` (a free-Pro code: redeem it instead) and 422 `code_not_for_plan`.

## What the owner runs

Nothing new to configure: no new env vars. The relay's database migrates itself to v14 on
start (a copy is taken first). Deploy as usual: after the "Build relay" workflow passes,
`gh workflow run relay-deploy.yml` in the ferrocast repo.

## Accounts (1.3)

Code: `crates/relay/src/accounts/admin.rs`. Same access rules as everything above (Docker
network only, bearer token, 60 a minute, `X-Admin-Email` on writes, every write in
`admin_audit` as `account.grant`, `account.revoke`, `account.sign_out`, `account.disable`,
`account.enable`). An account's id looks like `ac_0123…` (35 characters).

### `GET /admin/v1/accounts?q=&plan=&status=&page=1&per_page=50`

`q`: part of the email or name, or a full account id. `plan`: `pro` | `free` (Pro right now,
paid or free grant). `status`: `active` | `disabled`. Newest first.

```json
{ "total": 1, "page": 1, "per_page": 50, "items": [ {
  "id": "ac_…", "email": "asha@example.com", "name": "Asha", "avatar_url": "https://…" ,
  "created_at": "2026-10-21T10:00:00Z", "last_seen_at": "2026-10-21T11:00:00Z",
  "disabled": false, "disabled_at": null,
  "plan": "pro", "pro_source": "subscription",
  "providers": ["google", "email"], "pcs": 1, "sessions": 2 } ] }
```

`pro_source`: `subscription` | `grant` | `null`. `pcs`: signed-in PCs now; `sessions`: active sessions.

### `GET /admin/v1/accounts/{id}`

The list row plus:

- `identities`: `[{ provider: "google"|"email", email, created_at, last_used_at }]`
- `sessions` (last 50, newest use first): `[{ id, kind: "app"|"web", label, install: "AbC1…"|null,
  created_at, last_used_at, expires_at, revoked_at, revoked_by: "user"|"admin:<email>"|null, active }]`
- `installs` (PCs ever signed in): `[{ install: "AbC1…", first_at, last_at, signed_in }]`
- `subscriptions` (paid, belonging to the account): `[{ id: "sub_…", plan, status, pro, current_end,
  access_until, cancel_at_period_end, refunded_at, linked_by: "checkout"|"email_match", linked_at }]`
- `grants` (free Pro given to the account): same fields plus `history` (as in Give Pro)
- `codes_used`: `[{ code, kind, used_at }]` (by the account, its subscriptions or its PCs)

404 `not_found` for an unknown id.

### `POST /admin/v1/accounts/{id}/grant` `{ "days": 1..90, "note"? }` → 201

Gives (or extends) free Pro to the account, capped at 90 days from now (like Give Pro on an
install). It reaches the account's 3 most recently signed-in PCs and the web dashboard.

```json
{ "id": "comp_…", "account_id": "ac_…", "plan": "pro", "starts_at": "…", "ends_at": "…", "extended": false }
```

409 `has_subscription` when the account pays for Pro; 422 `invalid_days`, `invalid_text`.

### `POST /admin/v1/accounts/{id}/revoke` → 200

Ends the account's free Pro now: `{ "account_id", "revoked": ["comp_…"], "revoked_at" }`.
409 `already_ended` when none is running (paid subscriptions are cancelled or refunded in Razorpay).

### `POST /admin/v1/accounts/{id}/sign-out` → 200

Ends every session (app and web) now: `{ "account_id", "sessions_ended": 2 }`. The account's
PCs lose account Pro until they sign in again; their own subscriptions are untouched.

### `POST /admin/v1/accounts/{id}/disable` `{ "reason"? }` / `POST …/enable` → 200

Disable: signed out everywhere, sign-in refused (403 `account_disabled`), no account Pro on its
PCs, the web dashboard refused. The app keeps working without the account. Enable undoes it
(sessions stay ended). Both answer the list row.

### `GET /admin/v1/accounts/stats?days=30`

`days` 1..365 (the sign-up window).

```json
{
  "accounts": { "total": 120, "active_7d": 40, "active_30d": 75, "disabled": 1 },
  "signups": { "days": 30, "per_day": [ { "date": "2026-10-21", "count": 4 } ], "in_window": 37 },
  "pro": { "accounts_pro_now": 12, "accounts_paying_now": 9 },
  "conversion": { "paid_ever": 11, "rate_all_time": 0.0917, "window_signups": 37, "window_paid": 3,
                  "rate_window": 0.0811 },
  "now": "2026-10-21T12:00:00Z"
}
```

`active_*`: used the app or the web within the days. `paid_ever`: accounts with a Razorpay
subscription that was charged at least once. `rate_*`: a fraction (0..1, 4 decimals) or `null`
with nothing to divide by. `per_day` has one entry per UTC day, oldest first, zeros included.
