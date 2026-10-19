//! Cloudflare for SaaS custom hostnames (API.md §9.2, custom address): a streamer's
//! `tips.mychannel.com` is added to our zone, Cloudflare gets it a certificate, and its
//! traffic reaches the zone's fallback origin (the tip host), so the relay sees the
//! streamer's hostname in `Host`.
//!
//! [`CustomHostnames`] is the part of the API the relay uses (create, read, find by name,
//! delete); [`CloudflareHostnames`] is the real one (`CF_SAAS_TOKEN`, `CF_ZONE_ID`) and
//! [`FakeHostnames`] stands in for it in tests.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// What Cloudflare answered, in the terms the relay acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CfError {
    /// The zone already has this hostname.
    Duplicate,
    /// Cloudflare won't take this hostname (its message, for the log; the streamer gets our
    /// own words).
    Refused(String),
    /// There's no such custom hostname (deleted on Cloudflare's side).
    NotFound,
    /// No answer, a 5xx or 429: nothing changed, try later.
    Unavailable(String),
    /// Our side is wrong (a bad `CF_ZONE_ID`, a refused `CF_SAAS_TOKEN`, Cloudflare for SaaS
    /// not enabled on the zone): no streamer can fix it, so it's 503 `domains_unavailable`.
    Zone(String),
}

impl std::fmt::Display for CfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CfError::Duplicate => f.write_str("the hostname is already in the zone"),
            CfError::Refused(m) => write!(f, "refused: {m}"),
            CfError::NotFound => f.write_str("no such custom hostname"),
            CfError::Unavailable(m) => write!(f, "unavailable: {m}"),
            CfError::Zone(m) => write!(f, "zone setup: {m}"),
        }
    }
}

/// A DNS record that proves something to Cloudflare: `why` is `ownership` (the address is
/// the streamer's, before the CNAME is in place) or `certificate` (TXT certificate check).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub value: String,
    pub why: String,
}

/// One custom hostname as Cloudflare sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hostname {
    pub id: String,
    pub hostname: String,
    /// `pending`, `active`, `blocked`, `moved`, `deleted`, …
    pub status: String,
    /// `initializing`, `pending_validation`, `pending_issuance`, `pending_deployment`, `active`,
    /// `*_timed_out`, `expired`, …
    pub ssl_status: String,
    /// Optional TXT records (ownership, certificate).
    pub records: Vec<Record>,
    /// Cloudflare's own words about what's missing ("custom hostname does not CNAME to this zone.").
    pub errors: Vec<String>,
}

pub type CfFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, CfError>> + Send + 'a>>;

/// The custom hostname calls the relay makes. Boxed futures so the relay can hold any of
/// them as `dyn`.
pub trait CustomHostnames: Send + Sync + std::fmt::Debug {
    /// Adds `hostname` (certificate by HTTP validation, DV, TLS 1.2 or newer).
    fn create<'a>(&'a self, hostname: &'a str) -> CfFuture<'a, Hostname>;
    /// Reads one by Cloudflare's id.
    fn get<'a>(&'a self, id: &'a str) -> CfFuture<'a, Hostname>;
    /// Looks one up by its exact name (after a `Duplicate`).
    fn find<'a>(&'a self, hostname: &'a str) -> CfFuture<'a, Option<Hostname>>;
    /// Removes it (its certificate goes with it).
    fn delete<'a>(&'a self, id: &'a str) -> CfFuture<'a, ()>;
}

/// The body of a create call: HTTP validation works by itself once the CNAME points at us.
/// No `custom_metadata` (Enterprise only on custom hostnames; our zone is self-serve): who may
/// adopt an existing hostname is decided by the TXT proof instead.
pub fn create_body(hostname: &str) -> Value {
    json!({
        "hostname": hostname,
        "ssl": { "method": "http", "type": "dv", "settings": { "min_tls_version": "1.2" } },
    })
}

/// Drops repeats anywhere in the list (not just next to each other), keeping the order.
pub fn dedupe(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|e| seen.insert(e.clone()));
}

