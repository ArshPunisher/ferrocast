//! The login pages on the web host (docs/ACCOUNTS.md §4): plain HTML forms, no scripts.
//!
//! Every step is tied to the browser that started it by the `__Host-fc_login` cookie: its
//! hash is stored with the email link/code and with the Google round trip, and each form
//! carries a token derived from it (CSRF). So a link or code someone else asked for your
//! address can't sign them in, and another site can't post these forms.

use axum::extract::{ConnectInfo, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::store::GoogleId;
use super::*;
use crate::mail::report::esc;

const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src 'none'; \
    form-action 'self' https://accounts.google.com http://127.0.0.1:* http://localhost:*; \
    frame-ancestors 'none'; base-uri 'none'";

/// Characters of an emailed code: no look-alikes (0/O, 1/I/L).
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const CODE_LEN: usize = 8;
/// Sign-in emails a day beyond the weekly email's budget (Brevo's free plan sends 300).
const SIGN_IN_RESERVE: i64 = 25;

pub(super) fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/login", get(login_page))
        // The dashboard links to `/login/` (the website's page); here it's the same page.
        .route("/login/", get(login_page))
        .route("/login/email", post(email_start))
        .route("/login/code", post(email_code))
        .route("/login/link", get(link_page).post(link_use))
        .route("/login/google", post(google_start))
        .route("/login/google/callback", get(google_callback))
        .route("/logout", post(logout_form))
}

// ── Rendering ────────────────────────────────────────────────────────────────────────

// Design board 2026-10-21-accounts, option A: the Spark over the title, Google as a quiet
// button, the email form with one orange button (white text), light and dark by the system.
const STYLE: &str = "<style>:root{color-scheme:dark light;--bg:#0a0b0d;--s1:#101216;--s2:#16191e;\
--line:#252a31;--line2:#343a44;--text:#eef0f4;--text2:#b3b9c4;--text3:#8a919d;--molten:#ffa424;--arc:#33d4ff;\
--arc-text:#33d4ff;--arc-soft:rgb(51 212 255/.12);--err:#ff5d6c;--btn:#b35900;--btn-hover:#9e4f00}\
@media (prefers-color-scheme:light){:root{--bg:#f2f3f6;--s1:#fff;--s2:#f7f8fa;--line:#dfe2e8;\
--line2:#c9ced7;--text:#0e1014;--text2:#424854;--text3:#5f6672;--molten:#cf7400;--arc:#0888b8;--arc-text:#05678c;\
--arc-soft:rgb(8 136 184/.1);--err:#c42339}}\
*{box-sizing:border-box}html{-webkit-text-size-adjust:100%}\
body{margin:0;min-height:100vh;min-height:100dvh;display:flex;flex-direction:column;background:var(--bg);color:var(--text);\
font:16px/1.55 system-ui,-apple-system,'Segoe UI',sans-serif;-webkit-font-smoothing:antialiased}\
.top{display:flex;align-items:center;height:56px;padding:0 16px;border-bottom:1px solid var(--line)}\
.brand{display:inline-flex;align-items:center;gap:4px;color:var(--text);text-decoration:none;font-weight:800;font-size:19px;letter-spacing:-.02em}\
.brand svg{width:24px;height:24px}\
.wrap{flex:1;display:flex;justify-content:center;padding:32px 16px 48px}\
main{width:100%;max-width:400px;display:grid;gap:18px;align-content:start}\
.mark svg{display:block;width:40px;height:40px;color:var(--text)}\
.tile{width:44px;height:44px;border-radius:12px;display:grid;place-items:center;background:var(--arc-soft);color:var(--arc-text)}\
.tile svg{width:22px;height:22px}\
h1{margin:0;font-size:24px;line-height:1.15;font-weight:700;letter-spacing:-.02em;overflow-wrap:anywhere}\
p{margin:0;overflow-wrap:anywhere}.lead{color:var(--text2);font-size:14.5px}strong{color:var(--text)}\
form{margin:0;display:grid;gap:6px}\
label{font-size:13.5px;font-weight:600}\
input{width:100%;height:44px;padding:0 12px;border:1px solid var(--line2);border-radius:10px;background:var(--s1);\
color:var(--text);font:inherit;font-size:16px}\
input::placeholder{color:var(--text3)}\
input:focus{outline:none;border-color:var(--arc);box-shadow:0 0 0 3px var(--arc-soft)}\
.hint{color:var(--text3);font-size:13px}\
.btn{display:inline-flex;align-items:center;justify-content:center;gap:10px;width:100%;min-height:48px;padding:0 20px;\
border-radius:10px;border:1px solid var(--line2);background:var(--s1);color:var(--text);font:600 15px system-ui,sans-serif;\
text-decoration:none;cursor:pointer;touch-action:manipulation;white-space:nowrap}\
.btn svg{width:18px;height:18px;flex:none}.btn:active{transform:scale(.98)}\
.btn.primary{background:var(--btn);border-color:transparent;color:#fff;box-shadow:inset 0 1px 0 rgb(255 255 255/.18);margin-top:6px}\
.btn.ghost{background:transparent;border-color:transparent;color:var(--text2)}\
@media (hover:hover){.btn:hover{border-color:var(--text3)}.btn.primary:hover{background:var(--btn-hover)}\
.btn.ghost:hover{background:var(--s2);color:var(--text)}}\
.btn:focus-visible,a:focus-visible,input:focus-visible{outline:2px solid var(--arc-text);outline-offset:2px}\
.or{display:flex;align-items:center;gap:12px;color:var(--text3);font-size:12.5px}\
.or:before,.or:after{content:'';flex:1;height:1px;background:var(--line)}\
.row{display:flex;flex-wrap:wrap;gap:10px}.row .btn{width:auto}\
.err{color:var(--err);font-weight:600;font-size:14px}\
.muted{color:var(--text3);font-size:12.5px;text-align:center}\
.code input{font:600 1.2rem ui-monospace,monospace;letter-spacing:.12em;text-transform:uppercase}\
a{color:var(--arc-text);text-underline-offset:3px}\
@media (prefers-reduced-motion:reduce){.btn:active{transform:none}}</style>";

