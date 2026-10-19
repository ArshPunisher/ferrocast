//! Pro's pre-sales load test (docs/plan/PRO.md Gates; results in docs/plan/LOAD-TEST.md).
//! Ignored, so it never runs in CI. Run it with a release build:
//!
//! ```text
//! cargo test -p ferrocast-relay --release -- --ignored load_ --nocapture --test-threads=1
//! ```
//!
//! `load_200_subscriptions` (the driver) starts the fakes that live outside the relay (Razorpay
//! in test mode, Kick's OAuth, users and public-key endpoints), then runs this same test
//! binary again as a child process with only `load_child_relay`, which serves the real
//! `router()` on 127.0.0.1 with a temporary database, a file blob store and fake providers
//! (TTS, Twitch, Kick, Discord, mail, Cloudflare and DNS). Its memory and CPU are therefore
//! the relay's own (sampled with `ps`). The driver sets everything up through the HTTP API
//! only (signed Razorpay webhooks make 200 subscriptions with 1 to 3 installs each Pro), then
//! runs a busy hour at real rates for `LOAD_SECS` (default 600) seconds while the hourly
//! sweeps run every `LOAD_SWEEP_S` (default 60) seconds, and checks the pass criteria: no 5xx,
//! p95 under 200 ms for API routes, RSS under 700 MB, no SQLite "database is locked".
//! `LOAD_OUT` names a JSON file for the numbers.
//!
//! Nothing here talks to the live relay or any real provider, and no key is real.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rsa::sha2::{Digest, Sha256};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::bot::{KickBot, KickError, KickTokens, ModTarget, TokenInfo, TwitchCloud, TwitchError, UserTokens};
use crate::discord::{Channel, Connection, DiscordApi, Guild, Role, Token};
use crate::{AppState, Config, Db, now_ms, router};

const ES_SECRET: &str = "load-eventsub-secret-0123456789";
const DESKTOP_APP: &str = "load-desktop-client";
const WEBHOOK_SECRET: &str = "load-razorpay-webhook-secret-0123";
const EMAIL_KEY: &str = "load-email-key-0123456789abcdef0123";
const PEPPER: &[u8] = b"load test pepper, sixteen bytes or more";
const SECRET: [u8; 32] = [9u8; 32];

const SUBS: usize = 200;
const BOT_SUBS: usize = 100; // 0..100 cloud bot on Twitch
const KICK_SUBS: usize = 50; // 0..50 also on Kick
const DISCORD_SUBS: usize = 40; // 0..40 Discord perks
const TUNNEL_SUBS: std::ops::Range<usize> = 100..150; // 50 PCs with a tunnel and 1–2 phones
const TIP_SUBS: std::ops::Range<usize> = 150..180; // 30 branded tip pages
const DOMAIN_SUBS: std::ops::Range<usize> = 150..160; // 10 of them on their own address
const BACKUP_SUBS: std::ops::Range<usize> = 180..190; // 10 uploads of 20 MB

fn day() -> i64 {
    86_400_000
}

// ── Small helpers ────────────────────────────────────────────────────────────────────

/// RFC 3339 in UTC with milliseconds, as Twitch and Kick send it.
fn rfc3339(ms: i64) -> String {
    let (y, mo, d) = crate::quota::civil(ms.div_euclid(day()));
    let s = ms.rem_euclid(day()) / 1000;
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// A tiny deterministic generator (SplitMix64) so runs are comparable.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

// ── Fakes inside the relay process ───────────────────────────────────────────────────

/// Fake Twitch. Identities are spelled out in the codes and tokens, so the driver needs no
/// back channel: code `tw.<bot|channel>.<user id>.<login>`, the PC's token `desk.<user id>`.
#[derive(Debug, Default)]
struct LoadTwitch {
    sent: AtomicU64,
    subscribed: AtomicU64,
    unsubscribed: AtomicU64,
    revoked: AtomicU64,
}

fn tw_who(token: &str) -> Option<TokenInfo> {
    if let Some(id) = token.strip_prefix("desk.") {
        return Some(TokenInfo {
            user_id: id.into(),
            login: format!("s{id}"),
            scopes: vec![],
            client_id: DESKTOP_APP.into(),
        });
    }
    let code = token.strip_prefix("at.")?;
    let mut p = code.split('.');
    let (_, role, id, login) = (p.next()?, p.next()?, p.next()?, p.next()?);
    let scopes = if role == "bot" { crate::bot::twitch::BOT_SCOPES } else { crate::bot::twitch::CHANNEL_SCOPES };
    Some(TokenInfo {
        user_id: id.into(),
        login: login.into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        client_id: "load-cloud-client".into(),
    })
}

impl TwitchCloud for LoadTwitch {
    fn exchange_code<'a>(&'a self, code: &'a str, _r: &'a str) -> crate::bot::Fut<'a, UserTokens, TwitchError> {
        Box::pin(async move {
            let who = tw_who(&format!("at.{code}")).ok_or(TwitchError::BadRequest("bad code".into()))?;
            Ok(UserTokens {
                access_token: format!("at.{code}"),
                refresh_token: format!("rt.{code}"),
                scopes: who.scopes,
            })
        })
    }
    fn refresh<'a>(&'a self, rt: &'a str) -> crate::bot::Fut<'a, UserTokens, TwitchError> {
        Box::pin(async move {
            let code = rt.strip_prefix("rt.").unwrap_or(rt);
            Ok(UserTokens { access_token: format!("at.{code}"), refresh_token: rt.to_string(), scopes: vec![] })
        })
    }
    fn validate<'a>(&'a self, at: &'a str) -> crate::bot::Fut<'a, TokenInfo, TwitchError> {
        Box::pin(async move { tw_who(at).ok_or(TwitchError::Unauthorized) })
    }
    fn revoke<'a>(&'a self, _at: &'a str) -> crate::bot::Fut<'a, (), TwitchError> {
        Box::pin(async move {
            self.revoked.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }
    fn app_token(&self) -> crate::bot::Fut<'_, (String, u64), TwitchError> {
        Box::pin(async move { Ok(("app-token".to_string(), 3_600)) })
    }
    fn conduit<'a>(&'a self, _a: &'a str, _cb: &'a str, _s: &'a str) -> crate::bot::Fut<'a, String, TwitchError> {
        Box::pin(async move { Ok("conduit-load".to_string()) })
    }
    fn subscribe<'a>(
        &'a self,
        _app: &'a str,
        _conduit: &'a str,
        _kind: &'a str,
        _condition: Value,
    ) -> crate::bot::Fut<'a, String, TwitchError> {
        Box::pin(async move { Ok(format!("es-{}", self.subscribed.fetch_add(1, Ordering::Relaxed))) })
    }
    fn find_subscription<'a>(
        &'a self,
        _app: &'a str,
        _kind: &'a str,
        _condition: &'a Value,
    ) -> crate::bot::Fut<'a, Option<String>, TwitchError> {
        Box::pin(async move { Ok(None) })
    }
    fn unsubscribe<'a>(&'a self, _app: &'a str, _id: &'a str) -> crate::bot::Fut<'a, (), TwitchError> {
        Box::pin(async move {
            self.unsubscribed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }
    fn send_chat<'a>(
        &'a self,
        _app: &'a str,
        _broadcaster: &'a str,
        _sender: &'a str,
        _text: &'a str,
        _reply_to: Option<&'a str>,
    ) -> crate::bot::Fut<'a, Option<String>, TwitchError> {
        Box::pin(async move { Ok(Some(format!("sent-{}", self.sent.fetch_add(1, Ordering::Relaxed)))) })
    }
    fn stream_started<'a>(&'a self, _app: &'a str, _b: &'a str) -> crate::bot::Fut<'a, Option<i64>, TwitchError> {
        Box::pin(async move { Ok(Some(now_ms() - 3_600_000)) })
    }
    fn ban<'a>(&'a self, _u: &'a str, _t: ModTarget<'a>, _s: Option<u32>) -> crate::bot::Fut<'a, (), TwitchError> {
        Box::pin(async move { Ok(()) })
    }
    fn delete_message<'a>(
        &'a self,
        _u: &'a str,
        _t: ModTarget<'a>,
        _m: &'a str,
    ) -> crate::bot::Fut<'a, (), TwitchError> {
        Box::pin(async move { Ok(()) })
    }
}

#[derive(Debug, Default)]
struct LoadKick {
    sent: AtomicU64,
    refreshed: AtomicU64,
    revoked: AtomicU64,
}