/// Reads Cloudflare's `result` object for a custom hostname.
pub fn parse_hostname(v: &Value) -> Option<Hostname> {
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
    let id = s("/id");
    if id.is_empty() {
        return None;
    }
    let mut records = Vec::new();
    if let Some(o) = v.get("ownership_verification")
        && o["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("txt"))
        && let (Some(name), Some(value)) = (o["name"].as_str(), o["value"].as_str())
    {
        records.push(Record { kind: "TXT".into(), name: name.into(), value: value.into(), why: "ownership".into() });
    }
    for r in v.pointer("/ssl/validation_records").and_then(Value::as_array).into_iter().flatten() {
        if let (Some(name), Some(value)) = (r["txt_name"].as_str(), r["txt_value"].as_str()) {
            records.push(Record {
                kind: "TXT".into(),
                name: name.into(),
                value: value.into(),
                why: "certificate".into(),
            });
        }
    }
    let mut errors: Vec<String> = Vec::new();
    for e in v.get("verification_errors").and_then(Value::as_array).into_iter().flatten() {
        if let Some(m) = e.as_str() {
            errors.push(m.to_string());
        }
    }
    for e in v.pointer("/ssl/validation_errors").and_then(Value::as_array).into_iter().flatten() {
        if let Some(m) = e["message"].as_str().or_else(|| e.as_str()) {
            errors.push(m.to_string());
        }
    }
    errors.iter_mut().for_each(|e| *e = clip(e, 300));
    errors.retain(|e| !e.trim().is_empty());
    dedupe(&mut errors);
    Some(Hostname { id, hostname: s("/hostname"), status: s("/status"), ssl_status: s("/ssl/status"), records, errors })
}

fn clip(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Cloudflare's error codes about the zone, the token or the plan, never one hostname:
/// 7000/7003 no such route or zone id, 1001/1003 invalid zone, 6003/6111/9103/9106/9109/10000
/// authentication. "Not entitled" / "not enabled" (Cloudflare for SaaS off) are matched by words.
const ZONE_CODES: &[i64] = &[1001, 1003, 6003, 6111, 7000, 7003, 9103, 9106, 9109, 10000];

/// What a failed answer means for the relay. Only a hostname-specific 400/422 (Cloudflare's
/// 14xx custom hostname codes) is the streamer's to fix; zone, token and plan problems are
/// ours (`Zone`); everything else is "try later".
pub fn classify(status: u16, codes: &[i64], message: &str) -> CfError {
    let m = message.to_ascii_lowercase();
    // 1406: "Duplicate custom hostname found."
    if status == 409 || codes.contains(&1406) || m.contains("duplicate") {
        return CfError::Duplicate;
    }
    let zone_words = [
        "not entitled",
        "not enabled",
        "ssl for saas",
        "cloudflare for saas",
        "invalid zone",
        "zone identifier",
        "authentication",
        "access token",
    ];
    if matches!(status, 401 | 403)
        || codes.iter().any(|c| ZONE_CODES.contains(c))
        || zone_words.iter().any(|w| m.contains(w))
    {
        return CfError::Zone(if message.is_empty() {
            format!("Cloudflare answered {status}")
        } else {
            message.to_string()
        });
    }
    match status {
        404 => CfError::NotFound,
        400 | 422 if codes.iter().any(|c| (1400..1500).contains(c)) || (codes.is_empty() && m.contains("hostname")) => {
            CfError::Refused(if message.is_empty() { "refused".into() } else { message.to_string() })
        }
        _ => CfError::Unavailable(format!("Cloudflare answered {status}")),
    }
}

/// The real Cloudflare API.
pub struct CloudflareHostnames {
    http: reqwest::Client,
    /// `https://api.cloudflare.com/client/v4` (a local fake in tests).
    base: String,
    zone: String,
    token: String,
    /// A zone-level problem is logged once (until a call works again), not on every call.
    zone_logged: AtomicBool,
}

impl std::fmt::Debug for CloudflareHostnames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareHostnames")
            .field("base", &self.base)
            .field("zone", &self.zone)
            .field("token", &"<redacted>")
            .finish()
    }
}

pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";

impl CloudflareHostnames {
    pub fn new(base: &str, zone: &str, token: &str) -> CloudflareHostnames {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        CloudflareHostnames {
            http,
            base: base.trim_end_matches('/').to_string(),
            zone: zone.to_string(),
            token: token.to_string(),
            zone_logged: AtomicBool::new(false),
        }
    }