/// The Spark (brand.ts, X1), its two shard halves in the theme's Molten and Arc.
const SPARK: &str = "<svg viewBox=\"0 0 64 64\" aria-hidden=\"true\"><path fill=\"currentColor\" \
d=\"M17.33 54.5V25A15.5 15.5 0 0 1 32.83 9.5H39.33V18.5H32.83A6.5 6.5 0 0 0 26.33 25V54.5ZM10.83 25.5H33.33V33.5H10.83Z\"/>\
<path fill=\"var(--molten)\" d=\"M48.03 21.12L34.35 37.73L50.85 32.78L44.85 32.78Z\"/>\
<path fill=\"var(--arc)\" d=\"M36.72 39.14L42.73 39.14L39.02 50.8L53.17 34.2Z\"/></svg>";
/// Phosphor icons (MIT): Google's G and an envelope.
const ICON_GOOGLE: &str = "<svg viewBox=\"0 0 256 256\" fill=\"currentColor\" aria-hidden=\"true\"><path \
d=\"M224,128a96,96,0,1,1-21.95-61.09,8,8,0,1,1-12.33,10.18A80,80,0,1,0,207.6,136H128a8,8,0,0,1,0-16h88A8,8,0,0,1,224,128Z\"/></svg>";
const ICON_MAIL: &str = "<svg viewBox=\"0 0 256 256\" fill=\"currentColor\" aria-hidden=\"true\"><path \
d=\"M224,48H32a8,8,0,0,0-8,8V192a16,16,0,0,0,16,16H216a16,16,0,0,0,16-16V56A8,8,0,0,0,224,48ZM203.43,64,128,133.15,52.57,64ZM216,192H40V74.19l82.59,75.71a8,8,0,0,0,10.82,0L216,74.19V192Z\"/></svg>";

/// A login page: the Ferrocast header, then the Spark (or `icon`), the title and `body`.
fn page_with(status: StatusCode, title: &str, icon: Option<&str>, body: &str) -> Response {
    let top = match icon {
        Some(i) => format!("<span class=tile>{i}</span>"),
        None => format!("<span class=mark>{SPARK}</span>"),
    };
    let html = format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=robots content=noindex><meta name=referrer content=no-referrer><title>{t} · Ferrocast</title>{STYLE}\
         <header class=top><a class=brand href=\"/app/\">{SPARK}ferrocast</a></header>\
         <div class=wrap><main>{top}<h1>{t}</h1>{body}</main></div></html>",
        t = esc(title),
    );
    let mut r = (status, axum::response::Html(html)).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    r
}

/// A page in this module's look, with the Spark on top.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    page_with(status, title, None, body)
}

fn problem(status: StatusCode, title: &str, text: &str, again: &str) -> Response {
    page(
        status,
        title,
        &format!("<p class=lead>{}</p><a class=\"btn primary\" href=\"{}\">Try again</a>", esc(text), esc(again)),
    )
}

fn not_here() -> Response {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "There's nothing here.").into_response()
}

fn redirect(to: &str) -> Response {
    let mut r = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

/// `next` kept to the web dashboard: a path under `/app`, nothing that leaves the host.
pub(crate) fn safe_next(raw: Option<&str>) -> String {
    let n = raw.unwrap_or("").trim();
    let ok = (n == "/app" || n.starts_with("/app/"))
        && n.len() <= 512
        && !n.contains("//")
        && !n.contains('\\')
        && !n.chars().any(|c| c.is_control() || c.is_whitespace());
    if ok { n.to_string() } else { "/app/".to_string() }
}

fn query(raw: Option<&str>) -> HashMap<String, String> {
    url::form_urlencoded::parse(raw.unwrap_or("").as_bytes()).into_owned().collect()
}

fn form(headers: &HeaderMap, body: &[u8]) -> Option<HashMap<String, String>> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct.trim_start().to_ascii_lowercase().starts_with("application/x-www-form-urlencoded") || body.len() > 8192 {
        return None;
    }
    Some(url::form_urlencoded::parse(body).into_owned().collect())
}

fn hidden(name: &str, value: &str) -> String {
    format!("<input type=hidden name={name} value=\"{}\">", esc(value))
}

/// The login cookie, made if missing: (value, set it on the answer).
fn login_cookie(headers: &HeaderMap) -> (String, bool) {
    match cookie(headers, LOGIN_COOKIE).filter(|c| c.len() >= 20 && c.len() <= 64) {
        Some(c) => (c, false),
        None => (crate::random(24), true),
    }
}

