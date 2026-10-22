//! The weekly email: the pure parts (sender, hints, the schedule, links, the report), Brevo's
//! client against a fake Brevo, then every route through the real `router()` with
//! `FakeMailer` (each status), the confirm and unsubscribe pages, and the weekly job on an
//! explicit clock (`run_weekly(st, now)`: the relay's clock is the wall clock, so the tests
//! move "now" by hand instead of pausing tokio's timer).

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use base64::Engine as _;
use serde_json::{Value, json};

use super::*;
use crate::billing::store::Synced;
use crate::history::{Money, StreamIn};
use crate::vault::Vault;
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;
const ADDRESS: &str = "Asha.Streams@Gmail.com";
const STORED: &str = "Asha.Streams@gmail.com";

fn cfg() -> MailConfig {
    MailConfig::new(None, Some(b"a test key that is long enough"))
}

// ── Unit ─────────────────────────────────────────────────────────────────────────────

#[test]
fn the_sender_is_read_from_mail_from() {
    assert_eq!(
        Sender::parse(DEFAULT_FROM),
        Some(Sender { name: "Ferrocast".into(), email: "reports@mail.clearcraftlabs.com".into() })
    );
    assert_eq!(Sender::parse("reports@x.com").unwrap().name, "Ferrocast");
    assert_eq!(Sender::parse("\"Ferro Reports\" <a@b.co>").unwrap().name, "Ferro Reports");
    assert_eq!(Sender::parse("nope"), None);
    assert_eq!(MailConfig::new(Some("broken"), None).from.email, "reports@mail.clearcraftlabs.com");
    let shown = format!("{:?}", cfg());
    assert!(shown.contains("redacted") && !shown.contains("long enough"), "{shown}");
}

#[test]
fn addresses_are_checked_and_hinted() {
    assert_eq!(normalize("  Asha.Streams@Gmail.COM "), Some("Asha.Streams@gmail.com".into()));
    for bad in ["", "asha", "asha@", "@gmail.com", "a b@c.com", "a@b", "a@@b.com"] {
        assert_eq!(normalize(bad), None, "{bad:?}");
    }
    assert_eq!(address_hint("asha@gmail.com"), "a•••@gmail.com");
    assert_eq!(address_hint("x@y.co"), "x•••@y.co");
}

#[test]
fn each_subscription_gets_a_stable_day_and_they_spread_over_the_week() {
    assert_eq!(send_day("sub_ABC"), send_day("sub_ABC"));
    let mut days = [0usize; 7];
    for i in 0..7000 {
        let d = send_day(&format!("sub_{i:08}"));
        assert!((0..7).contains(&d));
        days[usize::try_from(d).unwrap()] += 1;
    }
    assert!(days.iter().all(|n| (850..1150).contains(n)), "spread evenly: {days:?}");
    // The moment is on that day at the send hour, inside the week of `now`.
    let now = 1_792_411_200_000; // Mon 19 Oct 2026 12:00 UTC
    let due = due_at("sub_ABC", now);
    assert_eq!(due, crate::history::week_start(now) + send_day("sub_ABC") * DAY + SEND_HOUR * 3_600_000);
    assert_eq!(due_at("sub_ABC", now + 3 * DAY), due, "the same week, the same moment");
    assert_eq!(due_at("sub_ABC", now + 7 * DAY), due + 7 * DAY);
}

#[test]
fn links_only_open_signed_unexpired_and_of_their_kind() {
    let c = cfg();
    let now = 1_000_000;
    let (t, hash) = c.confirm_token("sub_1", now + DAY).unwrap();
    assert_eq!(read_confirm(&c, &t, now), ConfirmLink::Good { sub: "sub_1".into(), hash: hash.clone() });
    assert_eq!(read_confirm(&c, &t, now + DAY + 1), ConfirmLink::Expired);
    // Any change breaks the signature: the subscription, the expiry, the nonce, the mac.
    let parts: Vec<&str> = t.split('.').collect();
    let forged = format!("c1.sub_2.{}.{}.{}", parts[2], parts[3], parts[4]);
    assert_eq!(read_confirm(&c, &forged, now), ConfirmLink::Bad);
    let longer = format!("c1.sub_1.{}.{}.{}", now + 99 * DAY, parts[3], parts[4]);
    assert_eq!(read_confirm(&c, &longer, now), ConfirmLink::Bad);
    let mut flipped = t.clone().into_bytes();
    let last = flipped.len() - 1;
    flipped[last] = if flipped[last] == b'0' { b'1' } else { b'0' };
    assert_eq!(read_confirm(&c, &String::from_utf8(flipped).unwrap(), now), ConfirmLink::Bad);
    assert_eq!(read_confirm(&c, "", now), ConfirmLink::Bad);
    assert_eq!(read_confirm(&c, &"x".repeat(400), now), ConfirmLink::Bad);
    // Another relay's key, or an unsubscribe link used as a confirm one, never opens.
    let other = MailConfig::new(None, Some(b"another key, also long enough"));
    assert_eq!(read_confirm(&other, &t, now), ConfirmLink::Bad);
    let tag = c.addr_tag("asha@gmail.com").unwrap();
    assert_eq!(tag, c.addr_tag(" Asha@Gmail.COM ").unwrap(), "case and spaces don't change the tag");
    assert_ne!(tag, c.addr_tag("other@gmail.com").unwrap());
    assert_ne!(tag, other.addr_tag("asha@gmail.com").unwrap(), "keyed: another relay's tag differs");
    let u = c.unsubscribe_token("sub_1", &tag).unwrap();
    assert_eq!(read_unsubscribe(&c, &u), Some(("sub_1".into(), tag.clone())));
    assert_eq!(read_unsubscribe(&c, &c.unsubscribe_token("sub_1", "42").unwrap()), None, "not a tag");
    assert_eq!(read_confirm(&c, &u, now), ConfirmLink::Bad);
    assert_eq!(read_unsubscribe(&c, &t), None);
    assert_eq!(read_unsubscribe(&other, &u), None);
    // Without a key there are no links at all.
    assert!(MailConfig::default().confirm_token("sub_1", now).is_none());
}

#[test]
fn an_email_never_shows_its_address() {
    let e = Email {
        to: "secret@x.com".into(),
        subject: "s".into(),
        html: "h".into(),
        text: "t".into(),
        headers: vec![],
        tag: "weekly",
    };
    let shown = format!("{e:?}");
    assert!(!shown.contains("secret@x.com") && shown.contains("redacted"), "{shown}");
    let b = BrevoMailer::new("xkeysib-secret".into(), "http://127.0.0.1:1/x".into());
    assert!(!format!("{b:?}").contains("xkeysib-secret"));
}

fn stream(platform: &str, start: i64, minutes: i64, title: &str, avg: i64, peak: i64) -> crate::history::StreamOut {
    crate::history::StreamOut {
        platform: platform.into(),
        key: format!("{platform}:{start}"),
        started_at: start,
        ended_at: Some(start + minutes * 60_000),
        title: Some(title.into()),
        category: None,
        peak_viewers: peak,
        avg_viewers: avg,
        chat_lines: 0,
        chatters: 0,
        new_chatters: 0,
        follows: 3,
        subs: 1,
        tips: vec![Money { currency: "INR".into(), minor: 25_000 }],
        platform_value: vec![],
        updated_at: start,
    }
}

#[test]
fn the_report_adds_up_the_week_and_compares_it() {
    let from = 1_792_281_600_000; // 18 Oct 2026
    let to = from + 7 * DAY;
    let week =
        [stream("twitch", from + DAY, 120, "Ranked", 40, 61), stream("kick", from + 2 * DAY, 60, "Chill", 10, 14)];
    let before = [stream("twitch", from - 3 * DAY, 90, "Old", 20, 30)];
    let r = report::build(&week, &before, from, to).unwrap();
    assert_eq!(r.week.streams, 2);
    assert_eq!(r.week.minutes, 180);
    assert_eq!(r.week.peak_viewers, 61);
    assert_eq!(r.week.avg_viewers, 30, "weighted by length: (40*120 + 10*60) / 180");
    assert_eq!(r.week.follows, 6);
    assert_eq!(r.week.tips.get("INR"), Some(&50_000));
    assert_eq!(r.best.as_ref().unwrap().title.as_deref(), Some("Ranked"));
    assert_eq!(r.previous.as_ref().unwrap().minutes, 90);
    let out = report::render(&r, "https://relay.example/mail/unsubscribe?t=u1.x&y");
    assert_eq!(out.subject, "Your week on stream: 2 streams, 3 hours");
    assert!(out.text.contains("Hours live: 3 (up 100% on last week)"), "{}", out.text);
    assert!(out.text.contains("Average viewers: 30 (up 50% on last week)"), "{}", out.text);
    assert!(out.text.contains("Tips: INR 500.00"), "{}", out.text);
    assert!(out.text.contains("Best stream: Ranked on Twitch"), "{}", out.text);
    assert!(out.text.contains("Stop these emails: https://relay.example/mail/unsubscribe?t=u1.x&y"));
    assert!(out.html.contains("18-24 Oct 2026"), "the dates");
    assert!(out.html.contains("href=\"https://relay.example/mail/unsubscribe?t=u1.x&amp;y\""), "the link is escaped");
    assert!(out.html.contains("lang=\"en\"") && out.html.contains("role=\"presentation\""));
    assert!(!out.html.contains("<style") && !out.html.contains("<script"), "inline styles only");
    // No week before: said once, no percentages.
    let r = report::build(&week, &[], from, to).unwrap();
    let out = report::render(&r, "u");
    assert!(out.text.contains("nothing to compare yet") && !out.text.contains('%'), "{}", out.text);
    // No streams: no report.
    assert!(report::build(&[], &before, from, to).is_none());
}

