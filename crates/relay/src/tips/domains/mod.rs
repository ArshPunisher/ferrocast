//! Pro's custom tip address (F6, API.md §9.2 "Your own address"): a streamer points a
//! subdomain they own (`tips.mychannel.com`) at their tip page.
//!
//! The address is a Cloudflare for SaaS custom hostname on our zone (`cloudflare`); its
//! traffic reaches the zone's fallback origin (the tip host) and so the relay, with the
//! streamer's hostname in `Host`. Besides the CNAME, the streamer adds a TXT record
//! `_ferrocast.<hostname>` with a value only this relay makes for this install (`proof`), so
//! a CNAME someone else left dangling can't be claimed. An address is live once Cloudflare
//! has it active and the TXT record matches; the hourly sweep checks both again, and an
//! address whose TXT record is gone three checks running stops (`proof_missing`).
//!
//! [`route_custom_hosts`] runs before every route and fails closed: Ferrocast's own hosts
//! (the relay, the tip host, the remote page, anything under their registrable domains, any
//! IP, `localhost` or machine name) pass untouched; a live address gets that install's tip
//! page (`/`), goal page (`/goal`) and images (`/t/a/…`), nothing else; every other host
//! gets a small "not set up yet" page on every path, and a request without a readable
//! `Host` gets a plain 400.
//!
//! One address per subscription (per install: its tip page), 100 on the relay (Cloudflare's
//! free allowance: only live addresses and unfinished ones younger than 7 days count), 10
//! changes a day. An address that isn't live 7 days after it was added (or last worked) is
//! removed, and one that isn't live after 24 hours can be claimed by someone else. A lapsed
//! install's address keeps working 7 days after the hourly sweep first sees the lapse, then
//! it's deleted from Cloudflare and here.

pub mod cloudflare;
pub mod hostname;
pub mod proof;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use cloudflare::{CfError, CloudflareHostnames, CustomHostnames, FakeHostnames, Hostname, Record};
pub use proof::{DohResolver, FakeDns, ProofDns, ProofKeys};

use super::Body;
use crate::billing::gate::require_pro;
use crate::quota::{Limit, Period};
use crate::{ApiError, AppState, Db, now_ms};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tip_domains (install_id TEXT PRIMARY KEY, sub_id TEXT NOT NULL, hostname TEXT NOT NULL UNIQUE, cf_id TEXT NOT NULL, status TEXT NOT NULL, ssl_status TEXT NOT NULL, verification_json TEXT NOT NULL DEFAULT '{}', created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, checked_at INTEGER NOT NULL, claimed_at INTEGER NOT NULL DEFAULT 0, live_at INTEGER, proven_at INTEGER, proof_misses INTEGER NOT NULL DEFAULT 0, lapsed_at INTEGER);
CREATE INDEX IF NOT EXISTS tip_domains_sub ON tip_domains(sub_id);
";

/// Addresses on the relay: Cloudflare for SaaS includes 100 custom hostnames.
pub const MAX_DOMAINS: usize = 100;
/// Adds and removals per subscription per UTC day (removing is never refused).
pub(crate) const CHANGES_PER_DAY: Limit = Limit { name: "tip_domain_changes", period: Period::Day, limit: 10 };
/// Cloudflare and the TXT record are checked for an address at most once a minute.
pub const REFRESH_MS: i64 = 60_000;
/// How long a lapsed install's address keeps working.
pub const LAPSED_KEEP_MS: i64 = 7 * 86_400_000;
/// An address that isn't live this long after it was added (or last worked) is removed, and
/// stops counting towards the relay's cap.
pub const UNFINISHED_KEEP_MS: i64 = 7 * 86_400_000;
/// An address that isn't live this long after it was added (or last worked) can be claimed
/// by another tip page.
pub const TAKEOVER_AFTER_MS: i64 = 86_400_000;
/// Hourly checks running without the TXT record before a live address stops.
pub const PROOF_MISSES: i64 = 3;
/// How long a host → install answer is remembered (the database stays the truth).
pub const CACHE_TTL_MS: i64 = 30_000;
/// How long "nobody's address" is remembered (kept apart, so a flood of made-up hosts never
/// pushes a working address out).
pub const NEGATIVE_TTL_MS: i64 = 5_000;
/// How long a TXT answer is remembered.
pub const TXT_TTL_MS: i64 = 30_000;
const CACHE_MAX: usize = 4096;
const TXT_CACHE_MAX: usize = 1024;
/// Ferrocast's own domain: nobody's custom address can be on it.
const OWN_DOMAINS: &[&str] = &["clearcraftlabs.com"];

/// What a host is, as far as serving goes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// Not anyone's address.
    Unknown,
    /// Someone's address, but it isn't working yet (or any more).
    Pending,
    /// A working address: this install's pages.
    Active(String),
}

/// Host → [`Seen`] answers. Addresses (`known`) and "nobody's" (`unknown`) are bounded apart;
/// `generation` goes up on every change, and a lookup only stores what it read if no change came
/// in between (so a slow read never brings back an answer a change just forgot).
#[derive(Debug, Default)]
struct HostCache {
    generation: u64,
    known: HashMap<String, (i64, Seen)>,
    unknown: HashMap<String, i64>,
}

