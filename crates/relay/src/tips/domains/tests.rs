//! Custom tip addresses through the relay's real `router()` with a fake Cloudflare and a
//! fake DNS: every status the routes answer, the TXT proof, the `Host` routing (failing
//! closed, with Ferrocast's own hosts left alone), the sweeps, and the real Cloudflare
//! client's requests against a local fake.

use std::sync::{Arc, Mutex};

use reqwest::Method as M;
use serde_json::{Value, json};

use super::cloudflare::{self, CfError, CloudflareHostnames, CustomHostnames, FakeHostnames};
use super::proof::{FakeDns, ProofDns, ProofKeys, record_name};
use super::{DomainRow, HostCache, Seen};
use crate::billing::store::Synced;
use crate::tips::{Provider, TipConfig};
use crate::{AppState, Config, Db, now_ms, router};

const DAY: i64 = 86_400_000;
const HOST: &str = "tips.mychannel.com";
const SECRET: [u8; 32] = [7u8; 32];

struct World {
    base: String,
    st: AppState,
    cf: Arc<FakeHostnames>,
    dns: Arc<FakeDns>,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn world_with(cf_on: bool, cap: usize) -> World {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("relay.db")).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let cfg = Config {
        public_url: base.clone(),
        tip_url: Some("https://tip.example".into()),
        kick_client_id: None,
        kick_client_secret: None,
        kick_id_url: base.clone(),
        kick_api_url: base.clone(),
    };
    let cf = Arc::new(FakeHostnames::new());
    let dns = Arc::new(FakeDns::new());
    let dyn_cf: Option<Arc<dyn CustomHostnames>> = cf_on.then(|| cf.clone() as Arc<dyn CustomHostnames>);
    let st =
        AppState::new(cfg, db).with_tip_domains(dyn_cf, Some(dns.clone() as Arc<dyn ProofDns>), Some(&SECRET), cap);
    let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(l, app).await });
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    World { base, st, cf, dns, http, _dir: dir }
}

async fn world() -> World {
    world_with(true, super::MAX_DOMAINS).await
}

fn keys() -> ProofKeys {
    ProofKeys::new(&SECRET).unwrap()
}

impl World {
    async fn install(&self) -> (String, String) {
        let v: Value = self.http.post(format!("{}/v1/installs", self.base)).send().await.unwrap().json().await.unwrap();
        let id = v["install_id"].as_str().unwrap().to_string();
        (format!("{id}.{}", v["install_secret"].as_str().unwrap()), id)
    }

    /// An install on `sub` (made active if new), with a tip page called `handle`.
    async fn pro(&self, sub: &str, handle: &str) -> (String, String) {
        let (auth, id) = self.install().await;
        if self.st.db.pro_sub(sub).unwrap().is_none() {
            self.st.db.add_pro_sub(sub, "monthly", "h", "active", &id).unwrap();
        } else {
            self.st.db.link_pro_install(&id, sub).unwrap();
        }
        self.set_sub(sub, "active", now_ms() + 30 * DAY);
        self.tip_page(&auth, handle).await;
        (auth, id)
    }

    fn set_sub(&self, sub: &str, status: &str, end: i64) {
        let s = Synced { status: status.into(), current_end: Some(end), cancel_at_end: Some(false) };
        self.st.db.sync_pro_sub(None, sub, &s).unwrap();
    }

    async fn tip_page(&self, auth: &str, handle: &str) {
        let c = TipConfig {
            handle: handle.into(),
            display_name: "Nova".into(),
            message: "Thanks!".into(),
            razorpay: Some(Provider {
                page_url: "https://razorpay.me/@novaplays".into(),
                secret: "rzp-webhook-secret".into(),
                ..Provider::default()
            }),
            ..TipConfig::default()
        };
        let res =
            self.http.put(format!("{}/v1/tips/config", self.base)).bearer_auth(auth).json(&c).send().await.unwrap();
        assert_eq!(res.status(), 200);
    }