impl KickBot for LoadKick {
    fn refresh<'a>(&'a self, rt: &'a str) -> crate::bot::Fut<'a, KickTokens, KickError> {
        Box::pin(async move {
            let n = self.refreshed.fetch_add(1, Ordering::Relaxed);
            Ok(KickTokens { access_token: format!("kat-{n}"), refresh_token: rt.to_string(), expires_in: 7_200 })
        })
    }
    fn send_chat<'a>(
        &'a self,
        _access: &'a str,
        _b: &'a str,
        _text: &'a str,
        _reply: Option<&'a str>,
    ) -> crate::bot::Fut<'a, Option<String>, KickError> {
        Box::pin(async move { Ok(Some(format!("ksent-{}", self.sent.fetch_add(1, Ordering::Relaxed)))) })
    }
    fn revoke<'a>(&'a self, _t: &'a str) -> crate::bot::Fut<'a, (), KickError> {
        Box::pin(async move {
            self.revoked.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }
    fn ban<'a>(
        &'a self,
        _a: &'a str,
        _b: &'a str,
        _u: &'a str,
        _m: Option<u32>,
        _r: Option<&'a str>,
    ) -> crate::bot::Fut<'a, (), KickError> {
        Box::pin(async move { Ok(()) })
    }
    fn delete_message<'a>(&'a self, _a: &'a str, _m: &'a str) -> crate::bot::Fut<'a, (), KickError> {
        Box::pin(async move { Ok(()) })
    }
}

/// Fake Discord: code `dc.<guild id>` installs the bot in that server; every server has a
/// text channel, a "Subscriber" role under Ferrocast's own, and no members.
#[derive(Debug, Default)]
struct LoadDiscord {
    calls: AtomicU64,
    left: AtomicU64,
}

impl LoadDiscord {
    fn tick(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

impl DiscordApi for LoadDiscord {
    fn exchange_code<'a>(&'a self, code: &'a str, _r: &'a str) -> crate::discord::Fut<'a, Token> {
        Box::pin(async move {
            self.tick();
            let guild =
                code.strip_prefix("dc.").map(|g| Guild { id: g.into(), name: format!("Server {g}"), icon: None });
            Ok(Token { access_token: format!("dat.{code}"), guild })
        })
    }
    fn revoke<'a>(&'a self, _t: &'a str) -> crate::discord::Fut<'a, ()> {
        Box::pin(async move {
            self.tick();
            Ok(())
        })
    }
    fn me<'a>(&'a self, _t: &'a str) -> crate::discord::Fut<'a, String> {
        Box::pin(async move { Ok("9999".to_string()) })
    }
    fn connections<'a>(&'a self, _t: &'a str) -> crate::discord::Fut<'a, Vec<Connection>> {
        Box::pin(async move { Ok(vec![]) })
    }
    fn guild<'a>(&'a self, g: &'a str) -> crate::discord::Fut<'a, Guild> {
        Box::pin(async move {
            self.tick();
            Ok(Guild { id: g.into(), name: format!("Server {g}"), icon: None })
        })
    }
    fn channels<'a>(&'a self, g: &'a str) -> crate::discord::Fut<'a, Vec<Channel>> {
        Box::pin(async move {
            self.tick();
            Ok(vec![Channel { id: snow(g, 30_000), name: "going-live".into(), kind: 0, position: 0 }])
        })
    }
    fn roles<'a>(&'a self, g: &'a str) -> crate::discord::Fut<'a, Vec<Role>> {
        Box::pin(async move {
            self.tick();
            let r =
                |id: String, name: &str, position, managed| Role { id, name: name.into(), color: 0, position, managed };
            Ok(vec![
                r(g.to_string(), "@everyone", 0, false),
                r(snow(g, 10_000), "Subscriber", 2, false),
                r(snow(g, 20_000), "Ferrocast", 5, true),
            ])
        })
    }
    fn bot_roles<'a>(&'a self, g: &'a str) -> crate::discord::Fut<'a, Vec<String>> {
        Box::pin(async move { Ok(vec![snow(g, 20_000)]) })
    }
    fn add_role<'a>(&'a self, _g: &'a str, _u: &'a str, _r: &'a str) -> crate::discord::Fut<'a, ()> {
        Box::pin(async move {
            self.tick();
            Ok(())
        })
    }
    fn remove_role<'a>(&'a self, _g: &'a str, _u: &'a str, _r: &'a str) -> crate::discord::Fut<'a, ()> {
        Box::pin(async move {
            self.tick();
            Ok(())
        })
    }
    fn post<'a>(&'a self, _c: &'a str, _b: Value) -> crate::discord::Fut<'a, String> {
        Box::pin(async move {
            self.tick();
            Ok("m1".to_string())
        })
    }
    fn edit<'a>(&'a self, _c: &'a str, _m: &'a str, _b: Value) -> crate::discord::Fut<'a, ()> {
        Box::pin(async move { Ok(()) })
    }
    fn leave<'a>(&'a self, _g: &'a str) -> crate::discord::Fut<'a, ()> {
        Box::pin(async move {
            self.left.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }
}

/// A Discord id (snowflake) next to a server's: its roles and channel.
fn snow(guild: &str, plus: u64) -> String {
    (guild.parse::<u64>().unwrap_or(880_000_000_000_000_000) + plus).to_string()
}

fn discord_cfg() -> crate::discord::DiscordConfig {
    let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    #[allow(clippy::unwrap_used)]
    crate::discord::DiscordConfig::from_values(
        Some("1561463863197835294".into()),
        Some("load-bot-token".into()),
        Some("load-client-secret".into()),
        Some(hex(key.verifying_key().as_bytes())),
    )
    .unwrap()
    .unwrap()
}

/// How long each sweep took, worst and total, in ms.
#[derive(Debug, Default)]
struct SweepTimes(Mutex<BTreeMap<&'static str, (u64, u64, u64)>>);
impl SweepTimes {
    fn note(&self, what: &'static str, d: Duration) {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let mut m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let e = m.entry(what).or_default();
        e.0 = e.0.max(ms);
        e.1 += ms;
        e.2 += 1;
    }
    fn json(&self) -> Value {
        let m = self.0.lock().unwrap_or_else(|p| p.into_inner());
        Value::Object(
            m.iter()
                .map(|(k, (max, total, n))| ((*k).to_string(), json!({ "max_ms": max, "total_ms": total, "runs": n })))
                .collect(),
        )
    }
}

/// The relay under test, in its own process. Does nothing unless the driver started it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load test: run with --ignored load_ in a release build"]
async fn load_child_relay() {
    if std::env::var("LOAD_CHILD").ok().as_deref() != Some("1") {
        eprintln!("load_child_relay only runs as the load test's child process");
        return;
    }
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
    let dir = std::path::PathBuf::from(env("LOAD_DIR"));
    let log = std::fs::File::create(env("LOAD_LOG")).unwrap();
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(log))
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
        .try_init();
    let kick_url = env("LOAD_KICK_URL");
    let db = Db::open(&dir.join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: None,
        kick_client_id: Some("load-kick-client".into()),
        kick_client_secret: Some("load-kick-secret".into()),
        kick_id_url: kick_url.clone(),
        kick_api_url: kick_url,
    };
    let billing = crate::billing::BillingConfig {
        key_id: Some(ferrocast_testkit::razorpay::KEY_ID.into()),
        key_secret: Some(ferrocast_testkit::razorpay::KEY_SECRET.into()),
        webhook_secret: Some(WEBHOOK_SECRET.into()),
        email_key: Some(EMAIL_KEY.into()),
        plan_monthly: Some(env("LOAD_PLAN_M")),
        plan_yearly: Some(env("LOAD_PLAN_Y")),
        api_url: env("LOAD_RZP_URL"),
        sales_open: true,
    };
    let vault = crate::vault::Vault::from_base64(&base64::engine::general_purpose::STANDARD.encode(SECRET)).unwrap();
    let blobs: Arc<dyn crate::blob::BlobStore> = Arc::new(crate::blob::FsBlobStore::new(dir.join("blobs")).unwrap());
    let tts = Arc::new(crate::tts::FakeTts::new());
    let twitch = Arc::new(LoadTwitch::default());
    let kick = Arc::new(LoadKick::default());
    let discord = Arc::new(LoadDiscord::default());
    let mailer = Arc::new(crate::mail::FakeMailer::new());
    let cf = Arc::new(crate::tips::domains::FakeHostnames::new());
    let dns = Arc::new(crate::tips::domains::FakeDns::new());
    #[allow(clippy::unwrap_used)]
    let twitch_cfg = crate::bot::TwitchConfig::from_values(
        Some("load-cloud-client".into()),
        Some("load-cloud-secret".into()),
        Some(ES_SECRET.into()),
    )
    .unwrap()
    .unwrap()
    .with_desktop_client_id(Some(DESKTOP_APP.into()));
    let state = AppState::new(cfg, db)
        .with_billing(billing)
        .with_vault(Some(vault))
        .with_blobs(Some(blobs))
        .with_tts(Some(tts.clone() as Arc<dyn crate::tts::TtsProvider>), 800_000)
        .with_remote(crate::remote::RemoteConfig::new(Some(&base), Some(PEPPER), None))
        .with_discord(crate::discord::Runtime::new(Some(discord_cfg()), Some(discord.clone() as Arc<dyn DiscordApi>)))
        .with_bot(crate::bot::Runtime::new(
            Some(twitch_cfg),
            Some(twitch.clone() as Arc<dyn TwitchCloud>),
            Some(kick.clone() as Arc<dyn KickBot>),
        ))
        .with_mail(
            Some(mailer.clone() as Arc<dyn crate::mail::Mailer>),
            crate::mail::MailConfig::new(None, Some(&SECRET)),
        )
        .with_tip_domains(
            Some(cf.clone() as Arc<dyn crate::tips::domains::CustomHostnames>),
            Some(dns.clone() as Arc<dyn crate::tips::domains::ProofDns>),
            Some(&SECRET),
            crate::tips::domains::MAX_DOMAINS,
        );
    crate::tips::domains::warm_up(&state);

    // The relay's own background work, as in main.rs, but every LOAD_SWEEP_S seconds.
    let times = Arc::new(SweepTimes::default());
    tokio::spawn(crate::bot::run(state.clone()));
    let every = std::env::var("LOAD_SWEEP_S").ok().and_then(|s| s.parse().ok()).unwrap_or(60u64);
    {
        let (st, times) = (state.clone(), times.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(every)).await;
                let t = Instant::now();
                let _ = st.db.prune();
                times.note("housekeeping", t.elapsed());
                let t = Instant::now();
                crate::tips::sweep(&st).await;
                times.note("tips", t.elapsed());
                let t = Instant::now();
                crate::backups::sweep(&st).await;
                times.note("backups", t.elapsed());
                let t = Instant::now();
                crate::remote::sweep(&st).await;
                times.note("remote", t.elapsed());
                let t = Instant::now();
                crate::discord::sweep(&st).await;
                times.note("discord", t.elapsed());
                let t = Instant::now();
                crate::mail::sweep(&st, now_ms()).await;
                times.note("mail", t.elapsed());
                let t = Instant::now();
                crate::bot::sweep(&st).await;
                times.note("bot", t.elapsed());
            }
        });
    }

    // Test-only routes for the driver: confirm links, a working custom address, the weekly
    // run at a chosen moment, and what the fakes saw.
    let debug = {
        use axum::routing::{get, post};
        let (m, st, cf2, dns2) = (mailer.clone(), state.clone(), cf.clone(), dns.clone());
        let st_w = state.clone();
        let st_s = state.clone();
        let (tw, kk, dc, tt, ml, tm) =
            (twitch.clone(), kick.clone(), discord.clone(), tts.clone(), mailer.clone(), times.clone());
        axum::Router::new()
            .route(
                "/__load/mail",
                get(move || {
                    let m = m.clone();
                    async move {
                        let links: Vec<String> = m
                            .sent()
                            .iter()
                            .filter_map(|e| {
                                let at = e.text.find("/mail/confirm?t=")?;
                                let start = e.text[..at].rfind("http")?;
                                e.text[start..].split_whitespace().next().map(String::from)
                            })
                            .collect();
                        axum::Json(json!(links))
                    }
                }),
            )
            .route(
                "/__load/domain",
                post(move |axum::Json(b): axum::Json<Value>| {
                    let (st, cf, dns) = (st.clone(), cf2.clone(), dns2.clone());
                    async move {
                        let (install, host) =
                            (b["install"].as_str().unwrap_or(""), b["hostname"].as_str().unwrap_or(""));
                        cf.set_status(host, "active", "active");
                        let keys = crate::tips::domains::ProofKeys::new(&SECRET).unwrap();
                        dns.set(&crate::tips::domains::proof::record_name(host), &[&keys.value(install, host)]);
                        let _ =
                            st.db.c().execute("UPDATE tip_domains SET claimed_at = 0 WHERE install_id = ?1", [install]);
                        axum::Json(json!({ "ok": true }))
                    }
                }),
            )
            .route(
                "/__load/weekly",
                post(move |axum::Json(b): axum::Json<Value>| {
                    let st = st_w.clone();
                    async move {
                        let at = b["now"].as_i64().unwrap_or_else(now_ms);
                        let t = Instant::now();
                        let s = crate::mail::run_weekly(&st, at).await;
                        axum::Json(
                            json!({ "stats": format!("{s:?}"), "sent": s.sent, "ms": t.elapsed().as_millis() as u64 }),
                        )
                    }
                }),
            )
            .route(
                "/__load/stats",
                get(move || {
                    let (st, tw, kk, dc, tt, ml, tm) =
                        (st_s.clone(), tw.clone(), kk.clone(), dc.clone(), tt.clone(), ml.clone(), tm.clone());
                    async move {
                        let count = |sql: &str| st.db.c().query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(-1);
                        axum::Json(json!({
                            "twitch_sent": tw.sent.load(Ordering::Relaxed),
                            "twitch_subscribed": tw.subscribed.load(Ordering::Relaxed),
                            "twitch_unsubscribed": tw.unsubscribed.load(Ordering::Relaxed),
                            "kick_sent": kk.sent.load(Ordering::Relaxed),
                            "kick_refreshed": kk.refreshed.load(Ordering::Relaxed),
                            "discord_calls": dc.calls.load(Ordering::Relaxed),
                            "discord_left": dc.left.load(Ordering::Relaxed),
                            "tts_calls": tt.calls().len(),
                            "mail_sent": ml.sent().len(),
                            "queue_rows": count("SELECT COUNT(*) FROM queue"),
                            "seen_rows": count("SELECT COUNT(*) FROM seen"),
                            "sweeps": tm.json(),
                        }))
                    }
                }),
            )
    };
    let app = router(state).merge(debug).into_make_service_with_connect_info::<std::net::SocketAddr>();
    println!("READY {base}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    axum::serve(l, app).await.unwrap();
}