fn with_login_cookie(mut r: Response, value: &str) -> Response {
    r.headers_mut().append(header::SET_COOKIE, set_cookie(LOGIN_COOKIE, value, LOGIN_COOKIE_S));
    r
}

fn browser_hash(c: &str) -> String {
    sha(&format!("ferrocast login browser v1|{c}"))
}

/// Why a sign-in step stopped. The pages here show it as a page; the website's JSON
/// endpoints (`login_api`) send it as `{error:{code,message,hint}}`.
pub(super) struct Fail {
    pub(super) status: StatusCode,
    pub(super) code: &'static str,
    title: &'static str,
    pub(super) text: String,
    /// Where "Try again" goes on a page.
    again: String,
    /// Seconds to wait, for a rate limit.
    pub(super) retry_after: Option<i64>,
}

impl Fail {
    fn new(status: StatusCode, code: &'static str, title: &'static str, text: impl Into<String>, again: &str) -> Fail {
        Fail { status, code, title, text: text.into(), again: again.to_string(), retry_after: None }
    }
    fn db(err: rusqlite::Error) -> Fail {
        tracing::error!(error = %err, "relay database error during sign-in");
        Fail::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage",
            "Try again",
            "The relay couldn't save that. Try again shortly.",
            "/login",
        )
    }
    fn page(self) -> Response {
        let mut r = problem(self.status, self.title, &self.text, &self.again);
        if let Some(w) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&w.to_string())
        {
            r.headers_mut().insert(header::RETRY_AFTER, v);
        }
        if self.code == "google_off" {
            r.headers_mut().insert("x-error-code", HeaderValue::from_static("google_off"));
        }
        r
    }
}

/// The checks every sign-in POST makes: the web host, a trusted origin, the login cookie and
/// the form token derived from it (`given`: a page's hidden field, or the website's
/// `X-CSRF-Token`). The cookie's value; `None` when this host has no sign-in.
pub(super) fn login_guard(st: &AppState, headers: &HeaderMap, given: &str) -> Result<Option<String>, Fail> {
    if !available(st) || !crate::remote::on_web_host(st, headers) {
        return Ok(None);
    }
    let Some(c) = cookie(headers, LOGIN_COOKIE) else {
        return Err(Fail::new(
            StatusCode::BAD_REQUEST,
            "cookies_needed",
            "Cookies are needed",
            "Signing in needs a cookie that this browser didn't send. Allow cookies for this site and try again.",
            "/login",
        ));
    };
    if !same_origin(st, headers) || given.is_empty() || !ct_eq(given, &form_token(&c)) {
        return Err(Fail::new(
            StatusCode::FORBIDDEN,
            "form_expired",
            "Start again",
            "This form is out of date or came from another site.",
            "/login",
        ));
    }
    Ok(Some(c))
}

/// The page form's guard: [`login_guard`] with "not here" and the problem as pages.
fn post_guard(st: &AppState, headers: &HeaderMap, f: &HashMap<String, String>) -> Result<String, Box<Response>> {
    match login_guard(st, headers, f.get("csrf").map(String::as_str).unwrap_or("")) {
        Ok(Some(c)) => Ok(c),
        Ok(None) => Err(Box::new(not_here())),
        Err(e) => Err(Box::new(e.page())),
    }
}

/// The flow an app sign-in is in, if it's still waiting.
pub(super) fn live_flow(st: &AppState, id: Option<&str>) -> Result<Option<store::Flow>, Fail> {
    let Some(id) = id.filter(|i| !i.is_empty()) else { return Ok(None) };
    let f = st.db.account_flow(id).map_err(Fail::db)?;
    match f {
        Some(f) if f.expires_at > now_ms() && f.code_hash.is_none() && f.used_at.is_none() => Ok(Some(f)),
        _ => Err(Fail::new(
            StatusCode::GONE,
            "flow_expired",
            "This sign-in expired",
            "Sign-ins from the app last 10 minutes. Go back to Ferrocast and press Sign in again.",
            "/login",
        )),
    }
}

fn back_to(flow: Option<&str>, next: &str) -> String {
    match flow {
        Some(f) if !f.is_empty() => {
            format!("/login?flow={}", url::form_urlencoded::byte_serialize(f.as_bytes()).collect::<String>())
        }
        _ => format!("/login?next={}", url::form_urlencoded::byte_serialize(next.as_bytes()).collect::<String>()),
    }
}

// ── GET /login ───────────────────────────────────────────────────────────────────────

