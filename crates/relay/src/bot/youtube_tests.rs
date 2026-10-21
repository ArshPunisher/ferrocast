//! The YouTube cloud bot (M12) with a fake Google (no network): the OAuth broker through the
//! real `router()` (single-use signed state, the PC's channel proven with its desktop token
//! and the desktop app's client id, every refusal revoking the fresh grant), disconnect and
//! lapse revoking, the proven owner reclaiming a channel; live detection only while the cloud
//! holds YouTube's lease and its cadence (paused clock); the chat stream's reopen wait and
//! page token; replies within the daily unit budget per subscription and relay-wide; the
//! Pacific-midnight reset; dedupe; handback to the PC; offline moderation; Google's own quota
//! answer; chat never stored.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use ferrocast_core::event::{Actor, Content, Event, EventKind, Millis, Platform, Roles};
use serde_json::{Value, json};

use super::tests::{config, enc, eventually, vault};

/// Waits on a paused clock in 1 s steps (the reader sleeps 30 s and more between streams).
async fn until(mut f: impl FnMut() -> bool) {
    for _ in 0..3_600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("condition never became true");
}
use super::youtube::*;
use super::*;
use crate::billing::store::Synced;
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;
const CLIENT: &str = "cloud-1.apps.googleusercontent.com";
const DESKTOP: &str = "desk-app.apps.googleusercontent.com";
const OWNER: &str = "UCowner0000000000000000";
const OTHER: &str = "UCother0000000000000000";
/// The PC's desktop YouTube token (what the engine adds to `connect`).
const DESK_TOKEN: &str = "ya29.desk";

// ── The fake ─────────────────────────────────────────────────────────────────────────

/// One opened stream's answers; `hold`: stays open (pending) after them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Script {
    items: Vec<Result<Option<Batch>, YoutubeError>>,
    hold: bool,
}

/// Google as the cloud sees it. Grants are modelled as Google keeps them: one grant per
/// Google account (channel) and client, so revoking any token of it ends every token of it
/// issued so far (`dead`), and a refresh with a dead token is refused.
#[derive(Debug, Default)]
pub(crate) struct Yt {
    pub(crate) codes: HashMap<String, GoogleTokens>,
    /// Token → whose grant (the channel).
    pub(crate) grants: HashMap<String, String>,
    /// Tokens whose grant was revoked.
    dead: HashSet<String>,
    /// Calls in flight / at most at once (live checks), and how long each takes.
    in_flight: u32,
    max_in_flight: u32,
    detect_takes: Option<Duration>,
    /// Opening the stream fails with these first.
    open_fails: VecDeque<YoutubeError>,
    /// The next send is refused with 401 once.
    send_401_once: bool,
    pub(crate) infos: HashMap<String, GoogleTokenInfo>,
    pub(crate) channels: HashMap<String, ChannelInfo>,
    pub(crate) revoked: Vec<String>,
    refresh_fails: bool,
    refreshes: u32,
    live: Option<Broadcast>,
    detect_fails: Option<YoutubeError>,
    pub(crate) detects: Vec<tokio::time::Instant>,
    scripts: VecDeque<Script>,
    /// (when, page token) per opened stream.
    opens: Vec<(tokio::time::Instant, Option<String>)>,
    pub(crate) sent: Vec<(String, String)>,
    send_fails: Option<YoutubeError>,
    /// (what, live chat or "", viewer or message, seconds).
    pub(crate) mods: Vec<(String, String, String, Option<u32>)>,
}

#[derive(Debug, Default)]
pub(crate) struct FakeYoutube(Mutex<Yt>);

impl FakeYoutube {
    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut Yt) -> T) -> T {
        f(&mut self.0.lock().unwrap())
    }
}

#[derive(Debug)]
struct FakeFeed(VecDeque<Result<Option<Batch>, YoutubeError>>, bool);

impl ChatFeed for FakeFeed {
    fn next(&mut self) -> Fut<'_, Option<Batch>, YoutubeError> {
        Box::pin(async move {
            match self.0.pop_front() {
                Some(r) => r,
                None if self.1 => std::future::pending().await,
                None => Ok(None),
            }
        })
    }
}

impl YoutubeCloud for FakeYoutube {
    fn exchange_code<'a>(&'a self, code: &'a str, redirect: &'a str) -> Fut<'a, GoogleTokens, YoutubeError> {
        Box::pin(async move {
            assert!(redirect.ends_with("/v1/bot/youtube/callback"), "{redirect}");
            self.with(|y| y.codes.remove(code).ok_or(YoutubeError::BadRequest("bad code".into())))
        })
    }
    fn refresh<'a>(&'a self, refresh_token: &'a str) -> Fut<'a, GoogleTokens, YoutubeError> {
        Box::pin(async move {
            self.with(|y| {
                y.refreshes += 1;
                if y.refresh_fails || y.dead.contains(refresh_token) {
                    return Err(YoutubeError::Unauthorized);
                }
                let access = format!("yat-{refresh_token}");
                if let Some(who) = y.grants.get(refresh_token).cloned() {
                    y.grants.insert(access.clone(), who);
                }
                Ok(GoogleTokens {
                    access_token: access,
                    refresh_token: String::new(),
                    scopes: vec![FORCE_SSL.into()],
                    expires_in: 3_600,
                })
            })
        })
    }
    fn token_info<'a>(&'a self, access_token: &'a str) -> Fut<'a, GoogleTokenInfo, YoutubeError> {
        Box::pin(async move { self.with(|y| y.infos.get(access_token).cloned().ok_or(YoutubeError::Unauthorized)) })
    }
    fn my_channel<'a>(&'a self, access_token: &'a str) -> Fut<'a, ChannelInfo, YoutubeError> {
        Box::pin(async move { self.with(|y| y.channels.get(access_token).cloned().ok_or(YoutubeError::NotFound)) })
    }
    fn revoke<'a>(&'a self, token: &'a str) -> Fut<'a, (), YoutubeError> {
        Box::pin(async move {
            self.with(|y| {
                y.revoked.push(token.to_string());
                // The whole grant of that account goes, whichever of its tokens was sent.
                if let Some(who) = y.grants.get(token).cloned() {
                    let all: Vec<String> =
                        y.grants.iter().filter(|(_, w)| **w == who).map(|(t, _)| t.clone()).collect();
                    y.dead.extend(all);
                }
                y.dead.insert(token.to_string());
            });
            Ok(())
        })
    }
    fn active_broadcast<'a>(&'a self, access_token: &'a str) -> Fut<'a, Option<Broadcast>, YoutubeError> {
        Box::pin(async move {
            assert!(access_token.starts_with("yat-"), "the cloud's own sign-in: {access_token}");
            let takes = self.with(|y| {
                y.in_flight += 1;
                y.max_in_flight = y.max_in_flight.max(y.in_flight);
                y.detect_takes
            });
            if let Some(d) = takes {
                tokio::time::sleep(d).await;
            }
            self.with(|y| {
                y.in_flight -= 1;
                y.detects.push(tokio::time::Instant::now());
                if let Some(e) = y.detect_fails.clone() {
                    return Err(e);
                }
                Ok(y.live.clone())
            })
        })
    }
    fn open_chat<'a>(
        &'a self,
        _access_token: &'a str,
        live_chat_id: &'a str,
        page_token: Option<String>,
    ) -> Fut<'a, Box<dyn ChatFeed>, YoutubeError> {
        Box::pin(async move {
            assert_eq!(live_chat_id, "chat-1");
            let s = self.with(|y| {
                y.opens.push((tokio::time::Instant::now(), page_token));
                match y.open_fails.pop_front() {
                    Some(e) => Err(e),
                    None => Ok(y.scripts.pop_front().unwrap_or_default()),
                }
            })?;
            Ok(Box::new(FakeFeed(s.items.into(), s.hold)) as Box<dyn ChatFeed>)
        })
    }
    fn send<'a>(
        &'a self,
        _access_token: &'a str,
        live_chat_id: &'a str,
        text: &'a str,
    ) -> Fut<'a, Option<String>, YoutubeError> {
        Box::pin(async move {
            self.with(|y| {
                if std::mem::take(&mut y.send_401_once) {
                    return Err(YoutubeError::Unauthorized);
                }
                if let Some(e) = y.send_fails.clone() {
                    return Err(e);
                }
                y.sent.push((live_chat_id.into(), text.into()));
                Ok(Some(format!("LCC.sent-{}", y.sent.len())))
            })
        })
    }
    fn delete_message<'a>(&'a self, _access_token: &'a str, message_id: &'a str) -> Fut<'a, (), YoutubeError> {
        Box::pin(async move {
            self.with(|y| y.mods.push(("delete".into(), String::new(), message_id.into(), None)));
            Ok(())
        })
    }
    fn ban<'a>(
        &'a self,
        _access_token: &'a str,
        live_chat_id: &'a str,
        channel_id: &'a str,
        seconds: Option<u32>,
    ) -> Fut<'a, (), YoutubeError> {
        Box::pin(async move {
            let what = if seconds.is_some() { "timeout" } else { "ban" };
            self.with(|y| y.mods.push((what.into(), live_chat_id.into(), channel_id.into(), seconds)));
            Ok(())
        })
    }
}

pub(crate) fn yt_cfg(per_sub: i64, total: i64) -> YoutubeConfig {
    YoutubeConfig::from_values(Some(CLIENT.into()), Some("yt-secret-SECRET".into()))
        .unwrap()
        .unwrap()
        .with_desktop_client_id(Some(DESKTOP.into()))
        .with_units(per_sub, total)
        .unwrap()
}

fn line(id: &str, who: &str, text: &str) -> Event {
    let actor = Actor {
        platform: Platform::Youtube,
        id: Some(who.into()),
        login: None,
        display_name: who.into(),
        avatar_url: None,
        color: None,
        roles: Roles::empty(),
        badges: vec![],
        anonymous: false,
    };
    Event::live(
        Platform::Youtube,
        Some(id.into()),
        Millis(now_ms()),
        EventKind::ChatMessage { actor, content: Content::plain(text), reply_to: None, is_action: false },
    )
}