    async fn call(&self, m: M, path: &str, auth: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let mut r = self.http.request(m, format!("{}{path}", self.base));
        if let Some(a) = auth {
            r = r.bearer_auth(a);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let res = r.send().await.unwrap();
        let s = res.status().as_u16();
        (s, res.json().await.unwrap_or(Value::Null))
    }

    async fn put(&self, auth: &str, hostname: &str) -> (u16, Value) {
        self.call(M::PUT, "/v1/pro/tips/domain", Some(auth), Some(json!({ "hostname": hostname }))).await
    }

    async fn get(&self, auth: &str) -> (u16, Value) {
        self.call(M::GET, "/v1/pro/tips/domain", Some(auth), None).await
    }

    async fn delete(&self, auth: &str) -> (u16, Value) {
        self.call(M::DELETE, "/v1/pro/tips/domain", Some(auth), None).await
    }

    /// A GET with this `Host`: status, body, headers.
    async fn on(&self, host: &str, path: &str) -> (u16, String, reqwest::header::HeaderMap) {
        let res = self.http.get(format!("{}{path}", self.base)).header("host", host).send().await.unwrap();
        let (s, h) = (res.status().as_u16(), res.headers().clone());
        (s, res.text().await.unwrap(), h)
    }

    /// As if the last check was long ago (the once-a-minute throttle is over, and the TXT
    /// answer isn't remembered any more).
    fn age_check(&self, install: &str) {
        self.st.db.c().execute("UPDATE tip_domains SET claimed_at = 0 WHERE install_id = ?1", [install]).unwrap();
        self.st.tip_domains.txt.lock().unwrap().clear();
    }

    /// Moves the address's dates `ms` into the past (added and last live that long ago).
    fn age(&self, install: &str, ms: i64) {
        self.st
            .db
            .c()
            .execute(
                "UPDATE tip_domains SET created_at = created_at - ?2, live_at = live_at - ?2 WHERE install_id = ?1",
                rusqlite::params![install, ms],
            )
            .unwrap();
    }

    fn row(&self, install: &str) -> Option<DomainRow> {
        self.st.db.tip_domain(install).unwrap()
    }

    fn used(&self, sub: &str) -> i64 {
        self.st.db.quota_used(sub, "tip_domain_changes", crate::quota::Period::Day).unwrap()
    }

    /// The streamer adds the TXT record this install needs for `host`.
    fn prove(&self, install: &str, host: &str) {
        self.dns.set(&record_name(host), &[&keys().value(install, host)]);
    }

    /// Cloudflare says it's live and the TXT record is there; the relay hears it on the next GET.
    async fn go_live(&self, auth: &str, install: &str, host: &str) {
        self.cf.set_status(host, "active", "active");
        self.prove(install, host);
        self.age_check(install);
        let (s, v) = self.get(auth).await;
        assert_eq!((s, v["status"].as_str()), (200, Some("live")), "{v}");
    }

    async fn sweep(&self) {
        let ids: Vec<String> = self.st.db.tip_domains_all().unwrap().into_iter().map(|r| r.install).collect();
        for id in &ids {
            self.age_check(id);
        }
        crate::tips::sweep(&self.st).await;
    }
}

fn code(v: &Value) -> Option<&str> {
    v["error"]["code"].as_str()
}

#[tokio::test]
async fn the_gate_and_the_feature_switch_come_first() {
    let w = world().await;
    for m in [M::GET, M::PUT, M::DELETE] {
        let (s, v) = w.call(m.clone(), "/v1/pro/tips/domain", None, Some(json!({ "hostname": HOST }))).await;
        assert_eq!((s, code(&v)), (401, Some("unauthorized")), "{m}");
        let (auth, _) = w.install().await;
        let (s, v) = w.call(m.clone(), "/v1/pro/tips/domain", Some(&auth), Some(json!({ "hostname": HOST }))).await;
        assert_eq!((s, code(&v)), (402, Some("pro_required")), "{m}");
        assert!(v["error"]["hint"].is_string());
    }
    let off = world_with(false, 100).await;
    let (auth, _) = off.pro("sub_off", "novaoff").await;
    for m in [M::GET, M::PUT, M::DELETE] {
        let (s, v) = off.call(m.clone(), "/v1/pro/tips/domain", Some(&auth), Some(json!({ "hostname": HOST }))).await;
        assert_eq!((s, code(&v)), (503, Some("domains_unavailable")), "{m}: {v}");
        assert!(v["error"]["hint"].is_string());
    }
    let (_, st) = off.call(M::GET, "/v1/pro/status", Some(&auth), None).await;
    assert_eq!(st["features"]["tip_domain"], false);

    // Cloudflare set but no secret to key the TXT proof with: off too.
    let no_key = AppState::new((*off.st.cfg).clone(), Db::open(&off._dir.path().join("other.db")).unwrap())
        .with_tip_domains(Some(Arc::new(FakeHostnames::new()) as Arc<dyn CustomHostnames>), None, None, 100);
    assert!(!super::available(&no_key));
}

#[tokio::test]
async fn addresses_are_checked_before_cloudflare_hears_of_them() {
    let w = world().await;
    let (auth, _) = w.pro("sub_v", "novav").await;
    // Bodies: not JSON-shaped as asked.
    let (s, v) = w.call(M::PUT, "/v1/pro/tips/domain", Some(&auth), Some(json!({ "host": HOST }))).await;
    assert_eq!((s, code(&v)), (400, Some("invalid")));
    for (typed, want) in [
        ("", "invalid_hostname"),
        ("*.mychannel.com", "invalid_hostname"),
        ("203.0.113.9", "invalid_hostname"),
        ("[2001:db8::1]", "invalid_hostname"),
        ("localhost", "invalid_hostname"),
        ("tips.localhost", "invalid_hostname"),
        ("tips.mychannel.com:8443", "invalid_hostname"),
        ("tips.mychannel.com/x", "invalid_hostname"),
        ("tips.clearcraftlabs.com", "invalid_hostname"),
        ("ferrocast-relay.clearcraftlabs.com", "invalid_hostname"),
        ("tip.example", "invalid_hostname"),
        ("evil.tip.example", "invalid_hostname"),
        ("bad_name.mychannel.com", "invalid_hostname"),
        // Finding 7: mixed scripts and look-alikes of our own domain.
        ("tips.p\u{430}ypal.com", "invalid_hostname"),
        ("tips.c1earcraftlabs.com", "invalid_hostname"),
        ("mychannel.com", "apex_not_supported"),
        ("mychannel.co.uk", "apex_not_supported"),
        ("bücher.de", "apex_not_supported"),
        // Finding 6: the whole Public Suffix List, private entries too.
        ("nova.github.io", "apex_not_supported"),
        ("nova.pages.dev", "apex_not_supported"),
        ("mychannel.com.au", "apex_not_supported"),
    ] {
        let (s, v) = w.put(&auth, typed).await;
        assert_eq!((s, code(&v)), (422, Some(want)), "{typed:?}: {v}");
        assert!(v["error"]["message"].is_string() && v["error"]["hint"].is_string(), "{v}");
    }
    let (_, v) = w.put(&auth, "mychannel.com").await;
    assert!(v["error"]["message"].as_str().unwrap().contains("tips.mychannel.com"), "suggests a subdomain: {v}");
    assert!(w.cf.calls().is_empty(), "nothing reached Cloudflare: {:?}", w.cf.calls());
    assert_eq!(w.used("sub_v"), 0, "refusals cost no changes");

    // An internationalised name is stored as punycode and shown in Unicode.
    let (s, v) = w.put(&auth, "Tips.Bücher.de.").await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["hostname"], "tips.xn--bcher-kva.de");
    assert_eq!(v["hostname_unicode"], "tips.bücher.de");
    assert_eq!(w.cf.calls(), vec!["create tips.xn--bcher-kva.de".to_string()]);

    // A tip page comes first.
    let (bare, _) = w.install().await;
    let id = bare.split('.').next().unwrap().to_string();
    w.st.db.add_pro_sub("sub_bare", "monthly", "h", "active", &id).unwrap();
    w.set_sub("sub_bare", "active", now_ms() + DAY);
    let (s, v) = w.put(&bare, "tips.other.com").await;
    assert_eq!((s, code(&v)), (422, Some("no_tip_page")), "{v}");
}