async fn login_page(State(st): State<AppState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    if !available(&st) || !crate::remote::on_web_host(&st, &headers) {
        return not_here();
    }
    let q = query(raw.as_deref());
    let next = safe_next(q.get("next").map(String::as_str));
    let flow = match live_flow(&st, q.get("flow").map(String::as_str)) {
        Ok(f) => f,
        Err(e) => return e.page(),
    };
    // Already signed in on the web: straight to the dashboard.
    if flow.is_none() && web_session(&st, &headers).is_ok() {
        return redirect(&next);
    }
    let (c, fresh) = login_cookie(&headers);
    let token = form_token(&c);
    let flow_id = flow.as_ref().map(|f| f.id.clone()).unwrap_or_default();
    let keep = format!("{}{}{}", hidden("csrf", &token), hidden("flow", &flow_id), hidden("next", &next));
    let lead = if flow.is_some() {
        "Sign in to link this Ferrocast app to your account. The app keeps working without one."
    } else {
        "The web dashboard needs an account. It’s free, and it’s the same one your app uses."
    };
    let google = if st.accounts.cfg.google.on() {
        format!(
            "<form method=post action=/login/google>{keep}<button class=btn type=submit>{ICON_GOOGLE}Continue with Google</button></form>\
             <div class=or>or</div>"
        )
    } else {
        String::new()
    };
    let body = format!(
        "<p class=lead>{lead}</p>{google}<form method=post action=/login/email>{keep}\
         <label for=email>Email</label><input id=email name=email type=email autocomplete=email \
         inputmode=email required maxlength=254 spellcheck=false placeholder=\"you@example.com\" aria-describedby=email-hint>\
         <p class=hint id=email-hint>We’ll email you a link and a code. No password to remember.</p>\
         <button class=\"btn primary\" type=submit>{ICON_MAIL}Email me a link</button></form>\
         <p class=muted>The link and code work once, for 15 minutes, in this browser.</p>\
         <p class=muted>By continuing you confirm you’re 18 or older, or have a parent’s or guardian’s consent. \
         <a href=\"{SITE}/terms/\">Terms</a> · <a href=\"{SITE}/privacy/\">Privacy</a></p>",
        lead = esc(lead),
    );
    let r = page(StatusCode::OK, "Sign in to Ferrocast", &body);
    if fresh { with_login_cookie(r, &c) } else { r }
}

/// The website, for the terms and privacy links under the form.
const SITE: &str = "https://ferrocast.clearcraftlabs.com";

// ── Email ────────────────────────────────────────────────────────────────────────────

fn new_code() -> String {
    let mut out = String::with_capacity(CODE_LEN);
    while out.len() < CODE_LEN {
        let mut b = [0u8; 16];
        #[allow(clippy::expect_used)]
        getrandom::fill(&mut b).expect("the OS random number generator is unavailable");
        for v in b {
            // 31 symbols: 248 = 8 * 31, so every symbol is equally likely.
            if usize::from(v) < 248 && out.len() < CODE_LEN {
                out.push(char::from(CODE_ALPHABET[usize::from(v) % CODE_ALPHABET.len()]));
            }
        }
    }
    out
}

fn normal_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn code_hash(magic_id: &str, code: &str) -> String {
    sha(&format!("ferrocast email code v1|{magic_id}|{code}"))
}

fn code_form(token: &str, magic_id: &str, error: Option<&str>) -> String {
    let err = error.map(|e| format!("<p class=err role=alert>{}</p>", esc(e))).unwrap_or_default();
    format!(
        "{err}<form method=post action=/login/code class=code>{}{}<label for=code>Code from the email</label>\
         <input id=code name=code required autocomplete=one-time-code maxlength=12 spellcheck=false \
         autocapitalize=characters><button class=\"btn primary\" type=submit>Sign in</button></form>",
        hidden("csrf", token),
        hidden("m", magic_id)
    )
}

// ── The steps, shared by these pages and the website's JSON endpoints ─────────────────

/// Rate limits, the shared email budget, then the sign-in email: its link and code work
/// once, for 15 minutes, in the browser holding login cookie `c`. The new sign-in's id.
pub(super) async fn send_magic(
    st: &AppState,
    ip: &str,
    c: &str,
    email: &str,
    flow: Option<&str>,
    next: &str,
) -> Result<String, Fail> {
    let again = back_to(flow, next);
    let email = email.trim().to_lowercase();
    if !crate::billing::valid_email(&email) {
        return Err(Fail::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_email",
            "Check the address",
            "That email address doesn't look right.",
            &again,
        ));
    }
    let Some(mailer) = st.mail.mailer() else {
        return Err(Fail::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "email_off",
            "Email sign-in is off",
            "This relay can't send email right now. Try Google, or try again later.",
            &again,
        ));
    };
    let limited = st
        .accounts
        .take("magic_15m", &email, 3, 15 * MINUTE)
        .and_then(|_| st.accounts.take("magic_day", &email, 10, DAY))
        .and_then(|_| st.accounts.take("magic_ip", ip, 20, 60 * MINUTE));
    if let Err(wait) = limited {
        let mut f = Fail::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many emails",
            format!("We've sent a lot of sign-in emails. Wait {} minutes, or use the last one.", (wait + 59) / 60),
            &again,
        );
        f.retry_after = Some(wait);
        return Err(f);
    }
    // Sign-in emails share Brevo's daily allowance with the weekly email, plus a small
    // reserve above the weekly budget so reports can't lock people out.
    let day = now_ms().div_euclid(DAY);
    match st.db.mail_budget_take(day, st.mail.budget() + SIGN_IN_RESERVE) {
        Ok(true) => {}
        Ok(false) => {
            return Err(Fail::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "email_busy",
                "Email sign-in is busy",
                "We've sent all the email we can today. Use Google, or try again tomorrow.",
                &again,
            ));
        }
        Err(err) => return Err(Fail::db(err)),
    }
    let id = store::new_id("ml_");
    let token = crate::random(32);
    let code = new_code();
    let now = now_ms();
    st.db
        .account_put_magic(
            &id,
            &sha(&token),
            &code_hash(&id, &code),
            &email,
            &browser_hash(c),
            flow,
            next,
            now,
            now + MAGIC_TTL_MS,
        )
        .map_err(Fail::db)?;
    // On the website the token rides in the fragment, so it never reaches a server log.
    let link = if st.remote.cfg.web.is_some() {
        format!("{}/login/link/#t={token}", page_origin(st))
    } else {
        format!("{}/login/link?t={token}", web_origin(st))
    };
    let shown = format!("{}-{}", &code[..4], &code[4..]);
    let text = format!(
        "Sign in to Ferrocast:\n{link}\n\nOr type this code on the sign-in page: {shown}\n\n\
         The link and the code work once, for 15 minutes, only in the browser where you asked for them.\n\
         If you didn't ask to sign in, ignore this email. Nobody can use it without that browser.\n\n{}\n",
        crate::mail::layout::POSTAL_ADDRESS
    );
    let mail = crate::mail::Email {
        to: email.clone(),
        subject: "Your Ferrocast sign-in link".into(),
        html: sign_in_html(&link, &shown),
        text,
        headers: Vec::new(),
        tag: "sign_in",
    };
    let sent = tokio::time::timeout(std::time::Duration::from_secs(15), mailer.send(&st.mail.from(), &mail)).await;
    match sent {
        Ok(Ok(())) | Ok(Err(crate::mail::MailError::Uncertain(_))) => {}
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "a sign-in email couldn't be sent");
            let _ = st.db.mail_budget_give_back(day);
            return Err(Fail::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "email_failed",
                "The email didn't go",
                "We couldn't send the sign-in email just now. Try again in a minute, or use Google.",
                &again,
            ));
        }
        Err(_) => tracing::warn!("a sign-in email took too long; it may still arrive"),
    }
    Ok(id)
}