fn batch(lines: Vec<Event>, page: &str) -> Result<Option<Batch>, YoutubeError> {
    Ok(Some(Batch { events: lines, next_page_token: Some(page.into()), offline: false }))
}

// ── A state without HTTP (paused clocks) ─────────────────────────────────────────────

fn bare(yt: Arc<FakeYoutube>, per_sub: i64, total: i64) -> (AppState, tempfile::TempDir) {
    bare_cfg(yt, yt_cfg(per_sub, total))
}

fn bare_cfg(yt: Arc<FakeYoutube>, ycfg: YoutubeConfig) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (on_db(yt, ycfg, &dir), dir)
}

/// A relay (fresh memory) on the database in `dir`: a restart.
fn on_db(yt: Arc<FakeYoutube>, ycfg: YoutubeConfig, dir: &tempfile::TempDir) -> AppState {
    let db = Db::open(&dir.join_path()).unwrap();
    let cfg = Config {
        public_url: "http://relay.test".into(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: "http://kick.test".into(),
        kick_api_url: "http://kick.test".into(),
    };
    let rt = Runtime::new(None, None, None).with_youtube(Some(ycfg), Some(yt as Arc<dyn YoutubeCloud>));
    AppState::new(cfg, db).with_vault(Some(vault())).with_bot(rt)
}

trait JoinPath {
    fn join_path(&self) -> std::path::PathBuf;
}

impl JoinPath for tempfile::TempDir {
    fn join_path(&self) -> std::path::PathBuf {
        self.path().join("relay.db")
    }
}

fn pro_sub(st: &AppState, install: &str, sub: &str) {
    st.db.add_install(install, &crate::hash("secret")).unwrap();
    st.db.add_pro_sub(sub, "monthly", "e", "created", install).unwrap();
    st.db
        .sync_pro_sub(
            None,
            sub,
            &Synced { status: "active".into(), current_end: Some(now_ms() + 30 * DAY), cancel_at_end: None },
        )
        .unwrap();
}

/// A connected, switched-on YouTube channel with a sealed sign-in and the test config.
fn ready_youtube(st: &AppState, install: &str, sub: &str, moderation: bool) {
    ready_youtube_as(st, install, sub, OWNER, "yrt-1", moderation);
}

fn ready_youtube_as(st: &AppState, install: &str, sub: &str, channel: &str, refresh: &str, moderation: bool) {
    pro_sub(st, install, sub);
    st.db
        .bot_set_account(sub, "youtube", (channel, "Asha"), Some((channel, "Asha")), FORCE_SSL, moderation, install, 1)
        .unwrap();
    seal_token(st, sub, "youtube", "bot", refresh);
    let cfg: ferrocast_core::cloudbot::BotConfig = serde_json::from_value(config()).unwrap();
    st.db.bot_push_config(sub, 0, &serde_json::to_string(&cfg).unwrap(), 0, 1).unwrap();
    st.db.bot_set_enabled(sub, true, 1).unwrap();
}

fn cloud_has_youtube(st: &AppState, sub: &str) {
    let past = Instant::now().checked_sub(TAKEOVER_AFTER + Duration::from_secs(1)).unwrap();
    st.bot.lease(sub, Platform::Youtube, |l| *l = Lease::new(past, now_ms() - 91_000));
}

fn chan(st: &AppState, sub: &str) -> store::Channel {
    st.db.bot_channel(sub, "youtube").unwrap().unwrap()
}

fn used(st: &AppState, sub: &str, call: Option<Call>) -> i64 {
    st.db.yt_units_used(sub, call, now_ms()).unwrap()
}

// ── Pure pieces ──────────────────────────────────────────────────────────────────────

/// 2026-10-19T12:00:00Z etc. as ms.
fn utc(y: i64, m: i64, d: i64, h: i64, mi: i64) -> i64 {
    super::twitch::parse_time(&format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:00Z")).unwrap()
}

#[test]
fn the_quota_day_ends_at_midnight_pacific_with_daylight_saving() {
    // Summer (PDT, UTC−7): 19 Oct 2026 ends at 07:00 UTC on the 20th.
    assert_eq!(pacific_day(utc(2026, 10, 19, 12, 0)), ("p2026-10-19".into(), utc(2026, 10, 20, 7, 0)));
    // Just before and after that midnight.
    assert_eq!(pacific_day(utc(2026, 10, 20, 6, 59)).0, "p2026-10-19");
    assert_eq!(pacific_day(utc(2026, 10, 20, 7, 0)).0, "p2026-10-20");
    // Winter (PST, UTC−8).
    assert_eq!(pacific_day(utc(2026, 12, 1, 3, 0)), ("p2026-11-30".into(), utc(2026, 12, 1, 8, 0)));
    // The day DST starts (8 Mar 2026) ends at 07:00 UTC; the day it ends (1 Nov) at 08:00.
    assert_eq!(pacific_day(utc(2026, 3, 8, 12, 0)), ("p2026-03-08".into(), utc(2026, 3, 9, 7, 0)));
    assert_eq!(pacific_day(utc(2026, 3, 8, 9, 0)), ("p2026-03-08".into(), utc(2026, 3, 9, 7, 0)));
    assert_eq!(pacific_day(utc(2026, 11, 1, 12, 0)), ("p2026-11-01".into(), utc(2026, 11, 2, 8, 0)));
    assert_eq!(pacific_offset_ms(utc(2026, 3, 8, 9, 59)), -8 * 3_600_000);
    assert_eq!(pacific_offset_ms(utc(2026, 3, 8, 10, 0)), -7 * 3_600_000);
    assert_eq!(pacific_offset_ms(utc(2026, 11, 1, 8, 59)), -7 * 3_600_000);
    assert_eq!(pacific_offset_ms(utc(2026, 11, 1, 9, 0)), -8 * 3_600_000);
    assert_eq!(utc_hm(utc(2026, 10, 20, 7, 0)), "07:00 UTC");
}

#[test]
fn units_are_counted_per_call_per_subscription_and_relay_wide_and_reset_at_pacific_midnight() {
    let (st, _d) = bare(Arc::new(FakeYoutube::default()), 120, 206);
    let day1 = utc(2026, 10, 19, 20, 0);
    let l = Some(Room::plain(120, 206));
    st.db.yt_take_units("a", Call::Detect, l, day1).unwrap();
    st.db.yt_take_units("a", Call::Read, l, day1).unwrap();
    st.db.yt_take_units("a", Call::Send, l, day1).unwrap();
    st.db.yt_take_units("a", Call::Send, l, day1).unwrap();
    assert_eq!(st.db.yt_units_used("a", None, day1).unwrap(), 106);
    assert_eq!(st.db.yt_units_used("a", Some(Call::Send), day1).unwrap(), 100);
    assert_eq!(st.db.yt_units_used("a", Some(Call::Read), day1).unwrap(), 5);
    // A third reply would pass 120: refused, nothing counted, back at midnight Pacific.
    match st.db.yt_take_units("a", Call::Send, l, day1) {
        Err(UnitsErr::Spent(s)) => {
            assert_eq!(s, Spent { relay_wide: false, reset_at: utc(2026, 10, 20, 7, 0), checks: false })
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(st.db.yt_units_used("a", None, day1).unwrap(), 106);
    // Another subscription spends the relay's 206 down: then the relay says no.
    st.db.yt_take_units("b", Call::Send, l, day1).unwrap();
    st.db.yt_take_units("b", Call::Send, l, day1).unwrap();
    match st.db.yt_take_units("b", Call::Detect, l, day1) {
        Err(UnitsErr::Spent(s)) => assert!(s.relay_wide),
        other => panic!("{other:?}"),
    }
    assert_eq!(st.db.yt_units_used(RELAY_SUB, None, day1).unwrap(), 206);
    // After midnight Pacific (07:00 UTC in October) both start again.
    let day2 = utc(2026, 10, 20, 7, 1);
    st.db.yt_take_units("a", Call::Send, l, day2).unwrap();
    assert_eq!(st.db.yt_units_used("a", None, day2).unwrap(), 50);
    // Without a limit (a call already made) it's counted anyway.
    st.db.yt_take_units("a", Call::Channel, None, day2).unwrap();
    assert_eq!(st.db.yt_units_used("a", Some(Call::Channel), day2).unwrap(), 1);
}

#[test]
fn settings_are_checked_and_never_show_the_secret() {
    assert!(YoutubeConfig::from_values(None, None).unwrap().is_none());
    assert!(YoutubeConfig::from_values(Some(CLIENT.into()), None).is_err());
    assert!(YoutubeConfig::from_values(Some("not-google".into()), Some("s".into())).is_err());
    let c = yt_cfg(1_500, 9_000);
    assert_eq!((c.units_per_sub, c.units), (1_500, 9_000));
    assert!(!format!("{c:?}").contains("yt-secret"), "{c:?}");
    assert!(c.clone().with_units(10_000, 9_000).is_err(), "a subscription can't have more than the relay");
    assert!(c.clone().with_units(40, 9_000).is_err(), "less than one reply");
    assert!(c.with_units(1_500, 2_000_000).is_err());
    assert_eq!(
        (
            YoutubeConfig::from_values(Some(CLIENT.into()), Some("s".into())).unwrap().unwrap().units_per_sub,
            DEFAULT_UNITS
        ),
        (1_500, 9_000)
    );
    let t = GoogleTokens { access_token: "ya29.x".into(), refresh_token: "1//r".into(), scopes: vec![], expires_in: 1 };
    assert!(!format!("{t:?}").contains("ya29") && !format!("{t:?}").contains("1//r"));
}

#[test]
fn googles_errors_are_told_apart() {
    let reason = |r: &str| json!({ "error": { "errors": [{ "reason": r }], "message": "m" } });
    assert_eq!(classify(403, &reason("quotaExceeded")), YoutubeError::Quota);
    assert_eq!(classify(403, &reason("rateLimitExceeded")), YoutubeError::RateLimited { retry_after_s: 30 });
    assert_eq!(classify(403, &reason("liveChatEnded")), YoutubeError::ChatEnded);
    assert_eq!(classify(404, &reason("liveChatNotFound")), YoutubeError::ChatEnded);
    assert_eq!(classify(403, &reason("forbidden")), YoutubeError::Forbidden("m".into()));
    assert_eq!(classify(401, &Value::Null), YoutubeError::Unauthorized);
    assert_eq!(classify(400, &json!({ "error": "invalid_grant" })), YoutubeError::Unauthorized);
    assert_eq!(classify(400, &reason("invalidValue")), YoutubeError::BadRequest("m".into()));
    assert_eq!(classify(503, &Value::Null), YoutubeError::Unavailable("status 503".into()));
}

#[test]
fn reopening_a_closed_stream_waits_longer_when_streams_keep_closing() {
    assert_eq!(reopen_wait(0), REOPEN_MIN);
    assert_eq!(reopen_wait(1), Duration::from_secs(2));
    assert_eq!(reopen_wait(2), Duration::from_secs(4));
    assert_eq!(reopen_wait(4), Duration::from_secs(16));
    assert_eq!(reopen_wait(9), REOPEN_MAX);
    assert_eq!(fail_wait(1), FAIL_WAIT_MIN);
}

#[test]
fn states_are_signed_expire_and_carry_the_owner() {
    let c = yt_cfg(1_500, 9_000);
    let s = sign_state(&c, true, "i1", OWNER, "n1", 1_000);
    let info = check_state(&c, &s, 2_000).unwrap();
    assert_eq!((info.moderation, info.install.as_str(), info.owner.as_str()), (true, "i1", OWNER));
    assert!(check_state(&c, &s, 1_000 + STATE_TTL_MS + 1).is_none(), "10 minutes");
    let forged = s.replace(OWNER, OTHER);
    assert!(check_state(&c, &forged, 2_000).is_none(), "another channel can't be signed in");
    let other = YoutubeConfig::from_values(Some(CLIENT.into()), Some("another".into())).unwrap().unwrap();
    assert!(check_state(&other, &s, 2_000).is_none());
}

// ── The broker, through the router ───────────────────────────────────────────────────

struct W {
    base: String,
    st: AppState,
    yt: Arc<FakeYoutube>,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn world() -> W {
    let yt = Arc::new(FakeYoutube::default());
    yt.with(|y| {
        y.infos.insert(DESK_TOKEN.into(), GoogleTokenInfo { client_id: DESKTOP.into(), scopes: vec![] });
        y.channels.insert(DESK_TOKEN.into(), ChannelInfo { id: OWNER.into(), title: "Asha".into() });
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let (mut st, dir) = bare(yt.clone(), 1_500, 9_000);
    st.cfg = Arc::new(Config { public_url: base.clone(), ..(*st.cfg).clone() });
    let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    W { base, st, yt, http, _dir: dir }
}

impl W {
    fn pro(&self, install: &str, sub: &str) -> String {
        pro_sub(&self.st, install, sub);
        format!("{install}.secret")
    }

    async fn call(
        &self,
        m: reqwest::Method,
        path: &str,
        auth: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value, String) {
        let mut r = self.http.request(m, format!("{}{path}", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let status = res.status().as_u16();
        let text = res.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null), text)
    }

    async fn connect(&self, auth: &str, body: Value) -> (u16, Value) {
        let (s, v, _) = self.call(reqwest::Method::POST, "/v1/pro/bot/youtube/connect", Some(auth), Some(body)).await;
        (s, v)
    }

    /// Google sends the browser back after the channel `who` granted `scopes`.
    async fn finish(&self, state: &str, refresh: &str, who: &str, scopes: &[&str]) -> (u16, String) {
        let code = format!("code-{}", crate::random(6));
        let access = format!("ya29.{code}");
        self.yt.with(|y| {
            y.codes.insert(
                code.clone(),
                GoogleTokens {
                    access_token: access.clone(),
                    refresh_token: refresh.into(),
                    scopes: scopes.iter().map(|s| s.to_string()).collect(),
                    expires_in: 3_600,
                },
            );
            y.channels.insert(access.clone(), ChannelInfo { id: who.into(), title: format!("title-{who}") });
            // One grant per Google account and client: every token of it belongs to `who`.
            y.grants.insert(access.clone(), who.into());
            if !refresh.is_empty() {
                y.grants.insert(refresh.into(), who.into());
            }
        });
        let (s, _, text) = self
            .call(
                reqwest::Method::GET,
                &format!("/v1/bot/youtube/callback?code={code}&state={}", enc(state)),
                None,
                None,
            )
            .await;
        (s, text)
    }

    async fn start(&self, auth: &str, moderation: bool) -> String {
        self.start_with(auth, moderation, DESK_TOKEN).await
    }

    /// Connect from a PC whose own YouTube sign-in is `desk`.
    async fn start_with(&self, auth: &str, moderation: bool, desk: &str) -> String {
        let (s, v) = self.connect(auth, json!({ "owner_token": desk, "moderation": moderation })).await;
        assert_eq!(s, 201, "{v}");
        let url = url::Url::parse(v["auth_url"].as_str().unwrap()).unwrap();
        url.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string()
    }
}

#[tokio::test]
async fn connecting_youtube_proves_the_pcs_channel_and_seals_the_sign_in() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    // Without the PC's own YouTube: told to connect it in Accounts.
    let (s, v) = w.connect(&auth, json!({})).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("youtube_not_linked")), "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("Accounts"));
    // A token from another Google app is refused.
    w.yt.with(|y| y.infos.insert("ya29.other".into(), GoogleTokenInfo { client_id: "x.apps".into(), scopes: vec![] }));
    let (s, v) = w.connect(&auth, json!({ "owner_token": "ya29.other" })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (403, Some("wrong_youtube_app")));
    // A token Google doesn't know: sign in again.
    let (s, v) = w.connect(&auth, json!({ "owner_token": "ya29.gone" })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("youtube_not_linked")));
    let (s, v) = w.connect(&auth, json!({ "account": "both", "owner_token": DESK_TOKEN })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (400, Some("invalid")));
    // The real thing: Google's consent page with the one scope, offline access, consent.
    let (s, v) = w.connect(&auth, json!({ "owner_token": DESK_TOKEN })).await;
    assert_eq!(s, 201, "{v}");
    let url = url::Url::parse(v["auth_url"].as_str().unwrap()).unwrap();
    let q: HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
    assert!(url.as_str().starts_with(AUTHORIZE_URL));
    assert_eq!(q["client_id"], CLIENT);
    assert_eq!(q["scope"], FORCE_SSL);
    assert_eq!(q["access_type"], "offline");
    assert!(q["prompt"].contains("consent"));
    assert_eq!(q["redirect_uri"], format!("{}/v1/bot/youtube/callback", w.base));
    assert!(!url.as_str().contains(DESK_TOKEN), "the PC's token is never in the link");
    let state = q["state"].clone();
    let (s, page) = w.finish(&state, "1//refresh-owner", OWNER, &[FORCE_SSL]).await;
    assert_eq!(s, 200, "{page}");
    assert!(page.contains(&pc_code(&w.st, "i1")), "{page}");
    assert!(page.contains("press Check again") && page.contains("<h1>Done</h1>"), "{page}");
    assert!(page.contains("third-party connections") && !page.contains(DESK_TOKEN), "{page}");
    let ch = chan(&w.st, "sub_1");
    assert_eq!((ch.bot_id.as_deref(), ch.channel_id.as_deref()), (Some(OWNER), Some(OWNER)));
    assert!(ch.ready() && !ch.moderation);
    // Sealed: the row never holds the refresh token in the clear, and it opens again.
    let sealed = w.st.db.bot_token("sub_1", "youtube", "bot").unwrap().unwrap();
    assert!(!String::from_utf8_lossy(&sealed.ct).contains("refresh-owner"));
    assert_eq!(open_token(&w.st, "sub_1", "youtube", "bot").as_deref(), Some("1//refresh-owner"));
    assert_eq!(used(&w.st, "sub_1", Some(Call::Channel)), 1, "the channel check is counted");
    // The state is single use.
    let (s, page) = w.finish(&state, "1//again", OWNER, &[FORCE_SSL]).await;
    assert_eq!(s, 400, "{page}");
    assert!(page.contains("already used"));
    // The view: a beta row with its quota line.
    let (_, v, _) = w.call(reqwest::Method::GET, "/v1/pro/bot", Some(&auth), None).await;
    assert_eq!(v["available"]["youtube"], true);
    assert_eq!(v["limits"]["youtube_daily_units"], 1_500);
    let row = v["channels"].as_array().unwrap().iter().find(|c| c["platform"] == "youtube").unwrap().clone();
    assert_eq!((row["state"].as_str(), row["beta"].as_bool()), (Some("ready"), Some(true)));
    assert_eq!((row["quota"]["used"].as_i64(), row["quota"]["limit"].as_i64()), (Some(1), Some(1_500)));
    assert_eq!(row["quota"]["exhausted"], false);
    assert!(row["quota"]["reset_at"].as_i64().unwrap() > now_ms());
    // Connected already: a second connect without the opt-in is a 409.
    let (s, v) = w.connect(&auth, json!({ "owner_token": DESK_TOKEN })).await;
    assert_eq!((s, v["error"]["code"].as_str()), (409, Some("already_connected")));
}

#[tokio::test]
async fn the_youtube_callback_refuses_and_revokes_what_it_should() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    // Another channel than the PC's: refused, the grant revoked.
    let state = w.start(&auth, false).await;
    let (s, page) = w.finish(&state, "1//wrong", OTHER, &[FORCE_SSL]).await;
    assert_eq!(s, 403, "{page}");
    assert!(page.contains("Asha"), "names the PC's channel: {page}");
    assert!(w.yt.with(|y| y.revoked.contains(&"1//wrong".to_string())));
    assert!(w.st.db.bot_channel("sub_1", "youtube").unwrap().is_none());
    // The scope unticked.
    let state = w.start(&auth, false).await;
    let (s, _) = w.finish(&state, "1//noscope", OWNER, &["https://www.googleapis.com/auth/youtube.readonly"]).await;
    assert_eq!(s, 400);
    assert!(w.yt.with(|y| y.revoked.contains(&"1//noscope".to_string())));
    // No refresh token: the access token is revoked instead.
    let state = w.start(&auth, false).await;
    let (s, _) = w.finish(&state, "", OWNER, &[FORCE_SSL]).await;
    assert_eq!(s, 502);
    assert!(w.yt.with(|y| y.revoked.iter().any(|t| t.starts_with("ya29.code-"))));
    // A forged or broken state, a cancel.
    let (s, _, _) =
        w.call(reqwest::Method::GET, "/v1/bot/youtube/callback?code=x&state=y.i1.9.n.UC.ff", None, None).await;
    assert_eq!(s, 400);
    let (s, _, _) = w.call(reqwest::Method::GET, "/v1/bot/youtube/callback?state=x", None, None).await;
    assert_eq!(s, 400);
    let (s, _, text) = w.call(reqwest::Method::GET, "/v1/bot/youtube/callback?error=access_denied", None, None).await;
    assert_eq!(s, 200);
    assert!(text.contains("cancelled"));
    // Pro ended between Connect and the callback.
    let state = w.start(&auth, false).await;
    w.st.db
        .sync_pro_sub(None, "sub_1", &Synced { status: "cancelled".into(), current_end: Some(1), cancel_at_end: None })
        .unwrap();
    let (s, _) = w.finish(&state, "1//lapsed", OWNER, &[FORCE_SSL]).await;
    assert_eq!(s, 402);
    assert!(w.yt.with(|y| y.revoked.contains(&"1//lapsed".to_string())));
    assert!(w.st.db.bot_channel("sub_1", "youtube").unwrap().is_none());
}

/// Whether `sub`'s stored YouTube sign-in still works at Google (a refresh, as the cloud does).
async fn signed_in(st: &AppState, sub: &str) -> bool {
    st.bot.yt(sub, |m| m.forget_access_all());
    access(st, sub).await.is_ok()
}

#[tokio::test]
async fn the_opt_in_and_a_reclaim_keep_the_grant_and_only_disconnect_revokes() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let state = w.start(&auth, false).await;
    assert_eq!(w.finish(&state, "1//first", OWNER, &[FORCE_SSL]).await.0, 200);
    // The moderation opt-in signs the same channel in again: the same Google grant, so the
    // old sign-in is only overwritten (a revoke would end the new one too).
    let state = w.start(&auth, true).await;
    assert_eq!(w.finish(&state, "1//moderate", OWNER, &[FORCE_SSL]).await.0, 200);
    assert!(w.yt.with(|y| y.revoked.is_empty()), "{:?}", w.yt.with(|y| y.revoked.clone()));
    assert!(signed_in(&w.st, "sub_1").await, "the new sign-in still works at Google");
    let ch = chan(&w.st, "sub_1");
    assert!(ch.moderation && can_moderate(&ch, Platform::Youtube));
    assert_eq!(open_token(&w.st, "sub_1", "youtube", "bot").as_deref(), Some("1//moderate"));
    // Another subscription's PC with the same channel takes it (it proved the channel): the
    // stale binding goes without a revoke (the same grant again).
    let auth2 = w.pro("i2", "sub_2");
    let state = w.start(&auth2, false).await;
    assert_eq!(w.finish(&state, "1//second-sub", OWNER, &[FORCE_SSL]).await.0, 200);
    assert!(w.st.db.bot_channel("sub_1", "youtube").unwrap().is_none(), "the stale binding went");
    assert!(w.st.db.bot_token("sub_1", "youtube", "bot").unwrap().is_none());
    assert!(w.yt.with(|y| y.revoked.is_empty()));
    assert!(signed_in(&w.st, "sub_2").await, "the reclaiming sign-in works");
    // Disconnect: revoked at Google, rows gone, memory forgotten.
    w.st.bot.yt("sub_2", |m| m.chat = Some("chat-1".into()));
    let (s, _, _) = w.call(reqwest::Method::DELETE, "/v1/pro/bot/youtube", Some(&auth2), None).await;
    assert_eq!(s, 204);
    assert!(w.yt.with(|y| y.revoked.contains(&"1//second-sub".to_string())));
    assert!(w.st.db.bot_channel("sub_2", "youtube").unwrap().is_none());
    assert!(w.st.db.bot_token("sub_2", "youtube", "bot").unwrap().is_none());
    assert_eq!(live_chat(&w.st, "sub_2"), None);
}

#[tokio::test]
async fn a_sign_in_for_another_channel_revokes_the_one_it_replaces() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let state = w.start(&auth, false).await;
    assert_eq!(w.finish(&state, "1//asha", OWNER, &[FORCE_SSL]).await.0, 200);
    // The PC now speaks for another channel, and the opt-in signs that one in.
    w.yt.with(|y| y.channels.insert(DESK_TOKEN.into(), ChannelInfo { id: OTHER.into(), title: "B".into() }));
    let state = w.start(&auth, true).await;
    assert_eq!(w.finish(&state, "1//b", OTHER, &[FORCE_SSL]).await.0, 200);
    assert!(w.yt.with(|y| y.revoked.contains(&"1//asha".to_string())), "another account's grant goes");
    assert!(signed_in(&w.st, "sub_1").await);
    assert_eq!(chan(&w.st, "sub_1").channel_id.as_deref(), Some(OTHER));
}

#[tokio::test]
async fn a_refused_sign_in_never_revokes_a_grant_someone_still_uses() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let state = w.start(&auth, false).await;
    assert_eq!(w.finish(&state, "1//asha", OWNER, &[FORCE_SSL]).await.0, 200);
    // Someone on another subscription (whose PC is signed in to OTHER) signs in with Asha's
    // Google account: refused, and Asha's grant (the same one) is left alone.
    let auth2 = w.pro("i2", "sub_2");
    w.yt.with(|y| {
        y.infos.insert("ya29.desk2".into(), GoogleTokenInfo { client_id: DESKTOP.into(), scopes: vec![] });
        y.channels.insert("ya29.desk2".into(), ChannelInfo { id: OTHER.into(), title: "B".into() });
    });
    let state = w.start_with(&auth2, false, "ya29.desk2").await;
    let (s, page) = w.finish(&state, "1//stray", OWNER, &[FORCE_SSL]).await;
    assert_eq!(s, 403, "{page}");
    assert!(w.yt.with(|y| y.revoked.is_empty()), "{:?}", w.yt.with(|y| y.revoked.clone()));
    assert!(signed_in(&w.st, "sub_1").await, "the bound channel still works");
    // Asha's own Pro lapsed (the binding is kept 30 days): a sign-in then is refused (402)
    // without ending the kept one either.
    let state = w.start(&auth, true).await;
    w.st.db
        .sync_pro_sub(None, "sub_1", &Synced { status: "cancelled".into(), current_end: Some(1), cancel_at_end: None })
        .unwrap();
    assert_eq!(w.finish(&state, "1//late", OWNER, &[FORCE_SSL]).await.0, 402);
    assert!(w.yt.with(|y| y.revoked.is_empty()));
    assert!(w.st.db.bot_channel("sub_1", "youtube").unwrap().is_some());
}