impl HostCache {
    fn get(&self, host: &str, now: i64) -> Option<Seen> {
        if let Some((_, s)) = self.known.get(host).filter(|(exp, _)| *exp > now) {
            return Some(s.clone());
        }
        self.unknown.get(host).filter(|exp| **exp > now).map(|_| Seen::Unknown)
    }

    fn put(&mut self, host: &str, seen: Seen, now: i64, generation: u64) {
        if generation != self.generation {
            return;
        }
        if seen == Seen::Unknown {
            if self.unknown.len() >= CACHE_MAX && !self.unknown.contains_key(host) {
                self.unknown.retain(|_, exp| *exp > now);
                if self.unknown.len() >= CACHE_MAX
                    && let Some(k) = self.unknown.iter().min_by_key(|(_, exp)| **exp).map(|(k, _)| k.clone())
                {
                    self.unknown.remove(&k);
                }
            }
            self.unknown.insert(host.to_string(), now + NEGATIVE_TTL_MS);
            return;
        }
        if self.known.len() >= CACHE_MAX && !self.known.contains_key(host) {
            self.known.retain(|_, (exp, _)| *exp > now);
            if self.known.len() >= CACHE_MAX {
                // Not-working ones go first; a working address goes only when all are.
                let victim = self
                    .known
                    .iter()
                    .min_by_key(|(_, (exp, s))| (matches!(s, Seen::Active(_)), *exp))
                    .map(|(k, _)| k.clone());
                if let Some(k) = victim {
                    self.known.remove(&k);
                }
            }
        }
        self.known.insert(host.to_string(), (now + CACHE_TTL_MS, seen));
    }

    fn forget(&mut self, host: &str) {
        self.generation += 1;
        self.known.remove(host);
        self.unknown.remove(host);
    }
}

/// The custom address runtime: Cloudflare (`None`: the feature is off), the TXT proof's DNS
/// and keys (no keys: off), the relay-wide cap, Ferrocast's own domains, and the caches.
#[derive(Debug)]
pub struct Runtime {
    cf: Option<Arc<dyn CustomHostnames>>,
    dns: Arc<dyn ProofDns>,
    keys: Option<ProofKeys>,
    cap: usize,
    /// Worked out once (the first request, or [`warm_up`] at start).
    own: OnceLock<Vec<String>>,
    cache: Mutex<HostCache>,
    txt: Mutex<HashMap<String, (i64, Vec<String>)>>,
    /// One add or removal at a time, so the per-subscription limit and the cap hold.
    writes: tokio::sync::Mutex<()>,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new(None, None, None, MAX_DOMAINS)
    }
}

impl Runtime {
    /// `dns`: `None` is Cloudflare's DNS-over-HTTPS. `secret`: the decoded `RELAY_VAULT_KEY`
    /// (16 bytes or more), which keys the TXT values and the install tags.
    pub fn new(
        cf: Option<Arc<dyn CustomHostnames>>,
        dns: Option<Arc<dyn ProofDns>>,
        secret: Option<&[u8]>,
        cap: usize,
    ) -> Runtime {
        Runtime {
            cf,
            dns: dns.unwrap_or_else(|| Arc::new(DohResolver::default())),
            keys: secret.and_then(ProofKeys::new),
            cap,
            own: OnceLock::new(),
            cache: Mutex::default(),
            txt: Mutex::default(),
            writes: tokio::sync::Mutex::new(()),
        }
    }