/// The code typed for sign-in `id`, checked in the browser that asked. A wrong code counts a
/// try (`wrong_code`, with how many are left); the right one uses the sign-in up.
pub(super) fn check_code(st: &AppState, c: &str, id: &str, typed: &str) -> Result<store::Magic, Fail> {
    let gone = || {
        Fail::new(
            StatusCode::GONE,
            "code_expired",
            "This code expired",
            "Codes work once, for 15 minutes, in the browser that asked. Ask for a new one.",
            "/login",
        )
    };
    let m = st.db.account_magic(id).map_err(Fail::db)?.ok_or_else(gone)?;
    let now = now_ms();
    if !ct_eq(&m.browser_hash, &browser_hash(c)) || m.used_at.is_some() || m.expires_at <= now || m.tries >= MAGIC_TRIES
    {
        return Err(gone());
    }
    let code = normal_code(typed);
    if code.len() != CODE_LEN || !ct_eq(&m.code_hash, &code_hash(&m.id, &code)) {
        let tries = st.db.account_magic_miss(&m.id).map_err(Fail::db)?;
        if tries >= MAGIC_TRIES {
            return Err(Fail::new(
                StatusCode::GONE,
                "too_many_codes",
                "Too many wrong codes",
                "That sign-in is closed now. Ask for a new email.",
                &back_to(m.flow_id.as_deref(), &m.next),
            ));
        }
        let left = MAGIC_TRIES - tries;
        return Err(Fail::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "wrong_code",
            "Check your email",
            format!("That code isn't right. {left} {} left.", if left == 1 { "try" } else { "tries" }),
            "/login",
        ));
    }
    if !st.db.account_magic_use(&m.id, now).map_err(Fail::db)? {
        return Err(gone());
    }
    Ok(m)
}

/// The emailed link's token `t`, used up in the browser that asked.
pub(super) fn take_link(st: &AppState, c: &str, t: &str) -> Result<store::Magic, Fail> {
    let gone = || {
        Fail::new(
            StatusCode::GONE,
            "link_expired",
            "This link expired",
            "Sign-in links work once, for 15 minutes, in the browser that asked. Ask for a new one.",
            "/login",
        )
    };
    if t.is_empty() {
        return Err(gone());
    }
    let m = st.db.account_magic_by_token(&sha(t)).map_err(Fail::db)?.ok_or_else(gone)?;
    if !ct_eq(&m.browser_hash, &browser_hash(c)) {
        return Err(Fail::new(
            StatusCode::FORBIDDEN,
            "other_browser",
            "Open it where you asked",
            "For your safety this link only works in the browser where you asked for it. Open it there, or type the code from the email on that sign-in page.",
            "/login",
        ));
    }
    if m.used_at.is_some() || m.expires_at <= now_ms() || !st.db.account_magic_use(&m.id, now_ms()).map_err(Fail::db)? {
        return Err(gone());
    }
    Ok(m)
}

/// Google's sign-in address for this browser (PKCE, a state and a nonce kept for the
/// callback on this host).
pub(super) fn google_url(st: &AppState, ip: &str, c: &str, flow: Option<&str>, next: &str) -> Result<String, Fail> {
    let again = back_to(flow, next);
    let g = &st.accounts.cfg.google;
    let Some(client_id) = g.client_id.as_deref().filter(|_| g.on()) else {
        return Err(Fail::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "google_off",
            "Google sign-in is off",
            "Google sign-in isn't set up on this relay yet. Use your email instead.",
            &again,
        ));
    };
    if st.accounts.take("google_ip", ip, 30, 60 * MINUTE).is_err() {
        return Err(Fail::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many tries",
            "Wait a few minutes and try again.",
            &again,
        ));
    }
    let state = crate::random(24);
    let nonce = crate::random(24);
    let verifier = crate::random(48);
    let challenge =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, Sha256::digest(&verifier));
    let now = now_ms();
    st.db
        .account_put_oauth(&sha(&state), &nonce, &verifier, &browser_hash(c), flow, next, now, now + OAUTH_TTL_MS)
        .map_err(Fail::db)?;
    let mut url = url::Url::parse(&g.auth_url).map_err(|_| {
        Fail::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "google_off",
            "Google sign-in is off",
            "Google's address is wrong on this relay.",
            &again,
        )
    })?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &callback_url(st))
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "select_account");
    Ok(url.into())
}