#[test]
fn stream_titles_are_escaped_in_the_report() {
    let from = 1_792_281_600_000;
    let evil = "<script>alert(1)</script> \"Tom & Jerry's\" <img src=x onerror=y>";
    let week = [stream("twitch", from + DAY, 60, evil, 5, 9)];
    let out = report::render(&report::build(&week, &[], from, from + 7 * DAY).unwrap(), "https://r/u");
    assert!(!out.html.contains("<script>") && !out.html.contains("<img src=x"), "{}", out.html);
    assert!(out.html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &quot;Tom &amp; Jerry&#39;s&quot;"));
    assert!(out.text.contains(evil), "plain text keeps the title as it is");
    let c = report::confirm("https://r/mail/confirm?t=a\"b<c");
    assert!(c.html.contains("href=\"https://r/mail/confirm?t=a&quot;b&lt;c\""));
}

#[test]
fn numbers_read_well() {
    assert_eq!(report::count(1_234_567), "1,234,567");
    assert_eq!(report::count(999), "999");
    assert_eq!(report::hours(87), "1.5");
    assert_eq!(report::hours(120), "2");
    assert_eq!(report::money("INR", 125_050), "INR 1,250.50");
    assert_eq!(report::money("JPY", 1500), "JPY 1,500");
    assert_eq!(report::delta(10, 10).as_deref(), Some("same as last week"));
    assert_eq!(report::delta(5, 10).as_deref(), Some("down 50% on last week"));
    assert_eq!(report::delta(3, 0).as_deref(), Some("none the week before"));
    assert_eq!(report::delta(0, 0), None);
    assert_eq!(report::span(1_792_281_600_000, 1_792_281_600_000 + 7 * DAY), "18-24 Oct 2026");
    assert_eq!(report::span(1_793_404_800_000, 1_793_404_800_000 + 7 * DAY), "31 Oct 2026 to 6 Nov 2026");
}

// ── Brevo's client against a fake Brevo ──────────────────────────────────────────────

#[derive(Debug, Default)]
struct FakeBrevo {
    answer: Option<(u16, Value)>,
    seen: Vec<(Option<String>, String, Value)>,
}

type Shared = Arc<std::sync::Mutex<FakeBrevo>>;

async fn fake_brevo() -> (String, Shared) {
    let shared: Shared = Arc::default();
    async fn send(
        State(s): State<Shared>,
        headers: HeaderMap,
        uri: axum::http::Uri,
        axum::Json(body): axum::Json<Value>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        let mut f = s.lock().unwrap();
        let key = headers.get("api-key").and_then(|v| v.to_str().ok()).map(String::from);
        f.seen.push((key, uri.to_string(), body));
        let (status, v) = f.answer.clone().unwrap_or((201, json!({ "messageId": "<1@smtp-relay.mailin.fr>" })));
        (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(v))
    }
    let app = axum::Router::new().route("/v3/smtp/email", axum::routing::post(send)).with_state(shared.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v3/smtp/email", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (url, shared)
}

fn sample_email() -> Email {
    Email {
        to: "asha@gmail.com".into(),
        subject: "Your week".into(),
        html: "<p>hi</p>".into(),
        text: "hi".into(),
        headers: vec![
            ("List-Unsubscribe".into(), "<https://r/mail/unsubscribe?t=u1.s.1.m>".into()),
            ("List-Unsubscribe-Post".into(), "List-Unsubscribe=One-Click".into()),
        ],
        tag: "weekly",
    }
}

#[tokio::test]
async fn brevo_gets_the_key_in_a_header_and_the_whole_email() {
    let (url, b) = fake_brevo().await;
    let m = BrevoMailer::new("xkeysib-test".into(), url);
    let from = Sender::parse(DEFAULT_FROM).unwrap();
    m.send(&from, &sample_email()).await.unwrap();
    let (key, uri, body) = b.lock().unwrap().seen[0].clone();
    assert_eq!(key.as_deref(), Some("xkeysib-test"));
    assert_eq!(uri, "/v3/smtp/email", "nothing in the query");
    assert_eq!(
        body,
        json!({
            "sender": { "name": "Ferrocast", "email": "reports@mail.clearcraftlabs.com" },
            "to": [{ "email": "asha@gmail.com" }],
            "subject": "Your week",
            "htmlContent": "<p>hi</p>",
            "textContent": "hi",
            "headers": {
                "List-Unsubscribe": "<https://r/mail/unsubscribe?t=u1.s.1.m>",
                "List-Unsubscribe-Post": "List-Unsubscribe=One-Click"
            },
            "tags": ["weekly"],
        })
    );
}

/// Finding (live, 2026-10-19): Brevo refused every confirm email with "headers is blank",
/// because an email without extra headers still sent `"headers": {}`.
#[tokio::test]
async fn an_email_without_headers_sends_no_headers_field() {
    let (url, b) = fake_brevo().await;
    let m = BrevoMailer::new("k".into(), url);
    let from = Sender::parse(DEFAULT_FROM).unwrap();
    m.send(&from, &Email { headers: vec![], tag: "confirm", ..sample_email() }).await.unwrap();
    let (_, _, body) = b.lock().unwrap().seen[0].clone();
    assert!(body.get("headers").is_none(), "{body}");
}

#[tokio::test]
async fn brevos_errors_map_to_what_the_relay_can_do_and_never_carry_the_address() {
    let (url, b) = fake_brevo().await;
    let m = BrevoMailer::new("k".into(), url);
    let from = Sender::parse(DEFAULT_FROM).unwrap();
    for (status, code, message, want) in [
        (400, "invalid_parameter", "email asha@gmail.com is not valid in to", "rejected"),
        (400, "invalid_parameter", "sender asha@gmail.com is not valid", "misconfigured"),
        (401, "unauthorized", "Key not found", "misconfigured"),
        (402, "not_enough_credits", "asha@gmail.com: not enough credits", "limited"),
        (403, "permission_denied", "unrecognised IP address", "misconfigured"),
        (404, "document_not_found", "", "misconfigured"),
        (429, "too_many_requests", "", "limited"),
        (500, "", "asha@gmail.com", "uncertain"),
        (503, "", "", "unavailable"),
    ] {
        b.lock().unwrap().answer = Some((status, json!({ "code": code, "message": message })));
        let e = m.send(&from, &sample_email()).await.unwrap_err();
        assert_eq!(kind(&e), want, "{status} {message}");
        assert!(!e.to_string().contains("asha"), "{e}");
    }
    let down = BrevoMailer::new("k".into(), "http://127.0.0.1:1/v3/smtp/email".into());
    assert!(matches!(down.send(&from, &sample_email()).await, Err(MailError::Unavailable(_))));
}

fn kind(e: &MailError) -> &'static str {
    match e {
        MailError::Rejected(_) => "rejected",
        MailError::Misconfigured(_) => "misconfigured",
        MailError::Limited(_) => "limited",
        MailError::Unavailable(_) => "unavailable",
        MailError::Uncertain(_) => "uncertain",
    }
}

/// Finding: any 400 paused the address. Only one clearly about the recipient does; the rest
/// is our request (stop the run, give the claim back).
#[test]
fn brevo_400s_pause_only_when_they_are_about_the_recipient() {
    use super::brevo::classify;
    for (code, message) in [
        ("invalid_parameter", "email is not valid in to"),
        ("invalid_parameter", "to[0].email is not a valid email"),
        ("invalid_parameter", "Invalid recipient"),
        ("missing_parameter", "to is missing"),
        ("", "Contact is blacklisted"),
        ("bad_request", "The recipient is blocked"),
    ] {
        assert_eq!(kind(&classify(400, code, message)), "rejected", "{code} {message}");
    }
    for (code, message) in [
        ("invalid_parameter", "sender email is not valid"),
        ("invalid_parameter", "unable to parse htmlContent"),
        ("missing_parameter", "subject is missing"),
        ("bad_request", "something went wrong"),
        ("", ""),
    ] {
        assert_eq!(kind(&classify(400, code, message)), "misconfigured", "{code} {message}");
    }
}

/// Finding: a 400 from Brevo logged only its code, so a missing field couldn't be found.
/// The log keeps the message, with any address blanked out.
#[test]
fn brevos_message_is_kept_without_addresses() {
    use super::brevo::classify;
    let e = classify(400, "missing_parameter", "sender is missing for viewer@example.com");
    let said = e.to_string();
    assert!(said.contains("missing_parameter: sender is missing for [address]"), "{said}");
    assert!(!said.contains("example.com"), "{said}");
}

/// Finding: the provider's daily limit was treated as an outage or our account. It stops
/// the day; a lost answer counts as sent; only a request never accepted is retried.
#[test]
fn brevos_limits_and_lost_answers_are_told_apart() {
    use super::brevo::classify;
    assert_eq!(kind(&classify(402, "not_enough_credits", "")), "limited");
    assert_eq!(kind(&classify(403, "not_enough_credits", "")), "limited");
    assert_eq!(kind(&classify(403, "permission_denied", "You have exceeded your daily sending limit")), "limited");
    assert_eq!(kind(&classify(403, "permission_denied", "unrecognised IP address")), "misconfigured");
    assert_eq!(kind(&classify(429, "too_many_requests", "")), "limited");
    assert_eq!(kind(&classify(503, "", "")), "unavailable");
    for status in [500, 502, 504, 520] {
        assert_eq!(kind(&classify(status, "", "")), "uncertain", "{status}");
    }
    // A recipient error keeps only Brevo's code, cleaned; its message can quote the address.
    let e = classify(400, "invalid_parameter", "a@b.co is not valid in to");
    assert_eq!(e.to_string(), "the mail provider refused the recipient: Brevo answered 400 invalid_parameter");
    let e = classify(400, "invalid<script>", "a@b.co");
    assert_eq!(
        e.to_string(),
        "the mail provider refused the relay's account: Brevo answered 400 invalidscript: [address]"
    );
}

/// Finding: a read timeout (Brevo got the request, the answer was lost) was given back and
/// could send twice. It counts as sent; a refused connection is retried.
#[tokio::test]
async fn a_slow_brevo_answer_is_lost_not_retried() {
    async fn slow() -> axum::http::StatusCode {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        axum::http::StatusCode::CREATED
    }
    let app = axum::Router::new().route("/v3/smtp/email", axum::routing::post(slow));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v3/smtp/email", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    let m = BrevoMailer::new("k".into(), url).with_timeout(std::time::Duration::from_millis(300));
    let from = Sender::parse(DEFAULT_FROM).unwrap();
    assert!(matches!(m.send(&from, &sample_email()).await, Err(MailError::Uncertain(_))));
}

// ── Through the router ───────────────────────────────────────────────────────────────

struct World {
    base: String,
    st: AppState,
    db: Arc<Db>,
    fake: Arc<FakeMailer>,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    text: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap_or("").to_string()
    }
}

