//! The per-claim proof for a custom address (API.md §9.2, "Your own address"): a TXT record
//! `_ferrocast.<hostname>` holding a value only this relay can make for this install and
//! this hostname. Cloudflare activating a hostname only shows its CNAME points at us; a
//! CNAME left dangling by its owner would let anyone claim it. The TXT record shows the
//! person claiming it controls the domain's DNS now.
//!
//! The value is `ferrocast-proof-` + 32 hex digits of HMAC-SHA256(key, install ‖ hostname),
//! the key derived (HKDF, its own label) from `RELAY_VAULT_KEY`. The relay reads the record
//! over DNS-over-HTTPS ([`DohResolver`], Cloudflare's JSON API, 5 s) behind [`ProofDns`]
//! ([`FakeDns`] in tests).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use hmac::{Hmac, Mac};
use rsa::sha2::Sha256;
use serde_json::Value;

/// The key made from the relay's secret for the TXT values.
#[derive(Clone)]
pub struct ProofKeys {
    proof: [u8; 32],
}

impl std::fmt::Debug for ProofKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProofKeys(<redacted>)")
    }
}

fn hex32(key: &[u8; 32], parts: &[&str]) -> String {
    #[allow(clippy::expect_used)] // HMAC takes a key of any length
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key");
    for p in parts {
        // Length-prefixed, so ("ab", "c") and ("a", "bc") never collide.
        mac.update(&(p.len() as u64).to_be_bytes());
        mac.update(p.as_bytes());
    }
    mac.finalize().into_bytes().iter().take(16).map(|b| format!("{b:02x}")).collect()
}

impl ProofKeys {
    /// From any secret of 16 bytes or more (the decoded `RELAY_VAULT_KEY`).
    pub fn new(secret: &[u8]) -> Option<ProofKeys> {
        (secret.len() >= 16).then(|| ProofKeys { proof: crate::bot::derive_key(secret, "tip domain txt proof v1") })
    }

    /// The TXT value `install` must publish for `hostname`.
    pub fn value(&self, install: &str, hostname: &str) -> String {
        format!("ferrocast-proof-{}", hex32(&self.proof, &[install, hostname]))
    }
}

/// Where the record goes.
pub fn record_name(hostname: &str) -> String {
    format!("_ferrocast.{hostname}")
}

pub type DnsFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>>;

/// Reads TXT records. `Ok(vec![])` when the name has none (or doesn't exist); `Err` when the
/// answer couldn't be had (nothing is concluded from that).
pub trait ProofDns: Send + Sync + std::fmt::Debug {
    fn txt<'a>(&'a self, name: &'a str) -> DnsFuture<'a>;
}

pub const DOH_URL: &str = "https://cloudflare-dns.com/dns-query";

/// Cloudflare's DNS-over-HTTPS JSON API.
#[derive(Debug)]
pub struct DohResolver {
    http: reqwest::Client,
    url: String,
}

impl DohResolver {
    pub fn new(url: &str) -> DohResolver {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap_or_default();
        DohResolver { http, url: url.to_string() }
    }
}

impl Default for DohResolver {
    fn default() -> Self {
        DohResolver::new(DOH_URL)
    }
}

/// The strings of one TXT answer's `data` (`"\"a\" \"b\""` → `ab`: a long value split in
/// 255-byte pieces is one value).
pub fn txt_data(data: &str) -> String {
    let d = data.trim();
    if !d.starts_with('"') {
        return d.to_string();
    }
    let mut out = String::new();
    let mut inside = false;
    let mut escaped = false;
    for c in d.chars() {
        match (inside, escaped, c) {
            (true, true, c) => {
                out.push(c);
                escaped = false;
            }
            (true, false, '\\') => escaped = true,
            (_, false, '"') => inside = !inside,
            (true, false, c) => out.push(c),
            _ => {}
        }
    }
    out
}

/// Reads a `application/dns-json` answer: NOERROR's TXT answers, nothing for NXDOMAIN.
pub fn parse_doh(v: &Value) -> Result<Vec<String>, String> {
    match v["Status"].as_i64() {
        Some(0) => Ok(v["Answer"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["type"].as_i64() == Some(16))
            .filter_map(|a| a["data"].as_str().map(txt_data))
            .collect()),
        Some(3) => Ok(Vec::new()),
        Some(n) => Err(format!("DNS answered status {n}")),
        None => Err("an unreadable DNS answer".into()),
    }
}

