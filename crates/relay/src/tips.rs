//! Tips (R-10, R-13, R-49): Ferrocast never touches the money.
//!
//! Each streamer uses their own provider page (a Razorpay Payment Page with UPI, a Ko-fi
//! page, a Stripe Payment Link) set to "viewer chooses the amount", and points that
//! provider's webhook here. The relay checks the provider's signature, keeps only what an
//! alert needs (name, amount, message, never email or phone), and queues it for the
//! streamer's desktop. The public tip page shows the streamer's display name only.
//!
//! Amounts leave the relay as integer minor units with the provider's digit count; the
//! desktop converts with Ferrocast's own currency table, so a provider's quirks (Stripe
//! sends ISK in hundredths) never change what an alert says.

use axum::extract::{FromRequest, Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::db::Saved;
use crate::{ApiError, AppState, authed, now_ms};

type HmacSha256 = Hmac<Sha256>;

/// What a streamer sets up, stored on the relay (the webhook secrets are needed to check
/// that a payment notice really came from the provider).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TipConfig {
    pub handle: String,
    pub display_name: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub razorpay: Option<Provider>,
    #[serde(default)]
    pub kofi: Option<Provider>,
    #[serde(default)]
    pub stripe: Option<Provider>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Provider {
    /// The provider's own payment page the tip button opens.
    pub page_url: String,
    /// Razorpay/Stripe webhook secret, or Ko-fi's verification token. Empty on a save keeps
    /// the stored one, so editing the page doesn't mean pasting every secret again.
    #[serde(default)]
    pub secret: String,
    /// Only payments made through this page or link count as tips (Razorpay
    /// `payment_page_id`/`payment_link_id`, Stripe `plink_…`). Without it, every payment on
    /// the account would alert, merch sales included.
    #[serde(default)]
    pub page_id: Option<String>,
    /// Accept the provider's test-mode notices (for trying the setup).
    #[serde(default)]
    pub test_mode: bool,
}

/// Words no tip page name may contain: ours, the platforms', and ones that look official.
const RESERVED_WORDS: &[&str] = &[
    "ferrocast",
    "clearcraft",
    "official",
    "twitch",
    "youtube",
    "kick",
    "razorpay",
    "stripe",
    "kofi",
    "ko-fi",
    "admin",
    "support",
    "staff",
    "security",
    "verify",
];

/// Whole names nobody may claim.
const RESERVED: &[&str] = &[
    "admin",
    "api",
    "app",
    "billing",
    "clearcraft",
    "clearcraftlabs",
    "contact",
    "ferrocast",
    "help",
    "kick",
    "login",
    "official",
    "pay",
    "payment",
    "razorpay",
    "root",
    "security",
    "settings",
    "staff",
    "stripe",
    "support",
    "test",
    "tip",
    "tips",
    "twitch",
    "verify",
    "youtube",
    "ko-fi",
    "kofi",
];

/// Handle changes allowed per install per day, and how long a released handle stays with
/// its last owner before anyone else can take it.
pub const HANDLE_CHANGES_PER_DAY: i64 = 3;
pub const HANDLE_HOLD_MS: i64 = 30 * 86_400_000;

fn valid_handle(h: &str) -> bool {
    (3..=30).contains(&h.len())
        && h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The parsed, normalised https URL on one of `hosts`, or `None`.
fn checked_page(url: &str, hosts: &[&str]) -> Option<String> {
    let u = url::Url::parse(url.trim()).ok()?;
    let ok = u.scheme() == "https"
        && u.username().is_empty()
        && u.password().is_none()
        && u.host_str().is_some_and(|h| hosts.iter().any(|ok| h == *ok || h.ends_with(&format!(".{ok}"))));
    ok.then(|| u.to_string())
}

const PROVIDERS: [(&str, &[&str], &str); 3] = [
    ("razorpay", &["razorpay.me", "pages.razorpay.com", "rzp.io"], "Razorpay"),
    ("kofi", &["ko-fi.com"], "Ko-fi"),
    ("stripe", &["buy.stripe.com"], "Stripe"),
];

impl TipConfig {
    fn provider_mut(&mut self, key: &str) -> Option<&mut Option<Provider>> {
        match key {
            "razorpay" => Some(&mut self.razorpay),
            "kofi" => Some(&mut self.kofi),
            "stripe" => Some(&mut self.stripe),
            _ => None,
        }
    }

    /// Fills blank secrets from the stored config, trims, and normalises URLs. Then
    /// refuses anything a viewer could be tricked by.
    pub fn prepare(&mut self, stored: Option<&TipConfig>) -> Result<(), String> {
        self.handle = self.handle.trim().to_lowercase();
        if !valid_handle(&self.handle) {
            return Err("The tip page name must be 3 to 30 lowercase letters, numbers, - or _.".into());
        }
        if RESERVED.contains(&self.handle.as_str()) || RESERVED_WORDS.iter().any(|w| self.handle.contains(w)) {
            return Err("That tip page name is reserved. Pick another.".into());
        }
        self.display_name = clean_text(&self.display_name, usize::MAX);
        if self.display_name.is_empty() || self.display_name.chars().count() > 40 {
            return Err("The display name must be 1 to 40 characters.".into());
        }
        if self.message.chars().count() > 280 {
            return Err("The tip page message can be up to 280 characters.".into());
        }
        self.message = clean_text(&self.message, 280);
        for (key, hosts, name) in PROVIDERS {
            let old = stored.and_then(|s| match key {
                "razorpay" => s.razorpay.clone(),
                "kofi" => s.kofi.clone(),
                _ => s.stripe.clone(),
            });
            let Some(slot) = self.provider_mut(key) else { continue };
            let Some(p) = slot.as_mut() else { continue };
            p.page_url = checked_page(&p.page_url, hosts)
                .ok_or_else(|| format!("The {name} link must be an https link on {}.", hosts.join(" or ")))?;
            p.secret = p.secret.trim().to_string();
            if p.secret.is_empty()
                && let Some(o) = &old
            {
                p.secret = o.secret.clone();
            }
            if p.secret.is_empty() {
                return Err(format!("Add the {name} webhook secret so payments can be checked."));
            }
            check_secret(key, name, &p.secret)?;
            p.page_id = p.page_id.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            if let Some(id) = &p.page_id {
                // Only ids a webhook actually carries can be matched; anything else would
                // silently drop every tip.
                let ok = matches!(key, "razorpay" | "stripe")
                    && id.starts_with("plink_")
                    && id.len() <= 64
                    && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
                if !ok {
                    return Err(format!(
                        "The {name} link id must be a Payment Link id that starts with plink_ (or leave it empty)."
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Refuses secrets that are clearly the wrong thing, before they're stored on a public server.
fn check_secret(key: &str, name: &str, secret: &str) -> Result<(), String> {
    if secret.len() > 200 {
        return Err(format!("That {name} secret is too long to be a webhook secret."));
    }
    if key == "stripe" {
        if ["sk_", "rk_", "pk_"].iter().any(|p| secret.starts_with(p)) {
            return Err("That's a Stripe API key. Paste the webhook signing secret instead (it starts with whsec_), \
                        and if you shared a live key anywhere, roll it in the Stripe dashboard."
                .into());
        }
        if !secret.starts_with("whsec_") {
            return Err("The Stripe webhook signing secret starts with whsec_. Copy it from the webhook's page.".into());
        }
    } else if secret.len() < 8 {
        return Err(format!("The {name} webhook secret is too short. Use at least 8 characters."));
    }
    Ok(())
}

/// Strips control, zero-width and text-direction characters (a name can't make an alert
/// read backwards or hide text), trims, and caps the length.
pub fn clean_text(s: &str, max: usize) -> String {
    let bad = |c: char| {
        c.is_control()
            // U+200D (zero-width joiner) stays: emoji like 👨‍👩‍👧 are built with it.
            || matches!(
                c,
                '\u{200B}' | '\u{200C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}'
                    | '\u{FEFF}' | '\u{00AD}' | '\u{061C}' | '\u{2028}' | '\u{2029}'
            )
    };
    s.chars().filter(|c| !bad(*c)).collect::<String>().trim().chars().take(max).collect()
}

/// JSON body whose errors use the `{error:{…}}` shape.
#[derive(Debug)]
pub struct Body<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::Json::<T>::from_request(req, state).await.map(|axum::Json(v)| Body(v)).map_err(|e| match e.status() {
            StatusCode::PAYLOAD_TOO_LARGE => ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!("The settings are over the {} KB limit.", crate::BODY_LIMIT / 1024),
            ),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_type",
                "Send the settings as JSON (Content-Type: application/json).",
            ),
            _ => ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid",
                format!("The settings aren't valid: {}", e.body_text()),
            ),
        })
    }
}

/// A webhook's raw body, with size errors in the `{error:{…}}` shape.
#[derive(Debug)]
pub struct Raw(pub axum::body::Bytes);

impl<S: Send + Sync> FromRequest<S> for Raw {
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::body::Bytes::from_request(req, state).await.map(Raw).map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    format!("Notices can be up to {} KB.", crate::BODY_LIMIT / 1024),
                )
            } else {
                ApiError::new(StatusCode::BAD_REQUEST, "bad_body", "The notice's body couldn't be read.")
            }
        })
    }
}