fn vault() -> Vault {
    Vault::from_base64(&base64::engine::general_purpose::STANDARD.encode([9u8; 32])).unwrap()
}

/// `mailer`: whether a mail sender is set up.
async fn world_with(mailer: bool, dir: Option<tempfile::TempDir>) -> World {
    world_full(mailer, dir, cfg(), vault()).await
}

async fn world_full(mailer: bool, dir: Option<tempfile::TempDir>, mail: MailConfig, vault: Vault) -> World {
    let dir = dir.unwrap_or_else(|| tempfile::tempdir().unwrap());
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let fake = Arc::new(FakeMailer::new());
    let m: Option<Arc<dyn Mailer>> = mailer.then(|| fake.clone() as Arc<dyn Mailer>);
    let st = AppState::new(cfg, db).with_vault(Some(vault)).with_mail(m, mail);
    let db = st.db.clone();
    let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    World { base, st, db, fake, http: reqwest::Client::new(), dir }
}

async fn world() -> World {
    world_with(true, None).await
}

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    fn pro(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        let s = Synced { status: "active".into(), current_end: Some(now_ms() + 30 * DAY), cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
        auth
    }

    fn lapse(&self, sub: &str, ended: i64) {
        let s = Synced { status: "completed".into(), current_end: Some(ended), cancel_at_end: None };
        self.db.sync_pro_sub(None, sub, &s).unwrap();
    }

    async fn call(&self, method: reqwest::Method, path: &str, auth: Option<&str>, body: Option<Value>) -> Answer {
        let mut r = self.http.request(method, format!("{}{path}", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        Answer { status, headers, text: res.text().await.unwrap() }
    }

    async fn get_view(&self, auth: &str) -> Answer {
        self.call(reqwest::Method::GET, "/v1/pro/history/email", Some(auth), None).await
    }

    async fn save(&self, auth: &str, address: &str) -> Answer {
        self.call(reqwest::Method::PUT, "/v1/pro/history/email", Some(auth), Some(json!({ "address": address }))).await
    }

    /// A page by full URL (as in an email), GET or POST.
    async fn page(&self, method: reqwest::Method, url: &str, body: Option<&str>) -> Answer {
        let url = if url.starts_with("http") { url.to_string() } else { format!("{}{url}", self.base) };
        let mut r = self.http.request(method, url);
        if let Some(b) = body {
            r = r.header("content-type", "application/x-www-form-urlencoded").body(b.to_string());
        }
        let res = r.send().await.unwrap();
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        Answer { status, headers, text: res.text().await.unwrap() }
    }

    /// The confirm link in the newest email.
    fn confirm_link(&self) -> String {
        link_in(&self.fake.last().expect("an email was sent").text, "/mail/confirm?t=")
    }

    /// Saves and confirms an address for `sub` (Pro already).
    async fn subscribe(&self, auth: &str) {
        assert_eq!(self.save(auth, ADDRESS).await.status, 202);
        let link = self.confirm_link();
        assert_eq!(self.page(reqwest::Method::POST, &link, None).await.status, 200);
    }

    /// What the dashboard would show: `on`, `paused`, `pending` or `None` without a row.
    fn status(&self, sub: &str) -> Option<String> {
        self.db.mail_row(sub).unwrap().map(|r| r.view_status().to_string())
    }

    /// An address on for `sub` straight in the database (no confirm email, no budget used).
    fn on_directly(&self, sub: &str, address: &str) {
        let a = store::Addr {
            sealed: vault().seal(sub, PURPOSE, address.as_bytes()).unwrap(),
            tag: self.st.mail.cfg.addr_tag(address).unwrap(),
        };
        let now = now_ms() - 3_600_000;
        self.db.mail_put_pending(sub, &a, "direct", now + DAY, now).unwrap();
        assert_eq!(self.db.mail_confirm(sub, "direct", now).unwrap(), store::Confirmed::On);
    }

    /// Pro subscriptions with an address on, all with the same report day.
    fn same_day_subs(&self, n: usize, day: Option<i64>) -> Vec<String> {
        let want = day.unwrap_or_else(|| send_day("sub_0000"));
        let subs: Vec<String> =
            (0..2000).map(|i| format!("sub_{i:04}")).filter(|s| send_day(s) == want).take(n).collect();
        assert_eq!(subs.len(), n);
        for (i, sub) in subs.iter().enumerate() {
            self.pro(&format!("i_{sub}"), sub);
            self.on_directly(sub, &format!("viewer{i}@example.com"));
        }
        subs
    }

    fn week_of(&self, sub: &str, due: i64) -> Option<i64> {
        self.db.mail_sent_at(sub, crate::history::week_start(due)).unwrap()
    }

    /// Adds an ended stream to the subscription's history.
    fn add_stream(&self, sub: &str, start: i64, title: &str) {
        let s = StreamIn {
            platform: "twitch".into(),
            key: format!("twitch:{start}"),
            started_at: start,
            ended_at: Some(start + 90 * 60_000),
            title: Some(title.into()),
            category: None,
            peak_viewers: 50,
            avg_viewers: 30,
            chat_lines: 100,
            chatters: 20,
            new_chatters: 2,
            follows: 4,
            subs: 1,
            tips: vec![],
            platform_value: vec![],
        };
        self.db.upsert_stream(sub, "i1", &s, start).unwrap();
    }

    /// The whole database file, for "is the address in it" checks.
    fn db_bytes(&self) -> Vec<u8> {
        let copy = self.dir.path().join(format!("copy-{}.db", crate::random(4)));
        self.db.c().execute("VACUUM INTO ?1", [copy.to_str().unwrap()]).unwrap();
        std::fs::read(&copy).unwrap()
    }
}

fn link_in(text: &str, marker: &str) -> String {
    let at = text.find(marker).expect("a link in the email");
    let start = text[..at].rfind("http").unwrap();
    text[start..].split_whitespace().next().unwrap().to_string()
}

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn every_route_needs_a_known_install_and_pro() {
    let w = world().await;
    let free = w.install("free");
    for m in [reqwest::Method::GET, reqwest::Method::PUT] {
        let body = (m == reqwest::Method::PUT).then(|| json!({ "address": "a@b.co" }));
        let a = w.call(m.clone(), "/v1/pro/history/email", None, body.clone()).await;
        assert_eq!((a.status, a.code().as_str()), (401, "unauthorized"), "{m}");
        let a = w.call(m.clone(), "/v1/pro/history/email", Some(&free), body).await;
        assert_eq!((a.status, a.code().as_str()), (402, "pro_required"), "{m}");
        assert!(a.json()["error"]["hint"].as_str().unwrap().contains("Settings › Pro"));
    }
    let a = w.call(reqwest::Method::DELETE, "/v1/pro/history/email", Some(&free), None).await;
    assert_eq!(a.status, 402, "no subscription at all");
    assert!(w.fake.sent().is_empty());
}

#[tokio::test]
async fn without_a_mail_sender_the_feature_is_off_and_answers_503() {
    let w = world_with(false, None).await;
    let auth = w.pro("i1", "sub_1");
    let a = w.get_view(&auth).await;
    assert_eq!((a.status, a.code().as_str()), (503, "mail_unavailable"));
    assert!(!a.json()["error"]["hint"].as_str().unwrap().is_empty());
    let a = w.save(&auth, ADDRESS).await;
    assert_eq!((a.status, a.code().as_str()), (503, "mail_unavailable"));
    let s = w.call(reqwest::Method::GET, "/v1/pro/status", Some(&auth), None).await.json();
    assert_eq!(s["features"]["weekly_email"], false);
    let page = w.page(reqwest::Method::GET, "/mail/confirm?t=x", None).await;
    assert_eq!(page.status, 503);
    assert!(w.db.mail_row("sub_1").unwrap().is_none());
}

#[tokio::test]
async fn the_status_lists_the_feature() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let s = w.call(reqwest::Method::GET, "/v1/pro/status", Some(&auth), None).await.json();
    assert_eq!(s["features"]["weekly_email"], true);
    let a = w.get_view(&auth).await;
    assert_eq!(a.status, 200);
    assert_eq!(a.json()["status"], "none");
    assert_eq!(a.json()["address_hint"], Value::Null);
    assert_eq!(a.json()["day"], day_name("sub_1"));
    assert_eq!(a.headers["cache-control"], "no-store");
}

#[tokio::test]
async fn bad_bodies_are_refused_before_anything_is_sent() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w
        .http
        .put(format!("{}/v1/pro/history/email", w.base))
        .bearer_auth(&auth)
        .body("address=a@b.co")
        .send()
        .await
        .unwrap();
    assert_eq!(a.status(), 415);
    let a =
        w.call(reqwest::Method::PUT, "/v1/pro/history/email", Some(&auth), Some(json!({ "email": "a@b.co" }))).await;
    assert_eq!((a.status, a.code().as_str()), (400, "invalid"));
    for bad in ["", "not-an-address", "a@b", "two@@at.com"] {
        let a = w.save(&auth, bad).await;
        assert_eq!((a.status, a.code().as_str()), (422, "invalid_email"), "{bad:?}");
        assert!(a.json()["error"]["hint"].as_str().unwrap().contains("typos"));
    }
    assert!(w.fake.sent().is_empty());
    assert_eq!(w.db.quota_used("sub_1", CONFIRMS.name, CONFIRMS.period).unwrap(), 0);
}