    fn cf(&self) -> Result<(Arc<dyn CustomHostnames>, ProofKeys), ApiError> {
        match (self.cf.clone(), self.keys.clone()) {
            (Some(cf), Some(keys)) => Ok((cf, keys)),
            _ => Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "domains_unavailable",
                "Custom addresses aren't set up on this relay yet.",
            )
            .hint("Your tip page keeps working at its usual link.")),
        }
    }

    fn own(&self, st: &AppState) -> &[String] {
        self.own.get_or_init(|| own_domains(st))
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HostCache> {
        self.cache.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn forget(&self, host: &str) {
        self.cache().forget(host);
    }
}

/// `features.tip_domain`: on once `CF_SAAS_TOKEN`, `CF_ZONE_ID` and `RELAY_VAULT_KEY` are set.
pub(crate) fn available(st: &AppState) -> bool {
    st.tip_domains.cf.is_some() && st.tip_domains.keys.is_some()
}

/// Works out Ferrocast's own domains now (at start) instead of on the first request.
pub fn warm_up(st: &AppState) {
    st.tip_domains.own(st);
}

// ── Store ────────────────────────────────────────────────────────────────────────────

/// What Cloudflare last said besides the two statuses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Verification {
    #[serde(default)]
    records: Vec<Record>,
    #[serde(default)]
    errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DomainRow {
    install: String,
    hostname: String,
    cf_id: String,
    status: String,
    ssl_status: String,
    verification: Verification,
    created_at: i64,
    checked_at: i64,
    live_at: Option<i64>,
    proven_at: Option<i64>,
    proof_misses: i64,
    lapsed_at: Option<i64>,
}

impl DomainRow {
    /// The TXT record was seen and hasn't been missing three checks running.
    fn proven(&self) -> bool {
        self.proven_at.is_some() && self.proof_misses < PROOF_MISSES
    }

    /// `pending` (waiting for the DNS records), `verifying` (the records work, the
    /// certificate is on its way), `live`, `failed` (Cloudflare gave up; remove and add it
    /// again), or `proof_missing` (it worked, then the TXT record went).
    fn state(&self) -> &'static str {
        let ssl = self.ssl_status.as_str();
        let ssl_failed = ssl.ends_with("timed_out") || matches!(ssl, "expired" | "deleted" | "inactive");
        let active = matches!(self.status.as_str(), "active" | "active_redeploying");
        match self.status.as_str() {
            "blocked" | "moved" | "deleted" | "pending_blocked" | "pending_deletion" => "failed",
            _ if self.proven_at.is_some() && self.proof_misses >= PROOF_MISSES => "proof_missing",
            _ if ssl_failed => "failed",
            _ if active && ssl == "active" && self.proven() => "live",
            _ if active && self.proven() => "verifying",
            _ => "pending",
        }
    }

    /// Cloudflare sends this address's traffic to us and the TXT record proves the claim.
    fn serving(&self) -> bool {
        matches!(self.status.as_str(), "active" | "active_redeploying") && self.proven()
    }

    /// When it was added or last seen working.
    fn since(&self) -> i64 {
        self.created_at.max(self.live_at.unwrap_or(0))
    }

    /// Counts towards the relay's cap: live, or unfinished but younger than 7 days.
    fn counts(&self, now: i64) -> bool {
        self.state() == "live" || now - self.since() < UNFINISHED_KEEP_MS
    }

    /// Another tip page may claim this address (not live for over 24 hours).
    fn stale(&self, now: i64) -> bool {
        self.state() != "live" && now - self.since() > TAKEOVER_AFTER_MS
    }

    /// The sweep removes it (not live for over 7 days).
    fn expired(&self, now: i64) -> bool {
        self.state() != "live" && now - self.since() > UNFINISHED_KEEP_MS
    }
}

const COLS: &str = "install_id, hostname, cf_id, status, ssl_status, verification_json, created_at, checked_at, live_at, proven_at, proof_misses, lapsed_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DomainRow> {
    let v: String = r.get(5)?;
    Ok(DomainRow {
        install: r.get(0)?,
        hostname: r.get(1)?,
        cf_id: r.get(2)?,
        status: r.get(3)?,
        ssl_status: r.get(4)?,
        verification: serde_json::from_str(&v).unwrap_or_default(),
        created_at: r.get(6)?,
        checked_at: r.get(7)?,
        live_at: r.get(8)?,
        proven_at: r.get(9)?,
        proof_misses: r.get(10)?,
        lapsed_at: r.get(11)?,
    })
}

fn verification_json(h: &Hostname) -> String {
    serde_json::to_string(&Verification { records: h.records.clone(), errors: h.errors.clone() })
        .unwrap_or_else(|_| "{}".into())
}

impl Db {
    fn tip_domain(&self, install: &str) -> rusqlite::Result<Option<DomainRow>> {
        self.c().query_row(&format!("SELECT {COLS} FROM tip_domains WHERE install_id = ?1"), [install], row).optional()
    }

    fn tip_domain_by_host(&self, host: &str) -> rusqlite::Result<Option<DomainRow>> {
        self.c().query_row(&format!("SELECT {COLS} FROM tip_domains WHERE hostname = ?1"), [host], row).optional()
    }

    /// Another install of the same subscription with an address.
    fn tip_domain_of_sub(&self, sub: &str, not_install: &str) -> rusqlite::Result<Option<DomainRow>> {
        self.c()
            .query_row(
                &format!("SELECT {COLS} FROM tip_domains WHERE sub_id = ?1 AND install_id != ?2 LIMIT 1"),
                params![sub, not_install],
                row,
            )
            .optional()
    }

    fn tip_domains_all(&self) -> rusqlite::Result<Vec<DomainRow>> {
        let c = self.c();
        let mut q = c.prepare(&format!("SELECT {COLS} FROM tip_domains"))?;
        let rows = q.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A new address; `claimed_at` is now, as Cloudflare was just asked.
    fn add_tip_domain(&self, install: &str, sub: &str, h: &Hostname, hostname: &str, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "INSERT INTO tip_domains (install_id, sub_id, hostname, cf_id, status, ssl_status, verification_json, created_at, updated_at, checked_at, claimed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?8, ?8)",
            params![install, sub, hostname, h.id, h.status, h.ssl_status, verification_json(h), now],
        )?;
        Ok(())
    }