#[tokio::test]
async fn without_the_settings_youtube_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let cfg = Config {
        public_url: "http://relay.test".into(),
        tip_url: None,
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: "http://k".into(),
        kick_api_url: "http://k".into(),
    };
    let rt = Runtime::new(None, None, None).with_youtube(Some(yt_cfg(1_500, 9_000)), None);
    let st = AppState::new(cfg, db).with_vault(Some(vault())).with_bot(rt);
    assert!(!st.bot.has(Platform::Youtube), "a config without a client is off");
    assert!(!available(&st));
    assert_eq!(st.bot.youtube().unwrap_err().status().as_u16(), 503);
}

// ── Live detection, reading, replies (paused clock) ──────────────────────────────────

#[tokio::test(start_paused = true)]
async fn live_checks_run_only_while_the_cloud_has_youtube_and_slow_down_after_half_an_hour() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    // The PC answers YouTube: nothing is asked of YouTube at all.
    desktop_seen(&st, "i1", &[Platform::Youtube]);
    for _ in 0..10 {
        step(&st, &chan(&st, "sub_1")).await;
        tokio::time::advance(Duration::from_secs(60)).await;
        desktop_seen(&st, "i1", &[Platform::Youtube]);
    }
    assert_eq!(yt.with(|y| y.detects.len()), 0);
    // The PC went quiet 90 s ago: the first look is at once (1 unit).
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), 1);
    assert_eq!(used(&st, "sub_1", Some(Call::Detect)), 1);
    // Every 2 minutes for the first half hour.
    tokio::time::advance(Duration::from_secs(60)).await;
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), 1, "not before 2 minutes");
    tokio::time::advance(Duration::from_secs(60)).await;
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), 2);
    for _ in 0..30 {
        tokio::time::advance(Duration::from_secs(60)).await;
        step(&st, &chan(&st, "sub_1")).await;
    }
    let n = yt.with(|y| y.detects.len());
    assert!((15..=17).contains(&n), "about one every 2 minutes for 32 minutes: {n}");
    // Then every 5.
    for _ in 0..30 {
        tokio::time::advance(Duration::from_secs(60)).await;
        step(&st, &chan(&st, "sub_1")).await;
    }
    let later = yt.with(|y| y.detects.len()) - n;
    assert!((5..=7).contains(&later), "one every 5 minutes: {later}");
    let gaps: Vec<u64> = yt.with(|y| y.detects.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect());
    assert!(gaps.iter().all(|g| *g >= 120), "{gaps:?}");
    // Switched off: nothing more.
    st.db.bot_set_enabled("sub_1", false, 1).unwrap();
    let before = yt.with(|y| y.detects.len());
    for _ in 0..10 {
        tokio::time::advance(Duration::from_secs(60)).await;
        tick(&st).await;
    }
    assert_eq!(yt.with(|y| y.detects.len()), before);
}