#[tokio::test]
async fn opt_in_sends_a_confirm_link_and_a_scanner_fetching_it_confirms_nothing() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w.save(&auth, ADDRESS).await;
    assert_eq!(a.status, 202);
    assert_eq!(a.json()["status"], "pending");
    assert_eq!(a.json()["address_hint"], "A•••@gmail.com");
    assert!(a.json()["expires_at"].as_i64().unwrap() > now_ms() + DAY - 60_000);
    let mail = w.fake.last().unwrap();
    assert_eq!((mail.to.as_str(), mail.tag), (STORED, "confirm"));
    assert_eq!(mail.subject, "Confirm your Ferrocast weekly email");
    let link = w.confirm_link();
    assert!(link.starts_with(&format!("{}/mail/confirm?t=c1.sub_1.", w.base)), "{link}");
    assert!(mail.html.contains(&report::esc(&link)));

    // A link scanner (or the person) opening the link: a page with a button, nothing changes.
    for _ in 0..3 {
        let p = w.page(reqwest::Method::GET, &link, None).await;
        assert_eq!(p.status, 200);
        assert!(p.text.contains("<form method=post") && p.text.contains(">Confirm</button>"), "{}", p.text);
        assert!(!p.text.contains("<script"));
        assert_eq!(p.headers["content-security-policy"], pages::CSP);
        assert_eq!(p.headers["cache-control"], "no-store");
        assert_eq!(p.headers["referrer-policy"], "no-referrer");
    }
    assert_eq!(w.status("sub_1").as_deref(), Some("pending"));
    assert_eq!(w.get_view(&auth).await.json()["status"], "pending");

    // The button's POST confirms.
    let p = w.page(reqwest::Method::POST, &link, None).await;
    assert_eq!(p.status, 200);
    assert!(p.text.contains("You&#39;re in") || p.text.contains("You're in"), "{}", p.text);
    assert!(p.text.contains(day_name("sub_1")));
    let v = w.get_view(&auth).await.json();
    assert_eq!(v["status"], "on");
    assert_eq!(v["address_hint"], "A•••@gmail.com");
    assert_eq!(v["expires_at"], Value::Null);
    assert!(v["next_at"].as_i64().unwrap() >= now_ms() - 7 * DAY);

    // Used: pressing it again changes nothing and says so.
    let p = w.page(reqwest::Method::POST, &link, None).await;
    assert_eq!(p.status, 200);
    assert!(p.text.contains("all set"), "{}", p.text);
}

#[tokio::test]
async fn tampered_expired_and_replaced_links_dont_confirm() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    assert_eq!(w.save(&auth, ADDRESS).await.status, 202);
    let first = w.confirm_link();
    // Tampered.
    let bad = format!("{}x", first);
    for m in [reqwest::Method::GET, reqwest::Method::POST] {
        let p = w.page(m, &bad, None).await;
        assert_eq!(p.status, 404);
        assert!(p.text.contains("This link doesn"), "{}", p.text);
    }
    // Expired (signed by this relay, a day old).
    let (old, _) = w.st.mail.cfg.confirm_token("sub_1", now_ms() - 1).unwrap();
    let p = w.page(reqwest::Method::POST, &format!("/mail/confirm?t={old}"), None).await;
    assert_eq!(p.status, 410);
    assert!(p.text.contains("expired") && p.text.contains("Resend"));
    // Resend: the first link stops working, the new one works.
    assert_eq!(w.save(&auth, ADDRESS).await.status, 202);
    let second = w.confirm_link();
    assert_ne!(first, second);
    assert_eq!(w.page(reqwest::Method::POST, &first, None).await.status, 404);
    assert_eq!(w.status("sub_1").as_deref(), Some("pending"));
    assert_eq!(w.page(reqwest::Method::POST, &second, None).await.status, 200);
    assert_eq!(w.status("sub_1").as_deref(), Some("on"));
    // Another subscription's link can't be pointed at this one.
    let other = second.replace("sub_1", "sub_2");
    assert_eq!(w.page(reqwest::Method::POST, &other, None).await.status, 404);
}