/// The saved config as the desktop may keep it: normalised, secrets replaced by flags.
fn public_config(cfg: &TipConfig) -> Value {
    let provider = |p: &Option<Provider>| {
        p.as_ref().map(|p| {
            json!({ "page_url": p.page_url, "page_id": p.page_id, "test_mode": p.test_mode, "has_secret": !p.secret.is_empty() })
        })
    };
    json!({
        "handle": cfg.handle, "display_name": cfg.display_name, "message": cfg.message,
        "razorpay": provider(&cfg.razorpay), "kofi": provider(&cfg.kofi), "stripe": provider(&cfg.stripe),
    })
}

/// `PUT /v1/tips/config`
pub async fn save_config(
    State(st): State<AppState>,
    headers: HeaderMap,
    Body(mut cfg): Body<TipConfig>,
) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    let stored = config_for(&st, &install);
    cfg.prepare(stored.as_ref()).map_err(|m| ApiError::new(StatusCode::BAD_REQUEST, "invalid", m))?;
    let json = serde_json::to_string(&cfg).unwrap_or_default();
    match st.db.set_tip_config(&install, &cfg.handle, &json, HANDLE_HOLD_MS, HANDLE_CHANGES_PER_DAY) {
        Ok(Saved::Yes) => {}
        Ok(Saved::Taken) => {
            return Err(ApiError::new(StatusCode::CONFLICT, "taken", "That tip page name is taken.")
                .hint("Pick another name; names someone just let go stay with them for 30 days."));
        }
        Ok(Saved::TooManyChanges) => {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "You changed your tip page name too often today.",
            )
            .hint("Try again tomorrow. Your current name keeps working."));
        }
        Err(e) => return Err(ApiError::db(e)),
    }
    let base = &st.cfg.public_url;
    Ok(axum::Json(json!({
        "config": public_config(&cfg),
        "page_url": page_url(&st.cfg, &cfg.handle),
        "webhooks": {
            "razorpay": format!("{base}/v1/tips/razorpay/{install}"),
            "kofi": format!("{base}/v1/tips/kofi/{install}"),
            "stripe": format!("{base}/v1/tips/stripe/{install}"),
        }
    }))
    .into_response())
}