    /// `CF_SAAS_TOKEN` and `CF_ZONE_ID`, both or nothing (the feature stays off).
    pub fn from_env() -> Option<CloudflareHostnames> {
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Some(CloudflareHostnames::new(API_BASE, &env("CF_ZONE_ID")?, &env("CF_SAAS_TOKEN")?))
    }

    fn url(&self, tail: &str) -> String {
        format!("{}/zones/{}/custom_hostnames{tail}", self.base, self.zone)
    }

    /// Sends a request and reads Cloudflare's envelope into its `result`.
    async fn call(&self, req: reqwest::RequestBuilder) -> Result<Value, CfError> {
        let res =
            req.bearer_auth(&self.token).send().await.map_err(|e| {
                CfError::Unavailable(if e.is_timeout() { "timed out".into() } else { "no answer".into() })
            })?;
        let status = res.status().as_u16();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        let codes: Vec<i64> =
            body["errors"].as_array().into_iter().flatten().filter_map(|e| e["code"].as_i64()).collect();
        let first = body["errors"][0]["message"].as_str().map(|m| clip(m, 300)).unwrap_or_default();
        let ok = (200..300).contains(&status) && body["success"].as_bool() != Some(false);
        if ok {
            self.zone_logged.store(false, Ordering::Relaxed);
            return Ok(body["result"].clone());
        }
        match classify(status, &codes, &first) {
            CfError::Zone(why) => {
                if !self.zone_logged.swap(true, Ordering::Relaxed) {
                    tracing::error!(status, codes = ?codes, message = %first, "Cloudflare for SaaS isn't usable: {why} (check CF_ZONE_ID, CF_SAAS_TOKEN's Custom Hostnames edit permission, and that Cloudflare for SaaS is on for the zone)");
                }
                Err(CfError::Zone(why))
            }
            CfError::Unavailable(m) => {
                tracing::warn!(status, codes = ?codes, "Cloudflare custom hostnames call failed");
                Err(CfError::Unavailable(m))
            }
            e => Err(e),
        }
    }

    fn hostname(v: Value) -> Result<Hostname, CfError> {
        parse_hostname(&v).ok_or_else(|| CfError::Unavailable("Cloudflare sent an unreadable answer".into()))
    }
}

impl CustomHostnames for CloudflareHostnames {
    fn create<'a>(&'a self, hostname: &'a str) -> CfFuture<'a, Hostname> {
        Box::pin(async move {
            let req = self.http.post(self.url("")).json(&create_body(hostname));
            match self.call(req).await {
                // A 404 here is the zone, not the hostname.
                Err(CfError::NotFound) => {
                    if !self.zone_logged.swap(true, Ordering::Relaxed) {
                        tracing::error!("Cloudflare doesn't know CF_ZONE_ID (a 404 on create)");
                    }
                    Err(CfError::Zone("Cloudflare doesn't know CF_ZONE_ID".into()))
                }
                r => Self::hostname(r?),
            }
        })
    }

    fn get<'a>(&'a self, id: &'a str) -> CfFuture<'a, Hostname> {
        Box::pin(async move { Self::hostname(self.call(self.http.get(self.url(&format!("/{id}")))).await?) })
    }

    fn find<'a>(&'a self, hostname: &'a str) -> CfFuture<'a, Option<Hostname>> {
        Box::pin(async move {
            let v = self.call(self.http.get(self.url("")).query(&[("hostname", hostname)])).await?;
            Ok(v.as_array().into_iter().flatten().filter_map(parse_hostname).find(|h| h.hostname == hostname))
        })
    }

    fn delete<'a>(&'a self, id: &'a str) -> CfFuture<'a, ()> {
        Box::pin(async move { self.call(self.http.delete(self.url(&format!("/{id}")))).await.map(|_| ()) })
    }
}

/// An in-memory Cloudflare for tests: hostnames start `pending` / `initializing` until a
/// test moves them on; any call can be made to fail.
#[derive(Debug, Default)]
pub struct FakeHostnames {
    zone: Mutex<HashMap<String, Hostname>>,
    next_id: Mutex<u64>,
    fail: Mutex<Option<CfError>>,
    calls: Mutex<Vec<String>>,
    hidden: Mutex<Vec<String>>,
}