// ── The driver ───────────────────────────────────────────────────────────────────────

/// Latencies (µs) and statuses per route group.
#[derive(Debug, Default)]
struct Group {
    lat: Vec<u32>,
    statuses: BTreeMap<u16, u64>,
    transport_errors: u64,
    /// The slowest answer and when it came (seconds into the busy hour).
    max_us: u32,
    max_at_s: f64,
}

#[derive(Debug, Default)]
struct Stats {
    groups: Mutex<BTreeMap<String, Group>>,
    /// 0 setup, 1 the busy hour, 2 after it: setup and the last answers in flight go in
    /// groups of their own (`setup:`, `after:`).
    phase: std::sync::atomic::AtomicU8,
    started: Mutex<Option<Instant>>,
}

impl Stats {
    fn note(&self, group: &str, status: Option<u16>, took: Duration) {
        let name = match self.phase.load(Ordering::Relaxed) {
            0 => format!("setup:{group}"),
            1 => group.to_string(),
            _ => format!("after:{group}"),
        };
        let at = self.started.lock().unwrap_or_else(|p| p.into_inner()).map_or(0.0, |t| t.elapsed().as_secs_f64());
        let mut g = self.groups.lock().unwrap_or_else(|p| p.into_inner());
        let e = g.entry(name).or_default();
        match status {
            Some(s) => {
                *e.statuses.entry(s).or_default() += 1;
                let us = u32::try_from(took.as_micros()).unwrap_or(u32::MAX);
                e.lat.push(us);
                if us > e.max_us {
                    (e.max_us, e.max_at_s) = (us, at);
                }
            }
            None => e.transport_errors += 1,
        }
    }
}

fn pct(sorted: &[u32], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    f64::from(sorted[i.min(sorted.len() - 1)]) / 1000.0
}

#[derive(Clone)]
struct Api {
    base: String,
    http: reqwest::Client,
    stats: Arc<Stats>,
}

struct Ans {
    status: u16,
    body: Value,
    text: String,
}