    /// Keeps the subscription current (Pro restored on another subscription).
    fn set_tip_domain_sub(&self, install: &str, sub: &str) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE tip_domains SET sub_id = ?2 WHERE install_id = ?1 AND sub_id != ?2",
            params![install, sub],
        )?;
        Ok(())
    }

    /// Takes the right to check now: true when nobody did in the last `every_ms`. A claim,
    /// not a check: `checked_at` moves only when Cloudflare answers.
    fn claim_tip_domain_check(&self, install: &str, now: i64, every_ms: i64) -> rusqlite::Result<bool> {
        let n = self.c().execute(
            "UPDATE tip_domains SET claimed_at = ?2 WHERE install_id = ?1 AND claimed_at <= ?3",
            params![install, now, now - every_ms],
        )?;
        Ok(n == 1)
    }

    /// What Cloudflare answered, and when.
    fn set_tip_domain_status(&self, install: &str, cf_id: &str, h: &Hostname, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE tip_domains SET status = ?3, ssl_status = ?4, verification_json = ?5, updated_at = ?6, checked_at = ?6
             WHERE install_id = ?1 AND cf_id = ?2",
            params![install, cf_id, h.status, h.ssl_status, verification_json(h), now],
        )?;
        Ok(())
    }

    /// The TXT record was found (proven now, misses forgotten) or not (`count_miss`: one more
    /// miss for an address that was proven).
    fn set_tip_domain_proof(
        &self,
        install: &str,
        cf_id: &str,
        found: bool,
        count_miss: bool,
        now: i64,
    ) -> rusqlite::Result<()> {
        let c = self.c();
        if found {
            c.execute(
                "UPDATE tip_domains SET proven_at = ?3, proof_misses = 0 WHERE install_id = ?1 AND cf_id = ?2",
                params![install, cf_id, now],
            )?;
        } else if count_miss {
            c.execute(
                "UPDATE tip_domains SET proof_misses = proof_misses + 1 WHERE install_id = ?1 AND cf_id = ?2 AND proven_at IS NOT NULL",
                params![install, cf_id],
            )?;
        }
        Ok(())
    }

    fn mark_tip_domain_live(&self, install: &str, cf_id: &str, now: i64) -> rusqlite::Result<()> {
        self.c().execute(
            "UPDATE tip_domains SET live_at = ?3 WHERE install_id = ?1 AND cf_id = ?2",
            params![install, cf_id, now],
        )?;
        Ok(())
    }

    fn delete_tip_domain(&self, install: &str, cf_id: &str) -> rusqlite::Result<bool> {
        Ok(self.c().execute("DELETE FROM tip_domains WHERE install_id = ?1 AND cf_id = ?2", params![install, cf_id])?
            > 0)
    }

    /// Clears or sets the lapse mark; true when the address lapsed over `keep_ms` ago.
    fn mark_tip_domain_lapse(&self, install: &str, pro: bool, now: i64, keep_ms: i64) -> rusqlite::Result<bool> {
        let c = self.c();
        if pro {
            c.execute("UPDATE tip_domains SET lapsed_at = NULL WHERE install_id = ?1", [install])?;
            return Ok(false);
        }
        c.execute(
            "UPDATE tip_domains SET lapsed_at = ?2 WHERE install_id = ?1 AND lapsed_at IS NULL",
            params![install, now],
        )?;
        let lapsed: Option<i64> = c
            .query_row("SELECT lapsed_at FROM tip_domains WHERE install_id = ?1", [install], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(lapsed.is_some_and(|at| at < now - keep_ms))
    }
}

// ── Serving ──────────────────────────────────────────────────────────────────────────

fn host_of(origin: &str) -> Option<String> {
    url::Url::parse(origin).ok()?.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase())
}

/// Ferrocast's own domains: the fixed ones and those of the relay, tip and remote hosts
/// (each host and its registrable domain).
fn own_domains(st: &AppState) -> Vec<String> {
    let mut v: Vec<String> = OWN_DOMAINS.iter().map(|d| (*d).to_string()).collect();
    for origin in [Some(st.cfg.public_url.as_str()), st.cfg.tip_url.as_deref(), st.remote.cfg.origin.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(h) = host_of(origin)
            && h.parse::<std::net::IpAddr>().is_err()
            && !h.starts_with('[')
        {
            v.push(hostname::registrable(&h).unwrap_or_else(|| h.clone()));
            v.push(h);
        }
    }
    v.sort();
    v.dedup();
    v
}

/// Where a streamer's CNAME points: the tip host (the zone's fallback origin), else the relay.
fn cname_target(st: &AppState) -> String {
    st.cfg.tip_url.as_deref().and_then(host_of).or_else(|| host_of(&st.cfg.public_url)).unwrap_or_default()
}

fn lookup(st: &AppState, host: &str) -> Seen {
    let now = now_ms();
    let generation = {
        let c = st.tip_domains.cache();
        if let Some(s) = c.get(host, now) {
            return s;
        }
        c.generation
    };
    let seen = match st.db.tip_domain_by_host(host) {
        Ok(Some(r)) if r.serving() => Seen::Active(r.install),
        Ok(Some(r)) => {
            // Maybe it works by now: check (at most once a minute), answer as is.
            let st = st.clone();
            tokio::spawn(async move {
                refresh(&st, r, false).await;
            });
            Seen::Pending
        }
        Ok(None) => Seen::Unknown,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't look up a custom tip address");
            return Seen::Unknown;
        }
    };
    st.tip_domains.cache().put(host, seen.clone(), now, generation);
    seen
}

