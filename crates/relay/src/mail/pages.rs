//! The weekly email's public pages: confirm and unsubscribe. Plain HTML, no scripts, nothing
//! from other sites, every value escaped, `no-store`, `no-referrer`, and a CSP that allows
//! only inline styles and forms posting back here.
//!
//! A GET never changes anything: mail scanners and link previews fetch links in emails, so
//! the GET page shows a button and only its POST confirms or unsubscribes. The unsubscribe
//! POST is also the RFC 8058 one-click target (`List-Unsubscribe-Post`), which needs no
//! cookie, no session and no particular body.

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::report::esc;
use super::store::Confirmed;
use super::{ConfirmLink, read_confirm, read_unsubscribe};
use crate::{AppState, now_ms};

#[derive(Debug, Deserialize)]
pub(crate) struct TokenQuery {
    #[serde(default)]
    t: String,
}

pub(crate) const CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// `body` is already-escaped HTML.
fn shell(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=robots content=noindex><title>{t} · Ferrocast</title>\
         <style>:root{{color-scheme:dark light;--bg:#0a0b0d;--card:#101216;--line:#252a31;--text:#eef0f4;--muted:#b3b9c4;--accent:#ffa424;--on:#120a00;--ring:#33d4ff}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#f2f3f6;--card:#fff;--line:#dfe2e8;--text:#0e1014;--muted:#424854;--accent:#cf7400;--on:#0a0600;--ring:#05678c}}}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:16px;box-sizing:border-box;background:var(--bg);color:var(--text);font:17px/1.5 system-ui,sans-serif}}\
         main{{width:100%;max-width:460px;background:var(--card);border:1px solid var(--line);border-radius:16px;padding:28px;box-sizing:border-box}}\
         h1{{font-size:1.6rem;line-height:1.2;margin:0 0 12px;letter-spacing:-.02em;overflow-wrap:anywhere}}p{{margin:0 0 12px}}.muted{{color:var(--muted);font-size:.94rem}}\
         form{{margin:20px 0 8px}}button{{min-height:48px;padding:0 24px;border:0;border-radius:10px;background:var(--accent);color:var(--on);font:700 1rem system-ui,sans-serif;cursor:pointer}}\
         button:focus-visible{{outline:2px solid var(--ring);outline-offset:3px}}</style>\
         <main><h1>{t}</h1>{body}</main></html>",
        t = esc(title),
    );
    let mut r = (status, Html(html)).into_response();
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    r
}

fn message(status: StatusCode, title: &str, text: &str) -> Response {
    shell(status, title, &format!("<p>{}</p>", esc(text)))
}

/// A page with one button that POSTs back to `action` (an already-built relative path).
fn with_button(title: &str, text: &str, action: &str, button: &str, note: &str) -> Response {
    shell(
        StatusCode::OK,
        title,
        &format!(
            "<p>{}</p><form method=post action=\"{}\"><button type=submit>{}</button></form><p class=muted>{}</p>",
            esc(text),
            esc(action),
            esc(button),
            esc(note)
        ),
    )
}

fn bad_link() -> Response {
    message(
        StatusCode::NOT_FOUND,
        "This link doesn't work",
        "It may be cut short, already used, or replaced by a newer one. In Ferrocast, open History and press Resend.",
    )
}

fn expired() -> Response {
    message(
        StatusCode::GONE,
        "This link expired",
        "Confirm links work for 24 hours. In Ferrocast, open History and press Resend for a new one.",
    )
}

fn unavailable() -> Response {
    message(
        StatusCode::SERVICE_UNAVAILABLE,
        "Not available right now",
        "Weekly emails aren't switched on here yet. Try again later.",
    )
}

fn storage() -> Response {
    message(
        StatusCode::SERVICE_UNAVAILABLE,
        "Try again in a moment",
        "Ferrocast couldn't save that just now. Reload this page to try again.",
    )
}

/// The confirm link's subscription and nonce hash, if it's a pending one.
fn pending_link(st: &AppState, t: &str) -> Result<(String, String), Box<Response>> {
    let now = now_ms();
    match read_confirm(&st.mail.cfg, t, now) {
        ConfirmLink::Bad => Err(Box::new(bad_link())),
        ConfirmLink::Expired => Err(Box::new(expired())),
        ConfirmLink::Good { sub, hash } => Ok((sub, hash)),
    }
}

fn day_line(sub: &str) -> String {
    format!("Your report arrives every {} morning (09:30 in India, 04:00 UTC).", super::day_name(sub))
}

/// `GET /mail/confirm?t=…`: explains, and a button that POSTs the confirm.
pub(crate) async fn confirm_page(State(st): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    if !super::available(&st) {
        return unavailable();
    }
    let (sub, hash) = match pending_link(&st, &q.t) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    match st.db.mail_row(&sub) {
        Ok(Some(row)) if row.confirm_hash.as_deref() == Some(hash.as_str()) => {
            if row.pending.is_none() {
                if row.on().is_some() {
                    return message(StatusCode::OK, "You're all set", &day_line(&sub));
                }
                return bad_link();
            }
        }
        Ok(_) => return bad_link(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read a mail address");
            return storage();
        }
    }
    with_button(
        "Confirm your weekly email",
        "Press Confirm to get a short report of your streams once a week.",
        &format!("/mail/confirm?t={}", url::form_urlencoded::byte_serialize(q.t.trim().as_bytes()).collect::<String>()),
        "Confirm",
        "Didn't ask for this? Close this page: nothing is sent without the confirm.",
    )
}

/// `POST /mail/confirm?t=…`: turns the address on (single use, the subscription must have Pro).
pub(crate) async fn confirm_post(State(st): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    if !super::available(&st) {
        return unavailable();
    }
    let (sub, hash) = match pending_link(&st, &q.t) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let now = now_ms();
    let pro = st.db.pro_sub(&sub).ok().flatten().is_some_and(|s| crate::billing::access::is_pro(&s.access(), now));
    if !pro {
        return message(
            StatusCode::PAYMENT_REQUIRED,
            "Pro isn't active",
            "Weekly emails are part of Ferrocast Pro. See Settings › Pro in Ferrocast, then press Resend in History.",
        );
    }
    match st.db.mail_confirm(&sub, &hash, now) {
        Ok(Confirmed::On) => message(StatusCode::OK, "You're in", &day_line(&sub)),
        Ok(Confirmed::Already) => message(StatusCode::OK, "You're all set", &day_line(&sub)),
        Ok(Confirmed::Expired) => expired(),
        Ok(Confirmed::Unknown) => bad_link(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't confirm a mail address");
            storage()
        }
    }
}

/// What an unsubscribe link whose address isn't saved any more says (200: for RFC 8058, a
/// one-click POST that finds nothing is still done).
fn out_of_date() -> Response {
    message(
        StatusCode::OK,
        "This link is out of date",
        "This link is out of date — use the newest email or stop emails in the dashboard.",
    )
}

/// `GET /mail/unsubscribe?t=…`: a button that POSTs the unsubscribe.
pub(crate) async fn unsubscribe_page(State(st): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    let Some((sub, tag)) = read_unsubscribe(&st.mail.cfg, &q.t) else { return bad_link() };
    let mine = |a: &Option<super::store::Addr>| a.as_ref().is_some_and(|a| a.tag == tag);
    match st.db.mail_row(&sub) {
        Ok(Some(row)) if mine(&row.active) || mine(&row.pending) => with_button(
            "Stop weekly emails?",
            "You won't get the weekly stream report any more. Your stream history stays in Ferrocast.",
            &format!(
                "/mail/unsubscribe?t={}",
                url::form_urlencoded::byte_serialize(q.t.trim().as_bytes()).collect::<String>()
            ),
            "Stop emails",
            "You can turn them on again in Ferrocast › History.",
        ),
        Ok(_) => out_of_date(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read a mail address");
            storage()
        }
    }
}

/// `POST /mail/unsubscribe?t=…`: RFC 8058 one-click, and the page's button. The address is
/// deleted, not just switched off, and only the address the link was sent to (matched by its
/// tag): saving the same address again keeps old links working, another address doesn't.
/// A link that matches nothing still answers 200 (RFC 8058), saying it's out of date.
pub(crate) async fn unsubscribe_post(State(st): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    let Some((sub, tag)) = read_unsubscribe(&st.mail.cfg, &q.t) else { return bad_link() };
    match st.db.mail_unsubscribe(&sub, &tag) {
        Ok(false) => out_of_date(),
        Ok(true) => message(
            StatusCode::OK,
            "You're unsubscribed",
            "No more weekly emails go to this address. You can turn them on again in Ferrocast › History.",
        ),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't unsubscribe a mail address");
            storage()
        }
    }
}