#[tokio::test]
async fn adding_an_address_gives_the_records_to_add_and_its_status() {
    let w = world().await;
    let (auth, id) = w.pro("sub_a", "nova").await;
    let (s, v) = w.put(&auth, " TIPS.MyChannel.com ").await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["hostname"], HOST);
    assert_eq!(v["hostname_unicode"], HOST);
    assert_eq!(v["status"], "pending");
    assert_eq!(v["dns"], json!([{ "type": "CNAME", "name": HOST, "host": "tips", "target": "tip.example" }]));
    // Finding 2: the per-claim TXT proof, a required step.
    assert_eq!(
        v["proof"],
        json!({ "type": "TXT", "name": "_ferrocast.tips.mychannel.com", "host": "_ferrocast.tips",
                "value": keys().value(&id, HOST), "found": false })
    );
    assert_eq!(v["verification"][0]["type"], "TXT");
    assert_eq!(v["verification"][0]["name"], "_cf-custom-hostname.tips.mychannel.com");
    assert_eq!(v["verification"][0]["why"], "ownership");
    assert!(v["errors"][0].as_str().unwrap().contains("_ferrocast.tips.mychannel.com"), "{v}");
    assert!(v["errors"][1].as_str().unwrap().contains("CNAME"), "{v}");
    assert_eq!(v["page_url"], "https://tips.mychannel.com");
    assert_eq!(v["goal_url"], "https://tips.mychannel.com/goal");
    assert!(v["lapsed_at"].is_null());
    assert_eq!(v["expires_at"], v["created_at"].as_i64().unwrap() + super::UNFINISHED_KEEP_MS);
    assert_eq!(w.used("sub_a"), 1);
    // The same address again: nothing new is made.
    let (s, v) = w.put(&auth, HOST).await;
    assert_eq!((s, v["hostname"].as_str()), (200, Some(HOST)), "{v}");
    assert_eq!(w.cf.count("create"), 1);
    assert_eq!(w.used("sub_a"), 1);
    // Another one while this one exists: remove first.
    let (s, v) = w.put(&auth, "live.mychannel.com").await;
    assert_eq!((s, code(&v)), (409, Some("domain_limit")), "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains(HOST));
    // Status and quota in /v1/pro/status.
    let (_, st) = w.call(M::GET, "/v1/pro/status", Some(&auth), None).await;
    assert_eq!(st["features"]["tip_domain"], true);
    assert_eq!(st["quotas"]["tip_domain_changes"]["used"], 1);
    assert_eq!(st["quotas"]["tip_domain_changes"]["limit"], 10);

    // Cloudflare active without the TXT record: still pending.
    w.cf.set_status(HOST, "active", "pending_validation");
    w.age_check(&id);
    let (_, v) = w.get(&auth).await;
    assert_eq!(v["status"], "pending", "{v}");
    // With it: verifying → live.
    w.prove(&id, HOST);
    w.age_check(&id);
    let (_, v) = w.get(&auth).await;
    assert_eq!((v["status"].as_str(), v["proof"]["found"].as_bool()), (Some("verifying"), Some(true)), "{v}");
    w.go_live(&auth, &id, HOST).await;
    let (_, v) = w.get(&auth).await;
    assert_eq!(v["verification"], json!([]), "nothing to add once live");
    assert_eq!(v["errors"], json!([]));
    assert!(v["expires_at"].is_null());
    assert_eq!(v["proof"]["found"], true, "the TXT record stays shown: it has to stay");
}

#[tokio::test]
async fn cloudflare_is_asked_at_most_once_a_minute() {
    let w = world().await;
    let (auth, id) = w.pro("sub_t", "novat").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    for _ in 0..3 {
        assert_eq!(w.get(&auth).await.0, 200);
    }
    assert_eq!(w.cf.count("get"), 0, "just made: no need to ask");
    w.age_check(&id);
    for _ in 0..3 {
        assert_eq!(w.get(&auth).await.0, 200);
    }
    assert_eq!(w.cf.count("get"), 1, "once, then the stored answer");
    assert_eq!(w.dns.calls(), 1, "the TXT record too");
    // Finding 10: Cloudflare down keeps the stored status, and `checked_at` doesn't move.
    let before = w.row(&id).unwrap().checked_at;
    w.cf.fail_with(CfError::Unavailable("down".into()));
    w.age_check(&id);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (s, v) = w.get(&auth).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("pending")), "{v}");
    assert_eq!(v["checked_at"], before, "not checked: Cloudflare didn't answer");
    w.cf.recover();
    w.age_check(&id);
    let (_, v) = w.get(&auth).await;
    assert!(v["checked_at"].as_i64().unwrap() > before, "checked once it answered: {v}");
    // Gone on Cloudflare's side: failed, with why.
    w.cf.forget(HOST);
    w.age_check(&id);
    let (_, v) = w.get(&auth).await;
    assert_eq!(v["status"], "failed", "{v}");
    assert!(v["errors"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains("no longer")), "{v}");
}