impl FakeHostnames {
    pub fn new() -> FakeHostnames {
        FakeHostnames::default()
    }

    /// Every call so far, as `create tips.a.com`, `get cf1`, `find x`, `delete cf1`.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// How many calls start with `what` (`"get"`, `"create"`).
    pub fn count(&self, what: &str) -> usize {
        self.calls().iter().filter(|c| c.split(' ').next() == Some(what)).count()
    }

    /// Every call fails with `e` until [`FakeHostnames::recover`].
    pub fn fail_with(&self, e: CfError) {
        *self.fail.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
    }

    pub fn recover(&self) {
        *self.fail.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Sets what Cloudflare says about `hostname` (`active` / `active` is live).
    pub fn set_status(&self, hostname: &str, status: &str, ssl_status: &str) {
        let mut z = self.zone.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(h) = z.values_mut().find(|h| h.hostname == hostname) {
            h.status = status.into();
            h.ssl_status = ssl_status.into();
            if status == "active" {
                h.errors.clear();
            }
        }
    }

    /// Adds a hostname as if made earlier (an orphan the relay doesn't know about).
    pub fn insert(&self, hostname: &str) -> String {
        self.made(hostname).id
    }

    /// Whether a find call by name sees it (Cloudflare can say "duplicate" for a name that's
    /// in another account's zone, which a find on ours never returns).
    pub fn hide(&self, hostname: &str) {
        self.hidden.lock().unwrap_or_else(|p| p.into_inner()).push(hostname.to_string());
    }

    /// Drops one on Cloudflare's side.
    pub fn forget(&self, hostname: &str) {
        self.zone.lock().unwrap_or_else(|p| p.into_inner()).retain(|_, h| h.hostname != hostname);
    }

    /// Whether the zone has `hostname`.
    pub fn has(&self, hostname: &str) -> bool {
        self.zone.lock().unwrap_or_else(|p| p.into_inner()).values().any(|h| h.hostname == hostname)
    }

    fn made(&self, hostname: &str) -> Hostname {
        let id = {
            let mut n = self.next_id.lock().unwrap_or_else(|p| p.into_inner());
            *n += 1;
            format!("cf{n}")
        };
        let h = Hostname {
            id: id.clone(),
            hostname: hostname.into(),
            status: "pending".into(),
            ssl_status: "initializing".into(),
            records: vec![Record {
                kind: "TXT".into(),
                name: format!("_cf-custom-hostname.{hostname}"),
                value: format!("token-{id}"),
                why: "ownership".into(),
            }],
            errors: vec!["custom hostname does not CNAME to this zone.".into()],
        };
        self.zone.lock().unwrap_or_else(|p| p.into_inner()).insert(id, h.clone());
        h
    }

    fn enter(&self, call: String) -> Result<(), CfError> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).push(call);
        match self.fail.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl CustomHostnames for FakeHostnames {
    fn create<'a>(&'a self, hostname: &'a str) -> CfFuture<'a, Hostname> {
        Box::pin(async move {
            self.enter(format!("create {hostname}"))?;
            if self.has(hostname) {
                return Err(CfError::Duplicate);
            }
            if hostname.starts_with("refused.") {
                return Err(CfError::Refused("This hostname is on a blocked list.".into()));
            }
            Ok(self.made(hostname))
        })
    }

    fn get<'a>(&'a self, id: &'a str) -> CfFuture<'a, Hostname> {
        Box::pin(async move {
            self.enter(format!("get {id}"))?;
            self.zone.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned().ok_or(CfError::NotFound)
        })
    }

    fn find<'a>(&'a self, hostname: &'a str) -> CfFuture<'a, Option<Hostname>> {
        Box::pin(async move {
            self.enter(format!("find {hostname}"))?;
            if self.hidden.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|h| h == hostname) {
                return Ok(None);
            }
            Ok(self.zone.lock().unwrap_or_else(|p| p.into_inner()).values().find(|h| h.hostname == hostname).cloned())
        })
    }

    fn delete<'a>(&'a self, id: &'a str) -> CfFuture<'a, ()> {
        Box::pin(async move {
            self.enter(format!("delete {id}"))?;
            self.zone.lock().unwrap_or_else(|p| p.into_inner()).remove(id).map(|_| ()).ok_or(CfError::NotFound)
        })
    }
}
