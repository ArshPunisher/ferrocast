//! The Discord bot: pure parts (state signing, rule checks and matching, signatures, embeds),
//! then every route and page through the real `router()` with a fake Discord (no network):
//! the gate, connecting a server (state tampering, a crafted `guild_id`), channels, go-live
//! posts, rules and their quota, viewer linking, perk events with Discord's 50013 / 429 / 404,
//! the expiry sweep, the lapse, disconnecting and the interactions endpoint.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};

use super::api::{Channel, Connection, DiscordApi, DiscordError, Fut, Guild, Role, Token, codes};
use super::*;
use crate::billing::store::Synced;
use crate::{Config, Db, router};

const DAY: i64 = 86_400_000;
pub(crate) const GUILD: &str = "900000000000000001";
const OTHER_GUILD: &str = "900000000000000002";
const CH_TEXT: &str = "900000000000000010";
const CH_NEWS: &str = "900000000000000011";
const CH_VOICE: &str = "900000000000000012";
const ROLE_VIP: &str = "900000000000000020";
pub(crate) const ROLE_SUB: &str = "900000000000000021";
const ROLE_ADMIN: &str = "900000000000000022";
const ROLE_BOT: &str = "900000000000000023";
pub(crate) const VIEWER: &str = "800000000000000001";
const VIEWER_2: &str = "800000000000000002";
pub(crate) const TWITCH_ID: &str = "141981764";
const YT_ID: &str = "UCabcdefghijklmnopqrstuv";
const APP_ID: &str = "1561463863197835294";

// ── A fake Discord ───────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub(crate) struct Fake {
    channels: Vec<Channel>,
    roles: Vec<Role>,
    bot_roles: Vec<String>,
    /// Members of `GUILD` and their roles.
    pub(crate) members: HashMap<String, BTreeSet<String>>,
    /// Servers the bot is in.
    pub(crate) joined: BTreeSet<String>,
    /// message id → (channel, body)
    messages: HashMap<String, (String, Value)>,
    next_message: u64,
    /// OAuth code → token; token → (user id, connections).
    pub(crate) codes: HashMap<String, Token>,
    pub(crate) users: HashMap<String, (String, Vec<Connection>)>,
    pub(crate) revoked: Vec<String>,
    /// method → the error it fails with.
    fail: HashMap<&'static str, DiscordError>,
    /// `add_role` for this role fails with 50013 (it's above Ferrocast's).
    refuse_role: Option<String>,
    calls: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct FakeDiscord(Mutex<Fake>);

impl FakeDiscord {
    pub(crate) fn standard() -> FakeDiscord {
        let f = FakeDiscord::default();
        {
            let mut d = f.0.lock().unwrap();
            let ch =
                |id: &str, name: &str, kind, position| Channel { id: id.into(), name: name.into(), kind, position };
            d.channels =
                vec![ch(CH_NEWS, "announcements", 5, 0), ch(CH_VOICE, "Lounge", 2, 1), ch(CH_TEXT, "going-live", 0, 2)];
            let role = |id: &str, name: &str, position, managed| Role {
                id: id.into(),
                name: name.into(),
                color: 0x5865F2,
                position,
                managed,
            };
            d.roles = vec![
                role(GUILD, "@everyone", 0, false),
                role(ROLE_SUB, "Subscriber", 2, false),
                role(ROLE_VIP, "VIP", 3, false),
                role(ROLE_BOT, "Ferrocast", 5, true),
                role(ROLE_ADMIN, "Admin", 10, false),
            ];
            d.bot_roles = vec![ROLE_BOT.into()];
            d.members.insert(VIEWER.into(), BTreeSet::new());
            d.members.insert(VIEWER_2.into(), BTreeSet::new());
            d.joined.insert(GUILD.into());
            d.joined.insert(OTHER_GUILD.into());
        }
        f
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut Fake) -> T) -> T {
        f(&mut self.0.lock().unwrap())
    }

    fn fail(&self, method: &'static str, e: Option<DiscordError>) {
        self.with(|d| match e {
            Some(e) => {
                d.fail.insert(method, e);
            }
            None => {
                d.fail.remove(method);
            }
        });
    }

    pub(crate) fn roles_of(&self, user: &str) -> BTreeSet<String> {
        self.with(|d| d.members.get(user).cloned().unwrap_or_default())
    }

    pub(crate) fn calls(&self, prefix: &str) -> usize {
        self.with(|d| d.calls.iter().filter(|c| c.starts_with(prefix)).count())
    }

    fn step<T>(
        &self,
        method: &'static str,
        detail: String,
        f: impl FnOnce(&mut Fake) -> Result<T, DiscordError>,
    ) -> Result<T, DiscordError> {
        let mut d = self.0.lock().unwrap();
        d.calls.push(format!("{method} {detail}"));
        if let Some(e) = d.fail.get(method) {
            return Err(e.clone());
        }
        f(&mut d)
    }
}

impl DiscordApi for FakeDiscord {
    fn exchange_code<'a>(&'a self, code: &'a str, redirect_uri: &'a str) -> Fut<'a, Token> {
        Box::pin(async move {
            self.step("exchange_code", format!("{code} {redirect_uri}"), |d| {
                d.codes.remove(code).ok_or_else(|| DiscordError::BadRequest("Invalid \"code\" in request.".into()))
            })
        })
    }
    fn revoke<'a>(&'a self, access_token: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            self.step("revoke", String::new(), |d| {
                d.revoked.push(access_token.to_string());
                Ok(())
            })
        })
    }
    fn me<'a>(&'a self, access_token: &'a str) -> Fut<'a, String> {
        Box::pin(async move {
            self.step("me", String::new(), |d| {
                d.users.get(access_token).map(|u| u.0.clone()).ok_or(DiscordError::Unauthorized)
            })
        })
    }
    fn connections<'a>(&'a self, access_token: &'a str) -> Fut<'a, Vec<Connection>> {
        Box::pin(async move {
            self.step("connections", String::new(), |d| {
                d.users.get(access_token).map(|u| u.1.clone()).ok_or(DiscordError::Unauthorized)
            })
        })
    }
    fn guild<'a>(&'a self, guild: &'a str) -> Fut<'a, Guild> {
        Box::pin(async move {
            self.step("guild", guild.into(), |_| {
                Ok(Guild { id: guild.into(), name: "Asha's Lounge".into(), icon: None })
            })
        })
    }
    fn channels<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<Channel>> {
        Box::pin(async move {
            self.step("channels", guild.into(), |d| {
                if d.joined.contains(guild) {
                    Ok(d.channels.clone())
                } else {
                    Err(DiscordError::NotFound(codes::UNKNOWN_GUILD))
                }
            })
        })
    }
    fn roles<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<Role>> {
        Box::pin(async move {
            self.step("roles", guild.into(), |d| {
                if d.joined.contains(guild) {
                    Ok(d.roles.clone())
                } else {
                    Err(DiscordError::NotFound(codes::UNKNOWN_GUILD))
                }
            })
        })
    }
    fn bot_roles<'a>(&'a self, guild: &'a str) -> Fut<'a, Vec<String>> {
        Box::pin(async move { self.step("bot_roles", guild.into(), |d| Ok(d.bot_roles.clone())) })
    }
    fn add_role<'a>(&'a self, guild: &'a str, user: &'a str, role: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            self.step("add_role", format!("{guild} {user} {role}"), |d| {
                if !d.joined.contains(guild) {
                    return Err(DiscordError::NotFound(codes::UNKNOWN_GUILD));
                }
                if !d.roles.iter().any(|r| r.id == role) {
                    return Err(DiscordError::NotFound(codes::UNKNOWN_ROLE));
                }
                if d.refuse_role.as_deref() == Some(role) {
                    return Err(DiscordError::MissingPermission);
                }
                d.members.get_mut(user).ok_or(DiscordError::NotFound(codes::UNKNOWN_MEMBER))?.insert(role.into());
                Ok(())
            })
        })
    }
    fn remove_role<'a>(&'a self, guild: &'a str, user: &'a str, role: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            self.step("remove_role", format!("{guild} {user} {role}"), |d| {
                d.members.get_mut(user).ok_or(DiscordError::NotFound(codes::UNKNOWN_MEMBER))?.remove(role);
                Ok(())
            })
        })
    }
    fn post<'a>(&'a self, channel: &'a str, body: Value) -> Fut<'a, String> {
        Box::pin(async move {
            self.step("post", channel.into(), |d| {
                if !d.channels.iter().any(|c| c.id == channel) {
                    return Err(DiscordError::NotFound(codes::UNKNOWN_CHANNEL));
                }
                d.next_message += 1;
                let id = format!("70000000000000{:04}", d.next_message);
                d.messages.insert(id.clone(), (channel.into(), body));
                Ok(id)
            })
        })
    }
    fn edit<'a>(&'a self, channel: &'a str, message: &'a str, body: Value) -> Fut<'a, ()> {
        Box::pin(async move {
            self.step("edit", message.into(), |d| match d.messages.get_mut(message) {
                Some(m) if m.0 == channel => {
                    m.1 = body;
                    Ok(())
                }
                _ => Err(DiscordError::NotFound(10008)),
            })
        })
    }
    fn leave<'a>(&'a self, guild: &'a str) -> Fut<'a, ()> {
        Box::pin(async move {
            self.step("leave", guild.into(), |d| {
                if d.joined.remove(guild) { Ok(()) } else { Err(DiscordError::NotFound(codes::UNKNOWN_GUILD)) }
            })
        })
    }
}

// ── The world ────────────────────────────────────────────────────────────────────────

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn config() -> DiscordConfig {
    DiscordConfig::from_values(
        Some(APP_ID.into()),
        Some("bot-token-SECRET".into()),
        Some("client-secret-SECRET".into()),
        Some(hex(signing_key().verifying_key().as_bytes())),
    )
    .unwrap()
    .unwrap()
}