#[tokio::test]
async fn one_address_per_subscription_and_none_twice() {
    let w = world().await;
    let (a1, _) = w.pro("sub_one", "novaone").await;
    let (a2, _) = w.pro("sub_one", "novatwo").await;
    let (b, _) = w.pro("sub_other", "novathree").await;
    assert_eq!(w.put(&a1, HOST).await.0, 201);
    let (s, v) = w.put(&a2, "live.mychannel.com").await;
    assert_eq!((s, code(&v)), (409, Some("domain_limit")), "the other PC of the same Pro: {v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("another PC"));
    let (s, v) = w.put(&b, HOST).await;
    assert_eq!((s, code(&v)), (409, Some("domain_taken")), "{v}");
    let (s, v) = w.put(&b, "TIPS.mychannel.com.").await;
    assert_eq!((s, code(&v)), (409, Some("domain_taken")), "normalised before the check: {v}");
    assert_eq!(w.cf.count("create"), 1);
    // Freed: the other subscription can have it now.
    assert_eq!(w.delete(&a1).await.0, 204);
    assert_eq!(w.put(&b, HOST).await.0, 201);
}

/// Finding 1: a claim that never worked can't hold an address (or a slot) for ever.
#[tokio::test]
async fn a_stale_claim_can_be_taken_over_after_a_day() {
    let w = world().await;
    let (squatter, sid) = w.pro("sub_sq", "squat").await;
    let (owner, oid) = w.pro("sub_own", "nova").await;
    assert_eq!(w.put(&squatter, HOST).await.0, 201);
    // Under a day old: still theirs.
    w.age(&sid, 23 * 3_600_000);
    let (s, v) = w.put(&owner, HOST).await;
    assert_eq!((s, code(&v)), (409, Some("domain_taken")), "{v}");
    // Over a day and never live: the real owner takes it.
    w.age(&sid, 2 * 3_600_000);
    let (s, v) = w.put(&owner, HOST).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(w.row(&oid).unwrap().hostname, HOST);
    assert!(w.row(&sid).is_none(), "the stale claim is gone");
    assert_eq!(w.cf.count("delete"), 1, "and its Cloudflare hostname");
    assert_eq!(w.cf.count("create"), 2);
    // The squatter's panel just shows no address.
    let (s, v) = w.get(&squatter).await;
    assert_eq!((s, code(&v)), (404, Some("not_found")), "{v}");

    // A live address is never taken over, however old.
    w.go_live(&owner, &oid, HOST).await;
    w.age(&oid, 30 * DAY);
    let (s, v) = w.put(&squatter, HOST).await;
    assert_eq!((s, code(&v)), (409, Some("domain_taken")), "{v}");
}

#[tokio::test]
async fn unfinished_addresses_go_after_seven_days() {
    let w = world().await;
    let (auth, id) = w.pro("sub_x", "nova").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    w.age(&id, 6 * DAY);
    w.sweep().await;
    assert!(w.row(&id).is_some(), "6 days: kept");
    w.age(&id, 2 * DAY);
    w.cf.fail_with(CfError::Unavailable("down".into()));
    w.sweep().await;
    assert!(w.row(&id).is_some(), "Cloudflare down: kept for the next sweep");
    w.cf.recover();
    w.sweep().await;
    assert!(w.row(&id).is_none(), "8 days and never live: removed");
    assert!(!w.cf.has(HOST));
    let (s, _) = w.get(&auth).await;
    assert_eq!(s, 404);

    // One that worked counts from when it last worked.
    let (auth2, id2) = w.pro("sub_y", "novay").await;
    assert_eq!(w.put(&auth2, HOST).await.0, 201);
    w.go_live(&auth2, &id2, HOST).await;
    w.age(&id2, 30 * DAY);
    w.sweep().await;
    assert!(w.row(&id2).is_some(), "live: kept");
    w.cf.set_status(HOST, "moved", "active");
    w.sweep().await;
    assert_eq!(w.row(&id2).unwrap().state(), "failed", "the sweep saw it stop");
    w.age(&id2, 8 * DAY);
    w.sweep().await;
    assert!(w.row(&id2).is_none(), "not live for 8 days since it last worked: removed");
}

#[tokio::test]
async fn the_cap_counts_only_live_and_young_unfinished_addresses() {
    let w = world_with(true, 2).await;
    let (a, aid) = w.pro("sub_c1", "novac1").await;
    let (b, bid) = w.pro("sub_c2", "novac2").await;
    let (c, _) = w.pro("sub_c3", "novac3").await;
    assert_eq!(w.put(&a, HOST).await.0, 201);
    assert_eq!(w.put(&b, "tips.otherchannel.com").await.0, 201);
    let (s, v) = w.put(&c, "tips.third.com").await;
    assert_eq!((s, code(&v)), (503, Some("domains_full")), "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("usual link"));
    assert_eq!(w.used("sub_c3"), 0);
    // A live one keeps its slot however old; an unfinished one past 7 days doesn't.
    w.go_live(&a, &aid, HOST).await;
    w.age(&aid, 30 * DAY);
    w.age(&bid, 8 * DAY);
    let (s, v) = w.put(&c, "tips.third.com").await;
    assert_eq!(s, 201, "{v}");
}

#[tokio::test]
async fn ten_changes_a_day_then_429() {
    let w = world().await;
    let (auth, _) = w.pro("sub_q", "novaq").await;
    w.st.db.try_take("sub_q", "tip_domain_changes", crate::quota::Period::Day, 10, 10).unwrap();
    let res = w
        .http
        .put(format!("{}/v1/pro/tips/domain", w.base))
        .bearer_auth(&auth)
        .json(&json!({ "hostname": HOST }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers()["retry-after"].to_str().unwrap().parse::<u64>().unwrap() > 0);
    assert!(res.headers().contains_key("x-quota-reset"));
    let v: Value = res.json().await.unwrap();
    assert_eq!(code(&v), Some("tip_domain_quota"));
    assert!(v["error"]["hint"].is_string());
    assert!(w.cf.calls().is_empty());
}

#[tokio::test]
async fn cloudflare_errors_change_nothing() {
    let w = world().await;
    let (auth, id) = w.pro("sub_e", "novae").await;
    w.cf.fail_with(CfError::Unavailable("down".into()));
    let (s, v) = w.put(&auth, HOST).await;
    assert_eq!((s, code(&v)), (502, Some("cloudflare_unavailable")), "{v}");
    assert!(v["error"]["hint"].is_string());
    assert_eq!(w.used("sub_e"), 0, "the change was given back");
    assert!(w.row(&id).is_none());
    // Finding 12: our zone or token is wrong: 503, nothing the streamer can fix.
    w.cf.fail_with(CfError::Zone("bad zone".into()));
    let (s, v) = w.put(&auth, HOST).await;
    assert_eq!((s, code(&v)), (503, Some("domains_unavailable")), "{v}");
    assert!(v["error"]["hint"].as_str().unwrap().contains("usual link"));
    assert_eq!(w.used("sub_e"), 0);
    w.cf.recover();
    // Cloudflare refuses the name itself: our words, as a 422 (never Cloudflare's).
    let (s, v) = w.put(&auth, "refused.mychannel.com").await;
    assert_eq!((s, code(&v)), (422, Some("invalid_hostname")), "{v}");
    assert!(!v["error"]["message"].as_str().unwrap().contains("blocked list"), "{v}");
    assert!(v["error"]["hint"].is_string());
    assert_eq!(w.used("sub_e"), 0);
    // Removing while Cloudflare is down: 502 and kept; with a zone problem, 503.
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    w.cf.fail_with(CfError::Unavailable("down".into()));
    let (s, v) = w.delete(&auth).await;
    assert_eq!((s, code(&v)), (502, Some("cloudflare_unavailable")));
    w.cf.fail_with(CfError::Zone("token refused".into()));
    let (s, v) = w.delete(&auth).await;
    assert_eq!((s, code(&v)), (503, Some("domains_unavailable")), "{v}");
    // A GET with a zone problem keeps the stored answer.
    w.age_check(&id);
    let (s, v) = w.get(&auth).await;
    assert_eq!((s, v["status"].as_str()), (200, Some("pending")), "{v}");
    assert!(w.row(&id).is_some());
    w.cf.recover();
    // Gone on Cloudflare already: removing still works.
    w.cf.forget(HOST);
    assert_eq!(w.delete(&auth).await.0, 204);
    let (s, v) = w.get(&auth).await;
    assert_eq!((s, code(&v)), (404, Some("not_found")));
    let (s, v) = w.delete(&auth).await;
    assert_eq!((s, code(&v)), (404, Some("not_found")));
}

/// Finding 4: a hostname Cloudflare already has is adopted only when this install's TXT
/// proof is published now (no `custom_metadata`: Enterprise only); otherwise 409 with the
/// record to add in the body.
#[tokio::test]
async fn only_a_duplicate_with_our_txt_proof_is_adopted() {
    let w = world().await;
    let (auth, id) = w.pro("sub_ad", "nova").await;
    let want = keys().value(&id, HOST);
    let orphan = w.cf.insert(HOST);
    // No TXT record, or someone else's value: 409, with the record that would prove it.
    for values in [vec![], vec![keys().value("someone-else", HOST)]] {
        let v: Vec<&str> = values.iter().map(String::as_str).collect();
        w.dns.set(&record_name(HOST), &v);
        let (s, v) = w.put(&auth, HOST).await;
        assert_eq!((s, code(&v)), (409, Some("domain_taken")), "{v}");
        assert_eq!(v["error"]["message"], "That address is set up somewhere else on Cloudflare.");
        assert!(v["error"]["hint"].as_str().unwrap().contains("TXT record"), "{v}");
        assert_eq!(
            v["proof"],
            json!({ "type": "TXT", "name": "_ferrocast.tips.mychannel.com", "host": "_ferrocast.tips", "value": want })
        );
        assert!(w.row(&id).is_none());
        assert!(w.cf.has(HOST), "never deleted");
    }
    // DNS down: not adopted either.
    w.prove(&id, HOST);
    w.dns.fail(true);
    assert_eq!(w.put(&auth, HOST).await.0, 409);
    w.dns.fail(false);
    // The streamer adds the record shown: adopted, proven at once. Asked fresh, not from a
    // remembered "no".
    let (s, v) = w.put(&auth, HOST).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(w.row(&id).unwrap().cf_id, orphan);
    assert_eq!(v["proof"]["found"], true, "{v}");
    assert_eq!(w.cf.count("create"), 4, "no second hostname was made");
    // Cloudflare says duplicate but our zone doesn't have it: set up elsewhere, with the proof.
    assert_eq!(w.delete(&auth).await.0, 204);
    w.cf.insert("tips.elsewhere.com");
    w.cf.hide("tips.elsewhere.com");
    w.prove(&id, "tips.elsewhere.com");
    let (s, v) = w.put(&auth, "tips.elsewhere.com").await;
    assert_eq!((s, code(&v)), (409, Some("domain_taken")), "{v}");
    assert_eq!(v["proof"]["name"], "_ferrocast.tips.elsewhere.com");
    assert_eq!(w.used("sub_ad"), 2, "the refused ones were given back (one add, one removal counted)");
}

#[tokio::test]
async fn removing_is_never_refused_by_the_daily_limit() {
    let w = world().await;
    let (auth, _) = w.pro("sub_r", "novar").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    let used = w.used("sub_r");
    w.st.db.try_take("sub_r", "tip_domain_changes", crate::quota::Period::Day, 10 - used, 10).unwrap();
    assert_eq!(w.delete(&auth).await.0, 204);
    assert!(!w.cf.has(HOST));
}

#[tokio::test]
async fn a_live_address_serves_only_its_tip_pages() {
    let w = world().await;
    let (auth, id) = w.pro("sub_h", "nova").await;
    let (s, _) = w
        .call(
            M::PUT,
            "/v1/pro/tips/goal",
            Some(&auth),
            Some(json!({ "title": "New mic", "target": 500_000, "currency": "INR" })),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(w.put(&auth, HOST).await.0, 201);

    // Pending: the "not set up yet" page, script-free, nothing from the request in it.
    let (s, html, h) = w.on(HOST, "/").await;
    assert_eq!(s, 404);
    assert!(html.contains("Not set up yet") && !html.contains("<script") && !html.contains(HOST), "{html}");
    assert!(h["content-security-policy"].to_str().unwrap().starts_with("default-src 'none'"));
    assert_eq!(w.on(HOST, "/goal").await.0, 404);

    w.go_live(&auth, &id, HOST).await;
    let (s, html, h) = w.on(HOST, "/").await;
    assert_eq!(s, 200, "{html}");
    assert!(html.contains("<h1>Nova</h1>") && html.contains("href=\"/goal\""), "{html}");
    assert!(!html.contains("<script"));
    assert_eq!(
        h["content-security-policy"],
        "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert_eq!(h["x-content-type-options"], "nosniff");
    let (s, html, _) = w.on(HOST, "/goal").await;
    assert_eq!(s, 200);
    assert!(html.contains("New mic") && html.contains("href=\"/\""), "the Tip button goes home: {html}");
    let (s, html, _) = w.on(HOST, "/goal?overlay=1").await;
    assert_eq!(s, 200);
    assert!(html.contains("http-equiv=refresh") && html.contains("class=ov"), "{html}");
    // Case, port and a trailing dot are the same address.
    for host in ["TIPS.MyChannel.COM", "tips.mychannel.com:443", "tips.mychannel.com.", "Tips.MyChannel.com.:8080"] {
        assert_eq!(w.on(host, "/").await.0, 200, "{host}");
    }
    // Images as on the tip host; nothing else (no API, no other handles).
    let (s, _, _) = w.on(HOST, &format!("/t/a/{}.webp", "0".repeat(64))).await;
    assert_ne!(s, 200);
    assert_eq!(w.on(HOST, "/nova").await.0, 404);
    assert_eq!(w.on(HOST, "/t/nova").await.0, 404);
    let (s, body, _) = w.on(HOST, "/v1/pro/status").await;
    assert_eq!(s, 404, "{body}");
    let res = w.http.post(format!("{}/v1/installs", w.base)).header("host", HOST).send().await.unwrap();
    assert_eq!(res.status(), 404, "the API isn't on someone's address");

    // Without the tip page: not set up.
    assert_eq!(w.call(M::DELETE, "/v1/tips/config", Some(&auth), None).await.0, 204);
    let (s, html, _) = w.on(HOST, "/").await;
    assert_eq!(s, 404);
    assert!(html.contains("Not set up yet"));
}

/// Finding 2: a CNAME someone left dangling (Cloudflare active) is never served without the
/// claimant's TXT record, and a live address whose TXT record goes stops after 3 sweeps.
#[tokio::test]
async fn nothing_is_served_without_the_txt_proof() {
    let w = world().await;
    let (auth, id) = w.pro("sub_dc", "nova").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    w.cf.set_status(HOST, "active", "active");
    // Someone else's value, or ours for another hostname, doesn't count.
    w.dns.set(&record_name(HOST), &[&keys().value("someone-else", HOST), &keys().value(&id, "tips.other.com")]);
    w.age_check(&id);
    let (_, v) = w.get(&auth).await;
    assert_eq!((v["status"].as_str(), v["proof"]["found"].as_bool()), (Some("pending"), Some(false)), "{v}");
    assert_eq!(w.on(HOST, "/").await.0, 404, "Cloudflare active alone isn't enough");
    // DNS down: nothing concluded.
    w.dns.fail(true);
    w.age_check(&id);
    assert_eq!(w.get(&auth).await.1["status"], "pending");
    w.dns.fail(false);
    w.go_live(&auth, &id, HOST).await;
    assert_eq!(w.on(HOST, "/").await.0, 200);

    // The record goes: two sweeps still serve, the third stops it.
    w.dns.set(&record_name(HOST), &[]);
    for n in 1..=2 {
        w.sweep().await;
        assert_eq!(w.row(&id).unwrap().state(), "live", "miss {n}");
        assert_eq!(w.on(HOST, "/").await.0, 200, "miss {n}");
    }
    // A GET in between doesn't count as a miss (only the hourly sweep does).
    w.age_check(&id);
    assert_eq!(w.get(&auth).await.1["status"], "live");
    w.sweep().await;
    let (_, v) = w.get(&auth).await;
    assert_eq!(v["status"], "proof_missing", "{v}");
    assert!(v["errors"][0].as_str().unwrap().contains("is gone"), "{v}");
    assert_eq!(w.on(HOST, "/").await.0, 404, "stopped serving");
    // Back again: live again on the next check.
    w.prove(&id, HOST);
    w.age_check(&id);
    assert_eq!(w.get(&auth).await.1["status"], "live");
    assert_eq!(w.on(HOST, "/").await.0, 200);
}

/// Finding 3: every host that isn't ours and isn't a working address gets "not set up yet"
/// on every path; Ferrocast's own hosts, IPs and `localhost` behave as before.
#[tokio::test]
async fn unknown_hosts_fail_closed_and_ours_behave_as_before() {
    let w = world().await;
    let (auth, id) = w.pro("sub_o", "nova").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    w.go_live(&auth, &id, HOST).await;
    // The tip host.
    assert_eq!(w.on("tip.example", "/nova").await.0, 200);
    assert_eq!(w.on("tip.example", "/").await.0, 307);
    assert_eq!(w.on("TIP.example", "/nova").await.0, 200);
    assert_eq!(w.on("tip.example", "/nobody").await.0, 404);
    // Health checks by IP and by `localhost`, and a container name.
    assert_eq!(w.http.get(format!("{}/health", w.base)).send().await.unwrap().status(), 200);
    for host in ["127.0.0.1", "127.0.0.1:8787", "[::1]:8787", "localhost", "localhost:8787", "ferrocast-relay:8787"] {
        assert_eq!(w.on(host, "/health").await.0, 200, "{host}");
    }
    assert_eq!(w.http.get(format!("{}/t/nova", w.base)).send().await.unwrap().status(), 200);
    assert_eq!(w.http.get(format!("{}/nova", w.base)).send().await.unwrap().status(), 404);
    let (s, _) = w.call(M::GET, "/v1/pro/tips/domain", Some(&auth), None).await;
    assert_eq!(s, 200);
    // Under our registrable domains: as before.
    assert_eq!(w.on("x.clearcraftlabs.com", "/t/nova").await.0, 200);
    assert_eq!(w.on("other.tip.example", "/health").await.0, 200);
    // Any other host: "not set up yet" on every path, any method.
    for path in ["/", "/goal", "/t/nova", "/health", "/v1/pro/status", "/nova"] {
        let (s, html, _) = w.on("tips.nobody.com", path).await;
        assert_eq!(s, 404, "{path}");
        assert!(html.contains("Not set up yet"), "{path}: {html}");
    }
    let res = w.http.post(format!("{}/v1/installs", w.base)).header("host", "tips.nobody.com").send().await.unwrap();
    assert_eq!(res.status(), 404);
    // No Host at all is the container's own health check (relay.Dockerfile): answered as ours.
    let (s, body) = raw_request(&w.base, b"GET /health HTTP/1.0\r\n\r\n").await;
    assert!(s.starts_with("HTTP/1.") && s.contains(" 200"), "{s}");
    assert!(body.ends_with("ok"), "{body}");
    // A Host that can't be read: a plain 400.
    let (s, body) = raw_request(&w.base, b"GET /health HTTP/1.1\r\nHost: \xff\xfe\r\nConnection: close\r\n\r\n").await;
    assert!(s.starts_with("HTTP/1.") && s.contains(" 400"), "{s}");
    assert!(body.contains("Host"), "{body}");
}

async fn raw_request(base: &str, raw: &[u8]) -> (String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("http://");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(raw).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.lines().next().unwrap_or("").to_string();
    (status, text)
}

#[tokio::test]
async fn removing_an_address_takes_it_down_at_once() {
    let w = world().await;
    let (auth, id) = w.pro("sub_d", "nova").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    w.go_live(&auth, &id, HOST).await;
    assert_eq!(w.on(HOST, "/").await.0, 200, "cached as live");
    assert_eq!(w.delete(&auth).await.0, 204);
    assert!(!w.cf.has(HOST));
    let (s, html, _) = w.on(HOST, "/").await;
    assert_eq!(s, 404, "the cache was told");
    assert!(html.contains("Not set up yet"));
    assert_eq!(w.used("sub_d"), 2, "adding and removing both count");
}

#[tokio::test]
async fn a_pending_address_is_checked_when_visited() {
    let w = world().await;
    let (auth, id) = w.pro("sub_p", "nova").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    w.cf.set_status(HOST, "active", "active");
    w.prove(&id, HOST);
    w.age_check(&id);
    assert_eq!(w.on(HOST, "/").await.0, 404, "answered from what's stored");
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if w.on(HOST, "/").await.0 == 200 {
            ok = true;
            break;
        }
    }
    assert!(ok, "the visit checked and the next one is served");
    assert_eq!(w.cf.count("get"), 1);
}

#[tokio::test]
async fn a_lapsed_address_works_seven_days_then_goes() {
    let w = world().await;
    let (auth, id) = w.pro("sub_l", "nova").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    w.go_live(&auth, &id, HOST).await;
    w.set_sub("sub_l", "cancelled", now_ms() - 10 * DAY);
    w.sweep().await;
    let r = w.row(&id).unwrap();
    let lapsed = r.lapsed_at.expect("marked lapsed");
    assert!(w.cf.has(HOST));
    assert_eq!(w.on(HOST, "/").await.0, 200, "still works (the free look)");
    // Pro again clears the mark.
    w.set_sub("sub_l", "active", now_ms() + DAY);
    w.sweep().await;
    assert!(w.row(&id).unwrap().lapsed_at.is_none());
    // Lapsed again, over 7 days ago: Cloudflare down keeps it for the next sweep.
    w.set_sub("sub_l", "cancelled", now_ms() - 10 * DAY);
    w.sweep().await;
    let old = now_ms() - super::LAPSED_KEEP_MS - 1000;
    w.st.db.c().execute("UPDATE tip_domains SET lapsed_at = ?1", [old]).unwrap();
    assert!(lapsed > old);
    w.cf.fail_with(CfError::Unavailable("down".into()));
    w.sweep().await;
    assert!(w.row(&id).is_some(), "kept until Cloudflare lets it go");
    w.cf.recover();
    w.sweep().await;
    assert!(w.row(&id).is_none());
    assert!(!w.cf.has(HOST));
    assert_eq!(w.on(HOST, "/").await.0, 404);
}

/// Finding 8: the sweep checks every address, live ones too, so one Cloudflare stops
/// sending us is no longer served.
#[tokio::test]
async fn the_sweep_checks_every_address() {
    let w = world().await;
    let (auth, id) = w.pro("sub_s", "nova").await;
    assert_eq!(w.put(&auth, HOST).await.0, 201);
    w.cf.set_status(HOST, "active", "active");
    w.prove(&id, HOST);
    w.sweep().await;
    assert_eq!(w.row(&id).unwrap().state(), "live");
    assert_eq!(w.on(HOST, "/").await.0, 200);
    w.cf.set_status(HOST, "moved", "active");
    w.sweep().await;
    assert_eq!(w.cf.count("get"), 2, "live ones are asked again");
    assert_eq!(w.row(&id).unwrap().state(), "failed");
    assert_eq!(w.on(HOST, "/").await.0, 404, "the cache was told");
}

/// Finding 8: a lookup that read the database before a change never stores its stale
/// answer; working addresses survive a flood of made-up hosts.
#[test]
fn the_host_cache_is_versioned_and_bounded() {
    let mut c = HostCache::default();
    let g = c.generation;
    c.forget(HOST); // a change lands while a lookup is reading
    c.put(HOST, Seen::Active("i1".into()), 0, g);
    assert_eq!(c.get(HOST, 1), None, "the stale read wasn't kept");
    c.put(HOST, Seen::Active("i1".into()), 0, c.generation);
    assert_eq!(c.get(HOST, 1), Some(Seen::Active("i1".into())));
    // Many unknown hosts: bounded, and the working address stays.
    for n in 0..(super::CACHE_MAX + 50) {
        c.put(&format!("h{n}.nobody.com"), Seen::Unknown, 0, c.generation);
    }
    assert!(c.unknown.len() <= super::CACHE_MAX);
    assert_eq!(c.get(HOST, 1), Some(Seen::Active("i1".into())));
    assert_eq!(c.get("h5000.nobody.com", super::NEGATIVE_TTL_MS + 1), None, "unknowns go after 5 s");
    assert_eq!(c.get(HOST, super::NEGATIVE_TTL_MS + 1), Some(Seen::Active("i1".into())));
    // Pending ones go before working ones when the address map is full.
    for n in 0..super::CACHE_MAX {
        c.put(&format!("p{n}.x.com"), Seen::Pending, 0, c.generation);
    }
    assert!(c.known.len() <= super::CACHE_MAX);
    assert_eq!(c.get(HOST, 1), Some(Seen::Active("i1".into())), "the working address stayed");
}

#[test]
fn cloudflares_errors_are_deduplicated_wherever_they_repeat() {
    let mut h = cf_hostname();
    h["verification_errors"] = json!(["a", "b", "a", "", "b"]);
    h["ssl"]["validation_errors"] = json!([{ "message": "a" }, { "message": "c" }]);
    let p = cloudflare::parse_hostname(&h).unwrap();
    assert_eq!(p.errors, vec!["a", "b", "c"]);
}

// ── The real client against a local fake Cloudflare ──────────────────────────────────

#[derive(Debug, Clone)]
struct Asked {
    method: String,
    path: String,
    query: String,
    auth: String,
    body: Value,
}

/// A fake Cloudflare answering `(status, body)` in order and recording each request.
async fn fake_cloudflare(answers: Vec<(u16, Value)>) -> (String, Arc<Mutex<Vec<Asked>>>) {
    use axum::extract::Request;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(answers)));
    let rec = seen.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let rec = rec.clone();
        let queue = queue.clone();
        async move {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
            rec.lock().unwrap().push(Asked {
                method: parts.method.to_string(),
                path: parts.uri.path().to_string(),
                query: parts.uri.query().unwrap_or("").to_string(),
                auth: parts.headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            });
            let (s, b) = queue.lock().unwrap().pop_front().unwrap_or((500, Value::Null));
            (axum::http::StatusCode::from_u16(s).unwrap(), axum::Json(b))
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/client/v4", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (base, seen)
}

fn ok(result: Value) -> Value {
    json!({ "success": true, "errors": [], "messages": [], "result": result })
}

fn failed(code: i64, message: &str) -> Value {
    json!({ "success": false, "errors": [{ "code": code, "message": message }], "messages": [], "result": null })
}

fn cf_hostname() -> Value {
    json!({
        "id": "0d89c70d-ad9f-4843-b99f-6cc0252067e9",
        "hostname": HOST,
        "status": "pending",
        "ssl": {
            "id": "s1", "status": "pending_validation", "method": "http", "type": "dv",
            "settings": { "min_tls_version": "1.2" },
            "validation_records": [
                { "http_url": "http://tips.mychannel.com/.well-known/pki-validation/x.txt", "http_body": "ca3-..." },
                { "txt_name": "_acme-challenge.tips.mychannel.com", "txt_value": "acme-token" }
            ],
            "validation_errors": [{ "message": "None of the A or AAAA records are owned by this account" }]
        },
        "ownership_verification": { "type": "txt", "name": "_cf-custom-hostname.tips.mychannel.com", "value": "5cc07c04-ea62" },
        "ownership_verification_http": { "http_url": "http://tips.mychannel.com/.well-known/cf-custom-hostname-challenge/0d89", "http_body": "5cc07c04" },
        "verification_errors": ["custom hostname does not CNAME to this zone."],
        "created_at": "2026-10-19T10:00:00Z"
    })
}

#[tokio::test]
async fn the_cloudflare_client_sends_the_documented_requests() {
    let (base, seen) = fake_cloudflare(vec![
        (200, ok(cf_hostname())),
        (200, ok(cf_hostname())),
        (200, ok(json!([cf_hostname()]))),
        (200, ok(json!({ "id": "0d89c70d-ad9f-4843-b99f-6cc0252067e9" }))),
    ])
    .await;
    let token = ["test", "-token-", "not-real"].concat();
    let cf = CloudflareHostnames::new(&base, "zone123", &token);
    assert!(!format!("{cf:?}").contains(&token), "the token never shows in Debug");
    let h = cf.create(HOST).await.unwrap();
    assert_eq!(h.id, "0d89c70d-ad9f-4843-b99f-6cc0252067e9");
    assert_eq!((h.status.as_str(), h.ssl_status.as_str()), ("pending", "pending_validation"));
    assert_eq!(
        h.records,
        vec![
            cloudflare::Record {
                kind: "TXT".into(),
                name: "_cf-custom-hostname.tips.mychannel.com".into(),
                value: "5cc07c04-ea62".into(),
                why: "ownership".into()
            },
            cloudflare::Record {
                kind: "TXT".into(),
                name: "_acme-challenge.tips.mychannel.com".into(),
                value: "acme-token".into(),
                why: "certificate".into()
            },
        ]
    );
    assert_eq!(h.errors.len(), 2, "{:?}", h.errors);
    assert_eq!(cf.get(&h.id).await.unwrap().hostname, HOST);
    assert_eq!(cf.find(HOST).await.unwrap().map(|h| h.id), Some(h.id.clone()));
    cf.delete(&h.id).await.unwrap();

    let s = seen.lock().unwrap().clone();
    let path = "/client/v4/zones/zone123/custom_hostnames";
    assert_eq!((s[0].method.as_str(), s[0].path.as_str()), ("POST", path));
    assert_eq!(
        s[0].body,
        json!({ "hostname": HOST, "ssl": { "method": "http", "type": "dv", "settings": { "min_tls_version": "1.2" } } })
    );
    assert_eq!(s[0].body, cloudflare::create_body(HOST));
    assert!(s[0].body.get("custom_metadata").is_none(), "Enterprise only: never sent");
    assert_eq!((s[1].method.as_str(), s[1].path.clone()), ("GET", format!("{path}/{}", h.id)));
    assert_eq!(
        (s[2].method.as_str(), s[2].path.as_str(), s[2].query.as_str()),
        ("GET", path, "hostname=tips.mychannel.com")
    );
    assert_eq!((s[3].method.as_str(), s[3].path.clone()), ("DELETE", format!("{path}/{}", h.id)));
    assert!(s.iter().all(|r| r.auth == format!("Bearer {token}")), "{s:?}");
}

#[tokio::test]
async fn the_cloudflare_client_reads_errors_into_what_the_relay_does() {
    let (base, _) = fake_cloudflare(vec![
        (409, failed(1406, "Duplicate custom hostname found.")),
        (400, failed(1406, "Duplicate custom hostname found.")),
        (400, failed(1409, "Hostname is not allowed.")),
        (403, failed(10000, "Authentication error")),
        (500, json!({})),
        (429, failed(971, "Please wait and consider throttling your request speed")),
        (404, failed(1436, "Custom hostname not found.")),
        (404, failed(7003, "Could not route to /zones/nope, perhaps your object identifier is invalid?")),
        (200, json!({ "success": true, "result": { "hostname": "x" } })),
        (400, failed(1001, "Invalid zone identifier")),
        (400, failed(1100, "SSL for SaaS is not enabled on this zone")),
        (400, failed(6007, "Malformed JSON in request body")),
        (404, failed(7003, "Could not route to /zones/nope, perhaps your object identifier is invalid?")),
    ])
    .await;
    let cf = CloudflareHostnames::new(&base, "zone123", "t");
    assert_eq!(cf.create(HOST).await, Err(CfError::Duplicate));
    assert_eq!(cf.create(HOST).await, Err(CfError::Duplicate));
    assert_eq!(cf.create(HOST).await, Err(CfError::Refused("Hostname is not allowed.".into())));
    assert!(matches!(cf.create(HOST).await, Err(CfError::Zone(_))), "a refused token is ours to fix");
    assert!(matches!(cf.create(HOST).await, Err(CfError::Unavailable(_))), "a 5xx");
    assert!(matches!(cf.create(HOST).await, Err(CfError::Unavailable(_))), "a 429");
    assert_eq!(cf.get("nope").await, Err(CfError::NotFound));
    assert!(matches!(cf.create(HOST).await, Err(CfError::Zone(_))), "a 404 on create is the zone");
    assert!(matches!(cf.get("x").await, Err(CfError::Unavailable(_))), "no id: unreadable");
    // Finding 12: zone-level 400s are ours, not the streamer's; an unknown 400 is "try later".
    assert!(matches!(cf.create(HOST).await, Err(CfError::Zone(_))), "a bad zone id");
    assert!(matches!(cf.create(HOST).await, Err(CfError::Zone(_))), "SaaS not enabled");
    assert!(matches!(cf.create(HOST).await, Err(CfError::Unavailable(_))), "not about the hostname");
    assert!(matches!(cf.get("x").await, Err(CfError::Zone(_))), "a bad zone on read isn't a missing hostname");
    // Nobody listening at all.
    let gone = CloudflareHostnames::new("http://127.0.0.1:9", "zone123", "t");
    assert!(matches!(gone.get("x").await, Err(CfError::Unavailable(_))));
}
