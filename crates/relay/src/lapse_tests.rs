//! Pro's lapse, end to end (docs/plan/PRO.md, Gates: "lapse tested end to end in Razorpay test
//! mode"): one subscription bought through the real checkout and webhooks (fake Razorpay), every
//! Pro feature set up through the real `router()` with fakes (history, tip branding + goal +
//! custom address, backups, voices, a paired phone, the cloud bot on Twitch, Kick and YouTube,
//! Discord with a rule and a granted role, the weekly email), then Razorpay says the
//! subscription lapsed. Checked at each stage: the gates (402 with the words and hint), what
//! stays open during the 30 days (export, backups, the phone stays paired), the cloud bot
//! stopping at the next hourly sweep, the custom address kept 7 days, everything deleted and
//! revoked 30 days on, and coming back: the same subscription recovering within the 30 days
//! gets everything back; a new subscription after the deletion starts clean.

use std::sync::Arc;
use std::time::Duration;

use ferrocast_testkit::razorpay::{FakeRazorpay, KEY_ID, KEY_SECRET};
use hmac::{Hmac, Mac};
use reqwest::Method as M;
use rsa::sha2::{Digest, Sha256};
use serde_json::{Value, json};

use crate::billing::BillingConfig;
use crate::billing::razorpay::sign as rzp_sign;
use crate::bot::tests::{
    BOT_ID, DESK, DESKTOP_APP, ES_SECRET, FakeKick, FakeTwitch, KICK_STREAMER, STREAMER_ID, cloud_takes_over_all,
    config, enc, eventually, fake_kick_web, twitch_cfg, vault, who,
};
use crate::bot::youtube::{ChannelInfo, FORCE_SSL, GoogleTokenInfo, GoogleTokens};
use crate::bot::youtube_tests::{FakeYoutube, yt_cfg};
use crate::bot::{KickBot, TokenInfo, TwitchCloud, UserTokens, YoutubeCloud};
use crate::discord::tests::{FakeDiscord, GUILD, ROLE_SUB, TWITCH_ID, VIEWER};
use crate::discord::{Connection, DiscordApi, Guild, Token};
use crate::mail::{FakeMailer, MailConfig, Mailer};
use crate::tips::domains::{CustomHostnames, FakeDns, FakeHostnames, ProofDns, ProofKeys};
use crate::tts::{FakeTts, TtsProvider};
use crate::{AppState, Config, Db, now_ms, router};

const DAY: i64 = 86_400_000;
const WEBHOOK_SECRET: &str = "whsec-lapse-0123456789abcdef";
const EMAIL_KEY: &str = "email-key-for-lapse-0123456789abcdef";
const HOST: &str = "tips.mychannel.com";
const SECRET: [u8; 32] = [7u8; 32];
const PEPPER: &[u8] = b"lapse pepper, at least sixteen bytes";
const YT_DESKTOP: &str = "desk-app.apps.googleusercontent.com";
const YT_DESK_TOKEN: &str = "ya29.desk";
const YT_OWNER: &str = "UCowner0000000000000000";
const ADDRESS: &str = "asha.streams@example.com";
const ACCENT: &str = "#ff5a1f";
const PRO_WORDS: &str = "This runs on Ferrocast's servers and is part of Pro.";