struct World {
    base: String,
    db: Arc<Db>,
    state: AppState,
    fake: Arc<FakeDiscord>,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

async fn world() -> World {
    world_with(true).await
}

async fn world_with(configured: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
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
    let fake = Arc::new(FakeDiscord::standard());
    let rt = if configured {
        Runtime::new(Some(config()), Some(fake.clone() as Arc<dyn DiscordApi>)).without_pauses()
    } else {
        Runtime::default()
    };
    let state = AppState::new(cfg, db).with_discord(rt);
    let db = state.db.clone();
    let app = router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    World { base, db, state, fake, http, dir }
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

impl World {
    fn install(&self, id: &str) -> String {
        self.db.add_install(id, &crate::hash("secret")).unwrap();
        format!("{id}.secret")
    }

    fn pro(&self, id: &str, sub: &str) -> String {
        let auth = self.install(id);
        self.db.add_pro_sub(sub, "monthly", "e", "created", id).unwrap();
        self.set_sub(sub, "active", Some(now_ms() + 30 * DAY));
        auth
    }

    fn set_sub(&self, sub: &str, status: &str, current_end: Option<i64>) {
        let s = Synced { status: status.into(), current_end, cancel_at_end: None };
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

    async fn get(&self, path: &str, auth: &str) -> Answer {
        self.call(reqwest::Method::GET, path, Some(auth), None).await
    }
    async fn put(&self, path: &str, auth: &str, body: Value) -> Answer {
        self.call(reqwest::Method::PUT, path, Some(auth), Some(body)).await
    }
    async fn post(&self, path: &str, auth: &str, body: Value) -> Answer {
        self.call(reqwest::Method::POST, path, Some(auth), Some(body)).await
    }
    async fn page(&self, path: &str) -> Answer {
        self.call(reqwest::Method::GET, path, None, None).await
    }

    /// Connects `GUILD` for this install the way a browser would: connect, Discord's page,
    /// the callback with the code Discord gives.
    async fn connect(&self, auth: &str) -> Answer {
        self.connect_guild(auth, GUILD, "Asha's Lounge").await
    }

    async fn connect_guild(&self, auth: &str, guild: &str, name: &str) -> Answer {
        let a = self.call(reqwest::Method::POST, "/v1/pro/discord/connect", Some(auth), None).await;
        assert_eq!(a.status, 201, "{}", a.text);
        let invite = url::Url::parse(a.json()["invite_url"].as_str().unwrap()).unwrap();
        let state = invite.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
        let code = format!("code-{}", crate::random(6));
        self.fake.with(|d| {
            d.codes.insert(
                code.clone(),
                Token {
                    access_token: format!("at-{code}"),
                    guild: Some(Guild { id: guild.into(), name: name.into(), icon: Some("abc123".into()) }),
                },
            )
        });
        self.page(&format!(
            "/discord/callback?code={code}&state={}&guild_id={guild}&permissions=268454912",
            enc(&state)
        ))
        .await
    }

    async fn view(&self, auth: &str) -> Value {
        let a = self.get("/v1/pro/discord", auth).await;
        assert_eq!(a.status, 200, "{}", a.text);
        a.json()
    }

    async fn add_rule(&self, auth: &str, rule: Value) -> Answer {
        self.put("/v1/pro/discord/rules", auth, rule).await
    }

    /// The viewer link flow for `user` with these connections; the result page.
    async fn link(&self, link_url: &str, user: &str, conns: Vec<Connection>) -> Answer {
        let path = link_url.strip_prefix(&self.base).unwrap().to_string();
        let start = self.page(&format!("{path}/start")).await;
        assert_eq!(start.status, 303, "{}", start.text);
        let to = url::Url::parse(start.headers["location"].to_str().unwrap()).unwrap();
        let state = to.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
        let code = format!("v-{}", crate::random(6));
        let token = format!("vt-{code}");
        self.fake.with(|d| {
            d.codes.insert(code.clone(), Token { access_token: token.clone(), guild: None });
            d.users.insert(token, (user.into(), conns));
        });
        self.page(&format!("/discord/callback?code={code}&state={}", enc(&state))).await
    }
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn conn(kind: &str, id: &str, name: &str, verified: bool) -> Connection {
    Connection { kind: kind.into(), id: id.into(), name: name.into(), verified }
}

fn sub_event(user: &str, tier: &str) -> Value {
    json!({ "platform": "twitch", "platform_user_id": user, "kind": "sub", "tier": tier, "at": now_ms() })
}

/// Connected, a channel, a sub rule on ROLE_SUB and a linked viewer (Twitch + YouTube).
async fn ready(w: &World) -> String {
    let auth = w.pro("i1", "sub_1");
    assert_eq!(w.connect(&auth).await.status, 200);
    let a = w.add_rule(&auth, json!({ "kind": "sub", "platform": "twitch", "role_id": ROLE_SUB })).await;
    assert_eq!(a.status, 201, "{}", a.text);
    let link = w.view(&auth).await["link_url"].as_str().unwrap().to_string();
    let page = w
        .link(
            &link,
            VIEWER,
            vec![conn("twitch", TWITCH_ID, "asha_fan", true), conn("youtube", YT_ID, "Asha Fan", true)],
        )
        .await;
    assert_eq!(page.status, 200, "{}", page.text);
    auth
}

// ── Pure parts ───────────────────────────────────────────────────────────────────────

#[test]
fn the_config_needs_all_four_and_never_shows_its_secrets() {
    assert!(DiscordConfig::from_values(None, None, None, None).unwrap().is_none());
    assert!(DiscordConfig::from_values(Some(APP_ID.into()), None, None, None).is_err(), "partly set is an error");
    let bad_key =
        DiscordConfig::from_values(Some(APP_ID.into()), Some("t".into()), Some("s".into()), Some("zz".into()));
    assert!(bad_key.unwrap_err().contains("DISCORD_PUBLIC_KEY"));
    let bad_id =
        DiscordConfig::from_values(Some("app".into()), Some("t".into()), Some("s".into()), Some("00".repeat(32)));
    assert!(bad_id.is_err());
    let shown = format!("{:?}", config());
    assert!(!shown.contains("bot-token-SECRET") && !shown.contains("client-secret-SECRET"), "{shown}");
    let http = HttpDiscord::new("x".into(), APP_ID.into(), "bot-token-SECRET".into(), "client-secret-SECRET".into());
    let shown = format!("{http:?}");
    assert!(!shown.contains("SECRET"), "{shown}");
    let t = Token { access_token: "viewer-token".into(), guild: None };
    assert!(!format!("{t:?}").contains("viewer-token"));
}

#[test]
fn the_invite_asks_for_exactly_four_permissions() {
    assert_eq!(PERMISSIONS, 268_435_456 + 2_048 + 16_384 + 1_024);
}

#[test]
fn a_state_is_tied_to_its_purpose_subject_and_ten_minutes() {
    let cfg = config();
    let t = 1_800_000_000_000;
    let s = sign_state(&cfg, Purpose::Install, "inst-1", t);
    assert_eq!(check_state(&cfg, &s, t + 1000), Some((Purpose::Install, "inst-1".to_string())));
    assert_eq!(check_state(&cfg, &s, t + STATE_TTL_MS + 1), None, "expired");
    // Change the subject, the purpose or the expiry: the MAC no longer matches.
    let swapped = s.replacen("inst-1", "inst-2", 1);
    assert_eq!(check_state(&cfg, &swapped, t), None);
    let relabelled = format!("l{}", &s[1..]);
    assert_eq!(check_state(&cfg, &relabelled, t), None);
    let parts: Vec<&str> = s.split('.').collect();
    let later = format!("{}.{}.{}.{}.{}", parts[0], parts[1], t + 99 * STATE_TTL_MS, parts[3], parts[4]);
    assert_eq!(check_state(&cfg, &later, t), None);
    assert_eq!(check_state(&cfg, "", t), None);
    assert_eq!(check_state(&cfg, "i.x.1.2", t), None);
    // Another app's secret never verifies ours.
    let other = DiscordConfig { client_secret: "else".into(), ..cfg.clone() };
    assert_eq!(check_state(&other, &s, t), None);
}

#[test]
fn rules_keep_only_what_their_kind_uses() {
    let r = |v: Value| perks::check_rule(serde_json::from_value(v).unwrap(), "dr_1".into(), 5);
    let ok = r(json!({ "kind": "sub_tier", "platform": "twitch", "tier": "2", "role_id": ROLE_VIP, "min_minor": 5, "currency": "INR" })).unwrap();
    assert_eq!((ok.tier.as_deref(), ok.min_minor, ok.currency), (Some("2"), None, None));
    assert_eq!(ok.keep_days, 31, "default");
    let tip =
        r(json!({ "kind": "tip_min", "min_minor": 50_000, "currency": "inr", "role_id": ROLE_VIP, "keep_days": 0 }))
            .unwrap();
    assert_eq!(
        (tip.currency.as_deref(), tip.min_minor, tip.keep_days, tip.platform),
        (Some("INR"), Some(50_000), 0, None)
    );
    let member = r(json!({ "kind": "member", "tier": "Gold", "role_id": ROLE_VIP })).unwrap();
    assert_eq!((member.platform.as_deref(), member.tier.as_deref()), (Some("youtube"), Some("Gold")));
    for (bad, field) in [
        (json!({ "kind": "boss", "role_id": ROLE_VIP }), "kind"),
        (json!({ "kind": "sub", "platform": "tiktok", "role_id": ROLE_VIP }), "platform"),
        (json!({ "kind": "sub", "platform": "youtube", "role_id": ROLE_VIP }), "platform"),
        (json!({ "kind": "sub", "role_id": "@VIP" }), "role_id"),
        (json!({ "kind": "sub", "role_id": ROLE_VIP, "keep_days": 400 }), "keep_days"),
        (json!({ "kind": "sub_tier", "tier": "4", "role_id": ROLE_VIP }), "tier"),
        (json!({ "kind": "member", "platform": "kick", "role_id": ROLE_VIP }), "platform"),
        (json!({ "kind": "tip_min", "currency": "INR", "role_id": ROLE_VIP }), "min_minor"),
        (json!({ "kind": "tip_min", "min_minor": 100, "currency": "rupees", "role_id": ROLE_VIP }), "currency"),
    ] {
        let e = r(bad.clone()).unwrap_err();
        assert_eq!(e.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert!(e.message().starts_with(field), "{bad}: {}", e.message());
    }
}

#[test]
fn rules_match_events_by_kind_platform_tier_and_amount() {
    let rule = |v: Value| perks::check_rule(serde_json::from_value(v).unwrap(), "dr".into(), 0).unwrap();
    let ev = |v: Value| serde_json::from_value::<perks::PerkEvent>(v).unwrap();
    let any_sub = rule(json!({ "kind": "sub", "role_id": ROLE_SUB }));
    let tier2 = rule(json!({ "kind": "sub_tier", "tier": "2", "role_id": ROLE_VIP }));
    let gold = rule(json!({ "kind": "member", "tier": "gold", "role_id": ROLE_VIP }));
    let member = rule(json!({ "kind": "member", "role_id": ROLE_VIP }));
    let big_tip = rule(json!({ "kind": "tip_min", "min_minor": 50_000, "currency": "INR", "role_id": ROLE_VIP }));
    let kick_sub = ev(json!({ "platform": "kick", "platform_user_id": "55", "kind": "sub", "at": 1 }));
    let t1 = ev(json!({ "platform": "twitch", "platform_user_id": "55", "kind": "sub", "tier": "1", "at": 1 }));
    let prime = ev(json!({ "platform": "twitch", "platform_user_id": "55", "kind": "sub", "tier": "prime", "at": 1 }));
    let t3 = ev(json!({ "platform": "twitch", "platform_user_id": "55", "kind": "sub", "tier": "3", "at": 1 }));
    let yt_gold =
        ev(json!({ "platform": "youtube", "platform_user_id": "UC1", "kind": "member", "tier": "Gold", "at": 1 }));
    let yt_silver =
        ev(json!({ "platform": "youtube", "platform_user_id": "UC1", "kind": "member", "tier": "Silver", "at": 1 }));
    let tip = |n: i64, cur: &str| {
        ev(
            json!({ "platform": "youtube", "platform_user_id": "UC1", "kind": "tip", "amount_minor": n, "currency": cur, "at": 1 }),
        )
    };
    assert!(perks::matches(&any_sub, &kick_sub) && perks::matches(&any_sub, &prime));
    assert!(!perks::matches(&any_sub, &yt_gold));
    assert!(!perks::matches(&tier2, &t1) && !perks::matches(&tier2, &prime) && perks::matches(&tier2, &t3));
    assert!(perks::matches(&gold, &yt_gold) && !perks::matches(&gold, &yt_silver), "level names, any case");
    assert!(perks::matches(&member, &yt_silver));
    assert!(perks::matches(&big_tip, &tip(50_000, "INR")) && !perks::matches(&big_tip, &tip(49_999, "INR")));
    assert!(!perks::matches(&big_tip, &tip(90_000, "USD")), "other currencies never count (R-32)");
    let twitch_only = rule(json!({ "kind": "sub", "platform": "twitch", "role_id": ROLE_SUB }));
    assert!(!perks::matches(&twitch_only, &kick_sub));
    assert_eq!(perks::ends(&any_sub, 1_000), Some(1_000 + 31 * DAY));
    assert_eq!(perks::ends(&big_tip, 1_000), Some(1_000 + 31 * DAY));
    let forever = rule(json!({ "kind": "sub", "role_id": ROLE_SUB, "keep_days": 0 }));
    assert_eq!(perks::ends(&forever, 1_000), None);
}

#[test]
fn signatures_are_checked_with_the_apps_public_key() {
    let key = signing_key();
    let pk = key.verifying_key();
    let body = br#"{"type":1}"#;
    let now = 1_800_000_000;
    let ts = now.to_string();
    let sig = hex(&key.sign(&[ts.as_bytes(), body].concat()).to_bytes());
    assert!(interactions::verify(&pk, &sig, &ts, body, now));
    assert!(!interactions::verify(&pk, &sig, &ts, br#"{"type":2}"#, now), "another body");
    assert!(!interactions::verify(&pk, &sig, &(now + 1).to_string(), body, now), "another timestamp");
    assert!(!interactions::verify(&pk, &sig, &ts, body, now + interactions::MAX_SKEW_S + 1), "too old");
    assert!(!interactions::verify(&pk, "abc", &ts, body, now));
    assert!(!interactions::verify(&pk, &"0".repeat(128), &ts, body, now));
    let other = SigningKey::from_bytes(&[9u8; 32]);
    let forged = hex(&other.sign(&[ts.as_bytes(), body].concat()).to_bytes());
    assert!(!interactions::verify(&pk, &forged, &ts, body, now), "signed by someone else");
}

#[test]
fn hex_is_strict() {
    assert_eq!(interactions::decode_hex("0aFf"), Some(vec![0x0a, 0xff]));
    // `u8::from_str_radix` alone takes a leading `+`; odd lengths and non-ASCII never pass.
    for bad in ["+1", "+a+b", "0", "0g", "é0", " 01", "0x01"] {
        assert_eq!(interactions::decode_hex(bad), None, "{bad:?}");
    }
    let key = hex(signing_key().verifying_key().as_bytes());
    assert!(interactions::parse_public_key(&key).is_some());
    assert!(interactions::parse_public_key(&format!("+{}", &key[1..])).is_none());
}

#[test]
fn durations_and_times_read_like_people_write_them() {
    assert_eq!(live::duration(45 * 60_000), "45 min");
    assert_eq!(live::duration(2 * 3_600_000 + 14 * 60_000), "2 h 14 min");
    assert_eq!(live::duration(3 * 3_600_000), "3 h");
    assert_eq!(live::iso(1_792_411_200_000 + 3_723_000), "2026-10-19T13:02:03Z");
}

#[test]
fn discords_errors_map_to_what_the_relay_can_do() {
    use super::api::classify;
    assert_eq!(classify(403, &json!({ "code": 50013 }), None), DiscordError::MissingPermission);
    assert_eq!(classify(403, &json!({ "code": 50001 }), None), DiscordError::MissingAccess);
    assert_eq!(classify(403, &json!({}), None), DiscordError::MissingPermission);
    assert_eq!(classify(429, &json!({ "retry_after": 2.3 }), None), DiscordError::RateLimited { retry_after_s: 3 });
    assert_eq!(classify(429, &json!({}), Some(7.0)), DiscordError::RateLimited { retry_after_s: 7 });
    assert_eq!(classify(404, &json!({ "code": 10004 }), None), DiscordError::NotFound(codes::UNKNOWN_GUILD));
    assert_eq!(classify(401, &json!({}), None), DiscordError::Unauthorized);
    assert!(
        matches!(classify(400, &json!({ "error_description": "Invalid code" }), None), DiscordError::BadRequest(m) if m == "Invalid code")
    );
    assert!(matches!(classify(502, &json!({}), None), DiscordError::Unavailable(_)));
    let e = discord_error(DiscordError::MissingPermission);
    assert_eq!((e.status(), e.code), (StatusCode::FORBIDDEN, "missing_permission"));
    assert_eq!(e.hint.as_deref(), Some(ROLE_HINT));
}

// ── The gate and the status ──────────────────────────────────────────────────────────

#[tokio::test]
async fn every_route_needs_a_known_install_with_pro() {
    let w = world().await;
    let free = w.install("free");
    let routes: [(reqwest::Method, &str); 10] = [
        (reqwest::Method::GET, "/v1/pro/discord"),
        (reqwest::Method::POST, "/v1/pro/discord/connect"),
        (reqwest::Method::PUT, "/v1/pro/discord/channel"),
        (reqwest::Method::PUT, "/v1/pro/discord/go-live"),
        (reqwest::Method::PUT, "/v1/pro/discord/rules"),
        (reqwest::Method::PUT, "/v1/pro/discord/rules/dr_1"),
        (reqwest::Method::DELETE, "/v1/pro/discord/rules/dr_1"),
        (reqwest::Method::POST, "/v1/pro/discord/perk-events"),
        (reqwest::Method::POST, "/v1/pro/discord/live"),
        (reqwest::Method::DELETE, "/v1/pro/discord"),
    ];
    for (m, path) in routes {
        let body = Some(json!({ "nonsense": true }));
        let a = w.call(m.clone(), path, None, body.clone()).await;
        assert_eq!((a.status, a.code().as_str()), (401, "unauthorized"), "{path}");
        let a = w.call(m.clone(), path, Some(&free), body).await;
        assert_eq!((a.status, a.code().as_str()), (402, "pro_required"), "{path}");
    }
    assert_eq!(w.fake.calls(""), 0, "Discord was never called");
}

#[tokio::test]
async fn without_the_discord_app_the_feature_is_off() {
    let w = world_with(false).await;
    let auth = w.pro("i1", "sub_1");
    let s = w.get("/v1/pro/status", &auth).await.json();
    assert_eq!(s["features"]["discord"], false);
    let a = w.get("/v1/pro/discord", &auth).await;
    assert_eq!((a.status, a.code().as_str()), (503, "discord_unavailable"));
    let a = w.call(reqwest::Method::POST, "/v1/pro/discord/connect", Some(&auth), None).await;
    assert_eq!(a.status, 503);
    assert_eq!(w.page("/discord/callback?code=x&state=y").await.status, 503);
    assert_eq!(w.page("/d/abcdefghijk/link").await.status, 503);
    let a = w.call(reqwest::Method::POST, "/discord/interactions", None, Some(json!({ "type": 1 }))).await;
    assert_eq!(a.status, 503);
}

#[tokio::test]
async fn the_status_says_discord_is_on_and_a_new_view_is_empty() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let s = w.get("/v1/pro/status", &auth).await.json();
    assert_eq!(s["features"]["discord"], true);
    let a = w.get("/v1/pro/discord", &auth).await;
    assert_eq!(a.headers["cache-control"], "no-store");
    let v = a.json();
    assert_eq!(v["connected"], false);
    assert_eq!(v["link_url"], Value::Null);
    assert_eq!(v["limits"], json!({ "guilds": 1, "rules": 10, "links": 5000 }));
    assert_eq!(v["go_live"], json!({ "enabled": false, "message": null }));
}

// ── Connecting a server ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn connect_gives_discords_install_page_with_a_signed_state() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w.call(reqwest::Method::POST, "/v1/pro/discord/connect", Some(&auth), None).await;
    assert_eq!(a.status, 201);
    let v = a.json();
    let expires = v["expires_at"].as_i64().unwrap();
    assert!((expires - now_ms() - STATE_TTL_MS).abs() < 5_000);
    let u = url::Url::parse(v["invite_url"].as_str().unwrap()).unwrap();
    assert_eq!((u.host_str(), u.path()), (Some("discord.com"), "/oauth2/authorize"));
    let q: HashMap<String, String> = u.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
    assert_eq!(q["client_id"], APP_ID);
    assert_eq!(q["scope"], "bot applications.commands");
    assert_eq!(q["permissions"], "268454912");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["redirect_uri"], format!("{}/discord/callback", w.base));
    assert_eq!(check_state(&config(), &q["state"], now_ms()), Some((Purpose::Install, "i1".to_string())));
}

#[tokio::test]
async fn the_callback_trusts_discords_token_answer_not_the_url() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    // The URL says OTHER_GUILD; Discord's token answer says GUILD. GUILD wins.
    let a = w.call(reqwest::Method::POST, "/v1/pro/discord/connect", Some(&auth), None).await;
    let invite = url::Url::parse(a.json()["invite_url"].as_str().unwrap()).unwrap();
    let state = invite.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    w.fake.with(|d| {
        d.codes.insert(
            "c1".into(),
            Token {
                access_token: "at-1".into(),
                guild: Some(Guild { id: GUILD.into(), name: "Asha's <Lounge>".into(), icon: None }),
            },
        )
    });
    let page = w.page(&format!("/discord/callback?code=c1&state={}&guild_id={OTHER_GUILD}", enc(&state))).await;
    assert_eq!(page.status, 200, "{}", page.text);
    assert!(page.text.contains("Asha&#39;s &lt;Lounge&gt;"), "escaped: {}", page.text);
    assert!(!page.text.contains("<script"));
    let csp = page.headers["content-security-policy"].to_str().unwrap();
    assert!(csp.starts_with("default-src 'none'") && !csp.contains("script-src"), "{csp}");
    assert_eq!(page.headers["referrer-policy"], "no-referrer");
    assert_eq!(page.headers["cache-control"], "no-store");
    assert_eq!(w.fake.with(|d| d.revoked.clone()), ["at-1"], "the install token is revoked at once");
    let v = w.view(&auth).await;
    assert_eq!(v["connected"], true);
    assert_eq!(v["guild"]["id"], GUILD);
    // Which PC claimed it: the page and Settings › Discord show the same code, never the id.
    let code = pc_code(&config(), "i1");
    assert!(page.text.contains(&code), "{}", page.text);
    assert!(!page.text.contains("i1"), "the install id isn't shown");
    assert_eq!(
        (v["connected_by"]["pc_code"].as_str(), v["connected_by"]["this_pc"].as_bool(), v["pc_code"].as_str()),
        (Some(code.as_str()), Some(true), Some(code.as_str()))
    );
    assert_ne!(pc_code(&config(), "i2"), code);
    // The same code again: Discord refuses it (single use).
    let again = w.page(&format!("/discord/callback?code=c1&state={}", enc(&state))).await;
    assert_eq!(again.status, 400);
}

#[tokio::test]
async fn a_tampered_expired_or_missing_state_changes_nothing() {
    let w = world().await;
    let _a = w.pro("i1", "sub_1");
    let _b = w.pro("i2", "sub_2");
    let cfg = config();
    let good = sign_state(&cfg, Purpose::Install, "i1", now_ms());
    // Someone swaps in another install's id: the MAC breaks.
    let swapped = good.replacen(".i1.", ".i2.", 1);
    let old = sign_state(&cfg, Purpose::Install, "i1", now_ms() - STATE_TTL_MS - 1000);
    for state in [swapped.as_str(), old.as_str(), "garbage", ""] {
        w.fake.with(|d| {
            d.codes.insert(
                "c".into(),
                Token {
                    access_token: "t".into(),
                    guild: Some(Guild { id: GUILD.into(), name: "x".into(), icon: None }),
                },
            )
        });
        let page = w.page(&format!("/discord/callback?code=c&state={}", enc(state))).await;
        assert_eq!(page.status, 400, "{state}");
        assert!(page.text.contains("expired"), "{state}");
    }
    let page = w.page("/discord/callback?code=c").await;
    assert_eq!(page.status, 400);
    assert_eq!(w.fake.calls("exchange_code"), 0, "Discord is never asked with a bad state");
    assert!(w.db.discord_guild("sub_1").unwrap().is_none() && w.db.discord_guild("sub_2").unwrap().is_none());
    // Cancelled on Discord's page.
    let page = w.page(&format!("/discord/callback?error=access_denied&state={}", enc(&good))).await;
    assert_eq!(page.status, 200);
    assert!(page.text.contains("Cancelled"));
}

#[tokio::test]
async fn one_server_per_subscription_and_one_subscription_per_server() {
    let w = world().await;
    let a1 = w.pro("i1", "sub_1");
    let a2 = w.pro("i2", "sub_2");
    assert_eq!(w.connect(&a1).await.status, 200);
    let a = w.call(reqwest::Method::POST, "/v1/pro/discord/connect", Some(&a1), None).await;
    assert_eq!((a.status, a.code().as_str()), (409, "already_connected"));
    // Another subscription can't claim GUILD.
    let page = w.connect(&a2).await;
    assert_eq!(page.status, 409);
    assert!(page.text.contains("another Ferrocast Pro"));
    assert!(w.db.discord_guild("sub_2").unwrap().is_none());
    assert_eq!(w.fake.calls("leave"), 0, "never leaves a server another Pro has");
    // Reconnecting the same server (the bot was kicked) keeps rules and settings.
    let state = sign_state(&config(), Purpose::Install, "i1", now_ms());
    w.fake.with(|d| {
        d.codes.insert(
            "re".into(),
            Token {
                access_token: "t".into(),
                guild: Some(Guild { id: GUILD.into(), name: "Renamed".into(), icon: None }),
            },
        )
    });
    let page = w.page(&format!("/discord/callback?code=re&state={}", enc(&state))).await;
    assert_eq!(page.status, 200);
    assert_eq!(w.view(&a1).await["guild"]["name"], "Renamed");
    // A second server for the same subscription (a connect from an older tab).
    w.fake.with(|d| {
        d.codes.insert(
            "x2".into(),
            Token {
                access_token: "t".into(),
                guild: Some(Guild { id: OTHER_GUILD.into(), name: "Two".into(), icon: None }),
            },
        )
    });
    let page = w.page(&format!("/discord/callback?code=x2&state={}", enc(&state))).await;
    assert_eq!(page.status, 409);
    assert!(page.text.contains("One server per Pro"));
    // Discord had already added the bot there: it leaves again, and the first server stays.
    assert!(!w.fake.with(|d| d.joined.contains(OTHER_GUILD)), "the bot left the second server");
    assert!(w.fake.with(|d| d.joined.contains(GUILD)));
    assert_eq!(w.view(&a1).await["guild"]["id"], GUILD);
}

#[tokio::test]
async fn a_lapsed_install_cant_finish_connecting() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w.call(reqwest::Method::POST, "/v1/pro/discord/connect", Some(&auth), None).await;
    assert_eq!(a.status, 201);
    w.set_sub("sub_1", "cancelled", Some(now_ms() - DAY));
    let state = sign_state(&config(), Purpose::Install, "i1", now_ms());
    w.fake.with(|d| {
        d.codes.insert(
            "c".into(),
            Token { access_token: "t".into(), guild: Some(Guild { id: GUILD.into(), name: "L".into(), icon: None }) },
        )
    });
    let page = w.page(&format!("/discord/callback?code=c&state={}", enc(&state))).await;
    assert_eq!(page.status, 402);
    assert!(w.db.discord_guild("sub_1").unwrap().is_none());
    // Discord had added the bot already: it doesn't stay in a server nobody connected.
    assert!(!w.fake.with(|d| d.joined.contains(GUILD)), "the bot left");
}

#[tokio::test]
async fn the_view_lists_text_channels_and_roles_ferrocast_can_give() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    let v = w.view(&auth).await;
    assert_eq!(
        v["channels"],
        json!([{ "id": CH_NEWS, "name": "announcements" }, { "id": CH_TEXT, "name": "going-live" }])
    );
    let roles: Vec<(String, bool)> = v["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap().to_string(), r["assignable"].as_bool().unwrap()))
        .collect();
    assert_eq!(roles, [("Admin".to_string(), false), ("VIP".to_string(), true), ("Subscriber".to_string(), true)]);
    assert_eq!(v["guild"]["icon_url"], format!("https://cdn.discordapp.com/icons/{GUILD}/abc123.png?size=64"));
    let link = v["link_url"].as_str().unwrap();
    assert!(link.starts_with(&format!("{}/d/", w.base)) && link.ends_with("/link"), "{link}");
    assert_eq!(v["warning"], Value::Null);
    // Discord down: the view still answers, with a warning.
    w.state.discord.forget_snapshot(GUILD);
    w.fake.fail("channels", Some(DiscordError::Unavailable("down".into())));
    let v = w.view(&auth).await;
    assert_eq!(v["channels"], json!([]));
    assert!(v["warning"].as_str().unwrap().contains("couldn't be reached"));
    // Kicked from the server.
    w.fake.fail("channels", None);
    w.fake.with(|d| d.joined.remove(GUILD));
    w.state.discord.forget_snapshot(GUILD);
    assert!(w.view(&auth).await["warning"].as_str().unwrap().contains("isn't in Asha's Lounge"));
}

// ── Channel and go-live ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_channel_must_be_a_text_channel_in_the_server() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w.put("/v1/pro/discord/channel", &auth, json!({ "channel_id": CH_TEXT })).await;
    assert_eq!((a.status, a.code().as_str()), (409, "not_connected"));
    w.connect(&auth).await;
    let a = w.put("/v1/pro/discord/channel", &auth, json!({ "channel_id": CH_VOICE })).await;
    assert_eq!((a.status, a.code().as_str()), (404, "channel_not_found"));
    let a = w.put("/v1/pro/discord/channel", &auth, json!({ "channel_id": "123456789012345678" })).await;
    assert_eq!(a.status, 404);
    let a = w.put("/v1/pro/discord/channel", &auth, json!({ "wrong": 1 })).await;
    assert_eq!((a.status, a.code().as_str()), (400, "invalid"));
    let r = w
        .http
        .put(format!("{}/v1/pro/discord/channel", w.base))
        .bearer_auth(&auth)
        .header("content-type", "text/plain")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 415);
    let a = w.put("/v1/pro/discord/channel", &auth, json!({ "channel_id": CH_TEXT })).await;
    assert_eq!(a.status, 200);
    assert_eq!(a.json()["channel_id"], CH_TEXT);
}