impl Api {
    async fn send(&self, group: &str, rb: reqwest::RequestBuilder) -> Option<Ans> {
        let t = Instant::now();
        match rb.send().await {
            Ok(res) => {
                let status = res.status().as_u16();
                let bytes = res.bytes().await.unwrap_or_default();
                self.stats.note(group, Some(status), t.elapsed());
                let text = String::from_utf8_lossy(&bytes).to_string();
                Some(Ans { status, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null), text })
            }
            Err(_) => {
                self.stats.note(group, None, t.elapsed());
                None
            }
        }
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") { path.to_string() } else { format!("{}{path}", self.base) }
    }

    async fn call(
        &self,
        group: &str,
        method: reqwest::Method,
        path: &str,
        auth: Option<&str>,
        body: Option<Value>,
    ) -> Option<Ans> {
        let mut r = self.http.request(method, self.url(path));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        self.send(group, r).await
    }

    fn ws(&self, path: &str) -> String {
        format!("{}{path}", self.base.replacen("http", "ws", 1))
    }
}

/// An address for `CF-Connecting-IP` (the relay trusts it from loopback), so per-address
/// limits (sign-ups, pairing) see many callers, as they would in production.
fn ip(n: usize) -> String {
    format!("10.{}.{}.{}", (n >> 16) & 255, (n >> 8) & 255, (n & 255).max(1))
}