impl ProofDns for DohResolver {
    fn txt<'a>(&'a self, name: &'a str) -> DnsFuture<'a> {
        Box::pin(async move {
            let res = self
                .http
                .get(&self.url)
                .query(&[("name", name), ("type", "TXT")])
                .header(reqwest::header::ACCEPT, "application/dns-json")
                .send()
                .await
                .map_err(|e| if e.is_timeout() { "timed out".to_string() } else { "no answer".to_string() })?;
            if !res.status().is_success() {
                return Err(format!("DNS-over-HTTPS answered {}", res.status().as_u16()));
            }
            let v: Value = res.json().await.map_err(|_| "an unreadable DNS answer".to_string())?;
            parse_doh(&v)
        })
    }
}

/// DNS for tests: records set by the test; any lookup can be made to fail.
#[derive(Debug, Default)]
pub struct FakeDns {
    records: Mutex<HashMap<String, Vec<String>>>,
    fail: Mutex<bool>,
    calls: Mutex<usize>,
}

impl FakeDns {
    pub fn new() -> FakeDns {
        FakeDns::default()
    }

    /// Sets (or with an empty list, removes) the TXT values at `name`.
    pub fn set(&self, name: &str, values: &[&str]) {
        self.records
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_string(), values.iter().map(|v| (*v).to_string()).collect());
    }

    pub fn fail(&self, on: bool) {
        *self.fail.lock().unwrap_or_else(|p| p.into_inner()) = on;
    }

    /// Lookups so far.
    pub fn calls(&self) -> usize {
        *self.calls.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ProofDns for FakeDns {
    fn txt<'a>(&'a self, name: &'a str) -> DnsFuture<'a> {
        Box::pin(async move {
            *self.calls.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            if *self.fail.lock().unwrap_or_else(|p| p.into_inner()) {
                return Err("down".into());
            }
            Ok(self.records.lock().unwrap_or_else(|p| p.into_inner()).get(name).cloned().unwrap_or_default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn values_are_per_install_and_hostname_and_keyed_to_the_relay() {
        let k = ProofKeys::new(&[7u8; 32]).unwrap();
        let v = k.value("inst1", "tips.mychannel.com");
        assert!(v.starts_with("ferrocast-proof-") && v.len() == 16 + 32, "{v}");
        assert_eq!(v, k.value("inst1", "tips.mychannel.com"), "stable");
        assert_ne!(v, k.value("inst2", "tips.mychannel.com"));
        assert_ne!(v, k.value("inst1", "live.mychannel.com"));
        assert_ne!(k.value("inst1x", "y"), k.value("inst1", "xy"), "length-prefixed");
        let other = ProofKeys::new(&[8u8; 32]).unwrap();
        assert_ne!(v, other.value("inst1", "tips.mychannel.com"), "another relay's key");
        assert!(ProofKeys::new(b"short").is_none());
        assert!(!format!("{k:?}").contains('7'));
        assert_eq!(record_name("tips.mychannel.com"), "_ferrocast.tips.mychannel.com");
    }

    #[test]
    fn doh_answers_are_read() {
        let ok = json!({ "Status": 0, "Answer": [
            { "name": "_ferrocast.tips.mychannel.com", "type": 16, "TTL": 300, "data": "\"ferrocast-proof-abc\"" },
            { "name": "_ferrocast.tips.mychannel.com", "type": 16, "TTL": 300, "data": "\"part one \" \"and \\\"two\\\"\"" },
            { "name": "x", "type": 5, "TTL": 300, "data": "target.example." }
        ]});
        assert_eq!(parse_doh(&ok), Ok(vec!["ferrocast-proof-abc".to_string(), "part one and \"two\"".to_string()]));
        assert_eq!(parse_doh(&json!({ "Status": 3 })), Ok(vec![]), "NXDOMAIN: no record");
        assert_eq!(parse_doh(&json!({ "Status": 0 })), Ok(vec![]));
        assert!(parse_doh(&json!({ "Status": 2 })).is_err(), "SERVFAIL concludes nothing");
        assert!(parse_doh(&json!(null)).is_err());
        assert_eq!(txt_data("plain"), "plain");
    }

    #[tokio::test]
    async fn the_resolver_asks_for_json_txt_answers() {
        use std::sync::Arc;
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let rec = seen.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let rec = rec.clone();
            async move {
                let accept = req.headers().get("accept").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                rec.lock().unwrap().push((req.uri().query().unwrap_or("").to_string(), accept));
                axum::Json(json!({ "Status": 0, "Answer": [{ "type": 16, "data": "\"v1\"" }] }))
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/dns-query", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        let r = DohResolver::new(&url);
        assert_eq!(r.txt("_ferrocast.tips.mychannel.com").await, Ok(vec!["v1".to_string()]));
        let s = seen.lock().unwrap().clone();
        assert_eq!(s[0].0, "name=_ferrocast.tips.mychannel.com&type=TXT");
        assert_eq!(s[0].1, "application/dns-json");
        assert!(DohResolver::new("http://127.0.0.1:9/dns-query").txt("x").await.is_err());
    }
}