#[tokio::test]
async fn go_live_needs_a_channel_and_a_short_line() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    let a = w.put("/v1/pro/discord/go-live", &auth, json!({ "enabled": true })).await;
    assert_eq!((a.status, a.code().as_str()), (422, "no_channel"));
    w.put("/v1/pro/discord/channel", &auth, json!({ "channel_id": CH_TEXT })).await;
    let a = w.put("/v1/pro/discord/go-live", &auth, json!({ "enabled": true, "message": "x".repeat(301) })).await;
    assert_eq!((a.status, a.code().as_str()), (400, "invalid"));
    let a =
        w.put("/v1/pro/discord/go-live", &auth, json!({ "enabled": true, "message": "  Come say hi \u{202E} " })).await;
    assert_eq!(a.status, 200);
    assert_eq!(a.json()["go_live"], json!({ "enabled": true, "message": "Come say hi" }));
    let a = w.put("/v1/pro/discord/go-live", &auth, json!({ "enabled": false, "message": "" })).await;
    assert_eq!(a.json()["go_live"], json!({ "enabled": false, "message": null }));
}

async fn live(w: &World, auth: &str, body: Value) -> Answer {
    w.post("/v1/pro/discord/live", auth, body).await
}

fn message_body(w: &World) -> Value {
    w.fake.with(|d| d.messages.values().next().map(|m| m.1.clone()).unwrap_or(Value::Null))
}