/// The page for an address that isn't connected. No scripts, nothing from the request.
fn not_set_up() -> Response {
    let body = "<h1>Not set up yet</h1><p class=lead>This address isn't connected to a tip page yet.</p>\
                <p class=fine>If it's yours, finish the steps under Tips › Your own address in Ferrocast. It can take a few minutes after the DNS records are added.</p>";
    let mut res = (StatusCode::NOT_FOUND, Html(super::shell("Not set up yet", body))).into_response();
    super::page_headers(&mut res);
    res
}

/// A request without a readable `Host`: nothing can be answered for it.
fn bad_host() -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Bad request: the Host header is missing or unreadable. Send the address you're visiting in Host.\n",
    )
        .into_response()
}

fn overlay_query(req: &Request) -> bool {
    req.uri()
        .query()
        .is_some_and(|q| url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "overlay" && v == "1"))
}

/// Runs before every route (see the module docs).
pub async fn route_custom_hosts(State(st): State<AppState>, req: Request, next: Next) -> Response {
    // HTTP/1.1 sends `Host`; HTTP/2 may send only the `:authority` (the URI's).
    let raw = match req.headers().get(header::HOST) {
        Some(h) => h.to_str().ok().map(str::to_string),
        None => req.uri().authority().map(|a| a.as_str().to_string()),
    };
    let Some(host) = raw.as_deref().and_then(hostname::normalize_host) else {
        return bad_host();
    };
    if hostname::is_ours(&host, st.tip_domains.own(&st)) {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    let get = req.method() == Method::GET || req.method() == Method::HEAD;
    match lookup(&st, &host) {
        // Fail closed: nobody's address, or one that isn't working, gets nothing else.
        Seen::Unknown | Seen::Pending => not_set_up(),
        Seen::Active(install) => {
            if get && path.starts_with("/t/a/") {
                return next.run(req).await;
            }
            let handle = super::config_for(&st, &install).map(|c| c.handle);
            match (get, path.as_str(), handle) {
                (true, "/", Some(h)) => super::page_html(&st, &h, Some("/goal")),
                (true, "/goal", Some(h)) => super::goal::page_for(&st, &h, overlay_query(&req), Some("/")),
                (_, "/" | "/goal", None) => not_set_up(),
                _ => super::not_found(),
            }
        }
    }
}

// ── Checks ───────────────────────────────────────────────────────────────────────────

/// The TXT values at `name`, remembered 30 s (errors aren't remembered).
async fn txt(st: &AppState, name: &str) -> Result<Vec<String>, String> {
    let now = now_ms();
    {
        let c = st.tip_domains.txt.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((exp, v)) = c.get(name)
            && *exp > now
        {
            return Ok(v.clone());
        }
    }
    let got = tokio::time::timeout(std::time::Duration::from_secs(6), st.tip_domains.dns.txt(name))
        .await
        .unwrap_or_else(|_| Err("timed out".into()))?;
    let mut c = st.tip_domains.txt.lock().unwrap_or_else(|p| p.into_inner());
    if c.len() >= TXT_CACHE_MAX && !c.contains_key(name) {
        c.retain(|_, (exp, _)| *exp > now);
        if c.len() >= TXT_CACHE_MAX
            && let Some(k) = c.iter().min_by_key(|(_, (exp, _))| *exp).map(|(k, _)| k.clone())
        {
            c.remove(&k);
        }
    }
    c.insert(name.to_string(), (now + TXT_TTL_MS, got.clone()));
    Ok(got)
}

/// Asks Cloudflare and reads the TXT record again when nobody did in the last minute;
/// returns the row as stored now. `sweep`: the hourly check, the only one that counts a
/// missing TXT record against a proven address.
async fn refresh(st: &AppState, r: DomainRow, sweep: bool) -> DomainRow {
    let (Some(cf), Some(keys)) = (st.tip_domains.cf.clone(), st.tip_domains.keys.clone()) else { return r };
    let now = now_ms();
    match st.db.claim_tip_domain_check(&r.install, now, REFRESH_MS) {
        Ok(true) => {}
        Ok(false) => return r,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't claim a custom address check");
            return r;
        }
    }
    let saved = match cf.get(&r.cf_id).await {
        Ok(h) => st.db.set_tip_domain_status(&r.install, &r.cf_id, &h, now),
        Err(CfError::NotFound) => {
            let gone = Hostname {
                status: "deleted".into(),
                ssl_status: "deleted".into(),
                errors: vec!["Cloudflare no longer has this address.".into()],
                ..Hostname::default()
            };
            st.db.set_tip_domain_status(&r.install, &r.cf_id, &gone, now)
        }
        // A zone problem is logged once by the client; the stored answer stays.
        Err(CfError::Zone(_)) => Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read a custom address from Cloudflare");
            Ok(())
        }
    };
    if let Err(e) = saved {
        tracing::warn!(error = %e, "couldn't save a custom address's status");
    }
    match txt(st, &proof::record_name(&r.hostname)).await {
        Ok(values) => {
            let want = keys.value(&r.install, &r.hostname);
            let found = values.iter().any(|v| v.trim() == want);
            if let Err(e) = st.db.set_tip_domain_proof(&r.install, &r.cf_id, found, sweep, now) {
                tracing::warn!(error = %e, "couldn't save a custom address's TXT check");
            }
        }
        Err(e) => tracing::warn!(error = %e, "couldn't read a custom address's TXT record"),
    }
    let mut fresh = st.db.tip_domain(&r.install).ok().flatten().filter(|f| f.cf_id == r.cf_id);
    if let Some(f) = fresh.as_mut()
        && f.state() == "live"
    {
        if let Err(e) = st.db.mark_tip_domain_live(&f.install, &f.cf_id, now) {
            tracing::warn!(error = %e, "couldn't save when a custom address worked");
        }
        f.live_at = Some(now);
    }
    st.tip_domains.forget(&r.hostname);
    fresh.unwrap_or(r)
}