#[tokio::test]
async fn confirming_needs_pro_at_that_moment() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    assert_eq!(w.save(&auth, ADDRESS).await.status, 202);
    let link = w.confirm_link();
    w.lapse("sub_1", now_ms() - DAY);
    let p = w.page(reqwest::Method::POST, &link, None).await;
    assert_eq!(p.status, 402);
    assert!(p.text.contains("Pro isn"));
    assert_eq!(w.status("sub_1").as_deref(), Some("pending"));
}

#[tokio::test]
async fn the_database_never_holds_the_address() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.subscribe(&auth).await;
    let bytes = w.db_bytes();
    for needle in [ADDRESS, STORED, "Asha.Streams", "asha.streams", "gmail.com"] {
        assert!(!has(&bytes, needle.as_bytes()), "{needle} is in the database");
    }
    let row = w.db.mail_row("sub_1").unwrap().unwrap();
    let active = row.active.unwrap();
    assert_eq!(active.sealed.key_v, crate::vault::KEY_V);
    assert_eq!(vault().open("sub_1", PURPOSE, &active.sealed).unwrap(), STORED.as_bytes());
    assert!(vault().open("sub_2", PURPOSE, &active.sealed).is_err(), "bound to its subscription");
    assert_eq!(active.tag.len(), 32, "only a keyed hash next to it");
    // The confirm nonce is kept only hashed.
    let link = w.fake.sent()[0].text.clone();
    let token = link_in(&link, "/mail/confirm?t=").rsplit_once("t=").unwrap().1.to_string();
    let nonce = token.split('.').nth(3).unwrap();
    assert!(!has(&bytes, nonce.as_bytes()));
}

#[tokio::test]
async fn three_confirm_emails_a_day_then_429() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    for _ in 0..CONFIRMS.limit {
        assert_eq!(w.save(&auth, ADDRESS).await.status, 202);
    }
    let a = w.save(&auth, "other@example.com").await;
    assert_eq!((a.status, a.code().as_str()), (429, "mail_rate"));
    let wait: i64 = a.headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=86_400).contains(&wait), "{wait}");
    assert!(a.headers.contains_key("x-quota-reset"));
    assert!(a.json()["error"]["hint"].as_str().unwrap().contains("spam"));
    assert_eq!(w.fake.sent().len(), 3);
    // Another subscription has its own three.
    let other = w.pro("i2", "sub_2");
    assert_eq!(w.save(&other, ADDRESS).await.status, 202);
}

#[tokio::test]
async fn mail_failures_say_what_happened_and_keep_the_old_address() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.subscribe(&auth).await;
    for (fail, status, code) in [
        (MailError::Unavailable("down".into()), 502, "mail_failed"),
        (MailError::Rejected("Brevo answered 400".into()), 422, "invalid_email"),
        (MailError::Misconfigured("Brevo answered 401".into()), 503, "mail_unavailable"),
        (MailError::Uncertain("timed out".into()), 502, "mail_failed"),
    ] {
        w.fake.fail_with(Some(fail));
        let a = w.save(&auth, "new@example.com").await;
        assert_eq!((a.status, a.code().as_str()), (status, code));
        assert!(!a.json()["error"]["hint"].as_str().unwrap().is_empty());
        assert_eq!(w.status("sub_1").as_deref(), Some("on"), "the working address stays");
    }
    assert_eq!(w.db.quota_used("sub_1", CONFIRMS.name, CONFIRMS.period).unwrap(), 1, "failures are given back");
    w.fake.fail_with(None);
}

#[tokio::test]
async fn the_same_address_again_is_a_409_and_stop_removes_it() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.subscribe(&auth).await;
    let a = w.save(&auth, "asha.streams@gmail.com").await;
    assert_eq!((a.status, a.code().as_str()), (409, "already_on"));
    let a = w.call(reqwest::Method::DELETE, "/v1/pro/history/email", Some(&auth), None).await;
    assert_eq!(a.status, 204);
    assert!(w.db.mail_row("sub_1").unwrap().is_none());
    assert_eq!(w.get_view(&auth).await.json()["status"], "none");
    // Twice is fine.
    assert_eq!(w.call(reqwest::Method::DELETE, "/v1/pro/history/email", Some(&auth), None).await.status, 204);
    // After Pro ended, stopping still works.
    w.subscribe(&auth).await;
    w.lapse("sub_1", now_ms() - DAY);
    assert_eq!(w.call(reqwest::Method::DELETE, "/v1/pro/history/email", Some(&auth), None).await.status, 204);
}

/// This subscription's report moment next week (in the future, inside its Pro period).
fn next_week_due(sub: &str) -> i64 {
    due_at(sub, now_ms() + 7 * DAY)
}

#[tokio::test]
async fn the_weekly_report_goes_once_on_its_day_with_one_click_unsubscribe() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.subscribe(&auth).await;
    let due = next_week_due("sub_1");
    w.add_stream("sub_1", due - 2 * DAY, "Ranked <b>grind</b> & chill");
    w.add_stream("sub_1", due - 9 * DAY, "Last week");
    let before = w.fake.sent().len();

    // An hour early: nothing.
    assert_eq!(run_weekly(&w.st, due - 3_600_000).await, RunStats::default());
    // On time: one report.
    let s = run_weekly(&w.st, due + 60_000).await;
    assert_eq!(s.sent, 1, "{s:?}");
    let mail = w.fake.last().unwrap();
    assert_eq!(w.fake.sent().len(), before + 1);
    assert_eq!((mail.to.as_str(), mail.tag), (STORED, "weekly"));
    assert!(mail.subject.starts_with("Your week on stream: 1 stream"), "{}", mail.subject);
    assert!(mail.html.contains("Ranked &lt;b&gt;grind&lt;/b&gt; &amp; chill"));
    assert!(mail.text.contains("same as last week"), "{}", mail.text);
    // Every later run that week, and a restarted relay on the same database, send nothing.
    assert_eq!(run_weekly(&w.st, due + 2 * 3_600_000).await.sent, 0);
    let dir = w.dir;
    let again = world_with(true, Some(dir)).await;
    assert_eq!(run_weekly(&again.st, due + 3 * 3_600_000).await.sent, 0);
    assert!(again.fake.sent().is_empty());
    assert!(again.db.mail_last_sent("sub_1").unwrap().is_some());
    // Next week: the next one.
    again.add_stream("sub_1", due + 3 * DAY, "Next");
    assert_eq!(run_weekly(&again.st, due + 7 * DAY + 60_000).await.sent, 1);

    // One-click unsubscribe from the headers (RFC 8058).
    let unsub = mail.headers.iter().find(|(k, _)| k == "List-Unsubscribe").unwrap().1.clone();
    let post = mail.headers.iter().find(|(k, _)| k == "List-Unsubscribe-Post").unwrap().1.clone();
    assert_eq!(post, "List-Unsubscribe=One-Click");
    let url = unsub.trim_start_matches('<').trim_end_matches('>').replace(&w.base, &again.base);
    assert!(mail.text.contains("/mail/unsubscribe?t=u1.sub_1."));
    // The GET page only asks.
    let p = again.page(reqwest::Method::GET, &url, None).await;
    assert_eq!(p.status, 200);
    assert!(p.text.contains(">Stop emails</button>"));
    assert_eq!(again.status("sub_1").as_deref(), Some("on"));
    // The mail app's POST removes the address.
    let p = again.page(reqwest::Method::POST, &url, Some("List-Unsubscribe=One-Click")).await;
    assert_eq!(p.status, 200);
    assert!(p.text.contains("unsubscribed"));
    assert!(again.db.mail_row("sub_1").unwrap().is_none());
    assert_eq!(again.page(reqwest::Method::POST, &url, None).await.status, 200, "again: still fine");
    assert_eq!(again.page(reqwest::Method::POST, &format!("{url}x"), None).await.status, 404);
}