#[tokio::test]
async fn go_live_posts_an_embed_once_and_edits_it_at_the_end() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    let started = now_ms() - 2 * 3_600_000 - 14 * 60_000;
    let online = json!({
        "platform": "twitch", "live": true, "name": "Asha", "title": "Ranked grind @everyone", "game": "Valorant",
        "url": "https://www.twitch.tv/asha", "thumbnail_url": "https://static-cdn.jtvnw.net/previews-ttv/live_user_asha-1280x720.jpg",
        "started_at": started,
    });
    // Off: nothing posted, and the desktop is told why (it posts with its own webhook then).
    let a = live(&w, &auth, online.clone()).await;
    assert_eq!(a.json(), json!({ "posted": false, "edited": false, "reason": "off" }));
    w.put("/v1/pro/discord/channel", &auth, json!({ "channel_id": CH_TEXT })).await;
    w.put("/v1/pro/discord/go-live", &auth, json!({ "enabled": true, "message": "We're on!" })).await;
    let a = live(&w, &auth, online.clone()).await;
    assert_eq!(a.json(), json!({ "posted": true, "edited": false, "reason": null }), "{}", a.text);
    let m = message_body(&w);
    assert_eq!(m["content"], "We're on!");
    assert_eq!(m["allowed_mentions"], json!({ "parse": [] }), "never pings anyone");
    let e = &m["embeds"][0];
    assert_eq!(e["title"], "Ranked grind @everyone");
    assert_eq!(e["url"], "https://www.twitch.tv/asha");
    assert_eq!(e["fields"][0]["value"], "Valorant");
    assert_eq!(e["timestamp"], live::iso(started));
    let img = e["image"]["url"].as_str().unwrap();
    assert!(
        img.starts_with("https://static-cdn.jtvnw.net/previews-ttv/live_user_asha-1280x720.jpg?t="),
        "cache-buster: {img}"
    );
    assert!(!m.to_string().to_lowercase().contains("ferrocast"), "no branding to the audience (R-06): {m}");
    // Online again (the platform said it twice): no second post.
    let a = live(&w, &auth, online.clone()).await;
    assert_eq!(a.json()["reason"], "recent");
    assert_eq!(w.fake.calls("post"), 1);
    // The end: the same message, edited.
    let a = live(&w, &auth, json!({ "platform": "twitch", "live": false, "started_at": started, "peak_viewers": 132 }))
        .await;
    assert_eq!(a.json(), json!({ "posted": false, "edited": true, "reason": null }));
    let m = message_body(&w);
    let fields = m["embeds"][0]["fields"].to_string();
    assert!(fields.contains("2 h 14 min") && fields.contains("132"), "{fields}");
    assert_eq!(m["components"], json!([]));
    // Back within 30 minutes: the message shows live again, still one post.
    let a = live(&w, &auth, online.clone()).await;
    assert_eq!(a.json(), json!({ "posted": false, "edited": true, "reason": "resumed" }));
    assert_eq!(w.fake.calls("post"), 1);
    assert!(message_body(&w)["embeds"][0]["image"]["url"].is_string());
    // Ended over 30 minutes ago: a new stream, a new post.
    live(&w, &auth, json!({ "platform": "twitch", "live": false, "started_at": started })).await;
    w.db.c().execute("UPDATE discord_posts SET ended_at = ended_at - 31 * 60000", []).unwrap();
    let a = live(&w, &auth, online.clone()).await;
    assert_eq!(a.json()["posted"], true);
    assert_eq!(w.fake.calls("post"), 2);
    // Another platform has its own post.
    let a = live(&w, &auth, json!({ "platform": "kick", "live": true, "name": "Asha", "url": "https://kick.com/asha", "started_at": now_ms() })).await;
    assert_eq!(a.json()["posted"], true);
}