// ── Routes ───────────────────────────────────────────────────────────────────────────

fn view(st: &AppState, r: &DomainRow) -> Value {
    let state = r.state();
    let host = hostname::registrable(&r.hostname)
        .and_then(|reg| r.hostname.strip_suffix(&reg).map(|l| l.trim_end_matches('.').to_string()))
        .unwrap_or_default();
    let proof_name = proof::record_name(&r.hostname);
    let proof_host = if host.is_empty() { proof_name.clone() } else { format!("_ferrocast.{host}") };
    let value = st.tip_domains.keys.as_ref().map(|k| k.value(&r.install, &r.hostname)).unwrap_or_default();
    let mut errors = Vec::new();
    match state {
        "live" => {}
        "proof_missing" => errors.push(format!(
            "The TXT record {proof_name} is gone, so this address stopped showing your tip page. Add it back."
        )),
        _ if !r.proven() => errors.push(format!("The TXT record {proof_name} isn't there yet.")),
        _ => {}
    }
    if state != "live" {
        errors.extend(r.verification.errors.iter().cloned());
    }
    cloudflare::dedupe(&mut errors);
    json!({
        "hostname": r.hostname,
        "hostname_unicode": hostname::to_unicode(&r.hostname),
        "status": state,
        "hostname_status": r.status,
        "ssl_status": r.ssl_status,
        "dns": [{ "type": "CNAME", "name": r.hostname, "host": host, "target": cname_target(st) }],
        "proof": { "type": "TXT", "name": proof_name, "host": proof_host, "value": value, "found": r.proven() },
        "verification": if state == "live" { Vec::new() } else { r.verification.records.clone() },
        "errors": errors,
        "page_url": format!("https://{}", r.hostname),
        "goal_url": format!("https://{}/goal", r.hostname),
        "created_at": r.created_at,
        "checked_at": r.checked_at,
        "expires_at": (state != "live").then(|| r.since() + UNFINISHED_KEEP_MS),
        "lapsed_at": r.lapsed_at,
        "delete_at": r.lapsed_at.map(|at| at + LAPSED_KEEP_MS),
    })
}

fn no_domain() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "You haven't added your own address yet.")
        .hint("Add one under Tips › Your own address.")
}

/// What a Cloudflare error means for a streamer's change: our zone or token is wrong (503,
/// nothing they can do), or Cloudflare didn't answer (502, try again).
fn cf_error(e: &CfError) -> ApiError {
    match e {
        CfError::Zone(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "domains_unavailable",
            "Custom addresses aren't working on this relay right now, so nothing changed.",
        )
        .hint("Your tip page keeps working at its usual link. Try again later."),
        _ => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "cloudflare_unavailable",
            "Cloudflare didn't answer, so nothing changed.",
        )
        .hint("Try again in a few minutes."),
    }
}