struct World {
    base: String,
    st: AppState,
    rzp: FakeRazorpay,
    tw: Arc<FakeTwitch>,
    kk: Arc<FakeKick>,
    yt: Arc<FakeYoutube>,
    dc: Arc<FakeDiscord>,
    mail: Arc<FakeMailer>,
    tts: Arc<FakeTts>,
    cf: Arc<FakeHostnames>,
    dns: Arc<FakeDns>,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

/// What one streamer set up: the PC's auth, the phone's token, the backup.
struct Streamer {
    auth: String,
    phone: String,
    backup: String,
    backup_bytes: Vec<u8>,
}

async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let kick_base = fake_kick_web().await;
    let rzp = FakeRazorpay::start().await;
    let cfg = Config {
        public_url: base.clone(),
        tip_url: Some("https://tip.example".into()),
        kick_client_id: Some("kick-client".into()),
        kick_client_secret: Some("kick-secret".into()),
        kick_id_url: kick_base.clone(),
        kick_api_url: kick_base,
    };
    let billing = BillingConfig {
        key_id: Some(KEY_ID.into()),
        key_secret: Some(KEY_SECRET.into()),
        webhook_secret: Some(WEBHOOK_SECRET.into()),
        email_key: Some(EMAIL_KEY.into()),
        plan_monthly: Some(rzp.add_plan("monthly", 19_900)),
        plan_yearly: Some(rzp.add_plan("yearly", 199_900)),
        api_url: rzp.base.clone(),
        sales_open: true,
    };
    let tw = Arc::new(FakeTwitch::default());
    tw.with(|t| t.tokens.insert(DESK.into(), who(STREAMER_ID, "asha_live", DESKTOP_APP)));
    let kk = Arc::new(FakeKick::default());
    let yt = Arc::new(FakeYoutube::default());
    yt.with(|y| {
        y.infos.insert(YT_DESK_TOKEN.into(), GoogleTokenInfo { client_id: YT_DESKTOP.into(), scopes: vec![] });
        y.channels.insert(YT_DESK_TOKEN.into(), ChannelInfo { id: YT_OWNER.into(), title: "Asha".into() });
    });
    let bot = crate::bot::Runtime::new(
        Some(twitch_cfg()),
        Some(tw.clone() as Arc<dyn TwitchCloud>),
        Some(kk.clone() as Arc<dyn KickBot>),
    )
    .with_youtube(Some(yt_cfg(1_500, 9_000)), Some(yt.clone() as Arc<dyn YoutubeCloud>));
    let dc = Arc::new(FakeDiscord::standard());
    let discord =
        crate::discord::Runtime::new(Some(crate::discord::tests::config()), Some(dc.clone() as Arc<dyn DiscordApi>))
            .without_pauses();
    let mail = Arc::new(FakeMailer::new());
    let tts = Arc::new(FakeTts::new());
    let cf = Arc::new(FakeHostnames::new());
    let dns = Arc::new(FakeDns::new());
    let blobs: Arc<dyn crate::blob::BlobStore> =
        Arc::new(crate::blob::FsBlobStore::new(dir.path().join("blobs")).unwrap());
    let st = AppState::new(cfg, Db::open(&dir.path().join("relay.db")).unwrap())
        .with_billing(billing)
        .with_vault(Some(vault()))
        .with_blobs(Some(blobs))
        .with_tts(Some(tts.clone() as Arc<dyn TtsProvider>), 800_000)
        .with_remote(crate::remote::RemoteConfig::new(Some(&base), Some(PEPPER), None))
        .with_discord(discord)
        .with_mail(
            Some(mail.clone() as Arc<dyn Mailer>),
            MailConfig::new(None, Some(b"a lapse test key that is long enough")),
        )
        .with_bot(bot)
        .with_tip_domains(
            Some(cf.clone() as Arc<dyn CustomHostnames>),
            Some(dns.clone() as Arc<dyn ProofDns>),
            Some(&SECRET),
            100,
        );
    let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    World { base, st, rzp, tw, kk, yt, dc, mail, tts, cf, dns, http, dir }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Twitch's EventSub timestamp for `ms`.
fn rfc3339(ms: i64) -> String {
    let (y, mo, d) = crate::quota::civil(ms.div_euclid(DAY));
    let s = ms.rem_euclid(DAY) / 1000;
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", s / 3600, s / 60 % 60, s % 60, ms.rem_euclid(1000))
}

fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_fn(64, 64, |x, y| image::Rgba([(x * 4) as u8, (y * 4) as u8, 90, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn sub_event(event: &str, id: &str) -> Value {
    json!({
        "entity": "event", "account_id": "acc_Fake", "event": event, "contains": ["subscription"],
        "payload": { "subscription": { "entity": { "id": id, "entity": "subscription", "status": "stale-snapshot",
                                                    "notes": { "product": "ferrocast-pro" } } } },
        "created_at": now_ms() / 1000,
    })
}

impl World {
    async fn call(&self, m: M, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value, String) {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{}{path}", self.base) };
        let mut r = self.http.request(m, url);
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        let text = res.text().await.unwrap();
        (s, serde_json::from_str(&text).unwrap_or(Value::Null), text)
    }

    async fn ok(&self, m: M, path: &str, auth: &str, body: Option<Value>, want: u16) -> Value {
        let (s, v, text) = self.call(m.clone(), path, Some(auth), body).await;
        assert_eq!(s, want, "{m} {path}: {text}");
        v
    }

    /// A page with this `Host` (the custom tip address).
    async fn on_host(&self, host: &str, path: &str) -> u16 {
        self.http.get(format!("{}{path}", self.base)).header("host", host).send().await.unwrap().status().as_u16()
    }

    async fn bytes(&self, path: &str, auth: &str) -> (u16, Vec<u8>) {
        let res = self.http.get(format!("{}{path}", self.base)).bearer_auth(auth).send().await.unwrap();
        (res.status().as_u16(), res.bytes().await.unwrap().to_vec())
    }

    /// Razorpay's webhook, signed as Razorpay signs it.
    async fn hook(&self, event_id: &str, body: &Value) {
        let raw = body.to_string();
        let res = self
            .http
            .post(format!("{}/billing/razorpay/webhook", self.base))
            .header("x-razorpay-signature", rzp_sign(WEBHOOK_SECRET, raw.as_bytes()))
            .header("x-razorpay-event-id", event_id)
            .header("content-type", "application/json")
            .body(raw)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    }

    /// A new install that buys Pro: checkout, Razorpay charges, `subscription.activated`.
    async fn buy(&self, auth: Option<String>) -> (String, String, String) {
        let auth = match auth {
            Some(a) => a,
            None => {
                let (s, v, _) = self.call(M::POST, "/v1/installs", None, None).await;
                assert_eq!(s, 201);
                format!("{}.{}", v["install_id"].as_str().unwrap(), v["install_secret"].as_str().unwrap())
            }
        };
        let install = auth.split('.').next().unwrap().to_string();
        let v = self
            .ok(M::POST, "/v1/pro/checkout", &auth, Some(json!({ "plan": "monthly", "email": "a@example.com" })), 201)
            .await;
        let sub = v["subscription_id"].as_str().unwrap().to_string();
        self.rzp.activate(&sub, 30);
        self.hook(&format!("evt_act_{sub}"), &sub_event("subscription.activated", &sub)).await;
        let v = self.ok(M::GET, "/v1/pro/status", &auth, None, 200).await;
        assert_eq!(v["pro"], true, "{v}");
        (auth, install, sub)
    }

    /// Razorpay changes the subscription (`days` from now as its period end) and says so.
    async fn razorpay_says(&self, sub: &str, event: &str, status: &str, days: Option<i64>) {
        self.rzp.set_status(sub, status, days);
        self.hook(&format!("evt_{status}_{}", crate::random(6)), &sub_event(event, sub)).await;
    }

    fn count(&self, sql: &str) -> i64 {
        self.st.db.c().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn blob_files(&self) -> usize {
        let mut n = 0;
        let mut stack = vec![self.dir.path().join("blobs")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    stack.push(e.path());
                } else {
                    n += 1;
                }
            }
        }
        n
    }

    /// One chat line from a viewer through Twitch's EventSub webhook (HMAC-signed, fresh).
    async fn twitch_chat(&self, text: &str) {
        let id = format!("es-{}", crate::random(6));
        let ts = rfc3339(now_ms());
        let body = json!({
            "subscription": { "type": "channel.chat.message", "version": "1",
                              "condition": { "broadcaster_user_id": STREAMER_ID, "user_id": BOT_ID } },
            "event": { "broadcaster_user_id": STREAMER_ID, "chatter_user_id": "77", "chatter_user_login": "viewer",
                       "chatter_user_name": "Viewer", "message_id": format!("m-{id}"),
                       "message": { "text": text, "fragments": [] }, "badges": [] }
        })
        .to_string();
        let mut mac = Hmac::<Sha256>::new_from_slice(ES_SECRET.as_bytes()).unwrap();
        mac.update(id.as_bytes());
        mac.update(ts.as_bytes());
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex(&mac.finalize().into_bytes()));
        let res = self
            .http
            .post(format!("{}/v1/bot/twitch/eventsub", self.base))
            .header("Twitch-Eventsub-Message-Id", &id)
            .header("Twitch-Eventsub-Message-Timestamp", &ts)
            .header("Twitch-Eventsub-Message-Signature", sig)
            .header("Twitch-Eventsub-Message-Type", "notification")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(res.status().is_success(), "EventSub answered {}", res.status());
    }

    fn twitch_replies(&self) -> usize {
        self.tw.with(|t| t.sent.len())
    }

    /// Every hourly sweep the relay runs (`main.rs`), at `now` where a sweep takes it.
    async fn hourly(&self) {
        let now = now_ms();
        self.st.db.prune().unwrap();
        crate::tips::sweep(&self.st).await;
        crate::backups::sweep(&self.st).await;
        crate::remote::sweep(&self.st).await;
        crate::discord::sweep(&self.st).await;
        crate::mail::run_weekly(&self.st, now).await;
        crate::mail::sweep(&self.st, now).await;
        crate::bot::sweep(&self.st).await;
    }

    // ── Setting every feature up ─────────────────────────────────────────────────────

    async fn set_up_everything(&self, auth: &str, install: &str, sub: &str) -> Streamer {
        // F7 history: a stream and its viewer samples.
        let start = now_ms() - 3 * 3_600_000;
        let stream = json!({
            "platform": "twitch", "key": "twitch:lapse", "started_at": start, "ended_at": start + 3_600_000,
            "title": "Ranked grind", "category": "VALORANT", "peak_viewers": 140, "avg_viewers": 96,
            "chat_lines": 1200, "chatters": 80, "new_chatters": 12, "follows": 9, "subs": 3, "tips": [],
            "platform_value": []
        });
        self.ok(M::POST, "/v1/pro/history/streams", auth, Some(stream), 201).await;
        let samples =
            json!({ "platform": "twitch", "key": "twitch:lapse", "samples": [{ "at": start, "viewers": 40 }] });
        self.ok(M::POST, "/v1/pro/history/samples", auth, Some(samples), 200).await;

        // F6 tip page: the page, branding (colours + avatar), a goal, and a custom address
        // proven with the TXT record and live at Cloudflare.
        let page = json!({ "handle": "novaplays", "display_name": "Nova", "message": "Thanks!",
                           "razorpay": { "page_url": "https://razorpay.me/@novaplays", "secret": "rzp-secret" } });
        self.ok(M::PUT, "/v1/tips/config", auth, Some(page), 200).await;
        let brand = json!({ "accent": ACCENT, "background": "#101014", "text": "#f4f4f6" });
        self.ok(M::PUT, "/v1/pro/tips/brand", auth, Some(brand), 200).await;
        let res = self
            .http
            .put(format!("{}/v1/pro/tips/brand/avatar", self.base))
            .bearer_auth(auth)
            .header("content-type", "image/png")
            .body(png())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
        let goal = json!({ "title": "New mic", "target": 1_000_000, "currency": "INR" });
        self.ok(M::PUT, "/v1/pro/tips/goal", auth, Some(goal), 200).await;
        let proof = ProofKeys::new(&SECRET).unwrap().value(install, HOST);
        self.dns.set(&crate::tips::domains::proof::record_name(HOST), &[&proof]);
        let v = self.ok(M::PUT, "/v1/pro/tips/domain", auth, Some(json!({ "hostname": HOST })), 201).await;
        assert_eq!(v["proof"]["value"], proof.as_str(), "{v}");
        self.cf.set_status(HOST, "active", "active");
        self.st.db.c().execute("UPDATE tip_domains SET claimed_at = 0", []).unwrap();
        let v = self.ok(M::GET, "/v1/pro/tips/domain", auth, None, 200).await;
        assert_eq!(v["status"], "live", "{v}");
        assert_eq!(self.on_host(HOST, "/").await, 200);

        // F5 a backup, one part.
        let backup_bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8 ^ (i / 251) as u8).collect();
        let sha = hex(&Sha256::digest(&backup_bytes));
        let v = self
            .ok(
                M::POST,
                "/v1/pro/backups",
                auth,
                Some(json!({ "bytes": backup_bytes.len(), "sha256": sha, "app_version": "0.9.0", "reason": "manual" })),
                201,
            )
            .await;
        let backup = v["id"].as_str().unwrap().to_string();
        let res = self
            .http
            .put(format!("{}/v1/pro/backups/{backup}/parts/0", self.base))
            .bearer_auth(auth)
            .header("content-type", "application/octet-stream")
            .body(backup_bytes.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        self.ok(M::POST, &format!("/v1/pro/backups/{backup}/complete"), auth, Some(json!({})), 201).await;

        // F3 a Ferrocast voice.
        let (s, _, _) = self
            .call(
                M::POST,
                "/v1/pro/tts",
                Some(auth),
                Some(json!({ "text": "Thanks for the tip!", "voice": "google:en-IN-Neural2-B" })),
            )
            .await;
        assert_eq!(s, 200);
        assert_eq!(self.tts.calls().len(), 1);

        // F2 a paired phone.
        let v = self
            .ok(M::POST, "/v1/pro/remote/codes", auth, Some(json!({ "role": "owner", "name": "Asha's phone" })), 201)
            .await;
        let (s, v, text) = self.call(M::POST, "/r/v1/pair", None, Some(json!({ "code": v["code"] }))).await;
        assert_eq!(s, 200, "{text}");
        let phone = v["token"].as_str().unwrap().to_string();
        self.ok(M::GET, "/r/v1/me", &phone, None, 200).await;

        // F1 the cloud bot: Twitch (bot account, then the channel), Kick through the broker,
        // YouTube with the PC's channel proven; a config; switched on; the PC quiet.
        self.twitch_step(auth, "bot", (BOT_ID, "ashabot"), crate::bot::twitch::BOT_SCOPES).await;
        self.twitch_step(auth, "channel", (STREAMER_ID, "asha_live"), crate::bot::twitch::CHANNEL_SCOPES).await;
        self.st.db.set_kick_owner(KICK_STREAMER, install).unwrap();
        let v = self.ok(M::POST, "/v1/pro/bot/kick/connect", auth, Some(json!({})), 201).await;
        let state = state_of(v["auth_url"].as_str().unwrap());
        let (s, _, text) =
            self.call(M::GET, &format!("/v1/kick/callback?code=kc&state={}", enc(&state)), None, None).await;
        assert_eq!(s, 200, "{text}");
        let v = self
            .ok(M::POST, "/v1/pro/bot/youtube/connect", auth, Some(json!({ "owner_token": YT_DESK_TOKEN })), 201)
            .await;
        self.youtube_finish(&state_of(v["auth_url"].as_str().unwrap()), "1//yt-refresh").await;
        self.ok(M::PUT, "/v1/pro/bot/config", auth, Some(json!({ "version": 0, "config": config() })), 200).await;
        self.ok(M::PUT, "/v1/pro/bot/enabled", auth, Some(json!({ "enabled": true })), 200).await;
        let v = self.ok(M::GET, "/v1/pro/bot", auth, None, 200).await;
        let states: Vec<(String, String)> = v["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["platform"].as_str().unwrap().into(), c["state"].as_str().unwrap().into()))
            .collect();
        assert_eq!(states.len(), 3, "{v}");
        assert!(states.iter().all(|(_, s)| s == "ready"), "{states:?}");
        assert_eq!(self.st.db.bot_channel(sub, "twitch").unwrap().unwrap().subscriptions.len(), 3);
        cloud_takes_over_all(&self.st, sub);
        self.twitch_chat("!lurk").await;
        eventually(|| self.twitch_replies() == 1).await;

        // F4 Discord: the server, a sub rule, a viewer who linked, a role given.
        let v = self.ok(M::POST, "/v1/pro/discord/connect", auth, None, 201).await;
        let state = state_of(v["invite_url"].as_str().unwrap());
        let code = format!("code-{}", crate::random(6));
        self.dc.with(|d| {
            d.codes.insert(
                code.clone(),
                Token {
                    access_token: format!("at-{code}"),
                    guild: Some(Guild { id: GUILD.into(), name: "Asha's Lounge".into(), icon: None }),
                },
            )
        });
        let path =
            format!("/discord/callback?code={code}&state={}&guild_id={GUILD}&permissions=268454912", enc(&state));
        let (s, _, text) = self.call(M::GET, &path, None, None).await;
        assert_eq!(s, 200, "{text}");
        let rule = json!({ "kind": "sub", "platform": "twitch", "role_id": ROLE_SUB });
        self.ok(M::PUT, "/v1/pro/discord/rules", auth, Some(rule), 201).await;
        let link = self.ok(M::GET, "/v1/pro/discord", auth, None, 200).await["link_url"].as_str().unwrap().to_string();
        let res = self.http.get(format!("{link}/start")).send().await.unwrap();
        assert_eq!(res.status(), 303);
        let state = state_of(res.headers()["location"].to_str().unwrap());
        let code = format!("v-{}", crate::random(6));
        let token = format!("vt-{code}");
        self.dc.with(|d| {
            d.codes.insert(code.clone(), Token { access_token: token.clone(), guild: None });
            let conns =
                vec![Connection { kind: "twitch".into(), id: TWITCH_ID.into(), name: "fan".into(), verified: true }];
            d.users.insert(token, (VIEWER.into(), conns));
        });
        let (s, _, text) =
            self.call(M::GET, &format!("/discord/callback?code={code}&state={}", enc(&state)), None, None).await;
        assert_eq!(s, 200, "{text}");
        let events = json!([{ "platform": "twitch", "platform_user_id": TWITCH_ID, "kind": "sub", "tier": "1",
                              "at": now_ms() - 1_000 }]);
        let v = self.ok(M::POST, "/v1/pro/discord/perk-events", auth, Some(events), 200).await;
        assert_eq!(v["granted"], 1, "{v}");
        assert!(self.dc.roles_of(VIEWER).contains(ROLE_SUB));

        // F7 the weekly email: saved, then confirmed from the link in the email.
        self.ok(M::PUT, "/v1/pro/history/email", auth, Some(json!({ "address": ADDRESS })), 202).await;
        let text = self.mail.last().expect("a confirm email").text;
        let at = text.find("/mail/confirm?t=").unwrap();
        let link = text[text[..at].rfind("http").unwrap()..].split_whitespace().next().unwrap().to_string();
        let (s, _, _) = self.call(M::POST, &link, None, None).await;
        assert_eq!(s, 200);
        assert_eq!(self.count("SELECT COUNT(*) FROM mail_addresses WHERE confirmed_at IS NOT NULL"), 1);

        Streamer { auth: auth.to_string(), phone, backup, backup_bytes }
    }

    async fn twitch_step(&self, auth: &str, account: &str, who: (&str, &str), scopes: &[&str]) {
        let v = self
            .ok(
                M::POST,
                "/v1/pro/bot/twitch/connect",
                auth,
                Some(json!({ "account": account, "owner_token": DESK })),
                201,
            )
            .await;
        let state = state_of(v["auth_url"].as_str().unwrap());
        let code = format!("code-{}", crate::random(6));
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        self.tw.with(|t| {
            t.codes.insert(
                code.clone(),
                (
                    UserTokens {
                        access_token: format!("at-{code}"),
                        refresh_token: format!("rt-{account}"),
                        scopes: scopes.clone(),
                    },
                    TokenInfo { user_id: who.0.into(), login: who.1.into(), scopes, client_id: "cloud-client".into() },
                ),
            )
        });
        let path = format!("/v1/bot/twitch/callback?code={code}&state={}&scope=x", enc(&state));
        let (s, _, text) = self.call(M::GET, &path, None, None).await;
        assert_eq!(s, 200, "{text}");
    }

    async fn youtube_finish(&self, state: &str, refresh: &str) {
        let code = format!("code-{}", crate::random(6));
        let access = format!("ya29.{code}");
        self.yt.with(|y| {
            y.codes.insert(
                code.clone(),
                GoogleTokens {
                    access_token: access.clone(),
                    refresh_token: refresh.into(),
                    scopes: vec![FORCE_SSL.into()],
                    expires_in: 3_600,
                },
            );
            y.channels.insert(access.clone(), ChannelInfo { id: YT_OWNER.into(), title: "Asha".into() });
            y.grants.insert(access.clone(), YT_OWNER.into());
            y.grants.insert(refresh.into(), YT_OWNER.into());
        });
        let path = format!("/v1/bot/youtube/callback?code={code}&state={}", enc(state));
        let (s, _, text) = self.call(M::GET, &path, None, None).await;
        assert_eq!(s, 200, "{text}");
    }

    // ── Checks ───────────────────────────────────────────────────────────────────────

    /// Every gated Pro route says 402 `pro_required` with the same words and hint.
    async fn all_gated(&self, auth: &str) {
        let routes: Vec<(M, &str, Option<Value>)> = vec![
            (M::GET, "/v1/pro/history", None),
            (M::POST, "/v1/pro/history/streams", Some(json!({}))),
            (M::POST, "/v1/pro/history/samples", Some(json!({}))),
            (M::POST, "/v1/pro/backups", Some(json!({}))),
            (M::GET, "/v1/pro/tips/brand", None),
            (M::PUT, "/v1/pro/tips/brand", Some(json!({ "accent": "#000000" }))),
            (M::GET, "/v1/pro/tips/goal", None),
            (M::PUT, "/v1/pro/tips/goal", Some(json!({}))),
            (M::GET, "/v1/pro/tips/domain", None),
            (M::PUT, "/v1/pro/tips/domain", Some(json!({ "hostname": HOST }))),
            (M::POST, "/v1/pro/tts", Some(json!({ "text": "hi", "voice": "google:en-IN-Neural2-B" }))),
            (M::GET, "/v1/pro/tts/voices", None),
            (M::POST, "/v1/pro/remote/codes", Some(json!({ "role": "owner", "name": "x" }))),
            (M::GET, "/v1/pro/bot", None),
            (M::PUT, "/v1/pro/bot/config", Some(json!({ "version": 1, "config": config() }))),
            (M::PUT, "/v1/pro/bot/enabled", Some(json!({ "enabled": true }))),
            (M::POST, "/v1/pro/bot/twitch/connect", Some(json!({}))),
            (M::POST, "/v1/pro/bot/youtube/connect", Some(json!({}))),
            (M::POST, "/v1/pro/discord/connect", None),
            (M::PUT, "/v1/pro/discord/rules", Some(json!({ "kind": "sub", "role_id": ROLE_SUB }))),
            (M::POST, "/v1/pro/discord/perk-events", Some(json!([]))),
            (M::GET, "/v1/pro/history/email", None),
            (M::PUT, "/v1/pro/history/email", Some(json!({ "address": ADDRESS }))),
        ];
        for (m, path, body) in routes {
            let (s, v, text) = self.call(m.clone(), path, Some(auth), body).await;
            assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{m} {path}: {text}");
            assert_eq!(v["error"]["message"], PRO_WORDS, "{m} {path}");
            assert!(v["error"]["hint"].as_str().unwrap().contains("Settings › Pro"), "{m} {path}: {text}");
        }
    }

    /// The phone is paused, not unpaired.
    async fn phone_paused(&self, p: &Streamer) {
        let (s, v, text) = self.call(M::GET, "/r/v1/me", Some(&p.phone), None).await;
        assert_eq!((s, v["error"]["code"].as_str()), (402, Some("pro_required")), "{text}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("paused"), "{text}");
        assert!(v["error"]["hint"].as_str().unwrap().contains("stays paired"), "{text}");
        assert_eq!(self.count("SELECT COUNT(*) FROM remote_devices"), 1, "the device and its token stay");
    }

    /// What stays open during the 30 days (R-04): the history export, the backups' list and
    /// downloads.
    async fn exports_open(&self, p: &Streamer) {
        let (s, text, _) = {
            let res = self
                .http
                .get(format!("{}/v1/pro/history/export.csv", self.base))
                .bearer_auth(&p.auth)
                .send()
                .await
                .unwrap();
            (res.status().as_u16(), res.text().await.unwrap(), ())
        };
        assert_eq!(s, 200, "{text}");
        assert!(text.contains("Ranked grind"), "{text}");
        let v = self.ok(M::GET, "/v1/pro/backups", &p.auth, None, 200).await;
        assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
        let (s, bytes) = self.bytes(&format!("/v1/pro/backups/{}", p.backup), &p.auth).await;
        assert_eq!(s, 200);
        assert_eq!(bytes, p.backup_bytes, "the backup comes back whole");
    }

    /// The tip page is branded only with Pro.
    async fn page_branded(&self) -> bool {
        let (s, _, text) = self.call(M::GET, "/t/novaplays", None, None).await;
        assert_eq!(s, 200);
        text.contains(ACCENT)
    }
}

fn state_of(url: &str) -> String {
    let u = url::Url::parse(url).unwrap();
    u.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string()
}

/// Halted (every renewal retry failed): Pro ends at once, the cloud stops at the next hourly
/// sweep, the custom address works 7 more days, data waits; then Razorpay recovers the same
/// subscription (the card was updated) within the 30 days and everything comes back.
#[tokio::test]
async fn lapse_halted_then_recovered_within_the_grace_gets_everything_back() {
    let w = world().await;
    let (auth, install, sub) = w.buy(None).await;
    let p = w.set_up_everything(&auth, &install, &sub).await;
    assert!(w.page_branded().await);

    // Renewal retries failed: the period ended 4 days ago, Razorpay halts it.
    w.razorpay_says(&sub, "subscription.halted", "halted", Some(-4)).await;
    let v = w.ok(M::GET, "/v1/pro/status", &auth, None, 200).await;
    assert_eq!(v["pro"], false, "{v}");
    w.all_gated(&auth).await;
    w.phone_paused(&p).await;
    w.exports_open(&p).await;
    assert!(!w.page_branded().await, "the plain page without Pro");
    assert_eq!(w.on_host(HOST, "/").await, 200, "the custom address keeps serving");
    let tts_calls = w.tts.calls().len();

    // The next hourly sweep: Twitch stops sending chat (EventSub subscriptions deleted), so
    // the cloud answers nothing; nothing is deleted or revoked yet.
    w.hourly().await;
    let ch = w.st.db.bot_channel(&sub, "twitch").unwrap().unwrap();
    assert!(ch.subscriptions.is_empty(), "no chat flows after Pro");
    assert_eq!(w.tw.with(|t| t.unsubscribed.len()), 3);
    let replies = w.twitch_replies();
    w.twitch_chat("!lurk").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.twitch_replies(), replies, "the cloud bot is quiet");
    assert!(w.tw.with(|t| t.revoked.is_empty()) && w.kk.with(|k| k.revoked.is_empty()));
    assert!(w.yt.with(|y| y.revoked.is_empty()));
    assert_eq!(w.count("SELECT COUNT(*) FROM tip_domains WHERE lapsed_at IS NOT NULL"), 1);
    assert_eq!(w.count("SELECT COUNT(*) FROM tip_brand WHERE lapsed_at IS NOT NULL"), 1);
    for table in
        ["streams", "cloud_backups", "tip_brand", "tip_goals", "mail_addresses", "bot_channels", "discord_guilds"]
    {
        assert!(w.count(&format!("SELECT COUNT(*) FROM {table}")) > 0, "{table} kept during the 30 days");
    }
    assert!(w.dc.with(|d| d.joined.contains(GUILD)), "the Discord bot stays");
    w.phone_paused(&p).await;
    w.exports_open(&p).await;

    // Seven days on, the custom address goes (Cloudflare and here); the rest waits.
    w.st.db.c().execute("UPDATE tip_domains SET lapsed_at = ?1", [now_ms() - 7 * DAY - 60_000]).unwrap();
    w.hourly().await;
    assert!(!w.cf.has(HOST), "removed from Cloudflare");
    assert_eq!(w.count("SELECT COUNT(*) FROM tip_domains"), 0);
    assert_eq!(w.on_host(HOST, "/").await, 404);
    assert_eq!(w.count("SELECT COUNT(*) FROM streams"), 1);

    // Razorpay recovers the same subscription (paid again) on day 10: everything is back.
    w.rzp.activate(&sub, 30);
    w.hook("evt_recovered", &sub_event("subscription.activated", &sub)).await;
    let v = w.ok(M::GET, "/v1/pro/status", &auth, None, 200).await;
    assert_eq!(v["pro"], true, "{v}");
    let v = w.ok(M::GET, "/v1/pro/history", &auth, None, 200).await;
    assert_eq!(v["streams"].as_array().unwrap().len(), 1, "{v}");
    w.exports_open(&p).await;
    let v = w.ok(M::GET, "/v1/pro/tips/goal", &auth, None, 200).await;
    assert_eq!(v["title"], "New mic", "{v}");
    assert!(w.page_branded().await, "branding is back");
    w.ok(M::GET, "/r/v1/me", &p.phone, None, 200).await;
    let v = w.ok(M::GET, "/v1/pro/history/email", &auth, None, 200).await;
    assert_eq!(v["status"], "on", "{v}");
    let v = w.ok(M::GET, "/v1/pro/discord", &auth, None, 200).await;
    assert_eq!(v["guild"]["id"], GUILD, "{v}");
    assert!(w.dc.roles_of(VIEWER).contains(ROLE_SUB));
    let (s, _, _) = w
        .call(M::POST, "/v1/pro/tts", Some(&auth), Some(json!({ "text": "back", "voice": "google:en-IN-Neural2-B" })))
        .await;
    assert_eq!((s, w.tts.calls().len()), (200, tts_calls + 1));
    let v = w.ok(M::GET, "/v1/pro/bot", &auth, None, 200).await;
    assert_eq!((v["enabled"].as_bool(), v["version"].as_i64()), (Some(true), Some(1)), "{v}");
    // The next hourly sweep subscribes Twitch's chat again and the cloud answers.
    w.hourly().await;
    assert_eq!(w.st.db.bot_channel(&sub, "twitch").unwrap().unwrap().subscriptions.len(), 3);
    cloud_takes_over_all(&w.st, &sub);
    w.twitch_chat("!lurk").await;
    eventually(|| w.twitch_replies() == replies + 1).await;
    // The lapse marks are gone, so nothing is counting down any more.
    assert_eq!(w.count("SELECT COUNT(*) FROM tip_brand WHERE lapsed_at IS NOT NULL"), 0);
    // The address was a shared slot: it went at 7 days and is added again by hand.
    let (s, v, _) = w.call(M::GET, "/v1/pro/tips/domain", Some(&auth), None).await;
    assert_eq!(s, 404, "{v}");
}

/// Cancelled at the end of the period: 30 days after it ended everything goes (rows, files,
/// sign-ins revoked at Twitch, Kick and YouTube, the Discord bot leaves, the address), and a
/// new subscription after that starts clean.
#[tokio::test]
async fn lapse_cancelled_deletes_everything_after_30_days_and_a_new_subscription_starts_clean() {
    let w = world().await;
    let (auth, install, sub) = w.buy(None).await;
    let p = w.set_up_everything(&auth, &install, &sub).await;
    assert!(w.blob_files() >= 2, "the backup and the avatar");

    w.razorpay_says(&sub, "subscription.cancelled", "cancelled", Some(-1)).await;
    w.all_gated(&auth).await;
    w.phone_paused(&p).await;
    w.exports_open(&p).await;
    w.hourly().await;
    assert!(w.st.db.bot_channel(&sub, "twitch").unwrap().unwrap().subscriptions.is_empty());
    assert_eq!(w.on_host(HOST, "/").await, 200);

    // 31 days after the period ended (and as long since the hourly sweep first saw it).
    let long_ago = now_ms() - 31 * DAY;
    w.st.db.c().execute("UPDATE pro_subs SET current_end = ?1", [long_ago]).unwrap();
    for table in ["tip_brand", "tip_goals", "tip_domains"] {
        w.st.db.c().execute(&format!("UPDATE {table} SET lapsed_at = ?1"), [long_ago]).unwrap();
    }
    w.hourly().await;
    for table in [
        "streams",
        "stream_samples",
        "cloud_backups",
        "tip_brand",
        "tip_goals",
        "tip_domains",
        "mail_addresses",
        "bot_channels",
        "bot_tokens",
        "bot_config",
        "discord_guilds",
        "discord_rules",
        "discord_links",
        "discord_grants",
    ] {
        assert_eq!(w.count(&format!("SELECT COUNT(*) FROM {table}")), 0, "{table} deleted 30 days after Pro");
    }
    assert_eq!(w.blob_files(), 0, "the backup and the avatar files are gone");
    assert!(!w.cf.has(HOST));
    assert_eq!(w.on_host(HOST, "/").await, 404);
    let tw_revoked = w.tw.with(|t| t.revoked.clone());
    assert!(tw_revoked.len() >= 2, "both Twitch sign-ins revoked: {tw_revoked:?}");
    assert!(!w.kk.with(|k| k.revoked.is_empty()), "the Kick sign-in revoked");
    assert!(w.yt.with(|y| y.revoked.contains(&"1//yt-refresh".to_string())), "the YouTube grant revoked");
    assert!(!w.dc.with(|d| d.joined.contains(GUILD)), "the Discord bot left the server");
    // The export is closed once nothing is kept; the phone is still told Pro is paused.
    let (s, _, _) = w.call(M::GET, "/v1/pro/history/export.csv", Some(&auth), None).await;
    assert_eq!(s, 402);
    let (s, _, _) = w.call(M::GET, "/v1/pro/backups", Some(&auth), None).await;
    assert_eq!(s, 402);

    // Pro again on the same PC: a new subscription with nothing from before.
    let (auth2, _, sub2) = w.buy(Some(auth.clone())).await;
    assert_ne!(sub2, sub);
    let v = w.ok(M::GET, "/v1/pro/history", &auth2, None, 200).await;
    assert!(v["streams"].as_array().unwrap().is_empty(), "{v}");
    let v = w.ok(M::GET, "/v1/pro/backups", &auth2, None, 200).await;
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    let v = w.ok(M::GET, "/v1/pro/bot", &auth2, None, 200).await;
    assert_eq!((v["enabled"].as_bool(), v["version"].as_i64()), (Some(false), Some(0)), "{v}");
    assert!(v["channels"].as_array().unwrap().is_empty(), "{v}");
    let v = w.ok(M::GET, "/v1/pro/discord", &auth2, None, 200).await;
    assert!(v["guild"].is_null(), "{v}");
    let v = w.ok(M::GET, "/v1/pro/history/email", &auth2, None, 200).await;
    assert_eq!(v["status"], "none", "{v}");
    let (s, _, _) = w.call(M::GET, "/v1/pro/tips/domain", Some(&auth2), None).await;
    assert_eq!(s, 404);
    assert!(!w.page_branded().await, "no branding left over");
    // The old phone belonged to the old subscription: it has to be paired again.
    let (s, v, _) = w.call(M::GET, "/r/v1/me", Some(&p.phone), None).await;
    assert_eq!(s, 401, "{v}");
}

/// Regression: a halted subscription is re-read from Razorpay every day (`reconcile`), which
/// moved its `synced_at` forward, and the 30-day rules counted from `synced_at` when a
/// subscription had no access end, so a halted subscription's data was never deleted. They
/// now count from the paid period's end plus the renewal grace (`SubRow::stopped_at`).
#[tokio::test]
async fn lapse_a_halted_subscription_rechecked_daily_is_still_deleted_after_30_days() {
    let w = world().await;
    let (auth, install, sub) = w.buy(None).await;
    w.set_up_everything(&auth, &install, &sub).await;
    // Halted long ago: the period ended 40 days back.
    w.razorpay_says(&sub, "subscription.halted", "halted", Some(-40)).await;
    // The daily re-check syncs it again (as it does every day).
    assert_eq!(crate::billing::reconcile(&w.st).await, 1);
    let synced: i64 = w.st.db.c().query_row("SELECT synced_at FROM pro_subs", [], |r| r.get(0)).unwrap();
    assert!(now_ms() - synced < 60_000, "re-synced just now");
    w.hourly().await;
    for table in ["streams", "cloud_backups", "mail_addresses", "bot_channels", "discord_guilds"] {
        assert_eq!(w.count(&format!("SELECT COUNT(*) FROM {table}")), 0, "{table}");
    }
}

/// Every table the carry-over moves, with how many rows of it a subscription has.
fn rows_of(w: &World, sub: &str) -> Vec<(&'static str, i64)> {
    [
        "streams",
        "cloud_backups",
        "bot_channels",
        "bot_tokens",
        "bot_config",
        "discord_guilds",
        "discord_rules",
        "mail_addresses",
        "remote_devices",
    ]
    .into_iter()
    .map(|t| {
        let n = w.st.db.c().query_row(&format!("SELECT COUNT(*) FROM {t} WHERE sub_id = ?1"), [sub], |r| r.get(0));
        (t, n.unwrap())
    })
    .collect()
}

/// Decided 2026-10-19: lapsed, then a new checkout (a new subscription id) on the same PC
/// within the 30 days: everything moves to the new id (sealed sign-ins and the address sealed
/// again for it), the Discord roles taken off at the lapse come back, the phone keeps working,
/// and the old id's day 30 deletes nothing. A replayed webhook moves nothing twice.
#[tokio::test]
async fn lapse_then_a_new_subscription_within_30_days_carries_everything_over() {
    let w = world().await;
    let (auth, install, sub) = w.buy(None).await;
    let p = w.set_up_everything(&auth, &install, &sub).await;
    let before = rows_of(&w, &sub);
    assert!(before.iter().all(|(_, n)| *n > 0), "{before:?}");

    // Cancelled; the period ended yesterday. The hourly sweep takes the perk roles off now.
    w.razorpay_says(&sub, "subscription.cancelled", "cancelled", Some(-1)).await;
    w.hourly().await;
    assert!(!w.dc.roles_of(VIEWER).contains(ROLE_SUB), "perk roles come off when Pro ends");
    assert!(w.dc.with(|d| d.joined.contains(GUILD)), "the bot stays in the server");
    w.phone_paused(&p).await;

    // Ten days later, a new checkout on the same PC.
    let (auth2, _, sub2) = w.buy(Some(auth.clone())).await;
    assert_ne!(sub2, sub);
    assert_eq!(rows_of(&w, &sub2), before, "every row moved to the new subscription");
    assert!(rows_of(&w, &sub).iter().all(|(_, n)| *n == 0), "{:?}", rows_of(&w, &sub));
    // The sealed values open for the new id only.
    let v = vault();
    let t = w.st.db.bot_token(&sub2, "twitch", "bot").unwrap().unwrap();
    assert_eq!(v.open(&sub2, &crate::bot::token_purpose("twitch", "bot"), &t).unwrap(), b"rt-bot");
    assert!(v.open(&sub, &crate::bot::token_purpose("twitch", "bot"), &t).is_err());
    let m = w.st.db.mail_row(&sub2).unwrap().unwrap();
    assert_eq!(v.open(&sub2, crate::mail::PURPOSE, &m.on().unwrap().sealed).unwrap(), ADDRESS.as_bytes());

    // Everything is there under the new subscription.
    let v = w.ok(M::GET, "/v1/pro/history", &auth2, None, 200).await;
    assert_eq!(v["streams"].as_array().unwrap().len(), 1, "{v}");
    w.exports_open(&p).await;
    let v = w.ok(M::GET, "/v1/pro/bot", &auth2, None, 200).await;
    assert_eq!((v["enabled"].as_bool(), v["version"].as_i64()), (Some(true), Some(1)), "{v}");
    assert_eq!(v["channels"].as_array().unwrap().len(), 3, "{v}");
    let v = w.ok(M::GET, "/v1/pro/discord", &auth2, None, 200).await;
    assert_eq!(v["guild"]["id"], GUILD, "{v}");
    assert!(w.dc.roles_of(VIEWER).contains(ROLE_SUB), "the paused roles are back");
    let v = w.ok(M::GET, "/v1/pro/history/email", &auth2, None, 200).await;
    assert_eq!(v["status"], "on", "{v}");
    w.ok(M::GET, "/r/v1/me", &p.phone, None, 200).await;
    assert!(w.page_branded().await);

    // Razorpay sends the activation again: nothing changes.
    w.hook(&format!("evt_act_{sub2}"), &sub_event("subscription.activated", &sub2)).await;
    w.hook("evt_act_again", &sub_event("subscription.activated", &sub2)).await;
    assert!(crate::billing::carry::carry_over(&w.st, &sub2).await.from.is_empty(), "nothing left to move");
    assert_eq!(rows_of(&w, &sub2), before);

    // The cloud bot runs again for the new subscription.
    w.hourly().await;
    assert_eq!(w.st.db.bot_channel(&sub2, "twitch").unwrap().unwrap().subscriptions.len(), 3);

    // The old subscription's day 30 passes: nothing of the streamer's is deleted or revoked.
    w.st.db.c().execute("UPDATE pro_subs SET current_end = ?1 WHERE id = ?2", (now_ms() - 31 * DAY, &sub)).unwrap();
    w.hourly().await;
    assert_eq!(rows_of(&w, &sub2), before);
    assert!(w.tw.with(|t| t.revoked.is_empty()) && w.kk.with(|k| k.revoked.is_empty()));
    assert!(w.yt.with(|y| y.revoked.is_empty()));
    assert!(w.dc.with(|d| d.joined.contains(GUILD)), "the Discord bot stays");
    assert!(w.dc.roles_of(VIEWER).contains(ROLE_SUB));
    w.exports_open(&p).await;
}

/// More than 30 days after the old subscription stopped, a new one takes nothing over (even
/// before the hourly sweep has deleted it).
#[tokio::test]
async fn lapse_a_new_subscription_after_30_days_takes_nothing_over() {
    let w = world().await;
    let (auth, install, sub) = w.buy(None).await;
    w.set_up_everything(&auth, &install, &sub).await;
    let before = rows_of(&w, &sub);
    w.razorpay_says(&sub, "subscription.cancelled", "cancelled", Some(-31)).await;
    let (_, _, sub2) = w.buy(Some(auth.clone())).await;
    assert!(rows_of(&w, &sub2).iter().all(|(_, n)| *n == 0), "{:?}", rows_of(&w, &sub2));
    assert_eq!(rows_of(&w, &sub), before, "left for the sweep");
    w.hourly().await;
    // (A paired phone stays until its token expires; it can't reach anything.)
    let gone = rows_of(&w, &sub);
    assert!(gone.iter().filter(|(t, _)| *t != "remote_devices").all(|(_, n)| *n == 0), "{gone:?}");
    assert!(rows_of(&w, &sub2).iter().all(|(_, n)| *n == 0));
}

/// Another person's install never pulls a lapsed subscription's data, even with the same
/// checkout email; nor does a test (comp) grant on the owner's own install.
#[tokio::test]
async fn lapse_another_install_or_a_test_grant_never_takes_the_data() {
    let w = world().await;
    let (auth, install, sub) = w.buy(None).await;
    w.set_up_everything(&auth, &install, &sub).await;
    let before = rows_of(&w, &sub);
    w.razorpay_says(&sub, "subscription.cancelled", "cancelled", Some(-1)).await;

    // Someone else's PC buys Pro (with the same email, even).
    let (auth_b, _, sub_b) = w.buy(None).await;
    assert!(rows_of(&w, &sub_b).iter().all(|(_, n)| *n == 0), "{:?}", rows_of(&w, &sub_b));
    let v = w.ok(M::GET, "/v1/pro/history", &auth_b, None, 200).await;
    assert!(v["streams"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(rows_of(&w, &sub), before);

    // A test grant on the owner's PC: not a target.
    let g = w.st.db.comp_grant(&install, 5).unwrap();
    assert!(crate::billing::carry::carry_over(&w.st, &g.sub_id).await.from.is_empty());
    assert_eq!(rows_of(&w, &sub), before);
    // A different checkout email from the same PC isn't taken as the same owner either.
    w.st.db.comp_revoke(&install).unwrap();
    w.st.db.c().execute("UPDATE pro_subs SET email_hash = 'other' WHERE id = ?1", [&sub]).unwrap();
    let (_, _, sub2) = w.buy(Some(auth)).await;
    assert!(rows_of(&w, &sub2).iter().all(|(_, n)| *n == 0));
    assert_eq!(rows_of(&w, &sub), before);
}