#[tokio::test]
async fn go_live_drops_links_that_arent_the_stream_and_reports_a_deleted_channel() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    w.put("/v1/pro/discord/channel", &auth, json!({ "channel_id": CH_TEXT })).await;
    w.put("/v1/pro/discord/go-live", &auth, json!({ "enabled": true })).await;
    let a = live(
        &w,
        &auth,
        json!({
            "platform": "youtube", "live": true, "name": "Asha", "url": "https://evil.example/phish",
            "thumbnail_url": "http://i.ytimg.com/vi/x/hq.jpg", "started_at": now_ms(),
        }),
    )
    .await;
    assert_eq!(a.json()["posted"], true);
    let m = message_body(&w);
    assert!(m["embeds"][0].get("url").is_none() && m["embeds"][0].get("image").is_none(), "{m}");
    assert!(m.get("components").is_none());
    assert_eq!(m["embeds"][0]["title"], "Asha is live on YouTube");
    let a = live(&w, &auth, json!({ "platform": "tiktok", "live": true, "started_at": now_ms() })).await;
    assert_eq!(a.status, 400);
    let a = live(&w, &auth, json!({ "platform": "twitch", "live": true, "started_at": now_ms() + DAY })).await;
    assert_eq!(a.status, 400);
    // The channel was deleted in Discord.
    w.fake.with(|d| d.channels.retain(|c| c.id != CH_TEXT));
    let a = live(&w, &auth, json!({ "platform": "twitch", "live": true, "started_at": now_ms() })).await;
    assert_eq!((a.status, a.code().as_str()), (404, "channel_not_found"));
    // Discord says the bot can't post there.
    w.fake.fail("post", Some(DiscordError::MissingPermission));
    let a = live(&w, &auth, json!({ "platform": "kick", "live": true, "started_at": now_ms() })).await;
    assert_eq!((a.status, a.code().as_str()), (403, "missing_permission"));
    let a = live(&w, &auth, json!({ "platform": "twitch", "live": true, "started_at": now_ms() })).await;
    assert_eq!(a.status, 403);
}

/// Connected, a channel, go-live on.
async fn live_ready(w: &World) -> String {
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    w.put("/v1/pro/discord/channel", &auth, json!({ "channel_id": CH_TEXT })).await;
    w.put("/v1/pro/discord/go-live", &auth, json!({ "enabled": true })).await;
    auth
}

fn online(started_at: i64) -> Value {
    json!({ "platform": "twitch", "live": true, "name": "Asha", "url": "https://www.twitch.tv/asha", "started_at": started_at })
}

#[tokio::test]
async fn a_lost_stream_end_never_silences_the_next_streams_post() {
    let w = world().await;
    let auth = live_ready(&w).await;
    let yesterday = now_ms() - 20 * 3_600_000;
    assert_eq!(live(&w, &auth, online(yesterday)).await.json()["posted"], true);
    let first = w.fake.with(|d| d.messages.keys().next().cloned().unwrap());
    // The end never arrived (PC off). Today's stream started long after yesterday's post:
    // a new stream, a new post, and yesterday's message no longer says live.
    let a = live(&w, &auth, online(now_ms())).await;
    assert_eq!(a.json(), json!({ "posted": true, "edited": false, "reason": null }), "{}", a.text);
    assert_eq!(w.fake.calls("post"), 2);
    let old = w.fake.with(|d| d.messages[&first].1.clone());
    assert!(old["embeds"][0]["description"].as_str().unwrap().contains("has ended"), "{old}");
    assert!(!old["embeds"][0]["fields"].to_string().contains("Streamed for"), "no made-up length: {old}");
    // Online again for the same stream (the desktop restarted): still one post for it.
    assert_eq!(live(&w, &auth, online(now_ms() - 60_000)).await.json()["reason"], "recent");
    assert_eq!(w.fake.calls("post"), 2);
}

#[tokio::test]
async fn a_queued_end_uses_when_it_ended_and_a_late_one_leaves_newer_posts_alone() {
    let w = world().await;
    let auth = live_ready(&w).await;
    let started = now_ms() - 3 * 3_600_000;
    live(&w, &auth, online(started)).await;
    // Sent an hour late from the desktop's queue: the length is to the real end.
    let end = started + 90 * 60_000;
    let a =
        live(&w, &auth, json!({ "platform": "twitch", "live": false, "started_at": started, "ended_at": end })).await;
    assert_eq!(a.json()["edited"], true, "{}", a.text);
    assert!(message_body(&w)["embeds"][0]["fields"].to_string().contains("1 h 30 min"));
    assert_eq!(w.db.discord_post("sub_1", "twitch").unwrap().unwrap().ended_at, Some(end));
    // A new stream's post, then a very late end for an older stream: the new post stays live.
    w.db.c().execute("UPDATE discord_posts SET ended_at = ended_at - 3 * 3600000", []).unwrap();
    assert_eq!(live(&w, &auth, online(now_ms())).await.json()["posted"], true);
    let a = live(&w, &auth, json!({ "platform": "twitch", "live": false, "started_at": started - 3_600_000 })).await;
    assert_eq!(a.json()["reason"], "no_post");
    assert!(w.db.discord_post("sub_1", "twitch").unwrap().unwrap().ended_at.is_none());
}

#[tokio::test]
async fn an_unreadable_saved_post_is_closed_not_an_error() {
    let w = world().await;
    let auth = live_ready(&w).await;
    let started = now_ms() - 3_600_000;
    live(&w, &auth, online(started)).await;
    w.db.c().execute("UPDATE discord_posts SET post = 'not json'", []).unwrap();
    let a = live(&w, &auth, json!({ "platform": "twitch", "live": false, "started_at": started })).await;
    assert_eq!((a.status, a.json()["reason"].as_str()), (200, Some("no_post")), "{}", a.text);
    assert!(w.db.discord_post("sub_1", "twitch").unwrap().unwrap().ended_at.is_some(), "marked ended");
}

#[tokio::test]
async fn a_post_discord_accepted_counts_even_if_saving_it_fails() {
    let w = world().await;
    let auth = live_ready(&w).await;
    w.db.c()
        .execute_batch(
            "CREATE TRIGGER no_posts BEFORE INSERT ON discord_posts BEGIN SELECT RAISE(FAIL, 'disk full'); END;",
        )
        .unwrap();
    let a = live(&w, &auth, online(now_ms())).await;
    assert_eq!(a.json(), json!({ "posted": true, "edited": false, "reason": null }), "{}", a.text);
    assert_eq!(w.fake.calls("post"), 1, "posted, so the desktop's webhook stays quiet");
}

#[tokio::test]
async fn two_reports_at_once_post_once() {
    let w = world().await;
    let auth = live_ready(&w).await;
    let started = now_ms();
    let (a, b) = tokio::join!(live(&w, &auth, online(started)), live(&w, &auth, online(started)));
    assert_eq!(w.fake.calls("post"), 1, "{} / {}", a.text, b.text);
    assert!(a.json()["posted"] == true || b.json()["posted"] == true);
    assert!(a.json()["reason"] == "recent" || b.json()["reason"] == "recent");
}

// ── Rules ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rules_are_added_edited_and_deleted_with_their_checks() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    let a = w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_SUB })).await;
    assert_eq!((a.status, a.code().as_str()), (409, "not_connected"));
    w.connect(&auth).await;
    let a = w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_ADMIN })).await;
    assert_eq!((a.status, a.code().as_str()), (403, "missing_permission"));
    assert_eq!(a.json()["error"]["hint"], ROLE_HINT);
    let a = w.add_rule(&auth, json!({ "kind": "sub", "role_id": "123456789012345678" })).await;
    assert_eq!((a.status, a.code().as_str()), (404, "role_not_found"));
    let a = w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_BOT })).await;
    assert_eq!(a.status, 404, "an integration's own role isn't offered");
    let a = w.add_rule(&auth, json!({ "kind": "tip_min", "role_id": ROLE_VIP })).await;
    assert_eq!((a.status, a.code().as_str()), (422, "invalid_rule"));
    let a = w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_SUB, "surprise": 1 })).await;
    assert_eq!(a.status, 400, "unknown fields are refused");
    let a = w
        .add_rule(
            &auth,
            json!({ "kind": "tip_min", "min_minor": 50_000, "currency": "INR", "role_id": ROLE_VIP, "keep_days": 0 }),
        )
        .await;
    assert_eq!(a.status, 201, "{}", a.text);
    let rule = a.json()["rules"][0].clone();
    let id = rule["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("dr_") && id.len() == 15, "{id}");
    assert_eq!(rule["min_minor"], 50_000);
    assert_eq!(rule["keep_days"], 0);
    // Edit: same id and creation time; whatever the dashboard sends back is fine.
    let mut edited = rule.clone();
    edited["min_minor"] = json!(100_000);
    let a = w.put(&format!("/v1/pro/discord/rules/{id}"), &auth, edited).await;
    assert_eq!(a.status, 200, "{}", a.text);
    let after = a.json()["rules"][0].clone();
    assert_eq!(
        (after["id"].clone(), after["created_at"].clone(), after["min_minor"].clone()),
        (rule["id"].clone(), rule["created_at"].clone(), json!(100_000))
    );
    let a = w.put("/v1/pro/discord/rules/dr_000000000000", &auth, json!({ "kind": "sub", "role_id": ROLE_SUB })).await;
    assert_eq!((a.status, a.code().as_str()), (404, "rule_not_found"));
    let a = w.call(reqwest::Method::DELETE, &format!("/v1/pro/discord/rules/{id}"), Some(&auth), None).await;
    assert_eq!(a.status, 204);
    let a = w.call(reqwest::Method::DELETE, &format!("/v1/pro/discord/rules/{id}"), Some(&auth), None).await;
    assert_eq!((a.status, a.code().as_str()), (404, "rule_not_found"));
    assert_eq!(w.view(&auth).await["rules"], json!([]));
}

#[tokio::test]
async fn ten_rules_at_most() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    for _ in 0..10 {
        assert_eq!(w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_SUB })).await.status, 201);
    }
    let a = w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_SUB })).await;
    assert_eq!((a.status, a.code().as_str()), (409, "too_many_rules"));
    assert_eq!(w.db.discord_rules("sub_1").unwrap().len(), 10);
}