/// How a sign-in ends.
pub(super) enum Finished {
    /// The app's loopback address, with its one-time code.
    App(String),
    /// A web session: its cookie, how long it lasts (s), and the dashboard path to open.
    Web { token: String, age: i64, next: String },
}

/// The person proved `email`: their account (made if new), then the app's one-time code, or
/// a web session.
pub(super) async fn finish_core(
    st: &AppState,
    headers: &HeaderMap,
    email: &str,
    google: Option<&GoogleId>,
    flow: Option<&str>,
    next: &str,
) -> Result<Finished, Fail> {
    let now = now_ms();
    let (account, made) = st.db.account_sign_in(email, google, now).map_err(Fail::db)?;
    if account.disabled_at.is_some() {
        return Err(Fail::new(
            StatusCode::FORBIDDEN,
            "account_disabled",
            "This account is turned off",
            "Write to support@clearcraftlabs.com if you think that's a mistake. The Ferrocast app still works without an account.",
            "/login",
        ));
    }
    if made {
        tracing::info!(account = %account.id, "account created");
    }
    attach_by_email(st, &account.id, email);
    if let Some(fid) = flow {
        let expired = || {
            Fail::new(
                StatusCode::GONE,
                "flow_expired",
                "This sign-in expired",
                "Press Sign in again in Ferrocast.",
                "/login",
            )
        };
        let f = st.db.account_flow(fid).map_err(Fail::db)?.ok_or_else(expired)?;
        let code = crate::random(32);
        if !st.db.account_flow_code(&f.id, &sha(&code), &account.id, now, now + APP_CODE_TTL_MS).map_err(Fail::db)? {
            return Err(expired());
        }
        let mut u = url::Url::parse(&f.redirect_uri).map_err(|_| expired())?;
        u.query_pairs_mut().append_pair("code", &code).append_pair("state", &f.state);
        return Ok(Finished::App(u.into()));
    }
    let (token, s) = new_session(st, &account, "web", &ua_label(headers), None)
        .map_err(|e| Fail::new(e.status(), "session_failed", "Try again", e.message().to_string(), "/login"))?;
    Ok(Finished::Web { token, age: (s.expires_at - now) / 1000, next: safe_next(Some(next)) })
}

/// The sign-in cookie for a web session, and the login cookie cleared.
pub(super) fn session_cookies(r: &mut Response, token: &str, age: i64) {
    r.headers_mut().append(header::SET_COOKIE, set_cookie(SESSION_COOKIE, token, age));
    r.headers_mut().append(header::SET_COOKIE, set_cookie(LOGIN_COOKIE, "", 0));
}

/// The rate-limit key of the request's address.
pub(super) fn ip_of(peer: std::net::SocketAddr, headers: &HeaderMap) -> String {
    crate::remote::limits::ip_key(crate::client_ip(peer, headers))
}

// ── The pages ────────────────────────────────────────────────────────────────────────

async fn email_start(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(f) = form(&headers, &body) else {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Start again", "That form couldn't be read.", "/login");
    };
    let c = match post_guard(&st, &headers, &f) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let next = safe_next(f.get("next").map(String::as_str));
    let flow = match live_flow(&st, f.get("flow").map(String::as_str)) {
        Ok(fl) => fl.map(|fl| fl.id),
        Err(e) => return e.page(),
    };
    let again = back_to(flow.as_deref(), &next);
    let email = f.get("email").map(|e| e.trim().to_lowercase()).unwrap_or_default();
    let id = match send_magic(&st, &ip_of(peer, &headers), &c, &email, flow.as_deref(), &next).await {
        Ok(id) => id,
        Err(e) => return e.page(),
    };
    let resend = format!(
        "<form method=post action=/login/email>{}{}{}{}<button class=btn type=submit>Resend link</button></form>",
        hidden("csrf", &form_token(&c)),
        hidden("flow", flow.as_deref().unwrap_or("")),
        hidden("next", &next),
        hidden("email", &email)
    );
    let body = format!(
        "<p class=lead>We sent a sign-in link to <strong>{}</strong>. Open it in this browser, or type the code \
         from the email here.</p>{}<div class=row>{resend}<a class=\"btn ghost\" href=\"{}\">Use another email</a></div>\
         <p class=muted>Nothing came? Check spam, then resend.</p>",
        esc(&email),
        code_form(&form_token(&c), &id, None),
        esc(&again)
    );
    page_with(StatusCode::OK, "Check your email", Some(ICON_MAIL), &body)
}

async fn email_code(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let Some(f) = form(&headers, &body) else {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Start again", "That form couldn't be read.", "/login");
    };
    let c = match post_guard(&st, &headers, &f) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let id = f.get("m").map(String::as_str).unwrap_or("");
    match check_code(&st, &c, id, f.get("code").map(String::as_str).unwrap_or("")) {
        Ok(m) => finish(&st, &headers, &m.email, None, m.flow_id.as_deref(), &m.next).await,
        Err(e) if e.code == "wrong_code" => {
            page(StatusCode::UNPROCESSABLE_ENTITY, "Check your email", &code_form(&form_token(&c), id, Some(&e.text)))
        }
        Err(e) => e.page(),
    }
}