/// Finding: links were bound to when the address was saved, so saving the same address
/// again broke every old link. They follow the address now: the same address keeps old
/// links working, another one never matches, and a link that matches nothing says so.
#[tokio::test]
async fn unsubscribe_links_follow_the_address_not_when_it_was_saved() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.subscribe(&auth).await;
    let old = w.db.mail_row("sub_1").unwrap().unwrap();
    let token = w.st.mail.cfg.unsubscribe_token("sub_1", &old.active.unwrap().tag).unwrap();
    let link = format!("/mail/unsubscribe?t={token}");
    // Stopped and saved again (another case): the old link still works.
    assert_eq!(w.call(reqwest::Method::DELETE, "/v1/pro/history/email", Some(&auth), None).await.status, 204);
    assert_eq!(w.save(&auth, "asha.streams@GMAIL.com").await.status, 202);
    w.page(reqwest::Method::POST, &w.confirm_link(), None).await;
    assert_eq!(w.status("sub_1").as_deref(), Some("on"));
    assert!(w.page(reqwest::Method::GET, &link, None).await.text.contains(">Stop emails</button>"));
    let p = w.page(reqwest::Method::POST, &link, None).await;
    assert!(p.text.contains("unsubscribed"), "{}", p.text);
    assert_eq!(w.status("sub_1"), None);
    // Another address: the old link never matches, and says what to do.
    assert_eq!(w.save(&auth, "other@example.com").await.status, 202);
    w.page(reqwest::Method::POST, &w.confirm_link(), None).await;
    for m in [reqwest::Method::GET, reqwest::Method::POST] {
        let p = w.page(m, &link, Some("List-Unsubscribe=One-Click")).await;
        assert_eq!(p.status, 200, "RFC 8058: still 200");
        assert!(p.text.contains("This link is out of date") && p.text.contains("stop emails in the dashboard"));
        assert!(!p.text.contains("<button"), "{}", p.text);
    }
    assert_eq!(w.status("sub_1").as_deref(), Some("on"), "the newer address stays");
}

#[tokio::test]
async fn quiet_weeks_late_confirms_and_lapsed_pro_send_nothing() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.subscribe(&auth).await;
    let due = next_week_due("sub_1");
    // No streams in the week: skipped (recorded once), nothing sent.
    let before = w.fake.sent().len();
    let s = run_weekly(&w.st, due + 60_000).await;
    assert_eq!((s.sent, s.skipped), (0, 1));
    assert_eq!(run_weekly(&w.st, due + 3_600_000).await.skipped, 0, "looked at once");
    assert_eq!(w.fake.sent().len(), before);
    assert_eq!(w.db.mail_sent_at("sub_1", crate::history::week_start(due)).unwrap(), Some(store::SKIPPED));
    // Missed by more than two days: not sent late.
    w.add_stream("sub_1", due + 5 * DAY, "x");
    let next = due + 7 * DAY;
    assert_eq!(run_weekly(&w.st, next + CATCH_UP_MS + 3_600_000).await.sent, 0);

    // Confirmed after this week's moment: the first report comes the week after.
    let auth2 = w.pro("i2", "sub_2");
    w.subscribe(&auth2).await;
    let past_due = due_at("sub_2", now_ms());
    if past_due < now_ms() {
        w.add_stream("sub_2", past_due - DAY, "x");
        assert_eq!(run_weekly(&w.st, now_ms()).await.sent, 0);
    }

    // Pro ended: no more reports.
    let auth3 = w.pro("i3", "sub_3");
    w.subscribe(&auth3).await;
    let due3 = next_week_due("sub_3");
    w.add_stream("sub_3", due3 - DAY, "x");
    w.lapse("sub_3", now_ms() - 1);
    let s = run_weekly(&w.st, due3 + 60_000).await;
    assert_eq!(s.sent, 0, "{s:?}");
}

#[tokio::test]
async fn a_refused_address_is_paused_and_a_provider_outage_retries() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.subscribe(&auth).await;
    let due = next_week_due("sub_1");
    w.add_stream("sub_1", due - DAY, "x");
    // Brevo down: nothing marked, the next run sends it.
    w.fake.fail_with(Some(MailError::Unavailable("down".into())));
    assert_eq!(run_weekly(&w.st, due + 60_000).await.retry, 1);
    assert_eq!(w.db.mail_sent_at("sub_1", crate::history::week_start(due)).unwrap(), None);
    w.fake.fail_with(None);
    assert_eq!(run_weekly(&w.st, due + 3_600_000).await.sent, 1);

    // Brevo refuses the address (4xx): paused, the dashboard says so, no more tries.
    w.add_stream("sub_1", due + 6 * DAY, "y");
    w.fake.fail_with(Some(MailError::Rejected("Brevo answered 400 invalid_parameter".into())));
    let next = due + 7 * DAY + 60_000;
    assert_eq!(run_weekly(&w.st, next).await.paused, 1);
    assert_eq!(w.status("sub_1").as_deref(), Some("paused"));
    let v = w.get_view(&auth).await.json();
    assert_eq!((v["status"].as_str(), v["paused_reason"].as_str()), (Some("paused"), Some("rejected")));
    w.fake.fail_with(None);
    assert_eq!(run_weekly(&w.st, next + 3_600_000).await, RunStats::default());
    // Saving it again starts over.
    assert_eq!(w.save(&auth, ADDRESS).await.status, 202);
    assert_eq!(w.status("sub_1").as_deref(), Some("pending"));
}

#[tokio::test]
async fn addresses_go_30_days_after_pro_ends_and_unconfirmed_ones_after_a_week() {
    let w = world().await;
    let a1 = w.pro("i1", "sub_1");
    let a2 = w.pro("i2", "sub_2");
    let a3 = w.pro("i3", "sub_3");
    w.subscribe(&a1).await;
    w.subscribe(&a2).await;
    assert_eq!(w.save(&a3, "pending@example.com").await.status, 202);
    let now = now_ms();
    w.lapse("sub_1", now - 31 * DAY);
    w.lapse("sub_2", now - 10 * DAY);
    let week = crate::history::week_start(now);
    w.db.mail_claim("sub_1", week, 5).unwrap();
    sweep(&w.st, now).await;
    assert!(w.db.mail_row("sub_1").unwrap().is_none(), "30 days after Pro ended");
    assert_eq!(w.db.mail_sent_at("sub_1", week).unwrap(), None, "its send log too");
    assert_eq!(w.status("sub_2").as_deref(), Some("on"), "within 30 days it's kept");
    assert_eq!(w.status("sub_3").as_deref(), Some("pending"));
    // A week after an unconfirmed link expired, the address goes.
    sweep(&w.st, now + CONFIRM_TTL_MS + 7 * DAY).await;
    assert!(w.db.mail_row("sub_3").unwrap().is_none());
    // Old send records go after 120 days.
    w.db.mail_claim("sub_9", week - 130 * DAY, 1).unwrap();
    sweep(&w.st, now).await;
    assert_eq!(w.db.mail_sent_at("sub_9", week - 130 * DAY).unwrap(), None);
}

// ── Review fixes (each a regression test) ────────────────────────────────────────────

fn rejected() -> MailError {
    MailError::Rejected("Brevo answered 400 invalid_parameter".into())
}

/// Adds a stream in the week before `due` for each subscription.
fn streams_for(w: &World, subs: &[String], due: i64) {
    for sub in subs {
        w.add_stream(sub, due - DAY, "x");
    }
}

/// Finding: two addresses refused one after the other paused both (a bad request of ours
/// would pause everyone). The second isn't paused, its claim is given back, the run stops.
#[tokio::test]
async fn two_different_refused_addresses_in_a_row_stop_the_run() {
    let w = world().await;
    let subs = w.same_day_subs(3, None);
    let due = next_week_due(&subs[0]);
    streams_for(&w, &subs, due);
    w.fake.fail_with(Some(rejected()));
    let s = run_weekly(&w.st, due + 60_000).await;
    assert_eq!((s.sent, s.paused, s.retry), (0, 1, 1), "{s:?}");
    assert_eq!(w.fake.tried(), 2, "the third isn't tried");
    assert_eq!(w.status(&subs[0]).as_deref(), Some("paused"));
    assert_eq!(w.status(&subs[1]).as_deref(), Some("on"), "the second isn't paused");
    assert_eq!(w.week_of(&subs[1], due), None, "its claim is given back");
    assert_eq!(w.week_of(&subs[2], due), None);
    // Brevo fine again: the rest go next hour.
    w.fake.fail_with(None);
    assert_eq!(run_weekly(&w.st, due + 3_600_000).await.sent, 2);
}

/// Finding: Brevo's daily limit (402 / 403 limit / 429) was an outage or "our account". It
/// stops for the day, every claim given back, and the next day carries on.
#[tokio::test]
async fn the_providers_daily_limit_stops_for_today_and_tomorrow_carries_on() {
    let w = world().await;
    let subs = w.same_day_subs(2, None);
    let due = next_week_due(&subs[0]);
    streams_for(&w, &subs, due);
    w.fake.script(vec![Err(MailError::Limited("Brevo answered 402 not_enough_credits".into()))]);
    let s = run_weekly(&w.st, due + 60_000).await;
    assert_eq!((s.sent, s.retry), (0, 1), "{s:?}");
    for sub in &subs {
        assert_eq!(w.week_of(sub, due), None, "given back");
        assert_eq!(w.status(sub).as_deref(), Some("on"));
    }
    assert_eq!(w.db.mail_budget_used(budget_day(due + 60_000)).unwrap(), w.st.mail.cfg.budget);
    // Later the same day: nothing tried.
    let s = run_weekly(&w.st, due + 2 * 3_600_000).await;
    assert_eq!((s.sent, s.deferred), (0, 1));
    assert_eq!(w.fake.tried(), 1);
    // Tomorrow, still inside the catch-up window: both go.
    assert_eq!(run_weekly(&w.st, due + DAY).await.sent, 2);
}