/// `GET /v1/pro/tips/domain`: the address with its status (checked again at most once a minute).
pub async fn get_domain(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    st.tip_domains.cf()?;
    let r = st.db.tip_domain(&ctx.install).map_err(ApiError::db)?.ok_or_else(no_domain)?;
    st.db.set_tip_domain_sub(&ctx.install, &ctx.sub_id).map_err(ApiError::db)?;
    let r = refresh(&st, r, false).await;
    Ok(axum::Json(view(&st, &r)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct DomainBody {
    hostname: String,
}

fn invalid_hostname(m: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_hostname", m)
        .hint("Use a subdomain you own, like tips.yourchannel.com.")
}

fn taken(host: &str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "domain_taken", format!("{host} is already in use."))
        .hint("If it's yours, remove it from the other Ferrocast first, or pick another subdomain.")
}

/// Whether `_ferrocast.<host>` holds `install`'s proof right now (asked fresh, not from the
/// cache, since the streamer may have just added it; a DNS error is "no").
async fn proof_published(st: &AppState, keys: &ProofKeys, install: &str, host: &str) -> bool {
    let name = proof::record_name(host);
    st.tip_domains.txt.lock().unwrap_or_else(|p| p.into_inner()).remove(&name);
    match txt(st, &name).await {
        Ok(values) => {
            let want = keys.value(install, host);
            values.iter().any(|v| v.trim() == want)
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read a TXT proof for an existing custom hostname");
            false
        }
    }
}

/// 409 `domain_taken` for a hostname Cloudflare already has that this install can't prove:
/// the usual error, plus the TXT record that would prove it (`proof`), so the dashboard can
/// show it before the record exists.
fn taken_elsewhere(keys: &ProofKeys, install: &str, host: &str) -> Response {
    let name = proof::record_name(host);
    let label = hostname::registrable(host)
        .and_then(|reg| host.strip_suffix(&reg).map(|l| l.trim_end_matches('.').to_string()))
        .filter(|l| !l.is_empty());
    let record_host = label.map_or_else(|| name.clone(), |l| format!("_ferrocast.{l}"));
    (
        StatusCode::CONFLICT,
        axum::Json(json!({
            "error": {
                "code": "domain_taken",
                "message": "That address is set up somewhere else on Cloudflare.",
                "hint": "If it's yours, add the TXT record below where you manage your domain, wait a few minutes, then try again. If it's on another Cloudflare account or service, remove it there first.",
            },
            "proof": { "type": "TXT", "name": name, "host": record_host, "value": keys.value(install, host) },
        })),
    )
        .into_response()
}

/// `PUT /v1/pro/tips/domain {hostname}` → 201 with the DNS records to add (200 when it's the
/// address this tip page already has).
pub async fn put_domain(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<Body<DomainBody>, ApiError>,
) -> Result<Response, ApiError> {
    // The gate first (401/402), then whether this relay has the feature, then the body.
    let ctx = require_pro(&st, &headers).await?;
    let (cf, keys) = st.tip_domains.cf()?;
    let Body(b) = body?;
    let checked = hostname::check(&b.hostname, st.tip_domains.own(&st)).map_err(|e| match e {
        hostname::HostErr::Invalid(m) => invalid_hostname(m),
        hostname::HostErr::Apex(reg) => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "apex_not_supported",
            format!("{reg} is a main domain. Use a subdomain of it, like tips.{reg}."),
        )
        .hint("A main domain can't point at another site with a CNAME record; a subdomain can."),
    })?;
    let host = checked.hostname;
    if st.db.tip_config(&ctx.install).map_err(ApiError::db)?.is_none() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "no_tip_page", "Create your tip page first.")
            .hint("Your own address shows your tip page, so it needs one."));
    }
    let _one_at_a_time = st.tip_domains.writes.lock().await;
    if let Some(r) = st.db.tip_domain(&ctx.install).map_err(ApiError::db)? {
        if r.hostname == host {
            st.db.set_tip_domain_sub(&ctx.install, &ctx.sub_id).map_err(ApiError::db)?;
            let r = refresh(&st, r, false).await;
            return Ok(axum::Json(view(&st, &r)).into_response());
        }
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "domain_limit",
            format!("This tip page already uses {}.", r.hostname),
        )
        .hint("Remove it first, then add the new address."));
    }
    let now = now_ms();
    // Someone else's claim that never worked (or stopped) over a day ago can be taken over.
    let stale = match st.db.tip_domain_by_host(&host).map_err(ApiError::db)? {
        Some(other) if other.stale(now) => Some(other),
        Some(_) => return Err(taken(&host)),
        None => None,
    };
    if let Some(r) = st.db.tip_domain_of_sub(&ctx.sub_id, &ctx.install).map_err(ApiError::db)? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "domain_limit",
            format!("Your Pro already has an address: {} (on another PC).", r.hostname),
        )
        .hint("Pro includes one address. Remove that one on the other PC first."));
    }
    let all = st.db.tip_domains_all().map_err(ApiError::db)?;
    let stale_install = stale.as_ref().map(|r| r.install.as_str());
    let used = all.iter().filter(|r| r.counts(now) && Some(r.install.as_str()) != stale_install).count();
    if used >= st.tip_domains.cap {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "domains_full",
            "Custom addresses are full on Ferrocast right now.",
        )
        .hint("Your tip page keeps working at its usual link. Try again in a few days."));
    }
    st.db
        .try_take(&ctx.sub_id, CHANGES_PER_DAY.name, CHANGES_PER_DAY.period, 1, CHANGES_PER_DAY.limit)
        .map_err(|e| e.into_api("tip_domain_quota", "That's enough address changes for today."))?;
    let give_back = || {
        let _ = st.db.give_back(&ctx.sub_id, CHANGES_PER_DAY.name, CHANGES_PER_DAY.period, 1);
    };
    if let Some(old) = stale {
        match cf.delete(&old.cf_id).await {
            Ok(()) | Err(CfError::NotFound) => {}
            Err(e) => {
                tracing::warn!(error = %e, "couldn't remove a stale custom tip address");
                give_back();
                return Err(cf_error(&e));
            }
        }
        if let Err(e) = st.db.delete_tip_domain(&old.install, &old.cf_id) {
            give_back();
            return Err(ApiError::db(e));
        }
        st.tip_domains.forget(&old.hostname);
        tracing::info!("a custom tip address that never worked was claimed by another tip page");
    }
    let mut adopted = false;
    let made = match cf.create(&host).await {
        Ok(h) => h,
        // In the zone but not here (a crash between the two, or someone's claim we removed):
        // adopted only when this install's TXT proof is published right now, which only the
        // domain's DNS owner can do. Otherwise 409 with the record to add.
        Err(CfError::Duplicate) => match cf.find(&host).await {
            Ok(Some(h)) if h.hostname == host && proof_published(&st, &keys, &ctx.install, &host).await => {
                tracing::info!("an existing custom hostname was adopted with a matching TXT proof");
                adopted = true;
                h
            }
            Ok(_) => {
                give_back();
                return Ok(taken_elsewhere(&keys, &ctx.install, &host));
            }
            Err(e) => {
                tracing::warn!(error = %e, "couldn't look up a duplicate custom tip address");
                give_back();
                return Err(cf_error(&e));
            }
        },
        Err(CfError::Refused(m)) => {
            tracing::info!(cloudflare = %m, "Cloudflare refused a custom tip address");
            give_back();
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_hostname",
                "Cloudflare won't take this address.",
            )
            .hint("Check it's spelled right and is a subdomain you own, or try another one."));
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't add a custom tip address");
            give_back();
            return Err(cf_error(&e));
        }
    };
    if let Err(e) = st.db.add_tip_domain(&ctx.install, &ctx.sub_id, &made, &host, now_ms()) {
        // Never leave a hostname in the zone that nothing here knows about.
        if let Err(ce) = cf.delete(&made.id).await {
            tracing::warn!(error = %ce, "couldn't take back a custom hostname after a failed save");
        }
        give_back();
        return Err(ApiError::db(e));
    }
    if adopted && let Err(e) = st.db.set_tip_domain_proof(&ctx.install, &made.id, true, false, now_ms()) {
        tracing::warn!(error = %e, "couldn't save an adopted custom address's TXT check");
    }
    st.tip_domains.forget(&host);
    let r = st.db.tip_domain(&ctx.install).map_err(ApiError::db)?.ok_or_else(no_domain)?;
    Ok((StatusCode::CREATED, axum::Json(view(&st, &r))).into_response())
}