async fn link_page(State(st): State<AppState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    if !available(&st) || !crate::remote::on_web_host(&st, &headers) {
        return not_here();
    }
    let q = query(raw.as_deref());
    let t = q.get("t").map(String::as_str).unwrap_or("");
    let m = match st.db.account_magic_by_token(&sha(t)) {
        Ok(m) => m,
        Err(err) => return ApiError::db(err).into_response(),
    };
    let Some(m) = m.filter(|m| m.used_at.is_none() && m.expires_at > now_ms() && !t.is_empty()) else {
        return problem(
            StatusCode::GONE,
            "This link expired",
            "Sign-in links work once, for 15 minutes. Ask for a new one.",
            "/login",
        );
    };
    let c = cookie(&headers, LOGIN_COOKIE);
    if !c.as_deref().is_some_and(|c| ct_eq(&m.browser_hash, &browser_hash(c))) {
        return page(
            StatusCode::FORBIDDEN,
            "Open it where you asked",
            "<p class=lead>For your safety this link only works in the browser where you asked for it.</p>\
             <p>Open it there, or type the code from the email on that sign-in page.</p>",
        );
    }
    // A GET never uses the link up (mail scanners open links); the button does.
    let body = format!(
        "<p class=lead>Sign in to Ferrocast as <strong>{}</strong>?</p><form method=post action=/login/link>{}{}\
         <button class=\"btn primary\" type=submit>Sign in</button></form><p class=muted>Didn't ask for this? Close this tab.</p>",
        esc(&m.email),
        hidden("csrf", &form_token(c.as_deref().unwrap_or(""))),
        hidden("t", t)
    );
    let mut r = page(StatusCode::OK, "Sign in", &body);
    r.headers_mut().insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    r
}

async fn link_use(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let Some(f) = form(&headers, &body) else {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Start again", "That form couldn't be read.", "/login");
    };
    let c = match post_guard(&st, &headers, &f) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match take_link(&st, &c, f.get("t").map(String::as_str).unwrap_or("")) {
        Ok(m) => finish(&st, &headers, &m.email, None, m.flow_id.as_deref(), &m.next).await,
        // The page keeps its old wording for a link opened in another browser.
        Err(e) if e.code == "other_browser" => Fail::new(
            StatusCode::GONE,
            "link_expired",
            "This link expired",
            "Sign-in links work once, for 15 minutes, in the browser that asked. Ask for a new one.",
            "/login",
        )
        .page(),
        Err(e) => e.page(),
    }
}

// ── Google ───────────────────────────────────────────────────────────────────────────

async fn google_start(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(f) = form(&headers, &body) else {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Start again", "That form couldn't be read.", "/login");
    };
    let c = match post_guard(&st, &headers, &f) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let next = safe_next(f.get("next").map(String::as_str));
    let flow = match live_flow(&st, f.get("flow").map(String::as_str)) {
        Ok(fl) => fl.map(|fl| fl.id),
        Err(e) => return e.page(),
    };
    match google_url(&st, &ip_of(peer, &headers), &c, flow.as_deref(), &next) {
        Ok(u) => redirect(&u),
        Err(e) => e.page(),
    }
}

pub(crate) fn callback_url(st: &AppState) -> String {
    format!("{}/login/google/callback", web_origin(st))
}