#[tokio::test(start_paused = true)]
async fn the_pcs_hint_sets_how_soon_the_cloud_looks() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    // The PC said its YouTube was offline just before it went quiet: the cloud waits 2 minutes.
    seen_frame(&st, "i1", &json!({ "type": "bot_seen", "platforms": [], "youtube_live": false }));
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), 0);
    tokio::time::advance(CHECK_FAST).await;
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), 1);
    // A second subscription whose PC saw the stream live: looked at once.
    ready_youtube_other(&st);
    seen_frame(&st, "i2", &json!({ "type": "bot_seen", "platforms": [], "youtube_live": true }));
    cloud_has_youtube(&st, "sub_2");
    step(&st, &chan(&st, "sub_2")).await;
    assert_eq!(yt.with(|y| y.detects.len()), 2);
}

fn ready_youtube_other(st: &AppState) {
    pro_sub(st, "i2", "sub_2");
    st.db.bot_set_account("sub_2", "youtube", (OTHER, "B"), Some((OTHER, "B")), FORCE_SSL, false, "i2", 1).unwrap();
    seal_token(st, "sub_2", "youtube", "bot", "yrt-2");
    st.db.bot_set_enabled("sub_2", true, 1).unwrap();
}

#[tokio::test(start_paused = true)]
async fn live_chat_is_read_answered_once_reopened_at_once_and_stopped_when_it_ends() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: Some(now_ms() - 60_000) });
        // Stream 1: a command, the same message again (a resend), then YouTube closes it.
        y.scripts.push_back(Script {
            items: vec![
                batch(vec![line("LCC.m1", "UCviewer", "!lurk")], "page-1"),
                batch(vec![line("LCC.m1", "UCviewer", "!lurk")], "page-2"),
            ],
            hold: false,
        });
        // Stream 2 (from page-2): quiet for a while, then the broadcast ends.
        y.scripts.push_back(Script {
            items: vec![Ok(Some(Batch { events: vec![], next_page_token: None, offline: true }))],
            hold: false,
        });
    });
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    assert!(chan(&st, "sub_1").live_since.is_some(), "live from the broadcast");
    until(|| yt.with(|y| y.opens.len()) >= 2).await;
    until(|| chan(&st, "sub_1").live_since.is_none()).await;
    let (opens, sent) = yt.with(|y| (y.opens.clone(), y.sent.clone()));
    assert_eq!(sent, vec![("chat-1".to_string(), "Enjoy the lurk, UCviewer!".to_string())], "answered once");
    assert_eq!(opens[0].1, None);
    assert_eq!(opens[1].1.as_deref(), Some("page-2"), "resumed from the last page token");
    // YouTube closing a stream that answered is normal: opened again at once, so a line
    // typed in between is answered in about a second, not after a long wait.
    assert!(opens[1].0 - opens[0].0 < REOPEN_MIN, "{:?}", opens[1].0 - opens[0].0);
    // Units: 1 detect + 2 opens + 1 reply.
    assert_eq!(used(&st, "sub_1", None), 1 + 10 + 50);
    assert_eq!(used(&st, "sub_1", Some(Call::Read)), 10);
    // The chat ended: no live chat, the next look 2 minutes on.
    assert_eq!(live_chat(&st, "sub_1"), None);
    // Kept in memory only (the offline chat), never in the database.
    assert_eq!(recent_chat(&st, "sub_1", Platform::Youtube).len(), 1);
    let dump: String = {
        let c = st.db.c();
        let mut q = c.prepare("SELECT COALESCE(error,'') || COALESCE(last_drop,'') FROM bot_channels").unwrap();
        q.query_map([], |r| r.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    };
    assert!(!dump.contains("lurk"));
}