/// The same limit on a confirm email: 503 `mail_busy`, "Try again tomorrow.", nothing
/// counted against the person's three, and no more tries today.
#[tokio::test]
async fn the_providers_limit_on_a_confirm_says_try_again_tomorrow() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.fake.script(vec![Err(MailError::Limited("Brevo answered 429 too_many_requests".into()))]);
    for _ in 0..2 {
        let a = w.save(&auth, ADDRESS).await;
        assert_eq!((a.status, a.code().as_str()), (503, "mail_busy"));
        assert_eq!(a.json()["error"]["hint"], "Try again tomorrow.");
        assert!(!a.json()["error"]["message"].as_str().unwrap().is_empty());
    }
    assert_eq!(w.fake.tried(), 1, "the second never reached the provider");
    assert_eq!(w.db.quota_used("sub_1", CONFIRMS.name, CONFIRMS.period).unwrap(), 0);
    assert_eq!(w.status("sub_1"), None);
}

/// Finding: a vault that opens nothing (a wrong `RELAY_VAULT_KEY`) paused every address.
/// Nothing is paused: the claim is given back and the run stops after the first.
#[tokio::test]
async fn a_vault_that_opens_nothing_pauses_nothing_and_stops_the_run() {
    let w = world().await;
    let subs = w.same_day_subs(2, None);
    let due = next_week_due(&subs[0]);
    streams_for(&w, &subs, due);
    let wrong = Vault::from_base64(&base64::engine::general_purpose::STANDARD.encode([7u8; 32])).unwrap();
    let again = world_full(true, Some(w.dir), cfg(), wrong).await;
    let s = run_weekly(&again.st, due + 60_000).await;
    assert_eq!((s.sent, s.paused, s.retry), (0, 0, 1), "{s:?}");
    assert_eq!(again.fake.tried(), 0);
    for sub in &subs {
        assert_eq!(again.status(sub).as_deref(), Some("on"));
        assert_eq!(again.week_of(sub, due), None);
    }
}

/// ...and only a row that alone doesn't open, while others in the run do, is paused
/// `unreadable`; a lone row that doesn't open is never paused.
#[tokio::test]
async fn only_an_address_that_alone_does_not_open_is_paused() {
    let w = world().await;
    let subs = w.same_day_subs(3, None);
    let due = next_week_due(&subs[0]);
    streams_for(&w, &subs, due);
    w.db.c().execute("UPDATE mail_addresses SET ct = X'00010203' WHERE sub_id = ?1", [&subs[0]]).unwrap();
    let s = run_weekly(&w.st, due + 60_000).await;
    assert_eq!((s.sent, s.paused, s.retry), (2, 1, 0), "{s:?}");
    let row = w.db.mail_row(&subs[0]).unwrap().unwrap();
    assert_eq!((row.view_status(), row.paused_reason.as_deref()), ("paused", Some("unreadable")));

    let lone = world().await;
    let one = lone.same_day_subs(1, None);
    lone.add_stream(&one[0], due - DAY, "x");
    lone.db.c().execute("UPDATE mail_addresses SET ct = X'00' WHERE sub_id = ?1", [&one[0]]).unwrap();
    let s = run_weekly(&lone.st, due + 60_000).await;
    assert_eq!((s.paused, s.retry), (0, 1), "{s:?}");
    assert_eq!(lone.status(&one[0]).as_deref(), Some("on"));
    assert_eq!(lone.week_of(&one[0], due), None);
}

/// Finding: saving a new address stopped the confirmed one at once. It keeps getting the
/// report while the new one waits; confirming swaps them; the sweep drops only the pending
/// part; Stop removes both.
#[tokio::test]
async fn a_new_address_waits_while_the_confirmed_one_keeps_working() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.subscribe(&auth).await;
    let a = w.save(&auth, "new@example.com").await;
    assert_eq!(a.status, 202);
    for v in [a.json(), w.get_view(&auth).await.json()] {
        assert_eq!(v["status"], "on", "{v}");
        assert_eq!(v["address_hint"], "A•••@gmail.com");
        assert_eq!(v["pending_hint"], "n•••@example.com");
        assert!(v["expires_at"].as_i64().unwrap() > now_ms());
        assert!(v["next_at"].is_i64());
    }
    let new_link = w.confirm_link();
    // The report still goes to the confirmed address.
    let due = next_week_due("sub_1");
    w.add_stream("sub_1", due - DAY, "x");
    assert_eq!(run_weekly(&w.st, due + 60_000).await.sent, 1);
    assert_eq!(w.fake.last().unwrap().to, STORED);
    // The new one's link swaps it in.
    assert!(w.page(reqwest::Method::GET, &new_link, None).await.text.contains(">Confirm</button>"));
    assert_eq!(w.page(reqwest::Method::POST, &new_link, None).await.status, 200);
    let v = w.get_view(&auth).await.json();
    assert_eq!((v["status"].as_str(), v["address_hint"].as_str()), (Some("on"), Some("n•••@example.com")));
    assert_eq!((v["pending_hint"].clone(), v["expires_at"].clone()), (Value::Null, Value::Null));
    assert!(w.page(reqwest::Method::GET, &new_link, None).await.text.contains("all set"));
    // An unconfirmed one next to it: the sweep drops only that part.
    assert_eq!(w.save(&auth, "third@example.com").await.status, 202);
    let third = w.confirm_link();
    sweep(&w.st, now_ms() + CONFIRM_TTL_MS + 7 * DAY).await;
    let row = w.db.mail_row("sub_1").unwrap().unwrap();
    assert!(row.pending.is_none() && row.confirm_hash.is_none());
    assert_eq!(w.status("sub_1").as_deref(), Some("on"));
    assert_eq!(w.get_view(&auth).await.json()["pending_hint"], Value::Null);
    assert_eq!(w.page(reqwest::Method::POST, &third, None).await.status, 404);
    // Stop removes both.
    let auth2 = w.pro("i2", "sub_2");
    w.subscribe(&auth2).await;
    assert_eq!(w.save(&auth2, "next@example.com").await.status, 202);
    assert_eq!(w.call(reqwest::Method::DELETE, "/v1/pro/history/email", Some(&auth2), None).await.status, 204);
    assert!(w.db.mail_row("sub_2").unwrap().is_none());
}

/// Finding: a lost answer was given back (a second email could follow). It counts as sent:
/// the claim stays, the budget counts it, the run stops; the rest go next run.
#[tokio::test]
async fn a_lost_answer_counts_as_sent_and_is_never_sent_again() {
    let w = world().await;
    let subs = w.same_day_subs(2, None);
    let due = next_week_due(&subs[0]);
    streams_for(&w, &subs, due);
    w.fake.script(vec![Err(MailError::Uncertain("timed out".into()))]);
    let s = run_weekly(&w.st, due + 60_000).await;
    assert_eq!((s.sent, s.unsure, s.retry), (0, 1, 0), "{s:?}");
    assert_eq!(w.week_of(&subs[0], due), Some(store::CLAIMED));
    assert_eq!(w.week_of(&subs[1], due), None);
    let s = run_weekly(&w.st, due + 3_600_000).await;
    assert_eq!(s.sent, 1);
    assert_eq!(w.fake.tried(), 2, "the lost one isn't tried again");
    assert_eq!(w.fake.last().unwrap().to, "viewer1@example.com");
    assert_eq!(w.db.mail_budget_used(budget_day(due)).unwrap(), 2, "the lost one counts");
    assert_eq!(w.db.mail_last_sent(&subs[0]).unwrap(), None, "but isn't shown as sent");
}