// ── Viewer linking ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_viewer_links_their_twitch_and_youtube_and_the_token_is_discarded() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    let link = w.view(&auth).await["link_url"].as_str().unwrap().to_string();
    let path = link.strip_prefix(&w.base).unwrap().to_string();
    let page = w.page(&path).await;
    assert_eq!(page.status, 200);
    assert!(page.text.contains("Asha&#39;s Lounge") && page.text.contains("/link/start"), "{}", page.text);
    assert!(page.text.contains("Kept:"), "the page says what's kept");
    assert!(
        !page.text.contains("<script") && !page.text.to_lowercase().contains("pro"),
        "no scripts, no upsell (R-06)"
    );
    assert!(page.headers["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    // Discord's sign-in asks for identify + connections only.
    let start = w.page(&format!("{path}/start")).await;
    assert_eq!(start.status, 303);
    let to = url::Url::parse(start.headers["location"].to_str().unwrap()).unwrap();
    let q: HashMap<String, String> = to.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
    assert_eq!(q["scope"], "identify connections");
    assert_eq!(q["redirect_uri"], format!("{}/discord/callback", w.base));
    assert!(matches!(check_state(&config(), &q["state"], now_ms()), Some((Purpose::Link, _))));
    let done = w
        .link(
            &link,
            VIEWER,
            vec![
                conn("twitch", TWITCH_ID, "asha_fan_tw", true),
                conn("youtube", YT_ID, "Asha Fan YT", true),
                conn("twitch", "999", "second_twitch", true),
                conn("steam", "765", "steamer", true),
                conn("youtube", "UCunverified00000000000", "Unverified", false),
            ],
        )
        .await;
    assert_eq!(done.status, 200, "{}", done.text);
    assert!(done.text.contains("Twitch: asha_fan_tw") && done.text.contains("YouTube: Asha Fan YT"), "{}", done.text);
    assert!(!done.text.contains("Unverified") && !done.text.contains("steamer"));
    assert_eq!(w.view(&auth).await["linked"], 1);
    assert_eq!(w.db.discord_users_for(GUILD, "twitch", TWITCH_ID).unwrap(), [VIEWER]);
    assert_eq!(w.db.discord_users_for(GUILD, "youtube", YT_ID).unwrap(), [VIEWER]);
    assert!(w.db.discord_users_for(GUILD, "youtube", "UCunverified00000000000").unwrap().is_empty());
    assert!(w.fake.with(|d| d.revoked.iter().any(|t| t.starts_with("vt-"))), "the viewer's token was revoked");
    // Only ids are stored: never names or tokens.
    let copy = w.dir.path().join("copy.db");
    w.db.c().execute("VACUUM INTO ?1", [copy.to_str().unwrap()]).unwrap();
    let bytes = std::fs::read(&copy).unwrap();
    let has = |needle: &str| bytes.windows(needle.len()).any(|x| x == needle.as_bytes());
    assert!(has(TWITCH_ID) && has(VIEWER));
    assert!(!has("asha_fan_tw") && !has("Asha Fan YT") && !has("vt-v-"), "no names, no tokens");
    // The same Twitch account linked by someone else later moves to them.
    let done = w.link(&link, VIEWER_2, vec![conn("twitch", TWITCH_ID, "x", true)]).await;
    assert_eq!(done.status, 200);
    assert_eq!(w.db.discord_users_for(GUILD, "twitch", TWITCH_ID).unwrap(), [VIEWER_2]);
    // Unlinking: remove the connection in Discord and open the link again.
    let done = w.link(&link, VIEWER, vec![]).await;
    assert!(done.text.contains("No Twitch or YouTube account"));
    assert!(w.db.discord_users_for(GUILD, "youtube", YT_ID).unwrap().is_empty());
    assert_eq!(w.view(&auth).await["linked"], 1);
}

#[tokio::test]
async fn link_pages_for_unknown_or_lapsed_servers_are_inactive() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    let link = w.view(&auth).await["link_url"].as_str().unwrap().to_string();
    let path = link.strip_prefix(&w.base).unwrap().to_string();
    for p in ["/d/nope-nope-nope/link", "/d/a/link", "/d/%3Cscript%3E/link", "/d/nope-nope-nope/link/start"] {
        let a = w.page(p).await;
        assert_eq!(a.status, 404, "{p}");
        assert!(!a.text.contains("<script"), "{p}");
    }
    // A link state can't be reused for another server's code.
    let state = sign_state(&config(), Purpose::Link, "nope-nope-nope", now_ms());
    let a = w.page(&format!("/discord/callback?code=c&state={}", enc(&state))).await;
    assert_eq!(a.status, 404);
    // Pro lapsed: the page is inactive.
    w.set_sub("sub_1", "cancelled", Some(now_ms() - DAY));
    assert_eq!(w.page(&path).await.status, 404);
    // A viewer's expired sign-in code.
    w.set_sub("sub_1", "active", Some(now_ms() + DAY));
    let code = path.split('/').nth(2).unwrap();
    let state = sign_state(&config(), Purpose::Link, code, now_ms());
    let a = w.page(&format!("/discord/callback?code=unknown&state={}", enc(&state))).await;
    assert_eq!(a.status, 400);
    assert!(a.text.contains("didn&#39;t work"), "{}", a.text);
}

#[tokio::test]
async fn linking_is_limited_per_address_and_per_server() {
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    w.connect(&auth).await;
    let link = w.view(&auth).await["link_url"].as_str().unwrap().to_string();
    let path = link.strip_prefix(&w.base).unwrap().to_string();
    for _ in 0..20 {
        assert_eq!(w.page(&format!("{path}/start")).await.status, 303);
    }
    let a = w.page(&format!("{path}/start")).await;
    assert_eq!(a.status, 429);
    assert_eq!(a.headers["retry-after"], "600");
    // 5,000 members per server: a full list refuses a new member, not one already in it.
    assert!(w.db.discord_link(GUILD, "u1", &[("twitch".into(), "1".into())], 2, 0).unwrap());
    assert!(w.db.discord_link(GUILD, "u2", &[("twitch".into(), "2".into())], 2, 0).unwrap());
    assert!(!w.db.discord_link(GUILD, "u3", &[("twitch".into(), "3".into())], 2, 0).unwrap());
    assert!(w.db.discord_link(GUILD, "u2", &[("youtube".into(), "UC2".into())], 2, 0).unwrap(), "relinking is fine");
    assert_eq!(w.db.discord_linked(GUILD).unwrap(), 2);
}

// ── Perk events and grants ───────────────────────────────────────────────────────────

async fn perks(w: &World, auth: &str, events: Value) -> Answer {
    w.post("/v1/pro/discord/perk-events", auth, events).await
}

#[tokio::test]
async fn a_linked_subscriber_gets_the_role_until_it_ends() {
    let w = world().await;
    let auth = ready(&w).await;
    let at = now_ms() - 1_000;
    let a = perks(
        &w,
        &auth,
        json!([
            { "platform": "twitch", "platform_user_id": TWITCH_ID, "kind": "sub", "tier": "1", "at": at },
            { "platform": "twitch", "platform_user_id": "424242", "kind": "sub", "tier": "1", "at": at },
            { "platform": "kick", "platform_user_id": "77", "kind": "sub", "at": at },
        ]),
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert_eq!(
        a.json(),
        json!({ "events": 3, "granted": 1, "blocked": 0, "skipped": 1, "invalid": 0, "not_linked": 1,
                "not_member": 0, "role_missing": 0, "too_old": 0, "guild_missing": false, "issue": null })
    );
    assert!(w.fake.roles_of(VIEWER).contains(ROLE_SUB));
    let g = w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap();
    assert_eq!(g.expires_at, Some(at + 31 * DAY));
    // A later sub moves the end later; an earlier (queued) one never shortens it.
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    let later = w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().expires_at.unwrap();
    assert!(later >= at + 31 * DAY);
    perks(
        &w,
        &auth,
        json!([{ "platform": "twitch", "platform_user_id": TWITCH_ID, "kind": "sub", "at": at - 10 * DAY }]),
    )
    .await;
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().expires_at, Some(later));
    // An event so old its role would already be over gives nothing.
    let old = perks(
        &w,
        &auth,
        json!([{ "platform": "twitch", "platform_user_id": TWITCH_ID, "kind": "sub", "at": now_ms() - 29 * DAY }]),
    )
    .await;
    assert_eq!(old.json()["granted"], 1, "29 days ago still has 2 days left");
    let older = perks(
        &w,
        &auth,
        json!([{ "platform": "twitch", "platform_user_id": TWITCH_ID, "kind": "sub", "at": now_ms() - 31 * DAY }]),
    )
    .await;
    assert_eq!(older.json()["too_old"], 1);
}

#[tokio::test]
async fn tips_and_memberships_follow_their_rules() {
    let w = world().await;
    let auth = ready(&w).await;
    w.add_rule(
        &auth,
        json!({ "kind": "tip_min", "min_minor": 50_000, "currency": "INR", "role_id": ROLE_VIP, "keep_days": 0 }),
    )
    .await;
    let small = json!({ "platform": "youtube", "platform_user_id": YT_ID, "kind": "tip", "amount_minor": 10_000, "currency": "INR", "at": now_ms() });
    assert_eq!(perks(&w, &auth, json!([small])).await.json()["granted"], 0);
    assert!(!w.fake.roles_of(VIEWER).contains(ROLE_VIP));
    let big = json!({ "platform": "youtube", "platform_user_id": YT_ID, "kind": "tip", "amount_minor": 50_000, "currency": "INR", "at": now_ms() });
    assert_eq!(perks(&w, &auth, json!([big])).await.json()["granted"], 1);
    assert!(w.fake.roles_of(VIEWER).contains(ROLE_VIP));
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_VIP).unwrap().unwrap().expires_at, None, "kept for good");
    // A member rule on the same role.
    w.add_rule(&auth, json!({ "kind": "member", "tier": "Gold", "role_id": ROLE_VIP, "keep_days": 31 })).await;
    let gold =
        json!({ "platform": "youtube", "platform_user_id": YT_ID, "kind": "member", "tier": "gold", "at": now_ms() });
    assert_eq!(perks(&w, &auth, json!([gold])).await.json()["granted"], 1);
    assert_eq!(
        w.db.discord_grant_row(GUILD, VIEWER, ROLE_VIP).unwrap().unwrap().expires_at,
        None,
        "forever beats a date"
    );
}

#[tokio::test]
async fn perk_events_are_checked_and_capped() {
    let w = world().await;
    let auth = ready(&w).await;
    let many: Vec<Value> = (0..101).map(|_| sub_event(TWITCH_ID, "1")).collect();
    let a = perks(&w, &auth, json!(many)).await;
    assert_eq!((a.status, a.code().as_str()), (413, "too_many_events"));
    let a = perks(&w, &auth, json!({ "events": [] })).await;
    assert_eq!(a.status, 400, "an array, not an object");
    let a = perks(&w, &auth, json!([])).await;
    assert_eq!(a.json()["events"], 0);
    assert_eq!(w.fake.calls("add_role"), 0);
}