#[tokio::test(start_paused = true)]
async fn streams_that_keep_closing_wait_longer_each_time() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: None });
        for _ in 0..4 {
            y.scripts.push_back(Script::default()); // closes at once
        }
        y.scripts.push_back(Script { items: vec![], hold: true });
    });
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    until(|| yt.with(|y| y.opens.len()) >= 5).await;
    let gaps: Vec<u64> = yt.with(|y| y.opens.windows(2).map(|w| (w[1].0 - w[0].0).as_secs()).collect());
    // Not a hot loop, but short: a line typed meanwhile still gets answered within seconds.
    assert_eq!(gaps, vec![1, 2, 4, 8], "doubling from 1 s");
    assert_eq!(used(&st, "sub_1", Some(Call::Read)), 25);
}

#[tokio::test(start_paused = true)]
async fn replies_stop_at_the_reserve_and_say_when_it_is_back_while_reading_goes_on() {
    let yt = Arc::new(FakeYoutube::default());
    // 200 a day, 80 kept for reading: replies may use 120. 1 detect + 5 read + 2 replies =
    // 106; a third (156) would eat into the reserve.
    let (st, _d) = bare_cfg(yt.clone(), yt_cfg(200, 9_000).with_reserves(80, 0));
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: None });
        y.scripts.push_back(Script {
            items: vec![batch(
                vec![line("LCC.a", "UC1", "!lurk"), line("LCC.b", "UC2", "!lurk"), line("LCC.c", "UC3", "!lurk")],
                "p1",
            )],
            hold: true,
        });
    });
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    eventually(|| chan(&st, "sub_1").error_code() == Some("youtube_quota")).await;
    assert_eq!(yt.with(|y| y.sent.len()), 2);
    assert_eq!(used(&st, "sub_1", None), 106);
    let ch = chan(&st, "sub_1");
    let e = ch.error.clone().unwrap();
    assert!(
        e["message"].as_str().unwrap().starts_with("YouTube's daily limit for the cloud bot is used up; it's back at")
    );
    assert_eq!(e["reset_at"].as_i64(), Some(pacific_day(now_ms()).1));
    assert_eq!(e["relay_wide"], false);
    assert!(ch.ready(), "a spent budget never blocks the channel");
    // Still reading: the ring has all three lines, and the reserve is there for it.
    assert_eq!(recent_chat(&st, "sub_1", Platform::Youtube).len(), 3);
    assert!(take(&st, "sub_1", Call::Read).is_ok(), "reading may use the reserve");
    assert!(take(&st, "sub_1", Call::Detect).is_ok(), "so may live checks");
    assert!(take(&st, "sub_1", Call::Send).is_err());
    assert!(take(&st, "sub_1", Call::Moderate).is_err(), "moderation keeps the reserve too");
    let row = view(&st, "sub_1", "i1").unwrap()["channels"][0].clone();
    assert_eq!((row["quota"]["used"].as_i64(), row["quota"]["exhausted"].as_bool()), (Some(112), Some(true)));
    assert_eq!(row["error"]["code"], "youtube_quota");
}

#[tokio::test(start_paused = true)]
async fn the_relay_wide_reserve_stops_replies_and_says_it_is_relay_wide() {
    let yt = Arc::new(FakeYoutube::default());
    // The relay's 1,000 keep 100 for reading; others spent 860 already.
    let (st, _d) = bare_cfg(yt.clone(), yt_cfg(1_000, 1_000).with_reserves(0, 100));
    ready_youtube(&st, "i1", "sub_1", false);
    for _ in 0..17 {
        st.db.yt_take_units("sub_x", Call::Send, None, now_ms()).unwrap();
    }
    st.db.yt_take_units("sub_x", Call::Read, None, now_ms()).unwrap();
    st.db.yt_take_units("sub_x", Call::Read, None, now_ms()).unwrap();
    st.bot.yt("sub_1", |m| m.chat = Some("chat-1".into()));
    st.db.bot_set_live("sub_1", "youtube", Some(now_ms())).unwrap();
    cloud_has_youtube(&st, "sub_1");
    let ch = chan(&st, "sub_1");
    // 860 + 50 = 910 > 900: the relay's reserve; the channel says it's the relay's limit.
    assert!(!say(&st, &ch, Platform::Youtube, "hello", None).await);
    let e = chan(&st, "sub_1").error.unwrap();
    assert_eq!((e["code"].as_str(), e["relay_wide"].as_bool()), (Some("youtube_quota"), Some(true)));
    assert!(take(&st, "sub_1", Call::Read).is_ok(), "reading uses the relay's reserve");
}

#[tokio::test(start_paused = true)]
async fn the_relay_wide_budget_and_googles_own_quota_answer_stop_every_call() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 60, 60);
    ready_youtube(&st, "i1", "sub_1", false);
    // Others already spent most of the relay's 60.
    st.db.yt_take_units("sub_x", Call::Send, None, now_ms()).unwrap();
    st.db.yt_take_units("sub_x", Call::Read, None, now_ms()).unwrap();
    st.db.yt_take_units("sub_x", Call::Read, None, now_ms()).unwrap();
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), 0, "no room left relay-wide");
    assert_eq!(chan(&st, "sub_1").error.unwrap()["relay_wide"], true);
    // Another relay: Google itself says the project's quota is gone.
    let yt2 = Arc::new(FakeYoutube::default());
    let (st2, _d2) = bare(yt2.clone(), 1_500, 9_000);
    ready_youtube(&st2, "i1", "sub_1", false);
    yt2.with(|y| y.detect_fails = Some(YoutubeError::Quota));
    cloud_has_youtube(&st2, "sub_1");
    step(&st2, &chan(&st2, "sub_1")).await;
    assert_eq!(yt2.with(|y| y.detects.len()), 1);
    // Until midnight Pacific nothing more goes to YouTube, whatever the cadence says.
    st2.bot.yt("sub_1", YtMem::retry_now);
    step(&st2, &chan(&st2, "sub_1")).await;
    assert_eq!(yt2.with(|y| y.detects.len()), 1);
    assert!(take(&st2, "sub_1", Call::Send).is_err());
}