/// Kick's endpoints the relay calls, with a real RSA key so webhook checks cost what they
/// do in production. Code `kick.<owner|bot>.<user id>` names who signed in.
async fn fake_kick(key: &rsa::RsaPrivateKey) -> String {
    use axum::routing::{get, post};
    use rsa::pkcs8::{EncodePublicKey, LineEnding};
    let pem = key.to_public_key().to_public_key_pem(LineEnding::LF).unwrap();
    let app = axum::Router::new()
        .route(
            "/public/v1/public-key",
            get(move || {
                let pem = pem.clone();
                async move { axum::Json(json!({ "data": { "public_key": pem }, "message": "OK" })) }
            }),
        )
        .route(
            "/oauth/token",
            post(|body: String| async move {
                let code = url::form_urlencoded::parse(body.as_bytes())
                    .find(|(k, _)| k == "code")
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_default();
                axum::Json(json!({ "access_token": format!("kat.{code}"), "refresh_token": format!("krt.{code}"),
                                   "expires_in": 7200, "token_type": "Bearer", "scope": "" }))
            }),
        )
        .route(
            "/public/v1/users",
            get(|headers: axum::http::HeaderMap| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
                let id: i64 = auth.rsplit('.').next().and_then(|s| s.parse().ok()).unwrap_or(1);
                axum::Json(json!({ "data": [{ "user_id": id, "name": format!("k{id}"), "profile_picture": "" }] }))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    base
}

/// One subscription as the driver knows it.
#[derive(Debug, Clone, Default)]
struct Sub {
    n: usize,
    id: String,
    /// `install_id.secret`, the first is the "main" PC.
    installs: Vec<String>,
    streamer: String,
    kick_channel: i64,
    stream_key: String,
    stream_start: i64,
    handle: String,
    domain: Option<String>,
    phones: Vec<String>,
    bot_version: i64,
}

fn install_id(auth: &str) -> &str {
    auth.split_once('.').map_or(auth, |(i, _)| i)
}

fn sub_event(event: &str, id: &str) -> Value {
    json!({
        "entity": "event", "account_id": "acc_Fake", "event": event, "contains": ["subscription"],
        "payload": { "subscription": { "entity": { "id": id, "entity": "subscription", "status": "stale-snapshot",
                                                    "notes": { "product": "ferrocast-pro" } } } },
        "created_at": now_ms() / 1000,
    })
}

fn bot_config() -> Value {
    json!({
        "bot_on": true,
        "commands": [
            { "id": "c1", "name": "!lurk", "aliases": [], "response": "Enjoy the lurk, {user}!", "roles": "everyone",
              "cooldown_s": 0, "user_cooldown_s": 0, "platforms": [], "enabled": true },
            { "id": "c2", "name": "!discord", "aliases": [], "response": "Join us: example.com/discord", "roles": "everyone",
              "cooldown_s": 30, "user_cooldown_s": 0, "platforms": [], "enabled": true },
            { "id": "c3", "name": "!up", "aliases": [], "response": "up {uptime}", "roles": "everyone",
              "cooldown_s": 0, "user_cooldown_s": 0, "platforms": [], "enabled": true }
        ],
        "timers": [{ "id": "t1", "name": "socials", "message": "Follow on {platform}", "every_min": 10,
                     "min_chat_lines": 0, "platforms": [], "enabled": true }],
        "auto_replies": [],
        "counters": [{ "name": "deaths", "command": "!deaths", "reply": "{counter}: {count}", "enabled": true, "value": 3 }]
    })
}

fn png() -> Vec<u8> {
    let img = image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([(x * 4) as u8, (y * 4) as u8, 128]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

async fn setup_sub(api: &Api, rzp: &ferrocast_testkit::razorpay::FakeRazorpay, n: usize) -> Sub {
    let mut s = Sub { n, ..Sub::default() };
    let email = format!("streamer{n}@example.com");
    let count = 1 + n % 3;
    for k in 0..count {
        let r = api
            .send("installs", api.http.post(api.url("/v1/installs")).header("cf-connecting-ip", ip(n * 4 + k + 1)))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{}", r.text);
        s.installs.push(format!(
            "{}.{}",
            r.body["install_id"].as_str().unwrap(),
            r.body["install_secret"].as_str().unwrap()
        ));
    }
    let main = s.installs[0].clone();
    let r = api
        .call(
            "billing",
            reqwest::Method::POST,
            "/v1/pro/checkout",
            Some(&main),
            Some(json!({ "plan": "monthly", "email": email })),
        )
        .await
        .unwrap();
    assert_eq!(r.status, 201, "{}", r.text);
    s.id = r.body["subscription_id"].as_str().unwrap().to_string();
    rzp.activate(&s.id, 30);
    let body = sub_event("subscription.activated", &s.id).to_string();
    let mac = {
        let mut m = Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
        m.update(body.as_bytes());
        hex(&m.finalize().into_bytes())
    };
    let r = api
        .send(
            "billing_webhook",
            api.http
                .post(api.url("/billing/razorpay/webhook"))
                .header("x-razorpay-signature", mac)
                .header("x-razorpay-event-id", format!("evt_act_{}", s.id))
                .header("content-type", "application/json")
                .body(body),
        )
        .await
        .unwrap();
    assert_eq!(r.status, 200, "{}", r.text);
    for (k, other) in s.installs.iter().enumerate().skip(1) {
        let r = api
            .send(
                "billing",
                api.http
                    .post(api.url("/v1/pro/restore"))
                    .bearer_auth(other)
                    .header("cf-connecting-ip", ip(100_000 + n * 4 + k))
                    .json(&json!({ "subscription_id": s.id, "email": email })),
            )
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{}", r.text);
    }
    for auth in &s.installs {
        let r = api.call("api_status", reqwest::Method::GET, "/v1/pro/status", Some(auth), None).await.unwrap();
        assert_eq!(r.body["pro"], true, "{}", r.text);
    }

    // History: a stream that started half an hour ago.
    s.stream_start = now_ms() - 1_800_000;
    s.stream_key = format!("twitch:{}", s.stream_start);
    let r = api
        .call(
            "api_history",
            reqwest::Method::POST,
            "/v1/pro/history/streams",
            Some(&main),
            Some(
                json!({ "platform": "twitch", "key": s.stream_key, "started_at": s.stream_start, "title": "Load test",
                         "peak_viewers": 40, "avg_viewers": 30, "chat_lines": 100, "chatters": 20 }),
            ),
        )
        .await
        .unwrap();
    assert!(matches!(r.status, 200 | 201), "{}", r.text);
    // The weekly email (confirmed later, with the links the fake mailer kept).
    let r = api
        .call("api_mail", reqwest::Method::PUT, "/v1/pro/history/email", Some(&main), Some(json!({ "address": email })))
        .await
        .unwrap();
    assert_eq!(r.status, 202, "{}", r.text);

    if n < BOT_SUBS {
        s.streamer = format!("{}", 100_000 + n);
        let desk = format!("desk.{}", s.streamer);
        for (account, who) in
            [("bot", format!("{}.bot{n}", 500_000 + n)), ("channel", format!("{}.s{}", s.streamer, s.streamer))]
        {
            let r = api
                .call(
                    "api_bot",
                    reqwest::Method::POST,
                    "/v1/pro/bot/twitch/connect",
                    Some(&main),
                    Some(json!({ "account": account, "owner_token": desk })),
                )
                .await
                .unwrap();
            assert_eq!(r.status, 201, "{}", r.text);
            let u = url::Url::parse(r.body["auth_url"].as_str().unwrap()).unwrap();
            let state = u.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
            let r = api
                .call(
                    "pages",
                    reqwest::Method::GET,
                    &format!(
                        "/v1/bot/twitch/callback?code={}&state={}&scope=x",
                        enc(&format!("tw.{account}.{who}")),
                        enc(&state)
                    ),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(r.status, 200, "{}", r.text);
        }
        if n < KICK_SUBS {
            s.kick_channel = 700_000 + n as i64;
            let r = api.call("kick_oauth", reqwest::Method::POST, "/v1/kick/oauth", Some(&main), None).await.unwrap();
            assert_eq!(r.status, 200, "{}", r.text);
            let u = url::Url::parse(r.body["url"].as_str().unwrap()).unwrap();
            let state = u.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
            let r = api
                .call(
                    "pages",
                    reqwest::Method::GET,
                    &format!("/v1/kick/callback?code=kick.owner.{}&state={}", s.kick_channel, enc(&state)),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(r.status, 200, "{}", r.text);
            let r = api
                .call("api_bot", reqwest::Method::POST, "/v1/pro/bot/kick/connect", Some(&main), Some(json!({})))
                .await
                .unwrap();
            assert_eq!(r.status, 201, "{}", r.text);
            let u = url::Url::parse(r.body["auth_url"].as_str().unwrap()).unwrap();
            let state = u.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
            let r = api
                .call(
                    "pages",
                    reqwest::Method::GET,
                    &format!("/v1/kick/callback?code=kick.bot.{}&state={}", 900_000 + n, enc(&state)),
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(r.status, 200, "{}", r.text);
        }
        let r = api
            .call(
                "api_bot",
                reqwest::Method::PUT,
                "/v1/pro/bot/config",
                Some(&main),
                Some(json!({ "version": 0, "config": bot_config() })),
            )
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{}", r.text);
        s.bot_version = r.body["version"].as_i64().unwrap_or(1);
        let r = api
            .call("api_bot", reqwest::Method::PUT, "/v1/pro/bot/enabled", Some(&main), Some(json!({ "enabled": true })))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{}", r.text);
    }
    if n < DISCORD_SUBS {
        let guild = format!("{}", 880_000_000_000_000_000 + n as u64);
        let r =
            api.call("api_discord", reqwest::Method::POST, "/v1/pro/discord/connect", Some(&main), None).await.unwrap();
        assert_eq!(r.status, 201, "{}", r.text);
        let u = url::Url::parse(r.body["invite_url"].as_str().unwrap()).unwrap();
        let state = u.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
        let r = api
            .call(
                "pages",
                reqwest::Method::GET,
                &format!(
                    "/discord/callback?code=dc.{guild}&state={}&guild_id={guild}&permissions=268454912",
                    enc(&state)
                ),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{}", r.text);
        let r = api
            .call(
                "api_discord",
                reqwest::Method::PUT,
                "/v1/pro/discord/rules",
                Some(&main),
                Some(json!({ "kind": "sub", "platform": "twitch", "role_id": snow(&guild, 10_000) })),
            )
            .await
            .unwrap();
        assert_eq!(r.status, 201, "{}", r.text);
    }
    if TIP_SUBS.contains(&n) {
        s.handle = format!("streamer{n}");
        let c = crate::tips::TipConfig {
            handle: s.handle.clone(),
            display_name: format!("Streamer {n}"),
            message: "Thanks for the support!".into(),
            razorpay: Some(crate::tips::Provider {
                page_url: "https://razorpay.me/@loadtest".into(),
                secret: "rzp-tip-secret".into(),
                ..crate::tips::Provider::default()
            }),
            ..crate::tips::TipConfig::default()
        };
        let r =
            api.send("api_tips", api.http.put(api.url("/v1/tips/config")).bearer_auth(&main).json(&c)).await.unwrap();
        assert_eq!(r.status, 200, "{}", r.text);
        let r = api
            .call(
                "api_tips",
                reqwest::Method::PUT,
                "/v1/pro/tips/brand",
                Some(&main),
                Some(json!({ "accent": "#ff5a1f", "background": "#101418", "text": "#f4f4f6" })),
            )
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{}", r.text);
        let r = api
            .send(
                "api_tips",
                api.http
                    .put(api.url("/v1/pro/tips/brand/avatar"))
                    .bearer_auth(&main)
                    .header("content-type", "image/png")
                    .body(png()),
            )
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{}", r.text);
        let r = api
            .call(
                "api_tips",
                reqwest::Method::PUT,
                "/v1/pro/tips/goal",
                Some(&main),
                Some(json!({ "title": "New mic", "target": 500_000, "currency": "INR" })),
            )
            .await
            .unwrap();
        assert_eq!(r.status, 200, "{}", r.text);
        if DOMAIN_SUBS.contains(&n) {
            let host = format!("tips.streamer{n}.example.net");
            let r = api
                .call(
                    "api_tips",
                    reqwest::Method::PUT,
                    "/v1/pro/tips/domain",
                    Some(&main),
                    Some(json!({ "hostname": host })),
                )
                .await
                .unwrap();
            assert_eq!(r.status, 201, "{}", r.text);
            api.call(
                "debug",
                reqwest::Method::POST,
                "/__load/domain",
                None,
                Some(json!({ "install": install_id(&main), "hostname": host })),
            )
            .await
            .unwrap();
            let r = api.call("api_tips", reqwest::Method::GET, "/v1/pro/tips/domain", Some(&main), None).await.unwrap();
            assert_eq!(r.body["status"], "live", "{}", r.text);
            s.domain = Some(host);
        }
    }
    if TUNNEL_SUBS.contains(&n) {
        for p in 0..(1 + n % 2) {
            let r = api
                .call(
                    "api_remote",
                    reqwest::Method::POST,
                    "/v1/pro/remote/codes",
                    Some(&main),
                    Some(json!({ "role": if p == 0 { "owner" } else { "mod" }, "name": format!("Phone {p}") })),
                )
                .await
                .unwrap();
            assert_eq!(r.status, 201, "{}", r.text);
            let code = r.body["code"].as_str().unwrap().to_string();
            let r = api
                .send(
                    "phone_pair",
                    api.http
                        .post(api.url("/r/v1/pair"))
                        .header("cf-connecting-ip", ip(200_000 + n * 4 + p))
                        .json(&json!({ "code": code })),
                )
                .await
                .unwrap();
            assert_eq!(r.status, 200, "{}", r.text);
            s.phones.push(r.body["token"].as_str().unwrap().to_string());
        }
    }
    s
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens a socket the way the engine does: on a refused or reset connect it tries again with
/// a backoff (0.2 s doubling, 8 tries). Retries count in `ws_<what>_retry`, a socket that
/// never opened in `ws_<what>_failed` (which fails the test).
async fn ws_connect(api: &Api, path: &str, auth: Option<&str>, what: &str) -> Option<Ws> {
    for attempt in 0..8u32 {
        let mut req = api.ws(path).into_client_request().unwrap();
        if let Some(a) = auth {
            req.headers_mut().insert("authorization", format!("Bearer {a}").parse().unwrap());
        }
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => return Some(ws),
            Err(e) => {
                if attempt == 0 {
                    eprintln!("ws {what}: {e} (retrying)");
                }
                api.stats.note(&format!("ws_{what}_retry"), None, Duration::ZERO);
                tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
            }
        }
    }
    api.stats.note(&format!("ws_{what}_failed"), None, Duration::ZERO);
    None
}

/// A PC's link socket: acks every item; `bot_seen` every 30 s when this PC runs the bot.
async fn link_client(api: Api, auth: String, bot_seen: bool, stop: Arc<AtomicBool>, items: Arc<AtomicU64>) {
    let Some(ws) = ws_connect(&api, "/v1/link", Some(&auth), "link").await else { return };
    let (mut tx, mut rx) = ws.split();
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    while !stop.load(Ordering::Relaxed) {
        tokio::select! {
            _ = tick.tick() => {
                if bot_seen && tx.send(WsMsg::text(json!({ "type": "bot_seen" }).to_string())).await.is_err() { return }
            }
            m = rx.next() => match m {
                Some(Ok(WsMsg::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if v["type"] == "item" {
                        items.fetch_add(1, Ordering::Relaxed);
                        let ack = json!({ "type": "ack", "id": v["id"] });
                        if tx.send(WsMsg::text(ack.to_string())).await.is_err() { return }
                    }
                }
                Some(Ok(_)) => {}
                _ => { api.stats.note("ws_link_dropped", None, Duration::ZERO); return }
            },
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

/// A PC's tunnel: answers every request at once with a small JSON body.
async fn tunnel_pc(api: Api, auth: String, stop: Arc<AtomicBool>) {
    let Some(ws) = ws_connect(&api, "/v1/remote/tunnel", Some(&auth), "tunnel").await else { return };
    let (mut tx, mut rx) = ws.split();
    let body = base64::engine::general_purpose::STANDARD
        .encode(json!({ "live": true, "uptime_s": 3600, "viewers": 42, "platforms": ["twitch"] }).to_string());
    while !stop.load(Ordering::Relaxed) {
        tokio::select! {
            m = rx.next() => match m {
                Some(Ok(WsMsg::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if v["t"] == "req" {
                        let res = json!({ "t": "res", "id": v["id"], "status": 200,
                                          "headers": [["content-type", "application/json"]], "body_b64": body });
                        if tx.send(WsMsg::text(res.to_string())).await.is_err() { return }
                    }
                }
                Some(Ok(_)) => {}
                _ => { api.stats.note("ws_tunnel_dropped", None, Duration::ZERO); return }
            },
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

/// A phone: a live socket open, and 2 requests a second through the tunnel.
async fn phone(api: Api, token: String, seed: u64, stop: Arc<AtomicBool>) {
    let r = api
        .call("phone_ticket", reqwest::Method::POST, "/r/v1/ticket", Some(&token), Some(json!({ "for": "live" })))
        .await;
    let live = match r {
        Some(a) if a.status == 200 => {
            let t = a.body["ticket"].as_str().unwrap_or("").to_string();
            ws_connect(&api, &format!("/r/api/v1/live?ticket={t}"), None, "phone").await
        }
        _ => None,
    };
    let reader = live.map(|ws| {
        tokio::spawn(async move {
            let (_tx, mut rx) = ws.split();
            while let Some(Ok(_)) = rx.next().await {}
        })
    });
    let mut rng = Rng(seed);
    tokio::time::sleep(Duration::from_millis(rng.below(500))).await;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let paths = ["/r/api/v1/status", "/r/api/v1/chat?limit=50", "/r/api/v1/status"];
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        let p = paths[rng.below(paths.len() as u64) as usize];
        api.call("phone_proxy", reqwest::Method::GET, p, Some(&token), None).await;
    }
    if let Some(h) = reader {
        h.abort();
    }
}

fn es_sign(id: &str, ts: &str, body: &str) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(ES_SECRET.as_bytes()).unwrap();
    m.update(id.as_bytes());
    m.update(ts.as_bytes());
    m.update(body.as_bytes());
    format!("sha256={}", hex(&m.finalize().into_bytes()))
}

/// One Twitch channel's chat: a message a second, one in five a command.
async fn twitch_chat(api: Api, s: Sub, stop: Arc<AtomicBool>) {
    let mut rng = Rng(s.n as u64 + 1);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_millis(rng.below(1000))).await;
    let mut i = 0u64;
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        i += 1;
        let text = match rng.below(10) {
            0 => "!lurk",
            1 => "!up",
            _ => "hello chat, what a play",
        };
        let id = format!("m-{}-{i}", s.n);
        let viewer = 1_000 + rng.below(300);
        let body = json!({
            "subscription": { "type": "channel.chat.message", "version": "1",
                              "condition": { "broadcaster_user_id": s.streamer, "user_id": format!("{}", 500_000 + s.n) } },
            "event": { "broadcaster_user_id": s.streamer, "chatter_user_id": viewer.to_string(), "chatter_user_login": format!("v{viewer}"),
                       "chatter_user_name": format!("V{viewer}"), "message_id": id, "message": { "text": text, "fragments": [] },
                       "badges": [], "color": "#FF0000" }
        })
        .to_string();
        let ts = rfc3339(now_ms());
        let rb = api
            .http
            .post(api.url("/v1/bot/twitch/eventsub"))
            .header("Twitch-Eventsub-Message-Id", &id)
            .header("Twitch-Eventsub-Message-Timestamp", &ts)
            .header("Twitch-Eventsub-Message-Signature", es_sign(&id, &ts, &body))
            .header("Twitch-Eventsub-Message-Type", "notification")
            .header("content-type", "application/json")
            .body(body);
        api.send("eventsub", rb).await;
    }
}

/// One Kick channel's chat: a message every 2 s, signed with Kick's (fake) key.
async fn kick_chat(api: Api, s: Sub, key: Arc<rsa::RsaPrivateKey>, stop: Arc<AtomicBool>) {
    use rsa::pkcs1v15::SigningKey;
    use rsa::signature::{SignatureEncoding, Signer};
    let signer = SigningKey::<Sha256>::new((*key).clone());
    let mut rng = Rng(s.n as u64 + 77);
    tokio::time::sleep(Duration::from_millis(rng.below(2000))).await;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut i = 0u64;
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        i += 1;
        let text = if rng.below(5) == 0 { "!lurk" } else { "gg" };
        let id = format!("k-{}-{i}", s.n);
        let viewer = 2_000 + rng.below(300);
        let body = json!({ "message_id": id, "broadcaster": { "user_id": s.kick_channel },
                           "sender": { "user_id": viewer, "username": format!("kv{viewer}"), "identity": { "badges": [] } },
                           "content": text })
        .to_string();
        let ts = rfc3339(now_ms());
        let sig = base64::engine::general_purpose::STANDARD
            .encode(signer.sign(format!("{id}.{ts}.{body}").as_bytes()).to_bytes());
        let rb = api
            .http
            .post(api.url("/v1/kick/webhook"))
            .header("Kick-Event-Message-Id", &id)
            .header("Kick-Event-Message-Timestamp", &ts)
            .header("Kick-Event-Signature", sig)
            .header("Kick-Event-Type", "chat.message.sent")
            .header("Kick-Event-Version", "1")
            .header("content-type", "application/json")
            .body(body);
        api.send("kick_webhook", rb).await;
    }
}

/// Visitors on the 30 branded tip pages: 5 a second in all.
async fn tip_visitors(api: Api, subs: Vec<Sub>, stop: Arc<AtomicBool>) {
    let mut rng = Rng(4242);
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        let s = &subs[rng.below(subs.len() as u64) as usize];
        let api2 = api.clone();
        let roll = rng.below(20);
        let (path, host) = match (&s.domain, roll) {
            (Some(d), 0..=5) => ("/".to_string(), Some(d.clone())),
            (_, 6..=8) => (format!("/t/{}/goal", s.handle), None),
            _ => (format!("/t/{}", s.handle), None),
        };
        tokio::spawn(async move {
            let mut rb = api2.http.get(api2.url(&path));
            if let Some(h) = host {
                rb = rb.header("host", h);
            }
            api2.send("tip_page", rb).await;
        });
    }
}

/// What a Pro PC does over a busy hour besides chat: history samples every 5 minutes, the
/// status, the bot panel and config pushes, Discord perk events, now and then.
async fn pc_routine(api: Api, s: Sub, stop: Arc<AtomicBool>) {
    let mut rng = Rng(s.n as u64 + 999);
    let main = s.installs[0].clone();
    // Spread the subscriptions over the 5 minutes.
    tokio::time::sleep(Duration::from_millis(rng.below(300_000))).await;
    let mut tick = tokio::time::interval(Duration::from_secs(300));
    let mut version = s.bot_version;
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let viewers = 20 + rng.below(100);
        api.call(
            "api_history",
            reqwest::Method::POST,
            "/v1/pro/history/samples",
            Some(&main),
            Some(json!({ "platform": "twitch", "key": s.stream_key, "samples": [{ "at": now_ms(), "viewers": viewers }] })),
        )
        .await;
        api.call(
            "api_history",
            reqwest::Method::POST,
            "/v1/pro/history/streams",
            Some(&main),
            Some(
                json!({ "platform": "twitch", "key": s.stream_key, "started_at": s.stream_start, "title": "Load test",
                         "peak_viewers": 140, "avg_viewers": 30, "chat_lines": 100 + rng.below(1000), "chatters": 20 }),
            ),
        )
        .await;
        for auth in &s.installs {
            api.call("api_status", reqwest::Method::GET, "/v1/pro/status", Some(auth), None).await;
        }
        if s.n < BOT_SUBS {
            api.call("api_bot", reqwest::Method::GET, "/v1/pro/bot", Some(&main), None).await;
            let mut cfg = bot_config();
            cfg["counters"][0]["value"] = json!(rng.below(50));
            if let Some(a) = api
                .call(
                    "api_bot",
                    reqwest::Method::PUT,
                    "/v1/pro/bot/config",
                    Some(&main),
                    Some(json!({ "version": version, "config": cfg })),
                )
                .await
                && let Some(v) = a.body["version"].as_i64()
            {
                version = v;
            }
        }
        if s.n < DISCORD_SUBS {
            let events: Vec<Value> = (0..3)
                .map(|k| json!({ "platform": "twitch", "platform_user_id": format!("{}", 3_000 + k), "kind": "sub", "tier": "1000", "at": now_ms() }))
                .collect();
            api.call(
                "api_discord",
                reqwest::Method::POST,
                "/v1/pro/discord/perk-events",
                Some(&main),
                Some(json!(events)),
            )
            .await;
            api.call("api_discord", reqwest::Method::GET, "/v1/pro/discord", Some(&main), None).await;
        }
        if TUNNEL_SUBS.contains(&s.n) {
            api.call("api_remote", reqwest::Method::GET, "/v1/pro/remote/devices", Some(&main), None).await;
        }
        if TIP_SUBS.contains(&s.n) {
            api.call("api_tips", reqwest::Method::GET, "/v1/pro/tips/goal", Some(&main), None).await;
        }
    }
}

/// Ferrocast voices: 40 requests a minute spread over every subscription (the relay-wide
/// limit is 60 a minute).
async fn tts_load(api: Api, subs: Vec<Sub>, stop: Arc<AtomicBool>) {
    let mut rng = Rng(31337);
    let mut tick = tokio::time::interval(Duration::from_millis(1500));
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        let s = &subs[rng.below(subs.len() as u64) as usize];
        let api2 = api.clone();
        let auth = s.installs[0].clone();
        tokio::spawn(async move {
            api2.call(
                "api_tts",
                reqwest::Method::POST,
                "/v1/pro/tts",
                Some(&auth),
                Some(json!({ "text": "Thanks for the five hundred rupees, you are amazing!", "voice": "google:hi-IN-Neural2-A" })),
            )
            .await;
        });
    }
}

/// One cloud backup of `bytes`, in 8 MB parts, at a random moment of the window.
async fn backup(api: Api, s: Sub, delay: Duration, bytes: usize, stop: Arc<AtomicBool>) {
    tokio::time::sleep(delay).await;
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let main = s.installs[0].clone();
    let data: Vec<u8> = (0..bytes).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let sha = hex(&Sha256::digest(&data));
    let Some(r) = api
        .call(
            "api_backups",
            reqwest::Method::POST,
            "/v1/pro/backups",
            Some(&main),
            Some(json!({ "bytes": bytes, "sha256": sha, "app_version": "0.9.0", "reason": "automatic" })),
        )
        .await
    else {
        return;
    };
    let (Some(id), Some(part)) = (r.body["id"].as_str(), r.body["part_size"].as_u64()) else { return };
    for (n, chunk) in data.chunks(part as usize).enumerate() {
        api.send(
            "backup_part",
            api.http
                .put(api.url(&format!("/v1/pro/backups/{id}/parts/{n}")))
                .bearer_auth(&main)
                .header("content-type", "application/octet-stream")
                .body(chunk.to_vec()),
        )
        .await;
    }
    api.call("backup_complete", reqwest::Method::POST, &format!("/v1/pro/backups/{id}/complete"), Some(&main), None)
        .await;
    api.call("api_backups", reqwest::Method::GET, "/v1/pro/backups", Some(&main), None).await;
}

/// `ps` for the child: RSS in KB and CPU time in seconds.
fn ps(pid: u32) -> Option<(u64, f64)> {
    let out =
        std::process::Command::new("ps").args(["-o", "rss=", "-o", "time=", "-p", &pid.to_string()]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let rss: u64 = it.next()?.parse().ok()?;
    let time = it.next()?;
    // [[dd-]hh:]mm:ss.cc
    let secs = time.split(':').rev().enumerate().fold(0.0, |acc, (i, p)| {
        let p = p.rsplit('-').next().unwrap_or(p);
        acc + p.parse::<f64>().unwrap_or(0.0) * 60f64.powi(i as i32)
    });
    Some((rss, secs))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "load test: run with --ignored load_ in a release build"]
async fn load_200_subscriptions() {
    if std::env::var("LOAD_CHILD").is_ok() {
        return;
    }
    let secs: u64 = std::env::var("LOAD_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    let rzp = ferrocast_testkit::razorpay::FakeRazorpay::start().await;
    let (pm, py) = (rzp.add_plan("monthly", 19_900), rzp.add_plan("yearly", 199_900));
    let key = Arc::new(rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 2048).unwrap());
    let kick_url = fake_kick(&key).await;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("relay.log");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "load_tests::load_child_relay", "--nocapture", "--test-threads=1"])
        .env("LOAD_CHILD", "1")
        .env("LOAD_DIR", &data)
        .env("LOAD_LOG", &log)
        .env("LOAD_KICK_URL", &kick_url)
        .env("LOAD_RZP_URL", &rzp.base)
        .env("LOAD_PLAN_M", &pm)
        .env("LOAD_PLAN_Y", &py)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let stdout = child.stdout.take().unwrap();
    // The child goes with the test, also when an assertion fails.
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = Kill(child);
    let base = tokio::task::spawn_blocking(move || {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = lines.next().expect("the relay child exited before it was ready").unwrap();
            // The test harness prints "test … ... " first on the same line.
            if let Some(b) = line.split_once("READY ").map(|(_, b)| b) {
                // Keep draining so the child never blocks on a full pipe.
                std::thread::spawn(move || for _ in lines {});
                return b.trim().to_string();
            }
        }
    })
    .await
    .unwrap();
    let stats = Arc::new(Stats::default());
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(512)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let api = Api { base, http, stats: stats.clone() };
    let (rss0, _) = ps(pid).unwrap_or_default();

    // ── Setup ──
    let t_setup = Instant::now();
    let subs: Vec<Sub> = futures_util::stream::iter(0..SUBS)
        .map(|n| {
            let (api, rzp) = (api.clone(), rzp.clone());
            async move { setup_sub(&api, &rzp, n).await }
        })
        .buffer_unordered(16)
        .collect()
        .await;
    let mut subs = subs;
    subs.sort_by_key(|s| s.n);
    // Confirm every weekly email address (the confirm page's button POSTs).
    let links: Vec<String> =
        serde_json::from_value(api.call("debug", reqwest::Method::GET, "/__load/mail", None, None).await.unwrap().body)
            .unwrap();
    assert_eq!(links.len(), SUBS, "one confirm email per subscription");
    let confirmed: Vec<u16> = futures_util::stream::iter(links)
        .map(|l| {
            let api = api.clone();
            async move { api.call("mail_confirm", reqwest::Method::POST, &l, None, None).await.map_or(0, |a| a.status) }
        })
        .buffer_unordered(16)
        .collect()
        .await;
    assert!(confirmed.iter().all(|s| *s == 200), "{confirmed:?}");
    let setup_s = t_setup.elapsed().as_secs_f64();
    let installs: usize = subs.iter().map(|s| s.installs.len()).sum();
    let phones: usize = subs.iter().map(|s| s.phones.len()).sum();
    eprintln!("setup: {SUBS} subscriptions, {installs} installs, {phones} phones in {setup_s:.1} s");

    // ── The busy hour ──
    let stop = Arc::new(AtomicBool::new(false));
    let items = Arc::new(AtomicU64::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for s in &subs {
        for (k, auth) in s.installs.iter().enumerate() {
            // Half the cloud bot PCs are on (they answer, the cloud stands by), half are off.
            let seen = k == 0 && s.n < BOT_SUBS && s.n % 2 == 0;
            tasks.spawn(link_client(api.clone(), auth.clone(), seen, stop.clone(), items.clone()));
        }
        if TUNNEL_SUBS.contains(&s.n) {
            tasks.spawn(tunnel_pc(api.clone(), s.installs[0].clone(), stop.clone()));
        }
    }
    // Let the sockets come up before the phones ask through them.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let t0 = Instant::now();
    *stats.started.lock().unwrap() = Some(t0);
    stats.phase.store(1, Ordering::Relaxed);
    let (_, cpu0) = ps(pid).unwrap_or_default();
    for s in &subs {
        for (p, token) in s.phones.iter().enumerate() {
            tasks.spawn(phone(api.clone(), token.clone(), (s.n * 10 + p) as u64, stop.clone()));
        }
        if s.n < BOT_SUBS {
            tasks.spawn(twitch_chat(api.clone(), s.clone(), stop.clone()));
        }
        if s.n < KICK_SUBS {
            tasks.spawn(kick_chat(api.clone(), s.clone(), key.clone(), stop.clone()));
        }
        tasks.spawn(pc_routine(api.clone(), s.clone(), stop.clone()));
    }
    let tip_subs: Vec<Sub> = subs.iter().filter(|s| TIP_SUBS.contains(&s.n)).cloned().collect();
    tasks.spawn(tip_visitors(api.clone(), tip_subs, stop.clone()));
    tasks.spawn(tts_load(api.clone(), subs.clone(), stop.clone()));
    let mut rng = Rng(7);
    for s in subs.iter().filter(|s| BACKUP_SUBS.contains(&s.n)) {
        let delay = Duration::from_secs(rng.below(secs.saturating_sub(60).max(1)));
        tasks.spawn(backup(api.clone(), s.clone(), delay, 20 * 1024 * 1024, stop.clone()));
    }
    // The weekly email for one day's subscriptions, halfway: at the next 04:00 UTC (the
    // report covers the week before it, so today's streams count).
    let weekly = {
        let api = api.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs / 2)).await;
            let now = now_ms();
            let next = now - now.rem_euclid(day()) + crate::mail::SEND_HOUR * 3_600_000;
            let at = if next > now { next } else { next + day() } + 60_000;
            api.call("weekly_run", reqwest::Method::POST, "/__load/weekly", None, Some(json!({ "now": at }))).await
        })
    };
    // Sample the child every 2 s.
    let mut samples: Vec<(f64, u64, f64)> = Vec::new();
    while t0.elapsed() < Duration::from_secs(secs) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some((rss, cpu)) = ps(pid) {
            samples.push((t0.elapsed().as_secs_f64(), rss, cpu));
        }
    }
    let (_, cpu1) = ps(pid).unwrap_or_default();
    let window = t0.elapsed().as_secs_f64();
    stats.phase.store(2, Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);
    let weekly = weekly.await.ok().flatten().map(|a| a.body).unwrap_or(Value::Null);
    stats.phase.store(2, Ordering::Relaxed);
    let child_stats =
        api.call("debug", reqwest::Method::GET, "/__load/stats", None, None).await.map(|a| a.body).unwrap_or_default();
    tokio::time::sleep(Duration::from_secs(2)).await;
    tasks.abort_all();
    drop(child);

    // ── Results ──
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    let locked = log_text.matches("database is locked").count() + log_text.matches("database table is locked").count();
    let db_errors = log_text.matches("relay database error").count();
    let dropped = log_text.matches("dropped webhook work").count();
    let warn_lines = log_text.lines().filter(|l| l.contains(" WARN ")).count();
    let error_lines = log_text.lines().filter(|l| l.contains("ERROR")).count();
    let mut warn_kinds: BTreeMap<String, usize> = BTreeMap::new();
    for l in log_text.lines().filter(|l| l.contains(" WARN ") || l.contains("ERROR")) {
        // The message after the target, without the fields.
        let msg = l.split_once(": ").map_or(l, |(_, m)| m);
        let msg: String = msg.chars().take(90).collect();
        *warn_kinds.entry(msg).or_default() += 1;
    }
    let max_rss_mb = samples.iter().map(|s| s.1).max().unwrap_or(0) as f64 / 1024.0;
    let end_rss_mb = samples.last().map_or(0, |s| s.1) as f64 / 1024.0;
    let cpu_avg = (cpu1 - cpu0) / window;
    let mut peak_cpu = 0f64;
    for w in samples.windows(2) {
        let dt = w[1].0 - w[0].0;
        if dt > 0.0 {
            peak_cpu = peak_cpu.max((w[1].2 - w[0].2) / dt);
        }
    }
    let groups = stats.groups.lock().unwrap();
    let mut table = String::from(
        "| group | requests | req/s | p50 ms | p95 ms | p99 ms | max ms (at s) | statuses | transport errors |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut out_groups = serde_json::Map::new();
    let mut fails: Vec<String> = Vec::new();
    for (name, g) in groups.iter() {
        let mut lat = g.lat.clone();
        lat.sort_unstable();
        let n = lat.len() as u64;
        let (p50, p95, p99, max) = (pct(&lat, 0.50), pct(&lat, 0.95), pct(&lat, 0.99), pct(&lat, 1.0));
        let rate = if name.contains(':') { 0.0 } else { n as f64 / window };
        let st: Vec<String> = g.statuses.iter().map(|(s, c)| format!("{s}×{c}")).collect();
        table.push_str(&format!(
            "| {name} | {n} | {rate:.1} | {p50:.1} | {p95:.1} | {p99:.1} | {max:.1} ({:.0}) | {} | {} |\n",
            g.max_at_s,
            st.join(" "),
            g.transport_errors
        ));
        out_groups.insert(
            name.clone(),
            json!({ "n": n, "rate": rate, "p50": p50, "p95": p95, "p99": p99, "max": max, "max_at_s": g.max_at_s,
                    "statuses": g.statuses.iter().map(|(s, c)| (s.to_string(), json!(c))).collect::<serde_json::Map<_, _>>(),
                    "transport_errors": g.transport_errors }),
        );
        if name.ends_with("_failed") {
            fails.push(format!("{name}: {} sockets never opened", g.transport_errors));
        }
        if name.contains(':') || name == "debug" || name == "weekly_run" {
            continue;
        }
        let five = g.statuses.iter().filter(|(s, _)| **s >= 500).map(|(_, c)| c).sum::<u64>();
        if five > 0 {
            fails.push(format!("{name}: {five} answers were 5xx"));
        }
        // A retried socket is only counted; one that never opened is reported above.
        if g.transport_errors > 0 && !name.ends_with("_retry") && !name.ends_with("_failed") {
            fails.push(format!("{name}: {} transport errors", g.transport_errors));
        }
        // Bulk work (8 MB parts, joining and hashing a 20 MB backup) isn't held to the 200 ms
        // rule; it's reported on its own.
        if !name.starts_with("backup_") && p95 > 200.0 {
            fails.push(format!("{name}: p95 {p95:.1} ms over 200 ms"));
        }
    }
    drop(groups);
    if max_rss_mb >= 700.0 {
        fails.push(format!("RSS {max_rss_mb:.0} MB at or over 700 MB"));
    }
    if locked > 0 {
        fails.push(format!("{locked} 'database is locked' errors"));
    }
    let summary = json!({
        "window_s": window, "setup_s": setup_s, "subs": SUBS, "installs": installs, "phones": phones,
        "rss_start_mb": rss0 as f64 / 1024.0, "rss_max_mb": max_rss_mb, "rss_end_mb": end_rss_mb,
        "cpu_avg_cores": cpu_avg, "cpu_peak_cores": peak_cpu, "link_items_acked": items.load(Ordering::Relaxed),
        "sqlite_locked": locked, "db_errors": db_errors, "dropped_webhook_work_logs": dropped,
        "warn_lines": warn_lines, "error_lines": error_lines, "warn_kinds": warn_kinds,
        "weekly": weekly, "child": child_stats, "groups": out_groups, "fails": fails,
        "rss_samples_mb": samples.iter().step_by(15).map(|s| json!([s.0.round(), s.1 / 1024])).collect::<Vec<_>>(),
    });
    eprintln!("\n{table}");
    eprintln!("{}", serde_json::to_string_pretty(&summary).unwrap());
    if let Ok(p) = std::env::var("LOAD_OUT") {
        std::fs::write(&p, format!("{}\n\n{table}", serde_json::to_string_pretty(&summary).unwrap())).unwrap();
    }
    assert!(fails.is_empty(), "load test failed: {fails:#?}");
}

#[test]
fn the_helpers_format_times_and_percentiles() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(1_736_870_886_000), "2025-01-14T16:08:06Z");
    assert_eq!(crate::kick::rfc3339_ms(&rfc3339(1_736_870_886_000)), Some(1_736_870_886_000));
    let v: Vec<u32> = (1..=100).map(|i| i * 1000).collect();
    assert_eq!(pct(&v, 0.5), 51.0);
    assert_eq!(pct(&v, 0.95), 95.0);
    assert_eq!(pct(&[], 0.5), 0.0);
    assert_eq!(ip(1), "10.0.0.1");
    assert!(tw_who("desk.42").is_some_and(|w| w.user_id == "42" && w.client_id == DESKTOP_APP));
    assert!(tw_who("at.tw.bot.7.ashabot").is_some_and(|w| w.login == "ashabot" && !w.scopes.is_empty()));
}