/// `DELETE /v1/pro/tips/domain` → 204: gone from Cloudflare and here (the tip page stays at
/// its usual link).
pub async fn delete_domain(State(st): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let ctx = require_pro(&st, &headers).await?;
    let (cf, _) = st.tip_domains.cf()?;
    let _one_at_a_time = st.tip_domains.writes.lock().await;
    let r = st.db.tip_domain(&ctx.install).map_err(ApiError::db)?.ok_or_else(no_domain)?;
    match cf.delete(&r.cf_id).await {
        Ok(()) | Err(CfError::NotFound) => {}
        Err(e) => {
            tracing::warn!(error = %e, "couldn't remove a custom tip address");
            return Err(cf_error(&e));
        }
    }
    st.db.delete_tip_domain(&ctx.install, &r.cf_id).map_err(ApiError::db)?;
    st.tip_domains.forget(&r.hostname);
    // Counted, but never refused: a streamer can always take their address down.
    let _ = st.db.try_take(&ctx.sub_id, CHANGES_PER_DAY.name, CHANGES_PER_DAY.period, 1, CHANGES_PER_DAY.limit);
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ── Sweep ────────────────────────────────────────────────────────────────────────────

/// Hourly (from `tips::sweep`): addresses of installs without Pro are marked lapsed and,
/// 7 days on, deleted from Cloudflare and here; every other address is checked again
/// (Cloudflare and the TXT record, live ones too), and one that isn't live 7 days after it
/// was added or last worked is removed.
pub async fn sweep(st: &AppState) {
    let rows = match st.db.tip_domains_all() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list custom tip addresses");
            return;
        }
    };
    for r in rows {
        let pro = super::install_is_pro(st, &r.install);
        match st.db.mark_tip_domain_lapse(&r.install, pro, now_ms(), LAPSED_KEEP_MS) {
            Ok(true) => remove(st, &r, Why::Lapsed).await,
            Ok(false) => {
                let f = refresh(st, r, true).await;
                if f.expired(now_ms()) {
                    remove(st, &f, Why::Unfinished).await;
                }
            }
            Err(e) => tracing::warn!(error = %e, "couldn't sweep a custom tip address"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    Lapsed,
    Unfinished,
}

async fn remove(st: &AppState, r: &DomainRow, why: Why) {
    let Some(cf) = st.tip_domains.cf.clone() else {
        tracing::warn!("a custom tip address waits to be removed: CF_SAAS_TOKEN / CF_ZONE_ID aren't set");
        return;
    };
    let _one_at_a_time = st.tip_domains.writes.lock().await;
    // As stored now (it may have gone live, or been removed, since the list was read).
    let Some(r) = st.db.tip_domain(&r.install).ok().flatten().filter(|f| f.cf_id == r.cf_id) else { return };
    if why == Why::Unfinished && !r.expired(now_ms()) {
        return;
    }
    match cf.delete(&r.cf_id).await {
        Ok(()) | Err(CfError::NotFound) => {
            if let Err(e) = st.db.delete_tip_domain(&r.install, &r.cf_id) {
                tracing::warn!(error = %e, "couldn't delete a custom tip address");
            }
            st.tip_domains.forget(&r.hostname);
            match why {
                Why::Lapsed => tracing::info!("a lapsed custom tip address was removed"),
                Why::Unfinished => tracing::info!("a custom tip address that never worked in 7 days was removed"),
            }
        }
        // Kept: the next sweep tries again.
        Err(e) => tracing::warn!(error = %e, "couldn't remove a custom tip address from Cloudflare"),
    }
}