#[tokio::test(start_paused = true)]
async fn the_pc_coming_back_stops_the_reader_and_a_takeover_starts_it_again() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: None });
        y.scripts.push_back(Script { items: vec![], hold: true });
        y.scripts.push_back(Script { items: vec![], hold: true });
    });
    cloud_has_youtube(&st, "sub_1");
    tick(&st).await;
    eventually(|| yt.with(|y| y.opens.len()) == 1).await;
    assert_eq!(live_chat(&st, "sub_1").as_deref(), Some("chat-1"));
    // The PC says bot_seen for YouTube: the cloud lets go at the next pass.
    seen_frame(&st, "i1", &json!({ "type": "bot_seen", "platforms": ["youtube"], "youtube_live": true }));
    tick(&st).await;
    assert!(st.bot.yt_reading().is_empty());
    assert_eq!(live_chat(&st, "sub_1"), None);
    // 90 s of silence again: it looks at once (the PC said live) and reads again.
    tokio::time::advance(TAKEOVER_AFTER).await;
    tick(&st).await;
    eventually(|| yt.with(|y| y.opens.len()) == 2).await;
    assert_eq!(yt.with(|y| y.detects.len()), 2);
}

#[tokio::test(start_paused = true)]
async fn offline_moderation_on_youtube_needs_the_opt_in_and_a_live_chat() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    let del = moderate::Action::Delete { message_id: "LCC.x1" };
    let e = moderate::run(&st, "sub_1", Platform::Youtube, del).await.unwrap_err();
    assert_eq!((e.status().as_u16(), e.code), (403, "needs_moderation_scope"));
    assert!(!cloud_state(&st, "sub_1").moderation.youtube);
    // With the opt-in.
    st.db
        .bot_set_account("sub_1", "youtube", (OWNER, "Asha"), Some((OWNER, "Asha")), FORCE_SSL, true, "i1", 1)
        .unwrap();
    assert!(cloud_state(&st, "sub_1").moderation.youtube);
    assert_eq!(cloud_state(&st, "sub_1").moderation.json()["youtube"], true);
    moderate::run(&st, "sub_1", Platform::Youtube, del).await.unwrap();
    // A ban needs the live chat.
    let ban = moderate::Action::Ban { user_id: "UCbad", reason: None };
    let e = moderate::run(&st, "sub_1", Platform::Youtube, ban).await.unwrap_err();
    assert_eq!((e.status().as_u16(), e.code), (409, "youtube_not_live"));
    st.bot.yt("sub_1", |m| m.chat = Some("chat-1".into()));
    moderate::run(&st, "sub_1", Platform::Youtube, ban).await.unwrap();
    let to = moderate::Action::Timeout { user_id: "UCbad", seconds: 300, reason: None };
    moderate::run(&st, "sub_1", Platform::Youtube, to).await.unwrap();
    assert_eq!(
        yt.with(|y| y.mods.clone()),
        vec![
            ("delete".to_string(), String::new(), "LCC.x1".to_string(), None),
            ("ban".to_string(), "chat-1".to_string(), "UCbad".to_string(), None),
            ("timeout".to_string(), "chat-1".to_string(), "UCbad".to_string(), Some(300)),
        ]
    );
    assert_eq!(used(&st, "sub_1", Some(Call::Moderate)), 150);
    // A spent budget: 429 with the reset time.
    // 110 a day keeps 22 for reading: one 50-unit action fits, the second doesn't.
    let (st2, _d2) = bare(Arc::new(FakeYoutube::default()), 110, 9_000);
    ready_youtube(&st2, "i1", "sub_1", true);
    moderate::run(&st2, "sub_1", Platform::Youtube, del).await.unwrap();
    let e = moderate::run(&st2, "sub_1", Platform::Youtube, del).await.unwrap_err();
    assert_eq!((e.status().as_u16(), e.code), (429, "youtube_quota"));
}

#[tokio::test]
async fn a_signed_out_sign_in_blocks_youtube_until_connected_again() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| y.refresh_fails = true);
    check_signin(&st, &chan(&st, "sub_1")).await;
    let ch = chan(&st, "sub_1");
    assert_eq!(ch.error_code(), Some("youtube_signed_out"));
    assert!(!ch.ready());
    assert!(!cloud_state(&st, "sub_1").on);
}

#[tokio::test]
async fn thirty_days_after_pro_ends_youtube_is_revoked_and_forgotten() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    st.bot.yt("sub_1", |m| m.chat = Some("chat-1".into()));
    st.db
        .sync_pro_sub(
            None,
            "sub_1",
            &Synced { status: "cancelled".into(), current_end: Some(now_ms() - 31 * DAY), cancel_at_end: None },
        )
        .unwrap();
    sweep(&st).await;
    assert!(yt.with(|y| y.revoked.contains(&"yrt-1".to_string())));
    assert!(st.db.bot_channel("sub_1", "youtube").unwrap().is_none());
    assert!(st.db.bot_token("sub_1", "youtube", "bot").unwrap().is_none());
    assert_eq!(live_chat(&st, "sub_1"), None);
}

// ── Review fixes (M12): quota thrift, reserves, the stream's resilience ─────────────

/// Steps `sub` once a minute for `mins` minutes; the live checks made meanwhile.
async fn minutes(st: &AppState, yt: &FakeYoutube, sub: &str, mins: u32) -> usize {
    let before = yt.with(|y| y.detects.len());
    for _ in 0..mins {
        tokio::time::advance(Duration::from_secs(60)).await;
        step(st, &chan(st, sub)).await;
    }
    yt.with(|y| y.detects.len()) - before
}

#[tokio::test(start_paused = true)]
async fn idle_live_checks_slow_down_to_hourly_after_six_hours_without_a_stream() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await; // at once on takeover
    assert_eq!(yt.with(|y| y.detects.len()), 1);
    let first_half_hour = minutes(&st, &yt, "sub_1", 30).await;
    assert!((14..=16).contains(&first_half_hour), "every 2 minutes: {first_half_hour}");
    let to_two_hours = minutes(&st, &yt, "sub_1", 90).await;
    assert!((17..=19).contains(&to_two_hours), "every 5 minutes: {to_two_hours}");
    let to_six_hours = minutes(&st, &yt, "sub_1", 240).await;
    assert!((15..=17).contains(&to_six_hours), "every 15 minutes: {to_six_hours}");
    let next_three_hours = minutes(&st, &yt, "sub_1", 180).await;
    assert!((2..=4).contains(&next_three_hours), "hourly: {next_three_hours}");
    // A whole idle day costs well under the old ~300 units.
    let rest_of_day = minutes(&st, &yt, "sub_1", 15 * 60).await;
    let day = used(&st, "sub_1", Some(Call::Detect));
    assert!(day <= 70, "{day} units for a day without a stream ({rest_of_day} in the last 15 h)");
}

#[tokio::test(start_paused = true)]
async fn free_signals_wake_the_live_check_at_once() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    minutes(&st, &yt, "sub_1", 7 * 60).await; // hourly by now
    let n = yt.with(|y| y.detects.len());
    // The same subscription goes live on Twitch: YouTube is looked at on the next pass.
    tokio::time::advance(Duration::from_secs(60)).await;
    set_live(&st, "sub_1", Platform::Twitch, Some(now_ms()));
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), n + 1, "woken by Twitch going live");
    // The PC's hint turning live (it saw the stream start before it went quiet).
    tokio::time::advance(Duration::from_secs(60)).await;
    hint(&st, "sub_1", true);
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), n + 2, "woken by the PC's youtube_live");
    // "Check now" (Try again's route) wakes it too, but never twice within 30 s.
    hint(&st, "sub_1", false);
    tokio::time::advance(Duration::from_secs(10)).await;
    st.bot.yt("sub_1", YtMem::retry_now);
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), n + 2, "30 s after the last look at the soonest");
    tokio::time::advance(WAKE_GAP).await;
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(yt.with(|y| y.detects.len()), n + 3);
}

#[tokio::test(start_paused = true)]
async fn live_checks_have_their_own_daily_cap_per_subscription_and_relay_wide() {
    let yt = Arc::new(FakeYoutube::default());
    let mut cfg = yt_cfg(1_500, 9_000);
    assert_eq!((cfg.detect_per_sub, cfg.detect_units), (150, 2_700), "150 a subscription, 30% of the relay");
    assert_eq!((cfg.read_reserve, cfg.relay_reserve), (300, 900));
    cfg.detect_per_sub = 3;
    cfg.detect_units = 5;
    let (st, _d) = bare_cfg(yt.clone(), cfg);
    ready_youtube(&st, "i1", "sub_1", false);
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    minutes(&st, &yt, "sub_1", 10).await;
    assert_eq!(yt.with(|y| y.detects.len()), 3, "the subscription's cap");
    let e = chan(&st, "sub_1").error.unwrap();
    assert_eq!(e["code"], "youtube_quota");
    assert!(e["message"].as_str().unwrap().contains("live checks"), "{e}");
    // Replies still have their budget.
    assert!(take(&st, "sub_1", Call::Send).is_ok());
    // A second subscription: only 2 left relay-wide.
    ready_youtube_as(&st, "i2", "sub_2", OTHER, "yrt-2", false);
    cloud_has_youtube(&st, "sub_2");
    step(&st, &chan(&st, "sub_2")).await;
    minutes(&st, &yt, "sub_2", 10).await;
    assert_eq!(yt.with(|y| y.detects.len()), 5, "the relay's cap");
    assert_eq!(chan(&st, "sub_2").error.unwrap()["relay_wide"], true);
}