#[tokio::test]
async fn one_bad_or_clock_skewed_event_never_costs_the_others_their_roles() {
    let w = world().await;
    let auth = ready(&w).await;
    w.add_rule(&auth, json!({ "kind": "member", "role_id": ROLE_VIP, "keep_days": 31 })).await;
    // Every kind of bad event, one good sub, and a membership from a PC whose clock runs a
    // day ahead.
    let ahead = now_ms() + DAY;
    let a = perks(
        &w,
        &auth,
        json!([
            { "platform": "tiktok", "platform_user_id": "1", "kind": "sub", "at": 1 },
            { "platform": "twitch", "platform_user_id": "../x", "kind": "sub", "at": 1 },
            { "platform": "twitch", "platform_user_id": "1", "kind": "follow", "at": 1 },
            { "platform": "twitch", "platform_user_id": "1", "kind": "tip", "currency": "INR", "at": 1 },
            { "platform": "twitch", "kind": "sub", "at": 1 },
            "not an event",
            sub_event(TWITCH_ID, "1"),
            { "platform": "youtube", "platform_user_id": YT_ID, "kind": "member", "at": ahead },
        ]),
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.text);
    let v = a.json();
    assert_eq!((v["events"].as_u64(), v["invalid"].as_u64()), (Some(8), Some(6)), "{v}");
    assert_eq!(v["granted"], 2, "the sub's and the member's roles went out: {v}");
    assert!(w.fake.roles_of(VIEWER).is_superset(&[ROLE_SUB.to_string(), ROLE_VIP.to_string()].into()));
    // The future one counts as now: its role ends 31 days from now, not from the skewed clock.
    let end = w.db.discord_grant_row(GUILD, VIEWER, ROLE_VIP).unwrap().unwrap().expires_at.unwrap();
    assert!(end <= now_ms() + 31 * DAY, "clamped to now, not a day ahead");
}

#[tokio::test]
async fn discords_refusals_reach_the_desktop_with_what_to_do() {
    let w = world().await;
    let auth = ready(&w).await;
    let ev = json!([sub_event(TWITCH_ID, "1")]);
    // 429: Discord's wait passes through, the batch comes again.
    w.fake.fail("add_role", Some(DiscordError::RateLimited { retry_after_s: 7 }));
    let a = perks(&w, &auth, ev.clone()).await;
    assert_eq!((a.status, a.code().as_str()), (429, "discord_rate"));
    assert_eq!(a.headers["retry-after"], "7");
    // Down.
    w.fake.fail("add_role", Some(DiscordError::Unavailable("x".into())));
    let a = perks(&w, &auth, ev.clone()).await;
    assert_eq!((a.status, a.code().as_str()), (502, "discord_unavailable"));
    w.fake.fail("add_role", None);
    // A viewer who left the server, and a role the streamer deleted, are counted, not errors.
    w.fake.with(|d| d.members.remove(VIEWER));
    assert_eq!(perks(&w, &auth, ev.clone()).await.json()["not_member"], 1);
    w.fake.with(|d| {
        d.members.insert(VIEWER.into(), BTreeSet::new());
        d.roles.retain(|r| r.id != ROLE_SUB);
    });
    let v = perks(&w, &auth, ev.clone()).await.json();
    assert_eq!((v["role_missing"].as_u64(), v["skipped"].as_u64()), (Some(1), Some(1)), "{v}");
    assert!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().is_none());
    assert_eq!(w.view(&auth).await["perk_issue"]["code"], "role_missing");
    // Not connected.
    let other = w.pro("i9", "sub_9");
    let a = perks(&w, &other, ev).await;
    assert_eq!((a.status, a.code().as_str()), (409, "not_connected"));
}

#[tokio::test]
async fn a_role_above_ferrocasts_or_a_removed_bot_blocks_only_its_own_grants() {
    let w = world().await;
    let auth = ready(&w).await;
    w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_VIP })).await;
    let vip_rule = w.view(&auth).await["rules"][1]["id"].as_str().unwrap().to_string();
    // Discord now refuses ROLE_VIP (someone moved it above Ferrocast's role after the rule).
    w.fake.with(|d| d.refuse_role = Some(ROLE_VIP.into()));
    let a = perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    assert_eq!(a.status, 200, "{}", a.text);
    let v = a.json();
    assert_eq!((v["granted"].as_u64(), v["blocked"].as_u64()), (Some(1), Some(1)), "{v}");
    assert!(w.fake.roles_of(VIEWER).contains(ROLE_SUB), "the other rule's role still went out");
    // The dashboard says which rule and what to do.
    let issue = w.view(&auth).await["perk_issue"].clone();
    assert_eq!(
        (issue["rule_id"].as_str(), issue["code"].as_str()),
        (Some(vip_rule.as_str()), Some("missing_permission")),
        "{issue}"
    );
    assert_eq!(issue["hint"], ROLE_HINT);
    // Fixed: the next perk clears it.
    w.fake.with(|d| d.refuse_role = None);
    let v = perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await.json();
    assert_eq!((v["granted"].as_u64(), v["blocked"].as_u64()), (Some(2), Some(0)), "{v}");
    assert!(w.view(&auth).await["perk_issue"].is_null());
    // 50001 Missing Access: the bot was removed. Counted as blocked, still 200, Discord isn't
    // asked again for the rest, and the dashboard says to connect again.
    let before = w.fake.calls("add_role");
    w.fake.fail("add_role", Some(DiscordError::MissingAccess));
    let a = perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    assert_eq!(a.status, 200, "{}", a.text);
    let v = a.json();
    assert_eq!((v["blocked"].as_u64(), v["guild_missing"].as_bool()), (Some(2), Some(true)), "{v}");
    assert_eq!(w.fake.calls("add_role"), before + 1, "after Missing Access it stops asking Discord");
    assert_eq!(w.view(&auth).await["perk_issue"]["code"], "guild_missing");
    // The server gone (10004) is the same.
    w.fake.fail("add_role", None);
    w.fake.with(|d| d.joined.remove(GUILD));
    let a = perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    assert_eq!((a.status, a.json()["guild_missing"].as_bool()), (200, Some(true)), "{}", a.text);
}

#[tokio::test]
async fn the_sweep_takes_ended_roles_back_and_keeps_the_rest() {
    let w = world().await;
    let auth = ready(&w).await;
    w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_VIP, "keep_days": 0 })).await;
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    assert!(w.fake.roles_of(VIEWER).is_superset(&[ROLE_SUB.to_string(), ROLE_VIP.to_string()].into()));
    sweep(&w.state).await;
    assert_eq!(w.fake.calls("remove_role"), 0, "nothing has ended");
    // ROLE_SUB's month is over; ROLE_VIP is for good.
    w.db.c()
        .execute(
            "UPDATE discord_grants SET expires_at = ?1 WHERE role_id = ?2",
            rusqlite::params![now_ms() - 1, ROLE_SUB],
        )
        .unwrap();
    // Discord busy: tried again next time, nothing forgotten.
    w.fake.fail("remove_role", Some(DiscordError::RateLimited { retry_after_s: 3 }));
    sweep(&w.state).await;
    assert!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().is_some());
    assert_eq!(
        w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().attempts,
        0,
        "a rate limit isn't a try"
    );
    w.fake.fail("remove_role", Some(DiscordError::MissingPermission));
    sweep(&w.state).await;
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().attempts, 1);
    // Backing off: the next hour's sweep doesn't ask again yet.
    let tries = w.fake.calls("remove_role");
    w.fake.fail("remove_role", None);
    sweep(&w.state).await;
    assert_eq!(w.fake.calls("remove_role"), tries);
    w.db.c().execute("UPDATE discord_grants SET retry_at = ?1", [now_ms() - 1]).unwrap();
    sweep(&w.state).await;
    let roles = w.fake.roles_of(VIEWER);
    assert!(!roles.contains(ROLE_SUB) && roles.contains(ROLE_VIP), "{roles:?}");
    assert!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().is_none());
    assert!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_VIP).unwrap().is_some());
    // A grant whose member left is just forgotten.
    w.db.discord_grant(GUILD, "800000000000000099", ROLE_SUB, "dr", Some(now_ms() - 5), now_ms()).unwrap();
    sweep(&w.state).await;
    assert!(w.db.discord_grant_row(GUILD, "800000000000000099", ROLE_SUB).unwrap().is_none());
}

#[tokio::test]
async fn a_role_that_never_comes_off_backs_off_then_is_forgotten_without_blocking_others() {
    let w = world().await;
    let auth = ready(&w).await;
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    let ended = now_ms() - DAY;
    w.db.c().execute("UPDATE discord_grants SET expires_at = ?1", [ended]).unwrap();
    // A permanently failing removal (the role went above Ferrocast's after it was given).
    w.fake.fail("remove_role", Some(DiscordError::MissingPermission));
    let mut waits = vec![];
    for attempt in 1..perks::SWEEP_MAX_ATTEMPTS {
        sweep(&w.state).await;
        let retry: i64 =
            w.db.c()
                .query_row("SELECT retry_at FROM discord_grants WHERE role_id = ?1", [ROLE_SUB], |r| r.get(0))
                .unwrap();
        assert_eq!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().attempts, attempt);
        waits.push(retry - now_ms());
        w.db.c().execute("UPDATE discord_grants SET retry_at = ?1", [now_ms() - 1]).unwrap();
    }
    assert!(waits.windows(2).all(|p| p[1] > p[0]), "the wait grows: {waits:?}");
    assert!(*waits.last().unwrap() <= DAY);
    // A newer ended grant isn't stuck behind it: retries are ordered by their retry time.
    w.db.discord_grant(GUILD, VIEWER_2, ROLE_SUB, "dr", Some(now_ms() - 10), now_ms()).unwrap();
    w.db.c()
        .execute(
            "UPDATE discord_grants SET retry_at = ?1 WHERE discord_user_id = ?2",
            rusqlite::params![now_ms() + 3_600_000, VIEWER],
        )
        .unwrap();
    let due = w.db.discord_grants_due(now_ms(), 10).unwrap();
    assert_eq!(due.iter().map(|g| g.discord_user_id.as_str()).collect::<Vec<_>>(), vec![VIEWER_2]);
    w.db.c()
        .execute(
            "UPDATE discord_grants SET retry_at = ?1 WHERE discord_user_id = ?2",
            rusqlite::params![now_ms() - 1, VIEWER],
        )
        .unwrap();
    // The fifth failure: forgotten (the role stays; the streamer can take it off by hand).
    sweep(&w.state).await;
    assert!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().is_none());
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER_2, ROLE_SUB).unwrap().unwrap().attempts, 1);
    // A renewal starts the count over.
    w.db.discord_grant(GUILD, VIEWER_2, ROLE_SUB, "dr", Some(now_ms() - 5), now_ms()).unwrap();
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER_2, ROLE_SUB).unwrap().unwrap().attempts, 0);
}