/// `DELETE /v1/tips/config`: takes the page down (the name stays held for its owner).
pub async fn delete_config(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let install = authed(&st, &headers)?;
    st.db.delete_tip_config(&install).map_err(ApiError::db)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn config_for(st: &AppState, install: &str) -> Option<TipConfig> {
    st.db.tip_config(install).ok().flatten().and_then(|j| serde_json::from_str(&j).ok())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// A tip page's public link: on the tip host when there is one, else under the relay.
pub(crate) fn page_url(cfg: &crate::Config, handle: &str) -> String {
    match &cfg.tip_url {
        Some(tip) => format!("{tip}/{handle}"),
        None => format!("{}/t/{handle}", cfg.public_url),
    }
}

/// Whether this request came in on the tip host (`RELAY_TIP_URL`).
fn on_tip_host(st: &AppState, headers: &HeaderMap) -> bool {
    let Some(tip) = &st.cfg.tip_url else { return false };
    let want = tip.split_once("://").map_or(tip.as_str(), |(_, h)| h);
    headers.get(axum::http::header::HOST).and_then(|h| h.to_str().ok()).is_some_and(|h| h.eq_ignore_ascii_case(want))
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Html(shell("Not found", "<p class=lead>There's no tip page here.</p>"))).into_response()
}

/// `GET /` on the tip host: there's nothing to list, so point at Ferrocast.
pub async fn tip_host_home(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !on_tip_host(&st, &headers) {
        return not_found();
    }
    axum::response::Redirect::temporary("https://ferrocast.clearcraftlabs.com").into_response()
}

/// `GET /{handle}` on the tip host: the tip page. Anywhere else it's not a page.
pub async fn tip_host_page(State(st): State<AppState>, headers: HeaderMap, path: Path<String>) -> Response {
    if !on_tip_host(&st, &headers) {
        return not_found();
    }
    page(State(st), path).await
}

/// `GET /t/{handle}`: the public tip page. Static HTML, no scripts, no trackers.
pub async fn page(State(st): State<AppState>, Path(handle): Path<String>) -> Response {
    let cfg = st
        .db
        .tip_config_by_handle(&handle.to_lowercase())
        .ok()
        .flatten()
        .and_then(|j| serde_json::from_str::<TipConfig>(&j).ok());
    let Some(cfg) = cfg else {
        return (
            StatusCode::NOT_FOUND,
            Html(shell("Not found", "<p class=lead>There's no tip page with this name.</p>")),
        )
            .into_response();
    };
    let mut buttons = String::new();
    for (p, label, hint) in [
        (&cfg.razorpay, "Tip with UPI or card", "Razorpay"),
        (&cfg.kofi, "Tip on Ko-fi", "Ko-fi"),
        (&cfg.stripe, "Tip with card", "Stripe"),
    ] {
        if let Some(p) = p {
            buttons.push_str(&format!(
                "<a class=btn href=\"{}\" rel=\"noopener noreferrer\">{label}<small>{hint}</small></a>",
                esc(&p.page_url)
            ));
        }
    }
    if buttons.is_empty() {
        buttons = "<p class=muted>Tips aren't set up yet.</p>".into();
    }
    let body = format!(
        "<h1>{}</h1>{}<div class=buttons>{buttons}</div>\
         <p class=fine>Your name and message can show on stream. Payments go straight to the streamer through the provider you pick; Ferrocast never sees your card or bank details.</p>",
        esc(&cfg.display_name),
        if cfg.message.is_empty() { String::new() } else { format!("<p class=lead>{}</p>", esc(&cfg.message)) }
    );
    let mut res = Html(shell(&format!("Tip {}", cfg.display_name), &body)).into_response();
    let h = res.headers_mut();
    h.insert(
        "content-security-policy",
        axum::http::HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
    );
    h.insert("referrer-policy", axum::http::HeaderValue::from_static("no-referrer"));
    res
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=robots content=noindex><title>{}</title><style>\
         :root{{color-scheme:dark;--bg:#0c0e12;--fg:#eef0f4;--muted:#9aa3b2;--accent:#3461f5}}\
         body{{margin:0;min-height:100dvh;display:grid;place-items:center;background:radial-gradient(1200px 600px at 50% 110%,#1b2a5c 0,var(--bg) 60%);color:var(--fg);font:16px/1.5 system-ui,-apple-system,Segoe UI,sans-serif;padding:24px}}\
         main{{width:min(440px,100%);text-align:center}}h1{{font-size:clamp(28px,7vw,40px);margin:0 0 8px;letter-spacing:-.02em}}\
         .lead{{color:var(--muted);margin:0 0 28px}}.buttons{{display:grid;gap:12px;margin-bottom:28px}}\
         .btn{{display:flex;justify-content:space-between;align-items:center;padding:16px 20px;border-radius:14px;background:#151a24;border:1px solid #263047;color:var(--fg);text-decoration:none;font-weight:600}}\
         .btn:hover,.btn:focus-visible{{border-color:var(--accent);outline:none}}.btn small{{color:var(--muted);font-weight:400}}\
         .fine{{color:var(--muted);font-size:13px}}.muted{{color:var(--muted)}}</style><main>{}</main></html>",
        esc(title),
        body
    )
}

/// What a tip notice carries. Amounts are minor units with the provider's digit count,
/// or a decimal string (Ko-fi).
struct Notice<'a> {
    provider: &'a str,
    status: &'a str,
    payment_id: &'a str,
    amount_minor: Option<i64>,
    digits: Option<u32>,
    amount: Option<&'a str>,
    currency: &'a str,
    name: &'a str,
    message: &'a str,
}

impl<'a> Notice<'a> {
    fn reversal(provider: &'a str, status: &'a str, payment_id: &'a str) -> Notice<'a> {
        Notice {
            provider,
            status,
            payment_id,
            amount_minor: None,
            digits: None,
            amount: None,
            currency: "",
            name: "",
            message: "",
        }
    }
}

/// Queues a tip once per (install, provider, payment, status): providers retry and replays
/// of a captured body must not alert twice (R-29). The key is scoped to the install so one
/// streamer's notices can never mark another's as already seen.
fn queue_tip(st: &AppState, install: &str, n: Notice<'_>) -> Response {
    if n.payment_id.is_empty() {
        return StatusCode::OK.into_response();
    }
    let key = format!("tip:{install}:{}:{}:{}", n.provider, n.payment_id, n.status);
    // Claim the key first, so two copies delivered at once can't both be queued.
    match st.db.first_time(&key) {
        Ok(true) => {}
        Ok(false) => return StatusCode::OK.into_response(),
        Err(e) => return ApiError::db(e).into_response(),
    }
    let name = clean_text(n.name, 40);
    let payload = json!({
        "provider": n.provider, "status": n.status, "payment_id": n.payment_id,
        "amount_minor": n.amount_minor, "digits": n.digits, "amount": n.amount, "currency": n.currency.to_ascii_uppercase(),
        "name": if name.is_empty() { "Someone".to_string() } else { name }, "message": clean_text(n.message, 300), "at": now_ms(),
    });
    if let Err(e) = st.enqueue(install, "tip", &payload) {
        // Let the provider's retry try again.
        if let Err(err) = st.db.unsee(&key) {
            tracing::error!(error = %err, "couldn't release a tip id after a failed enqueue");
        }
        return e.into_response();
    }
    StatusCode::OK.into_response()
}

fn verify_hmac_hex(secret: &str, data: &[u8], sig_hex: &str) -> bool {
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else { return false };
    mac.update(data);
    let expected: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
    bool::from(expected.as_bytes().ct_eq(sig_hex.trim().to_lowercase().as_bytes()))
}

/// Razorpay signs the raw body with the webhook secret (HMAC-SHA256, hex).
pub fn razorpay_signature_ok(secret: &str, body: &[u8], sig: &str) -> bool {
    verify_hmac_hex(secret, body, sig)
}

/// Stripe: `Stripe-Signature: t=…,v1=…`, HMAC-SHA256 of `t.body`, within five minutes.
pub fn stripe_signature_ok(secret: &str, body: &[u8], header: &str, now_s: i64) -> bool {
    let mut t = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => t = v.parse::<i64>().ok(),
            Some(("v1", v)) => sigs.push(v.to_string()),
            _ => {}
        }
    }
    let Some(t) = t else { return false };
    if now_s.abs_diff(t) > 300 {
        return false;
    }
    let mut signed = t.to_string().into_bytes();
    signed.push(b'.');
    signed.extend_from_slice(body);
    sigs.iter().any(|s| verify_hmac_hex(secret, &signed, s))
}

/// How many minor-unit digits a provider uses for `currency` in its amounts. Stripe's
/// documented zero-decimal list, and its special case: ISK, HUF, TWD and UGX are sent in
/// hundredths even though they have no minor unit.
pub fn provider_digits(provider: &str, currency: &str) -> u32 {
    let c = currency.to_ascii_uppercase();
    const ZERO: [&str; 16] = [
        "BIF", "CLP", "DJF", "GNF", "JPY", "KMF", "KRW", "MGA", "PYG", "RWF", "VND", "VUV", "XAF", "XOF", "XPF", "UGX",
    ];
    const THREE: [&str; 5] = ["BHD", "JOD", "KWD", "OMR", "TND"];
    if provider == "stripe" && matches!(c.as_str(), "ISK" | "HUF" | "TWD" | "UGX") {
        return 2;
    }
    if ZERO.contains(&c.as_str()) {
        0
    } else if THREE.contains(&c.as_str()) {
        3
    } else {
        2
    }
}

fn note(entity: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| entity.pointer(&format!("/notes/{k}")).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("")
        .to_string()
}

/// Razorpay notices older than this are refused: the `seen` ids that stop replays are
/// kept for 30 days, so an older captured body can't be replayed past them.
const RAZORPAY_MAX_AGE_S: i64 = 7 * 86_400;

/// `POST /v1/tips/razorpay/{install}`
///
/// Without a pinned link every captured payment on the account is a tip. With one
/// (`plink_…`), only `payment_link.paid` for that link counts: Razorpay's payment entity
/// doesn't say which link it came from, the link event does.
pub async fn razorpay(
    State(st): State<AppState>,
    Path(install): Path<String>,
    headers: HeaderMap,
    Raw(body): Raw,
) -> Response {
    let Some(p) = config_for(&st, &install).and_then(|c| c.razorpay) else {
        return not_set_up("Razorpay");
    };
    let sig = headers.get("x-razorpay-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !razorpay_signature_ok(&p.secret, &body, sig) {
        return bad_signature("this page's webhook secret");
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return bad_json();
    };
    if v["created_at"].as_i64().is_some_and(|t| now_ms() / 1000 - t > RAZORPAY_MAX_AGE_S) {
        return StatusCode::OK.into_response();
    }
    let pay = &v["payload"]["payment"]["entity"];
    let captured = |link_notes: &Value| {
        let currency = pay["currency"].as_str().unwrap_or("INR");
        let pick = |keys: &[&str]| {
            let n = note(pay, keys);
            if n.is_empty() { note(link_notes, keys) } else { n }
        };
        let (name, message) = (pick(&["name", "full_name", "your_name"]), pick(&["message", "msg", "comment"]));
        queue_tip(
            &st,
            &install,
            Notice {
                provider: "razorpay",
                status: "captured",
                payment_id: pay["id"].as_str().unwrap_or(""),
                amount_minor: pay["amount"].as_i64(),
                digits: Some(provider_digits("razorpay", currency)),
                amount: None,
                currency,
                name: &name,
                message: &message,
            },
        )
    };
    match v["event"].as_str().unwrap_or("") {
        "payment.captured" if p.page_id.is_none() => captured(&Value::Null),
        "payment_link.paid" => {
            let link = &v["payload"]["payment_link"]["entity"];
            match &p.page_id {
                Some(want) if link["id"].as_str() != Some(want.as_str()) => StatusCode::OK.into_response(),
                _ => captured(&link.clone()),
            }
        }
        // Only a payment refunded in full takes the tip back; partial refunds leave it.
        "refund.processed" if pay["status"] == "refunded" => {
            let id = v["payload"]["refund"]["entity"]["payment_id"].as_str().or(pay["id"].as_str()).unwrap_or("");
            queue_tip(&st, &install, Notice::reversal("razorpay", "refunded", id))
        }
        "payment.dispute.created" => {
            let id = v["payload"]["dispute"]["entity"]["payment_id"].as_str().unwrap_or("");
            queue_tip(&st, &install, Notice::reversal("razorpay", "disputed", id))
        }
        _ => StatusCode::OK.into_response(),
    }
}

fn not_set_up(provider: &str) -> Response {
    ApiError::new(StatusCode::NOT_FOUND, "not_set_up", format!("No {provider} tips are set up for this address."))
        .hint("Copy the webhook address again from the Tips screen in Ferrocast.")
        .into_response()
}

fn bad_signature(what: &str) -> Response {
    ApiError::new(StatusCode::UNAUTHORIZED, "bad_signature", format!("The notice isn't signed with {what}."))
        .hint("Paste the same secret in Ferrocast that the provider shows for this webhook.")
        .into_response()
}

fn bad_json() -> Response {
    ApiError::new(StatusCode::BAD_REQUEST, "bad_json", "The notice isn't JSON.").into_response()
}

#[derive(Debug, Deserialize)]
pub struct KofiForm {
    data: String,
}

/// `POST /v1/tips/kofi/{install}`: Ko-fi posts a form with a `data` JSON field holding its
/// verification token.
pub async fn kofi(State(st): State<AppState>, Path(install): Path<String>, Raw(body): Raw) -> Response {
    let Some(p) = config_for(&st, &install).and_then(|c| c.kofi) else {
        return not_set_up("Ko-fi");
    };
    let Ok(form) = serde_urlencoded_data(&body) else {
        return ApiError::new(StatusCode::BAD_REQUEST, "bad_form", "Ko-fi's form is missing its data field.")
            .into_response();
    };
    let Ok(v) = serde_json::from_str::<Value>(&form.data) else {
        return ApiError::new(StatusCode::BAD_REQUEST, "bad_json", "Ko-fi's data isn't JSON.").into_response();
    };
    let token = v["verification_token"].as_str().unwrap_or("");
    if !bool::from(token.as_bytes().ct_eq(p.secret.as_bytes())) {
        return ApiError::new(StatusCode::UNAUTHORIZED, "bad_token", "The Ko-fi verification token doesn't match.")
            .hint("Copy the verification token from Ko-fi's webhook settings into Ferrocast.")
            .into_response();
    }
    if !matches!(v["type"].as_str(), Some("Donation" | "Subscription")) {
        return StatusCode::OK.into_response();
    }
    // Ko-fi's "send a test" uses this fixed id; it only alerts while test mode is on.
    let id = v["kofi_transaction_id"].as_str().or(v["message_id"].as_str()).unwrap_or("");
    if id == KOFI_TEST_ID && !p.test_mode {
        return StatusCode::OK.into_response();
    }
    // Ko-fi lets supporters hide their message; then only the amount shows.
    let public = v["is_public"].as_bool().unwrap_or(true);
    queue_tip(
        &st,
        &install,
        Notice {
            provider: "ko_fi",
            status: "captured",
            payment_id: id,
            amount_minor: None,
            digits: None,
            amount: Some(v["amount"].as_str().unwrap_or("0")),
            currency: v["currency"].as_str().unwrap_or("USD"),
            name: if public { v["from_name"].as_str().unwrap_or("") } else { "" },
            message: if public { v["message"].as_str().unwrap_or("") } else { "" },
        },
    )
}

const KOFI_TEST_ID: &str = "00000000-1111-2222-3333-444444444444";

fn serde_urlencoded_data(body: &[u8]) -> Result<KofiForm, ()> {
    url::form_urlencoded::parse(body)
        .find(|(k, _)| k == "data")
        .map(|(_, v)| KofiForm { data: v.into_owned() })
        .ok_or(())
}

/// `POST /v1/tips/stripe/{install}`
pub async fn stripe(
    State(st): State<AppState>,
    Path(install): Path<String>,
    headers: HeaderMap,
    Raw(body): Raw,
) -> Response {
    let Some(p) = config_for(&st, &install).and_then(|c| c.stripe) else {
        return not_set_up("Stripe");
    };
    let header = headers.get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !stripe_signature_ok(&p.secret, &body, header, now_ms() / 1000) {
        return bad_signature("this link's webhook secret");
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return bad_json();
    };
    if v["livemode"].as_bool() == Some(false) && !p.test_mode {
        return StatusCode::OK.into_response();
    }
    let obj = &v["data"]["object"];
    let intent = obj["payment_intent"].as_str().unwrap_or("");
    match v["type"].as_str().unwrap_or("") {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded"
            if obj["payment_status"] == "paid" =>
        {
            if let Some(want) = &p.page_id
                && obj["payment_link"].as_str() != Some(want.as_str())
            {
                return StatusCode::OK.into_response(); // a sale, not a tip
            }
            let currency = obj["currency"].as_str().unwrap_or("usd").to_uppercase();
            let field = |key: &str| {
                obj["custom_fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|f| f["key"].as_str().is_some_and(|k| k.contains(key)))
                    .and_then(|f| f["text"]["value"].as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let (name, message) = (field("name"), field("message"));
            queue_tip(
                &st,
                &install,
                Notice {
                    provider: "stripe",
                    status: "captured",
                    // Refunds name the payment intent, so it's the only key that can match them.
                    payment_id: intent,
                    amount_minor: obj["amount_total"].as_i64(),
                    digits: Some(provider_digits("stripe", &currency)),
                    amount: None,
                    currency: &currency,
                    name: &name,
                    message: &message,
                },
            )
        }
        // `charge.refunded` also fires for partial refunds; only a full refund reverses.
        "charge.refunded" if obj["refunded"] == true => {
            queue_tip(&st, &install, Notice::reversal("stripe", "refunded", intent))
        }
        // Inquiries ("warning_…") take no money; only a real dispute reverses. A dispute the
        // streamer later wins isn't added back: goals stay on the safe side.
        "charge.dispute.created" if !obj["status"].as_str().is_some_and(|s| s.starts_with("warning_")) => {
            queue_tip(&st, &install, Notice::reversal("stripe", "disputed", intent))
        }
        _ => StatusCode::OK.into_response(),
    }
}

#[cfg(test)]
pub(crate) fn sign_hex(secret: &str, data: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac takes any key");
    mac.update(data);
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