#[tokio::test(start_paused = true)]
async fn failed_opens_back_off_keep_their_streak_across_restarts_and_stop_after_six() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: None });
        for _ in 0..7 {
            y.open_fails.push_back(YoutubeError::Unavailable("reset".into()));
        }
        y.scripts.push_back(Script { items: vec![batch(vec![], "p9")], hold: true });
    });
    cloud_has_youtube(&st, "sub_1");
    tick(&st).await;
    until(|| yt.with(|y| y.opens.len()) >= 3).await;
    // The PC comes back and goes again: the reader restarts, the streak doesn't.
    seen_frame(&st, "i1", &json!({ "type": "bot_seen", "platforms": ["youtube"], "lease": 2 }));
    tick(&st).await;
    assert!(st.bot.yt_reading().is_empty());
    tokio::time::advance(TAKEOVER_AFTER).await;
    for _ in 0..200 {
        tick(&st).await;
        tokio::time::advance(Duration::from_secs(20)).await;
        if chan(&st, "sub_1").error_code() == Some("youtube_unavailable") {
            break;
        }
    }
    let ch = chan(&st, "sub_1");
    assert_eq!(ch.error_code(), Some("youtube_unavailable"));
    assert!(ch.ready() && !ch.blocked(), "retryable, never blocking");
    assert_eq!(yt.with(|y| y.opens.len()), 6, "6 failed opens in all, not 6 after the restart");
    let gaps: Vec<u64> = yt.with(|y| y.opens.windows(2).map(|w| (w[1].0 - w[0].0).as_secs()).collect());
    assert_eq!(&gaps[..2], &[30, 60], "backing off: {gaps:?}");
    assert!(gaps[3..].iter().all(|g| *g >= 240), "{gaps:?}");
    // Stopped: live checks go on (hourly) but nothing opens for an hour.
    for _ in 0..170 {
        tick(&st).await;
        tokio::time::advance(Duration::from_secs(20)).await;
    }
    assert_eq!(yt.with(|y| y.opens.len()), 6, "resting for the hour");
    // An hour on, still live: one more try (it fails: stopped again at once).
    for _ in 0..20 {
        tick(&st).await;
        tokio::time::advance(Duration::from_secs(20)).await;
    }
    until(|| yt.with(|y| y.opens.len()) >= 7).await;
    assert_eq!(st.bot.yt("sub_1", |m| m.fail_streak), 7);
    // Try again: the streak starts over and the next open reads (the error clears).
    let ctx = crate::billing::gate::ProCtx { install: "i1".into(), sub_id: "sub_1".into() };
    retry(&st, &ctx, "youtube", Some(chan(&st, "sub_1"))).await.unwrap();
    for _ in 0..10 {
        tick(&st).await;
        tokio::time::advance(Duration::from_secs(20)).await;
        if yt.with(|y| y.opens.len()) >= 8 {
            break;
        }
    }
    eventually(|| st.bot.yt("sub_1", |m| m.fail_streak) == 0 && chan(&st, "sub_1").error.is_none()).await;
}

#[test]
fn failed_opens_wait_thirty_seconds_doubling_up_to_an_hour() {
    assert_eq!(fail_wait(0), Duration::ZERO);
    assert_eq!(fail_wait(1), Duration::from_secs(30));
    assert_eq!(fail_wait(5), Duration::from_secs(480));
    assert_eq!(fail_wait(8), Duration::from_secs(3_600));
    assert_eq!(fail_wait(30), FAIL_WAIT_MAX);
}

#[tokio::test(start_paused = true)]
async fn a_silent_stream_is_opened_again_from_the_page_token() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: None });
        // One page, then the stream hangs (no close, no error), twice more after that.
        y.scripts.push_back(Script { items: vec![batch(vec![], "p1")], hold: true });
        y.scripts.push_back(Script { items: vec![], hold: true });
        y.scripts.push_back(Script { items: vec![], hold: true });
    });
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    until(|| yt.with(|y| y.opens.len()) >= 3).await;
    let opens = yt.with(|y| y.opens.clone());
    assert_eq!(opens[1].1.as_deref(), Some("p1"), "resumed from the page token");
    let gaps: Vec<u64> = opens.windows(2).map(|w| (w[1].0 - w[0].0).as_secs()).collect();
    // 90 s silent, opened again at once; then 90 s + 30 s (two silences in a row: a failure).
    assert_eq!(gaps, vec![90, 120]);
    // One silence isn't a failure; two in a row are.
    assert_eq!(st.bot.yt("sub_1", |m| m.fail_streak), 1);
    assert_eq!(FEED_IDLE, Duration::from_secs(90));
}

#[tokio::test(start_paused = true)]
async fn live_checks_run_eight_at_a_time_and_never_one_after_another() {
    let yt = Arc::new(FakeYoutube::default());
    yt.with(|y| y.detect_takes = Some(Duration::from_secs(10)));
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    for i in 0..12 {
        let (install, sub) = (format!("i{i}"), format!("sub_{i}"));
        ready_youtube_as(&st, &install, &sub, &format!("UCchan{i:02}"), &format!("yrt-{i}"), false);
        cloud_has_youtube(&st, &sub);
    }
    let started = tokio::time::Instant::now();
    tick(&st).await;
    assert_eq!(yt.with(|y| y.detects.len()), 12);
    assert_eq!(yt.with(|y| y.max_in_flight), CHECK_CONCURRENCY as u32);
    assert_eq!(CHECK_CONCURRENCY, 8);
    assert!(started.elapsed() <= Duration::from_secs(21), "two rounds, not twelve: {:?}", started.elapsed());
}

#[tokio::test(start_paused = true)]
async fn an_older_pc_naming_platforms_without_the_lease_version_covers_youtube_too() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    // A PC from before M12: names Twitch, never YouTube, but answers YouTube itself.
    let old = json!({ "type": "bot_seen", "platforms": ["twitch"] });
    assert_eq!(seen_platforms(&old), vec![Platform::Twitch, Platform::Youtube]);
    tokio::time::advance(TAKEOVER_AFTER).await;
    for _ in 0..10 {
        seen_frame(&st, "i1", &old);
        step(&st, &chan(&st, "sub_1")).await;
        tokio::time::advance(Duration::from_secs(30)).await;
    }
    assert_eq!(yt.with(|y| y.detects.len()), 0, "the cloud never answers YouTube alongside it");
    // A current PC says `lease: 2`: what it names is all it answers.
    let new = json!({ "type": "bot_seen", "platforms": ["twitch"], "lease": 2 });
    assert_eq!(seen_platforms(&new), vec![Platform::Twitch]);
    assert_eq!(LEASE_VERSION, 2);
}

#[tokio::test(start_paused = true)]
async fn a_spent_day_notice_clears_once_the_reset_has_passed() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    let s = Spent { relay_wide: false, reset_at: now_ms() + 60_000, checks: false };
    st.db.bot_set_error("sub_1", "youtube", Some(&spent_error(s))).unwrap();
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(chan(&st, "sub_1").error_code(), Some("youtube_quota"), "not yet");
    let s = Spent { relay_wide: false, reset_at: now_ms() - 1, checks: false };
    st.db.bot_set_error("sub_1", "youtube", Some(&spent_error(s))).unwrap();
    // Even while the PC answers YouTube (no call), the stale notice goes.
    desktop_seen(&st, "i1", &[Platform::Youtube]);
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(chan(&st, "sub_1").error, None);
}

#[tokio::test(start_paused = true)]
async fn units_are_taken_only_once_the_sign_in_works_and_a_401_retry_is_counted() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| y.refresh_fails = true);
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    assert_eq!(used(&st, "sub_1", None), 0, "no sign-in, no units");
    // A send Google refuses with 401 once: that one keeps a unit, the retry counts 50.
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| {
        y.refresh_fails = false;
        y.send_401_once = true;
    });
    st.bot.yt("sub_1", |m| m.chat = Some("chat-1".into()));
    send(&st, &chan(&st, "sub_1"), "hi").await.unwrap();
    assert_eq!(used(&st, "sub_1", Some(Call::Send)), 51);
    assert_eq!(used(&st, RELAY_SUB, None), 51);
}

#[tokio::test(start_paused = true)]
async fn googles_quota_stop_outlives_a_restart_and_unused_locks_go() {
    let yt = Arc::new(FakeYoutube::default());
    let dir = tempfile::tempdir().unwrap();
    let st = on_db(yt.clone(), yt_cfg(1_500, 9_000), &dir);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| y.detect_fails = Some(YoutubeError::Quota));
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    assert!(take(&st, "sub_1", Call::Read).is_err());
    // A restart: the stop is read back, so nothing goes to Google until midnight Pacific.
    let st2 = on_db(yt.clone(), yt_cfg(1_500, 9_000), &dir);
    assert!(take(&st2, "sub_1", Call::Read).is_ok(), "not loaded yet");
    load_google_out(&st2);
    assert_eq!(take(&st2, "sub_1", Call::Read).unwrap_err().map(|s| s.reset_at), Some(pacific_day(now_ms()).1));
    // A refresh lock for a subscription that no longer has YouTube is dropped by the pass.
    access(&st2, "sub_1").await.unwrap();
    access(&st2, "sub_gone").await.ok();
    assert_eq!(st2.bot.yt_lock_count(), 2);
    tick(&st2).await;
    assert_eq!(st2.bot.yt_lock_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn timers_on_youtube_post_at_most_one_every_fifteen_minutes() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    assert_eq!(st.bot.youtube_cfg().unwrap().timer_gap, TIMER_GAP);
    assert_eq!(TIMER_GAP, Duration::from_secs(15 * 60));
    ready_youtube(&st, "i1", "sub_1", false);
    // Two timers due at once (every 5 minutes, the shortest; the takeover 6 minutes ago).
    let mut cfg = config();
    cfg["timers"] = json!([
        { "id": "t1", "name": "a", "message": "one", "every_min": 5, "min_chat_lines": 0, "platforms": [], "enabled": true },
        { "id": "t2", "name": "b", "message": "two", "every_min": 5, "min_chat_lines": 0, "platforms": [], "enabled": true }
    ]);
    let cfg: ferrocast_core::cloudbot::BotConfig = serde_json::from_value(cfg).unwrap();
    st.db.bot_push_config("sub_1", 1, &serde_json::to_string(&cfg).unwrap(), 0, 2).unwrap();
    st.db.bot_set_live("sub_1", "youtube", Some(now_ms() - 600_000)).unwrap();
    st.bot.yt("sub_1", |m| m.chat = Some("chat-1".into()));
    let past = Instant::now().checked_sub(TAKEOVER_AFTER + Duration::from_secs(360)).unwrap();
    st.bot.lease("sub_1", Platform::Youtube, |l| *l = Lease::new(past, now_ms() - 451_000));
    timer_tick(&st).await;
    assert_eq!(yt.with(|y| y.sent.len()), 1, "one of the two");
    // The slot: closed for 15 minutes, then open again.
    tokio::time::advance(Duration::from_secs(14 * 60)).await;
    assert!(!timer_slot(&st, "sub_1"));
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(timer_slot(&st, "sub_1"));
    // Configurable (0 = off), checked.
    assert!(yt_cfg(1_500, 9_000).with_timer_gap_min(-1).is_err());
    assert!(yt_cfg(1_500, 9_000).with_timer_gap_min(2_000).is_err());
    assert_eq!(yt_cfg(1_500, 9_000).with_timer_gap_min(0).unwrap().timer_gap, Duration::ZERO);
}