/// Google's answer comes back to this host (it holds the login cookie). Started from the
/// website, a problem goes back to the website's sign-in page with the reason.
async fn google_callback(State(st): State<AppState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    if !available(&st) || !crate::remote::on_web_host(&st, &headers) {
        return not_here();
    }
    let q = query(raw.as_deref());
    let show = |f: Fail| -> Response {
        if st.remote.cfg.web.is_some() {
            let to = format!(
                "{}/login/?error={}",
                page_origin(&st),
                url::form_urlencoded::byte_serialize(f.text.as_bytes()).collect::<String>()
            );
            redirect(&to)
        } else {
            f.page()
        }
    };
    let expired = || {
        Fail::new(
            StatusCode::BAD_REQUEST,
            "google_expired",
            "Start again",
            "This Google sign-in expired, was already used, or was started in another browser.",
            "/login",
        )
    };
    let Some(state) = q.get("state").filter(|s| !s.is_empty() && s.len() <= 128) else { return show(expired()) };
    let now = now_ms();
    let o = match st.db.account_take_oauth(&sha(state), now) {
        Ok(Some(o)) => o,
        Ok(None) => return show(expired()),
        Err(err) => return show(Fail::db(err)),
    };
    let c = cookie(&headers, LOGIN_COOKIE);
    if !c.as_deref().is_some_and(|c| ct_eq(&o.browser_hash, &browser_hash(c))) {
        return show(expired());
    }
    let again = back_to(o.flow_id.as_deref(), &o.next);
    if q.contains_key("error") {
        // Cancelled at Google: the app hears it on its loopback, the web goes back to /login.
        if let Some(fid) = &o.flow_id
            && let Ok(Some(f)) = st.db.account_flow(fid)
            && let Ok(mut u) = url::Url::parse(&f.redirect_uri)
        {
            u.query_pairs_mut().append_pair("error", "access_denied").append_pair("state", &f.state);
            return redirect(u.as_str());
        }
        return show(Fail::new(
            StatusCode::OK,
            "cancelled",
            "Sign-in cancelled",
            "Google sign-in was cancelled.",
            &again,
        ));
    }
    let g = &st.accounts.cfg.google;
    let (Some(id), Some(secret)) = (g.client_id.as_deref(), g.client_secret.as_deref()) else {
        return show(Fail::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "google_off",
            "Google sign-in is off",
            "Google sign-in isn't set up on this relay yet. Use your email instead.",
            &again,
        ));
    };
    let Some(code) = q.get("code").filter(|c| !c.is_empty() && c.len() <= 2048) else { return show(expired()) };
    let cb = callback_url(&st);
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", id),
        ("client_secret", secret),
        ("redirect_uri", cb.as_str()),
        ("code_verifier", o.verifier.as_str()),
    ];
    let unavailable = |why: &str| {
        tracing::warn!(why, "Google sign-in failed");
        Fail::new(
            StatusCode::BAD_GATEWAY,
            "google_failed",
            "Google didn't answer",
            "Google didn't accept the sign-in just now. Try again.",
            &again,
        )
    };
    let tokens: serde_json::Value = match st.http.post(&g.token_url).form(&form).send().await {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(v) => v,
            Err(_) => return show(unavailable("unreadable token answer")),
        },
        Ok(r) => return show(unavailable(&format!("token endpoint HTTP {}", r.status().as_u16()))),
        Err(_) => return show(unavailable("token endpoint unreachable")),
    };
    let Some(id_token) = tokens["id_token"].as_str() else { return show(unavailable("no id_token")) };
    let claims = match google::verify(&st.accounts.keys, &st.http, g, id_token, &o.nonce, now).await {
        Ok(c) => c,
        Err(google::TokenError::Unverified) => {
            return show(Fail::new(
                StatusCode::FORBIDDEN,
                "email_unverified",
                "Email not verified",
                "Google says this account's email isn't verified. Verify it with Google, or sign in by email.",
                &again,
            ));
        }
        Err(google::TokenError::Keys(why)) => return show(unavailable(&why)),
        Err(err) => {
            tracing::warn!(error = %err, "refused a Google id_token");
            return show(Fail::new(
                StatusCode::UNAUTHORIZED,
                "google_refused",
                "Sign-in refused",
                "Google's answer didn't check out. Try again.",
                &again,
            ));
        }
    };
    let gid = GoogleId { subject: claims.sub, name: claims.name, picture: claims.picture };
    match finish_core(&st, &headers, &claims.email, Some(&gid), o.flow_id.as_deref(), &o.next).await {
        Ok(done) => finished(&st, done),
        Err(e) => show(e),
    }
}

// ── Finishing ────────────────────────────────────────────────────────────────────────

/// A finished sign-in as a redirect: the app's loopback, or the dashboard with the session
/// cookie (on the website when there is one).
fn finished(st: &AppState, done: Finished) -> Response {
    match done {
        Finished::App(u) => redirect(&u),
        Finished::Web { token, age, next } => {
            let to = if st.remote.cfg.web.is_some() { format!("{}{next}", page_origin(st)) } else { next };
            let mut r = redirect(&to);
            session_cookies(&mut r, &token, age);
            r
        }
    }
}

async fn finish(
    st: &AppState,
    headers: &HeaderMap,
    email: &str,
    google: Option<&GoogleId>,
    flow: Option<&str>,
    next: &str,
) -> Response {
    match finish_core(st, headers, email, google, flow, next).await {
        Ok(done) => finished(st, done),
        Err(e) => e.page(),
    }
}

/// `POST /logout` (form with `csrf`, the web session's CSRF token).
async fn logout_form(State(st): State<AppState>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if !available(&st) || !crate::remote::on_web_host(&st, &headers) {
        return not_here();
    }
    let f = form(&headers, &body).unwrap_or_default();
    if let Ok(a) = web_session(&st, &headers) {
        let given = f.get("csrf").map(String::as_str).unwrap_or("");
        if !same_origin(&st, &headers) || !ct_eq(given, &csrf_token(&a.secret)) {
            return problem(StatusCode::FORBIDDEN, "Start again", "This form is out of date.", "/app/");
        }
        let _ = st.db.account_revoke(&a.account.id, &a.session.id, "user", now_ms());
    }
    let mut r = redirect("/login");
    r.headers_mut().append(header::SET_COOKIE, set_cookie(SESSION_COOKIE, "", 0));
    r
}

/// The sign-in email's HTML: the button, the code in a box, and when it stops working.
pub(crate) fn sign_in_html(link: &str, shown: &str) -> String {
    use crate::mail::layout as l;
    let body = format!(
        "{}{}{}{}{}",
        l::title("Sign in to Ferrocast"),
        l::para("Press the button to finish signing in. It works in the browser where you asked for it."),
        l::button(link, "Sign in"),
        l::code_box("Or type this code on the sign-in page", shown),
        l::note(
            "The link and the code work once, for 15 minutes. Didn't ask to sign in? Ignore this email. Nobody can use it without your browser."
        ),
    );
    l::page(
        "Your Ferrocast sign-in link",
        &format!("Your code is {shown}. It works once, for 15 minutes."),
        &body,
        &l::footer("Ferrocast by ClearCraft Labs"),
    )
}