#[tokio::test]
async fn a_grant_renewed_while_the_sweep_removes_it_gets_its_role_back() {
    let w = world().await;
    let auth = ready(&w).await;
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    w.db.c().execute("UPDATE discord_grants SET expires_at = ?1", [now_ms() - 1]).unwrap();
    let due = w.db.discord_grants_due(now_ms(), 10).unwrap();
    assert_eq!(due.len(), 1);
    // A new sub arrives between the sweep listing the grant and removing its role.
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    w.fake.remove_role(GUILD, VIEWER, ROLE_SUB).await.unwrap();
    assert_eq!(w.db.discord_ungrant(&due[0]).unwrap(), store::Ungranted::Renewed);
    // Through the sweep itself: an ended grant that a renewal overtakes.
    w.db.c().execute("UPDATE discord_grants SET expires_at = ?1", [now_ms() - 1]).unwrap();
    let stale = w.db.discord_grants_due(now_ms(), 10).unwrap();
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    // The sweep works from its stale list (made just before the renewal).
    assert!(
        stale[0].expires_at.unwrap()
            < w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().expires_at.unwrap()
    );
    perks::sweep_grants(&w.state, stale, now_ms()).await;
    assert!(w.fake.roles_of(VIEWER).contains(ROLE_SUB), "the role went back on");
    assert!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().is_some());
}

#[tokio::test]
async fn after_pro_ends_the_server_is_kept_30_days_then_the_bot_leaves() {
    let w = world().await;
    let auth = ready(&w).await;
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    w.set_sub("sub_1", "halted", None);
    assert_eq!(w.get("/v1/pro/discord", &auth).await.status, 402);
    assert_eq!(perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await.status, 402);
    sweep(&w.state).await;
    assert!(w.db.discord_guild("sub_1").unwrap().is_some(), "kept for now");
    w.set_sub("sub_1", "cancelled", Some(now_ms() - 31 * DAY));
    sweep(&w.state).await;
    assert!(w.db.discord_guild("sub_1").unwrap().is_none());
    assert_eq!(w.db.discord_linked(GUILD).unwrap(), 0);
    assert!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().is_none());
    assert!(!w.fake.with(|d| d.joined.contains(GUILD)), "the bot left");
}

#[tokio::test]
async fn disconnecting_leaves_the_server_and_forgets_it_even_after_pro() {
    let w = world().await;
    let auth = ready(&w).await;
    w.set_sub("sub_1", "cancelled", Some(now_ms() - DAY));
    let a = w.call(reqwest::Method::DELETE, "/v1/pro/discord", Some(&auth), None).await;
    assert_eq!(a.status, 204, "open after Pro ends (R-04)");
    assert_eq!(w.fake.calls("leave"), 1);
    assert!(w.db.discord_guild("sub_1").unwrap().is_none());
    assert!(w.db.discord_rules("sub_1").unwrap().is_empty());
    assert_eq!(w.db.discord_linked(GUILD).unwrap(), 0);
    // Nothing connected: still 204. Discord failing to answer doesn't block it.
    let a = w.call(reqwest::Method::DELETE, "/v1/pro/discord", Some(&auth), None).await;
    assert_eq!(a.status, 204);
    w.set_sub("sub_1", "active", Some(now_ms() + DAY));
    w.fake.with(|d| d.joined.insert(GUILD.into()));
    w.connect(&auth).await;
    w.fake.fail("leave", Some(DiscordError::Unavailable("down".into())));
    let a = w.call(reqwest::Method::DELETE, "/v1/pro/discord", Some(&auth), None).await;
    assert_eq!(a.status, 204);
    assert!(w.db.discord_guild("sub_1").unwrap().is_none());
}

#[tokio::test]
async fn sixty_requests_a_minute_per_subscription() {
    let into = now_ms().rem_euclid(60_000);
    if into > 40_000 {
        tokio::time::sleep(std::time::Duration::from_millis(u64::try_from(60_100 - into).unwrap())).await;
    }
    let w = world().await;
    let auth = w.pro("i1", "sub_1");
    for _ in 0..RATE.limit {
        assert_eq!(w.get("/v1/pro/discord", &auth).await.status, 200);
    }
    let a = w.get("/v1/pro/discord", &auth).await;
    assert_eq!((a.status, a.code().as_str()), (429, "discord_rate"));
    assert!(a.headers.contains_key("retry-after") && a.headers.contains_key("x-quota-reset"));
    let s = w.get("/v1/pro/status", &auth).await.json();
    assert!(s["quotas"].get("discord_requests").is_none(), "a rate isn't a quota");
}

// ── Interactions ─────────────────────────────────────────────────────────────────────

async fn interact(w: &World, body: &str, sig: Option<String>, ts: Option<String>) -> Answer {
    let mut r = w
        .http
        .post(format!("{}/discord/interactions", w.base))
        .header("content-type", "application/json")
        .body(body.to_string());
    if let Some(s) = sig {
        r = r.header("x-signature-ed25519", s);
    }
    if let Some(t) = ts {
        r = r.header("x-signature-timestamp", t);
    }
    let res = r.send().await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    Answer { status, headers, text: res.text().await.unwrap() }
}

fn sign(body: &str, ts: &str) -> String {
    hex(&signing_key().sign(&[ts.as_bytes(), body.as_bytes()].concat()).to_bytes())
}

#[tokio::test]
async fn interactions_answer_ping_only_when_signed_by_discord() {
    let w = world().await;
    let ping = r#"{"type":1,"id":"1","application_id":"1561463863197835294"}"#;
    let ts = (now_ms() / 1000).to_string();
    let a = interact(&w, ping, None, None).await;
    assert_eq!((a.status, a.code().as_str()), (401, "bad_signature"));
    let a = interact(&w, ping, Some("00".repeat(64)), Some(ts.clone())).await;
    assert_eq!(a.status, 401);
    let a = interact(&w, ping, Some(sign(ping, &ts)), Some((now_ms() / 1000 + 1).to_string())).await;
    assert_eq!(a.status, 401, "the timestamp is signed too");
    let old = (now_ms() / 1000 - 3600).to_string();
    let a = interact(&w, ping, Some(sign(ping, &old)), Some(old)).await;
    assert_eq!(a.status, 401, "a replay an hour later");
    let a = interact(&w, ping, Some(sign(ping, &ts)), Some(ts.clone())).await;
    assert_eq!(a.status, 200, "{}", a.text);
    assert_eq!(a.json(), json!({ "type": 1 }));
    let cmd = r#"{"type":2,"data":{"name":"ferrocast"}}"#;
    let a = interact(&w, cmd, Some(sign(cmd, &ts)), Some(ts.clone())).await;
    assert_eq!(a.json()["type"], 4);
    assert_eq!(a.json()["data"]["flags"], 64, "a private reply, never one the channel sees");
    let junk = "not json";
    let a = interact(&w, junk, Some(sign(junk, &ts)), Some(ts)).await;
    assert_eq!(a.status, 400);
}

#[tokio::test]
async fn perk_roles_come_off_when_pro_ends_and_go_back_on_when_it_returns() {
    const GONE: &str = "800000000000000099";
    let w = world().await;
    let auth = ready(&w).await;
    w.add_rule(&auth, json!({ "kind": "sub", "role_id": ROLE_VIP, "keep_days": 0 })).await;
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    // A grant to a member who has left the server since.
    w.db.discord_grant(GUILD, GONE, ROLE_SUB, "dr", Some(now_ms() + DAY), now_ms()).unwrap();
    let both: BTreeSet<String> = [ROLE_SUB.to_string(), ROLE_VIP.to_string()].into();
    sweep(&w.state).await;
    assert_eq!(w.fake.calls("remove_role"), 0, "nothing comes off while Pro is on");

    // Pro ended yesterday (well inside the 30 days the server is kept).
    w.set_sub("sub_1", "cancelled", Some(now_ms() - DAY));
    // Discord rate limits: the round stops, nothing is counted as a try.
    w.fake.fail("remove_role", Some(DiscordError::RateLimited { retry_after_s: 3 }));
    sweep(&w.state).await;
    assert_eq!(w.fake.calls("remove_role"), 1, "stopped at the first rate limit");
    assert_eq!(w.db.discord_grant_paused(GUILD, VIEWER, ROLE_SUB).unwrap(), Some(false));
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().attempts, 0);
    // A failure for one member doesn't stop the others: every grant is tried.
    w.fake.fail("remove_role", Some(DiscordError::MissingPermission));
    let before = w.fake.calls("remove_role");
    sweep(&w.state).await;
    assert_eq!(w.fake.calls("remove_role") - before, 3);
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_VIP).unwrap().unwrap().attempts, 1);
    // Discord is fine again and the retry is due: every role the bot gave comes off now,
    // not 30 days later; the grants wait, paused.
    w.fake.fail("remove_role", None);
    w.db.c().execute("UPDATE discord_grants SET retry_at = ?1", [now_ms() - 1]).unwrap();
    sweep(&w.state).await;
    assert!(w.fake.roles_of(VIEWER).is_disjoint(&both), "{:?}", w.fake.roles_of(VIEWER));
    for (user, role) in [(VIEWER, ROLE_SUB), (VIEWER, ROLE_VIP), (GONE, ROLE_SUB)] {
        assert_eq!(w.db.discord_grant_paused(GUILD, user, role).unwrap(), Some(true), "{user} {role}");
    }
    assert!(w.fake.with(|d| d.joined.contains(GUILD)), "the bot stays during the 30 days");
    let calls = w.fake.calls("remove_role");
    sweep(&w.state).await;
    assert_eq!(w.fake.calls("remove_role"), calls, "paused grants aren't taken off again");

    // Pro is back. Giving roles back failing for a member backs off instead of retrying
    // every hour; then the roles go back on and the member who left is forgotten.
    w.set_sub("sub_1", "active", Some(now_ms() + 30 * DAY));
    w.fake.fail("add_role", Some(DiscordError::MissingPermission));
    sweep(&w.state).await;
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().attempts, 1);
    let calls = w.fake.calls("add_role");
    sweep(&w.state).await;
    assert_eq!(w.fake.calls("add_role"), calls, "backing off");
    w.fake.fail("add_role", None);
    w.db.c().execute("UPDATE discord_grants SET retry_at = ?1", [now_ms() - 1]).unwrap();
    sweep(&w.state).await;
    assert_eq!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().unwrap().attempts, 0);
    assert!(w.fake.roles_of(VIEWER).is_superset(&both));
    assert_eq!(w.db.discord_grant_paused(GUILD, VIEWER, ROLE_SUB).unwrap(), Some(false));
    assert_eq!(w.db.discord_grant_paused(GUILD, VIEWER, ROLE_VIP).unwrap(), Some(false));
    assert_eq!(w.db.discord_grant_paused(GUILD, GONE, ROLE_SUB).unwrap(), None);
}

#[tokio::test]
async fn a_paused_perk_that_ends_meanwhile_doesnt_come_back() {
    let w = world().await;
    let auth = ready(&w).await;
    perks(&w, &auth, json!([sub_event(TWITCH_ID, "1")])).await;
    w.set_sub("sub_1", "cancelled", Some(now_ms() - DAY));
    sweep(&w.state).await;
    assert!(!w.fake.roles_of(VIEWER).contains(ROLE_SUB));
    // Its month ran out while Pro was off: the expiry sweep forgets it, and Pro returning
    // doesn't give it back.
    w.db.c().execute("UPDATE discord_grants SET expires_at = ?1", [now_ms() - 1]).unwrap();
    w.set_sub("sub_1", "active", Some(now_ms() + 30 * DAY));
    sweep(&w.state).await;
    assert!(!w.fake.roles_of(VIEWER).contains(ROLE_SUB));
    assert!(w.db.discord_grant_row(GUILD, VIEWER, ROLE_SUB).unwrap().is_none());
}