#[test]
fn a_busy_day_splits_googles_pot_fairly_but_never_below_the_floor() {
    let cfg = yt_cfg(1_500, 9_000);
    // Alone or a few: the subscription's own limit.
    assert_eq!(fair_limit(&cfg, 1), 1_500);
    assert_eq!(fair_limit(&cfg, 6), 1_500);
    // Ten streamers today: 900 each; fifty: the floor.
    assert_eq!(fair_limit(&cfg, 10), 900);
    assert_eq!(fair_limit(&cfg, 50), FAIR_FLOOR);
    // Replies keep the read reserve inside the fair share.
    let r = fair_room(&cfg, Call::Send, 10);
    assert_eq!(r.sub, 900 - cfg.read_reserve);
    assert_eq!(fair_room(&cfg, Call::Read, 10).sub, 900);
    assert_eq!(fair_room(&cfg, Call::Read, 1), Room::of(&cfg, Call::Read));
}

#[test]
fn who_shares_today_counts_only_subscriptions_that_spent_units() {
    let (st, _d) = bare(Arc::new(FakeYoutube::default()), 1_500, 9_000);
    let now = utc(2026, 10, 19, 20, 0);
    assert_eq!(st.db.yt_active_today("me", now).unwrap(), 1, "me, even before my first call");
    for s in ["a", "b", "me"] {
        st.db.yt_take_units(s, Call::Detect, None, now).unwrap();
    }
    assert_eq!(st.db.yt_active_today("me", now).unwrap(), 3, "a, b and me; the relay's own row isn't one");
    assert_eq!(st.db.yt_active_today("me", utc(2026, 10, 20, 8, 0)).unwrap(), 1, "a new Pacific day");
}

// ── Cloud mode (docs/CLOUD.md §11) ───────────────────────────────────────────────────

/// `sub` attached to a new account with cloud mode on; the account id.
fn cloud_account_for(st: &AppState, email: &str, sub: &str) -> String {
    let (a, _) = st.db.account_sign_in(email, None, now_ms()).unwrap();
    st.db.account_attach_sub(&a.id, sub, "checkout", now_ms()).unwrap();
    st.db.cloud_enable(&a.id, now_ms()).unwrap();
    a.id
}

#[tokio::test(start_paused = true)]
async fn a_cloud_mode_account_sees_youtube_chat_on_its_bus_with_the_bot_off() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    st.db.bot_set_enabled("sub_1", false, 1).unwrap();
    let account = cloud_account_for(&st, "asha@example.com", "sub_1");
    // A dashboard is open, so chat is kept on the bus.
    let bus = st.cloud.buses.open(&account, 10).unwrap();
    bus.state().dashboards = 1;
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: None });
        y.scripts.push_back(Script { items: vec![batch(vec![line("LCC.c1", "UCviewer", "!lurk")], "p1")], hold: true });
    });
    cloud_has_youtube(&st, "sub_1");
    // The bot is off, but the account's cloud mode makes the reader run.
    tick(&st).await;
    until(|| bus.state().chat.len() == 1).await;
    let e = bus.state().chat[0].clone();
    assert_eq!(e.platform, Platform::Youtube);
    assert!(yt.with(|y| y.sent.is_empty()), "the bot is off: nothing answered");
    // Units are the subscription's own (1 detect + 1 open), nothing extra for the bus.
    assert_eq!(used(&st, "sub_1", None), 1 + 5);

    // Cloud mode asleep: the reader stops for it (the bot is still off).
    st.db.cloud_sleep_idle(now_ms() + 1, now_ms()).unwrap();
    tick(&st).await;
    assert!(st.bot.yt_reading().is_empty(), "nobody needs YouTube now");
}

#[tokio::test(start_paused = true)]
async fn the_bot_page_drives_youtube_for_an_account_without_a_pc() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    let account = cloud_account_for(&st, "asha@example.com", "sub_1");
    // With a PC linked, the PC's pushed config stays YouTube's.
    assert_eq!(st.db.pro_installs("sub_1").unwrap(), 1);
    assert!(super::youtube_follows_account(&st, &account).is_none());
    let before = st.db.bot_config("sub_1").unwrap().unwrap();
    let mut cfg: ferrocast_core::cloudbot::BotConfig = serde_json::from_value(config()).unwrap();
    cfg.commands.retain(|c| c.name != "!lurk");
    super::account_save_config(&st, &account, 0, cfg.clone()).unwrap();
    assert_eq!(st.db.bot_config("sub_1").unwrap().unwrap().version, before.version, "untouched");

    // No PC: the Bot page's settings and switch are YouTube's too.
    st.db.c().execute("DELETE FROM pro_installs WHERE sub_id = 'sub_1'", []).unwrap();
    assert_eq!(super::youtube_follows_account(&st, &account).as_deref(), Some("sub_1"));
    let (v, _) = super::account_config(&st, &account).unwrap();
    super::account_save_config(&st, &account, v, cfg).unwrap();
    let mirrored: ferrocast_core::cloudbot::BotConfig =
        serde_json::from_str(&st.db.bot_config("sub_1").unwrap().unwrap().json).unwrap();
    assert!(!mirrored.commands.iter().any(|c| c.name == "!lurk"), "the YouTube bot has the page's commands");
    let view = super::account_set_enabled(&st, &account, false).await.unwrap();
    assert!(!st.db.bot_config("sub_1").unwrap().unwrap().enabled);
    assert_eq!(view["youtube"]["follows_this_page"], true);
    assert_eq!(view["enabled"], false);
    let view = super::account_set_enabled(&st, &account, true).await.unwrap();
    assert!(st.db.bot_config("sub_1").unwrap().unwrap().enabled, "only YouTube: its switch");
    assert_eq!(view["enabled"], true);
}

/// A connected, switched-on YouTube channel no PC ever pushed a config for (the web connect
/// with the bot switched on from the Account page).
fn youtube_without_a_pc_config(st: &AppState, install: &str, sub: &str) {
    pro_sub(st, install, sub);
    st.db.bot_set_account(sub, "youtube", (OWNER, "Asha"), Some((OWNER, "Asha")), FORCE_SSL, false, "web", 1).unwrap();
    seal_token(st, sub, "youtube", "bot", "yrt-1");
    st.db.bot_set_enabled(sub, true, 1).unwrap();
}

#[tokio::test(start_paused = true)]
async fn with_no_pc_config_the_youtube_bot_answers_with_the_cloud_bot_pages_commands() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    youtube_without_a_pc_config(&st, "i1", "sub_1");
    assert_eq!(st.db.bot_config("sub_1").unwrap().unwrap().version, 0);
    let account = cloud_account_for(&st, "arx@example.com", "sub_1");
    let mut cfg: ferrocast_core::cloudbot::BotConfig = serde_json::from_value(config()).unwrap();
    cfg.commands[0].name = "!ferrocast".into();
    cfg.commands[0].response = "Ferrocast runs this chat.".into();
    super::account_save_config(&st, &account, 0, cfg).unwrap();
    // Nothing written under the subscription (a PC may still push), and a fresh relay reads
    // the page's config too.
    assert_eq!(st.db.bot_config("sub_1").unwrap().unwrap().version, 0);
    st.bot.mem("sub_1", |m| m.config = None);
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: None });
        // History first (as YouTube's first answer), then the owner's command.
        let mut old = line("LCC.old", OWNER, "!ferrocast");
        old.origin = Origin::Backfill;
        y.scripts.push_back(Script {
            items: vec![batch(vec![old], "p1"), batch(vec![line("LCC.new", OWNER, "!ferrocast please")], "p2")],
            hold: true,
        });
    });
    cloud_has_youtube(&st, "sub_1");
    step(&st, &chan(&st, "sub_1")).await;
    until(|| yt.with(|y| !y.sent.is_empty())).await;
    assert_eq!(yt.with(|y| y.sent.clone()), vec![("chat-1".to_string(), "Ferrocast runs this chat.".to_string())]);
}

#[tokio::test(start_paused = true)]
async fn a_stream_youtube_ends_at_once_every_time_stops_and_says_so() {
    let yt = Arc::new(FakeYoutube::default());
    let (st, _d) = bare(yt.clone(), 1_500, 9_000);
    ready_youtube(&st, "i1", "sub_1", false);
    yt.with(|y| {
        y.live = Some(Broadcast { live_chat_id: "chat-1".into(), started_at: None });
        for _ in 0..5 {
            y.scripts.push_back(Script { items: vec![Err(YoutubeError::ChatEnded)], hold: false });
        }
    });
    cloud_has_youtube(&st, "sub_1");
    for _ in 0..30 {
        step(&st, &chan(&st, "sub_1")).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        if chan(&st, "sub_1").error_code() == Some("youtube_chat_unavailable") {
            break;
        }
        tokio::time::advance(Duration::from_secs(120)).await;
    }
    let ch = chan(&st, "sub_1");
    assert_eq!(ch.error_code(), Some("youtube_chat_unavailable"), "{:?}", ch.error);
    assert!(ch.error.as_ref().unwrap()["hint"].as_str().unwrap().contains("Try again"));
    let opens = yt.with(|y| y.opens.len());
    assert_eq!(opens, QUICK_ENDS_GIVE_UP as usize);
    // It rests: no more opens for a while, so no units go on a chat that won't open.
    for _ in 0..10 {
        tokio::time::advance(Duration::from_secs(120)).await;
        step(&st, &chan(&st, "sub_1")).await;
    }
    assert_eq!(yt.with(|y| y.opens.len()), opens);
}