/// Finding: nothing kept the relay inside Brevo's 300 a day. A shared budget: the weekly
/// run stops at it and gives the claim back (tomorrow, still in the catch-up window, goes
/// on); a confirm over it is 503 `mail_busy`.
#[tokio::test]
async fn the_daily_budget_is_shared_by_confirms_and_reports() {
    assert_eq!(MailConfig::default().budget, DAILY_BUDGET);
    const { assert!(DAILY_BUDGET <= 300) };
    let w = world_full(true, None, cfg().with_budget(2), vault()).await;
    let subs = w.same_day_subs(3, None);
    let due = next_week_due(&subs[0]);
    streams_for(&w, &subs, due);
    let s = run_weekly(&w.st, due + 60_000).await;
    assert_eq!((s.sent, s.deferred), (2, 1), "{s:?}");
    assert_eq!(w.week_of(&subs[2], due), None, "given back for tomorrow");
    assert_eq!(run_weekly(&w.st, due + 3_600_000).await.sent, 0);
    assert_eq!(run_weekly(&w.st, due + DAY).await.sent, 1, "the next day carries on");
    // Confirms count against the same budget (today's).
    let a1 = w.pro("c1", "sub_c1");
    let a2 = w.pro("c2", "sub_c2");
    assert_eq!(w.save(&a1, "one@example.com").await.status, 202);
    assert_eq!(w.save(&a1, "one@example.com").await.status, 202);
    let a = w.save(&a2, "two@example.com").await;
    assert_eq!((a.status, a.code().as_str()), (503, "mail_busy"));
    assert_eq!(a.json()["error"]["hint"], "Try again tomorrow.");
    assert_eq!(w.db.quota_used("sub_c2", CONFIRMS.name, CONFIRMS.period).unwrap(), 0, "not counted");
    // A send that didn't go gives its place back.
    let w = world_full(true, None, cfg().with_budget(1), vault()).await;
    let auth = w.pro("i1", "sub_1");
    w.fake.script(vec![Err(MailError::Unavailable("down".into()))]);
    assert_eq!(w.save(&auth, ADDRESS).await.status, 502);
    assert_eq!(w.save(&auth, ADDRESS).await.status, 202);
}

/// Finding: a moment late in the week (Sunday 04:00) missed by an outage until Monday was
/// lost: only this week's moment was looked at. Last week's still counts within the window.
#[tokio::test]
async fn last_weeks_moment_still_goes_early_in_the_next_week() {
    let w = world().await;
    let subs = w.same_day_subs(1, Some(6));
    let sub = subs[0].as_str();
    let due = next_week_due(sub);
    w.add_stream(sub, due - DAY, "Sunday");
    let monday = crate::history::week_start(due) + WEEK_MS + 3_600_000;
    assert_eq!(next_at(&w.st, sub, None, monday), due, "still due");
    assert_eq!(next_at(&w.st, sub, Some(due + 1), monday), due + WEEK_MS, "confirmed after it");
    assert_eq!(run_weekly(&w.st, monday).await.sent, 1);
    assert!(w.week_of(sub, due).unwrap() > 0, "in last week's slot");
    assert_eq!(run_weekly(&w.st, monday + 3_600_000).await.sent, 0);
    assert_eq!(next_at(&w.st, sub, None, monday + 3_600_000), due + WEEK_MS);
}

/// Finding: the row wasn't read again after the claim, so an address stopped or changed
/// mid-run still got the old report. Both are left alone (claims given back).
#[tokio::test]
async fn an_address_stopped_or_changed_mid_run_is_left_alone() {
    let w = world().await;
    let subs = w.same_day_subs(3, None);
    let due = next_week_due(&subs[0]);
    streams_for(&w, &subs, due);
    let changed = store::Addr {
        sealed: vault().seal(&subs[2], PURPOSE, b"changed@example.com").unwrap(),
        tag: w.st.mail.cfg.addr_tag("changed@example.com").unwrap(),
    };
    let (db, stopped, swapped) = (w.db.clone(), subs[1].clone(), subs[2].clone());
    w.fake.during_send(move || {
        let now = now_ms();
        db.mail_delete(&stopped).unwrap();
        db.mail_put_pending(&swapped, &changed, "h2", now + DAY, now).unwrap();
        db.mail_confirm(&swapped, "h2", now).unwrap();
    });
    let s = run_weekly(&w.st, due + 60_000).await;
    assert_eq!(s.sent, 1, "{s:?}");
    assert_eq!(w.fake.tried(), 1);
    assert_eq!(w.week_of(&subs[1], due), None);
    assert_eq!(w.week_of(&subs[2], due), None);
    // Next run: the new address gets it.
    assert_eq!(run_weekly(&w.st, due + 3_600_000).await.sent, 1);
    assert_eq!(w.fake.last().unwrap().to, "changed@example.com");
}

/// Finding: a week without streams was final at once, so a stream uploaded after the moment
/// never got its report. It's looked at again until the catch-up window ends.
#[test]
fn every_email_carries_the_postal_address() {
    let addr = super::layout::POSTAL_ADDRESS;
    let from = 1_792_281_600_000;
    let week = [stream("twitch", from + DAY, 60, "Ranked", 5, 9)];
    let r = report::render(&report::build(&week, &[], from, from + 7 * DAY).unwrap(), "https://r/u");
    let c = report::confirm("https://r/mail/confirm?t=x");
    for (name, html, text) in [("weekly", &r.html, &r.text), ("confirm", &c.html, &c.text)] {
        assert!(html.contains(addr), "{name} html");
        assert!(text.contains(addr), "{name} text");
    }
    let s = crate::accounts::pages::sign_in_html("https://x/login/link?t=y", "ABCD-EFGH");
    assert!(s.contains(addr), "sign-in html");
}

#[tokio::test]
async fn a_quiet_week_still_sends_when_a_stream_lands_late() {
    let w = world().await;
    let subs = w.same_day_subs(2, None);
    let due = next_week_due(&subs[0]);
    assert_eq!(run_weekly(&w.st, due + 60_000).await.skipped, 2);
    w.add_stream(&subs[0], due - DAY, "uploaded late");
    let s = run_weekly(&w.st, due + 3 * 3_600_000).await;
    assert_eq!((s.sent, s.skipped), (1, 0), "{s:?}");
    // After the window it's final.
    w.add_stream(&subs[1], due - DAY, "too late");
    assert_eq!(run_weekly(&w.st, due + CATCH_UP_MS + 3_600_000).await.sent, 0);
    assert_eq!(w.week_of(&subs[1], due), Some(store::SKIPPED));
}

/// Writes a sample report and confirm email as HTML for a look in a browser:
/// `MAIL_SAMPLE_DIR=/tmp/x cargo test -p ferrocast-relay write_sample_emails -- --ignored`.
#[test]
#[ignore = "writes files; run by hand"]
fn write_sample_emails() {
    let dir = std::path::PathBuf::from(std::env::var("MAIL_SAMPLE_DIR").unwrap());
    let from = 1_792_281_600_000;
    let mut a = stream("twitch", from + DAY + 19 * 3_600_000, 194, "Ranked grind to Diamond <3", 64, 118);
    a.follows = 21;
    a.subs = 4;
    let week = [
        a,
        stream("kick", from + DAY + 19 * 3_600_000, 194, "Ranked grind to Diamond <3", 22, 35),
        stream("twitch", from + 3 * DAY, 135, "Chill Sunday, chat & coffee", 41, 66),
        stream("twitch", from + 5 * DAY, 210, "Viewer games night", 48, 90),
    ];
    let before =
        [stream("twitch", from - 4 * DAY, 160, "Old", 38, 70), stream("twitch", from - 2 * DAY, 120, "Old", 30, 51)];
    let r = report::render(
        &report::build(&week, &before, from, from + 7 * DAY).unwrap(),
        "https://ferrocast-relay.clearcraftlabs.com/mail/unsubscribe?t=u1.sub_x.1.abc",
    );
    std::fs::write(dir.join("weekly.html"), &r.html).unwrap();
    std::fs::write(dir.join("weekly.txt"), format!("Subject: {}\n\n{}", r.subject, r.text)).unwrap();
    let c = report::confirm("https://ferrocast-relay.clearcraftlabs.com/mail/confirm?t=c1.sub_x.1.n.m");
    std::fs::write(dir.join("confirm.html"), &c.html).unwrap();
    let r = report::render_with(
        &report::build(&week, &before, from, from + 7 * DAY).unwrap(),
        "https://ferrocast-relay.clearcraftlabs.com/mail/unsubscribe?t=u1.sub_x.1.abc",
        Some("https://ferrocast-remote.clearcraftlabs.com/app/#/history"),
    );
    std::fs::write(dir.join("weekly-button.html"), &r.html).unwrap();
    let s = crate::accounts::pages::sign_in_html("https://ferrocast.clearcraftlabs.com/login/link?t=x", "SMEN-RGQB");
    std::fs::write(dir.join("sign-in.html"), s).unwrap();
}
