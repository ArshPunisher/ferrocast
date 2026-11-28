//! The plugin catalog (OBS-COMPETE.md §4 step 5, owner's pick 11 Nov): a list of plugins kept in
//! a GitHub repo, signed with the owner's catalog key (minisign: `cargo tauri signer`, its own
//! key, not the app updater's), browsed in Settings › Plugins and installed in one click.
//!
//! Nothing in it is trusted until checked: the list's signature against the key built into the
//! app; each plugin's file against the size and SHA-256 the signed list gives; the plugin it
//! holds against the id the list says; then the usual install checks (`plugins::install`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;

use crate::plugins::{MAX_ZIP, PluginInfo};
use crate::{Engine, EngineError};

/// Where the catalog is (its `.sig` beside it).
pub const CATALOG_URL: &str = "https://raw.githubusercontent.com/ArshPunisher/ferrocast-plugins/main/catalog.json";

/// The owner's catalog public key (base64 of a minisign public key, as `cargo tauri signer
/// generate` writes it; made 11 Nov 2026, its private half kept off GitHub). Empty would close
/// the catalog ("opens soon").
pub const CATALOG_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDI4QTNGQTgzQjgxRjc3QTkKUldTcGR4KzRnL3FqS0tEYmlsQlJJNlFhN3hXQTAzaWFvVUpudmRQaC90a1h5M1UweThwY0hxNVEK";

/// Hosts a plugin's file may come from (a release asset on GitHub redirects to its storage).
const HOSTS: [&str; 4] = [
    "github.com",
    "raw.githubusercontent.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// The biggest catalog read.
const MAX_LIST: usize = 1 << 20;
/// How long a read catalog is kept.
const KEEP: Duration = Duration::from_secs(600);

/// Where the catalog comes from and the key it's checked with (the tests' own, in tests).
#[derive(Debug, Clone)]
pub struct Source {
    pub url: String,
    pub key: String,
    /// Plain http to this machine allowed (tests' local server only).
    pub local: bool,
}

#[cfg(any(test, feature = "test-catalog"))]
static SOURCE: Mutex<Option<Source>> = Mutex::new(None);
static KEPT: Mutex<Option<(Instant, Vec<Entry>)>> = Mutex::new(None);

/// The tests' catalog in place of the real one (not in a release build).
#[cfg(any(test, feature = "test-catalog"))]
pub fn set_source(s: Source) {
    *SOURCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
    *KEPT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn source() -> Source {
    #[cfg(any(test, feature = "test-catalog"))]
    if let Some(s) = SOURCE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return s;
    }
    Source { url: CATALOG_URL.into(), key: CATALOG_KEY.into(), local: false }
}

/// A plugin in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    pub kind: String,
    #[serde(default)]
    pub actions: Vec<String>,
    /// Its file: an https address on GitHub, or a path beside the catalog.
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

fn refused(status: u16, code: &'static str, message: impl Into<String>, hint: &str) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

/// What a verified signature's trusted comment says (it is signed too): when it was signed and
/// which file name it was signed under. The file name keeps one catalog's signature from
/// passing as another's, even with the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedFor {
    pub timestamp: i64,
    pub file: String,
}

/// Checks `list` against its minisign signature (`sig`, base64 as `cargo tauri signer sign`
/// writes it) and the key (base64 too), and reads the signed trusted comment
/// (`timestamp:<epoch>\tfile:<name>`). The comment is required.
pub fn verify_signed(list: &[u8], sig: &str, key: &str) -> Result<SignedFor, String> {
    let b64 = |s: &str| {
        base64::engine::general_purpose::STANDARD.decode(s.trim()).ok().and_then(|b| String::from_utf8(b).ok())
    };
    let key = b64(key).ok_or("the catalog key isn't readable")?;
    let key = minisign_verify::PublicKey::decode(&key).map_err(|e| format!("the catalog key: {e}"))?;
    let sig = b64(sig).ok_or("the catalog's signature isn't readable")?;
    let sig = minisign_verify::Signature::decode(&sig).map_err(|e| format!("the catalog's signature: {e}"))?;
    key.verify(list, &sig, false).map_err(|_| "the catalog's signature doesn't match".to_string())?;
    let mut timestamp = None;
    let mut file = None;
    for part in sig.trusted_comment().split('\t') {
        if let Some(t) = part.strip_prefix("timestamp:") {
            timestamp = t.trim().parse::<i64>().ok();
        } else if let Some(f) = part.strip_prefix("file:") {
            file = Some(f.trim().to_string());
        }
    }
    match (timestamp, file) {
        (Some(timestamp), Some(file)) => Ok(SignedFor { timestamp, file }),
        _ => Err("the catalog's signature has no readable timestamp and file name".into()),
    }
}

/// The file name the plugin catalog is signed under.
pub const PLUGIN_CATALOG_FILE: &str = "catalog.json";

/// Checks the plugin catalog: its signature, and that it was signed as the plugin catalog (a
/// Store catalog's signature, made with the same key, is refused here).
pub fn verify(list: &[u8], sig: &str, key: &str) -> Result<(), String> {
    let s = verify_signed(list, sig, key)?;
    if s.file != PLUGIN_CATALOG_FILE {
        return Err("the signature is for another catalog".into());
    }
    Ok(())
}

/// The address of an entry's file: an https one on GitHub, or a plain path beside the catalog
/// (no `..`, no other scheme). None when it's neither.
pub fn file_url(entry: &str, src: &Source) -> Option<reqwest::Url> {
    let base = reqwest::Url::parse(&src.url).ok()?;
    let u = if entry.contains("://") {
        reqwest::Url::parse(entry).ok()?
    } else {
        let plain = !entry.is_empty()
            && !entry.starts_with('/')
            && !entry.contains('\\')
            && entry.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
            && entry.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c));
        if !plain {
            return None;
        }
        base.join(entry).ok()?
    };
    allowed(&u, src).then_some(u)
}

fn allowed(u: &reqwest::Url, src: &Source) -> bool {
    let host = u.host_str().unwrap_or_default();
    if !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    let same_local_port = || {
        // No readable catalog address: no local redirects at all.
        let Ok(base) = reqwest::Url::parse(&src.url) else {
            return false;
        };
        base.host_str() == Some(host) && base.port_or_known_default() == u.port_or_known_default()
    };
    match u.scheme() {
        "https" => {
            HOSTS.contains(&host)
                || host == "ferrocast.clearcraftlabs.com"
                || (src.local && host == "127.0.0.1" && same_local_port())
        }
        "http" => src.local && host == "127.0.0.1" && same_local_port(),
        _ => false,
    }
}

/// The entries of a checked catalog that make sense; the rest left out (and logged).
pub fn entries(list: &[u8], src: &Source) -> Result<Vec<Entry>, String> {
    #[derive(Deserialize)]
    struct List {
        plugins: Vec<Value>,
    }
    let l: List = serde_json::from_slice(list).map_err(|e| format!("the catalog isn't readable: {e}"))?;
    let mut out: Vec<Entry> = Vec::new();
    for v in l.plugins {
        let Ok(e) = serde_json::from_value::<Entry>(v) else {
            tracing::warn!("a plugin catalog entry isn't readable: left out");
            continue;
        };
        let ok = crate::plugins::good_id(&e.id)
            && ["effect", "logic", "panel"].contains(&e.kind.as_str())
            && e.sha256.len() == 64
            && e.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            && (1..=MAX_ZIP).contains(&e.size)
            && file_url(&e.url, src).is_some()
            && !out.iter().any(|o| o.id == e.id);
        if ok {
            out.push(e);
        } else {
            tracing::warn!(id = %e.id, "a plugin catalog entry doesn't make sense: left out");
        }
    }
    Ok(out)
}

/// Whether `new` is a later version than `old`, by semver order: the numbers dot by dot, a
/// release after its own prerelease, prerelease parts compared one by one (numbers as numbers,
/// words after numbers). Build metadata (`+x`) doesn't order. A version that can't be read is
/// never "newer": a garbled or replayed one is not offered as an update.
pub fn newer(new: &str, old: &str) -> bool {
    use std::cmp::Ordering;
    let (Some((a, ap)), Some((b, bp))) = (parse_version(new), parse_version(old)) else { return false };
    let len = a.len().max(b.len());
    for i in 0..len {
        match cmp_digits(a.get(i).map_or("0", String::as_str), b.get(i).map_or("0", String::as_str)) {
            Ordering::Equal => {}
            o => return o == Ordering::Greater,
        }
    }
    match (ap.is_empty(), bp.is_empty()) {
        (true, true) => false,
        (true, false) => true,
        (false, true) => false,
        (false, false) => {
            for (x, y) in ap.iter().zip(bp.iter()) {
                let o = match (is_digits(x), is_digits(y)) {
                    (true, true) => cmp_digits(x, y),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => x.cmp(y),
                };
                if o != Ordering::Equal {
                    return o == Ordering::Greater;
                }
            }
            ap.len() > bp.len()
        }
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Two decimal numbers of any length: by length once leading zeros are gone, then digit by digit.
fn cmp_digits(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// A version as `newer` reads it: its numbers, and its prerelease identifiers (build metadata is
/// checked and left out). One grammar for everything that takes a version: dotted numbers, then
/// an optional `-prerelease` of dotted `[0-9A-Za-z-]` identifiers, then an optional `+build`.
fn parse_version(v: &str) -> Option<(Vec<String>, Vec<String>)> {
    let (rest, build) = match v.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (v, None),
    };
    if let Some(b) = build
        && (b.is_empty()
            || !b.split('.').all(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')))
    {
        return None;
    }
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let nums: Vec<String> = core.split('.').map(str::to_string).collect();
    if nums.is_empty() || !nums.iter().all(|n| is_digits(n)) {
        return None;
    }
    let pre: Vec<String> = match pre {
        Some(p) => p.split('.').map(str::to_string).collect(),
        None => Vec::new(),
    };
    if pre.iter().any(|id| id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')) {
        return None;
    }
    Some((nums, pre))
}

/// Whether a version is one `newer` can compare (the Store admits no other).
pub fn readable_version(v: &str) -> bool {
    parse_version(v).is_some()
}

/// A client that follows redirects only to the allowed hosts, over https (or the tests' local
/// server).
pub(crate) fn client(src: &Source) -> Result<reqwest::Client, EngineError> {
    let base_url = src.url.clone();
    let local = src.local;
    let policy = reqwest::redirect::Policy::custom(move |a| {
        let s = Source { url: base_url.clone(), key: String::new(), local };
        if a.previous().len() >= 5 || !allowed(a.url(), &s) { a.stop() } else { a.follow() }
    });
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(policy)
        .build()
        .map_err(|e| EngineError::Internal(e.to_string()))
}

/// Reads at most `max` bytes from `url`; more is refused (never all read).
pub(crate) async fn fetch_capped(
    http: &reqwest::Client,
    url: reqwest::Url,
    max: usize,
) -> Result<Vec<u8>, EngineError> {
    let unreachable = |e: String| {
        refused(
            502,
            "catalog_unreachable",
            format!("The catalog server didn't answer: {e}"),
            "Check the internet and try again.",
        )
    };
    let mut r = http.get(url).send().await.map_err(|e| unreachable(e.to_string()))?;
    if !r.status().is_success() {
        return Err(unreachable(format!("it said {}", r.status())));
    }
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(|e| unreachable(e.to_string()))? {
        if out.len() + chunk.len() > max {
            return Err(refused(
                413,
                "catalog_too_big",
                format!("That's bigger than the {} KB it may be.", max / 1024),
                "The catalog is wrong; nothing was installed.",
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn fetch(http: &reqwest::Client, url: reqwest::Url, max: usize) -> Result<Vec<u8>, EngineError> {
    fetch_capped(http, url, max).await
}

pub(crate) fn hex_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex(b: &[u8]) -> String {
    hex_bytes(b)
}

impl Engine {
    /// The catalog's plugins, checked, kept 10 minutes (`fresh` reads it again).
    async fn catalog_entries(&self, fresh: bool) -> Result<Vec<Entry>, EngineError> {
        if !fresh
            && let Some((at, l)) = KEPT.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
            && at.elapsed() < KEEP
        {
            return Ok(l.clone());
        }
        let src = source();
        if src.key.trim().is_empty() {
            return Err(refused(
                503,
                "catalog_not_open",
                "The plugin catalog isn't open yet.",
                "Add a plugin from its file for now.",
            ));
        }
        let url = reqwest::Url::parse(&src.url).map_err(|e| EngineError::Internal(e.to_string()))?;
        let mut sig_url = url.clone();
        sig_url.set_path(&format!("{}.sig", url.path()));
        let http = client(&src)?;
        let list = fetch(&http, url, MAX_LIST).await?;
        let sig = fetch(&http, sig_url, 4096).await?;
        let sig = String::from_utf8(sig).unwrap_or_default();
        if let Err(why) = verify(&list, &sig, &src.key) {
            tracing::warn!(%why, "the plugin catalog was refused");
            return Err(refused(
                502,
                "catalog_not_signed",
                "The plugin catalog isn't signed by Ferrocast, so it wasn't used.",
                "Try again later; nothing was installed.",
            ));
        }
        let l = entries(&list, &src).map_err(|e| refused(502, "catalog_unreadable", e, "Try again later."))?;
        *KEPT.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), l.clone()));
        Ok(l)
    }

    /// `GET /plugins/catalog` → `{ plugins: [entry + { installed, update }] }`.
    pub async fn plugin_catalog(&self, fresh: bool) -> Result<Value, EngineError> {
        let list = self.catalog_entries(fresh).await?;
        let have = self.plugins().await;
        let shown: Vec<Value> = list
            .into_iter()
            .map(|e| {
                let installed = have.iter().find(|p| p.id == e.id).map(|p| p.version.clone());
                let update = installed.as_deref().is_some_and(|v| newer(&e.version, v));
                let mut v = serde_json::to_value(&e).unwrap_or(Value::Null);
                v["installed"] = json!(installed);
                v["update"] = json!(update);
                v
            })
            .collect();
        Ok(json!({ "plugins": shown }))
    }

    /// `POST /plugins/catalog/install` `{ id }`: its file fetched, checked against the signed
    /// catalog (size, SHA-256, the plugin's id), then installed as a file would be.
    pub async fn install_from_catalog(&self, id: &str) -> Result<PluginInfo, EngineError> {
        let list = self.catalog_entries(false).await?;
        let e = list.into_iter().find(|e| e.id == id).ok_or_else(|| {
            refused(404, "catalog_no_plugin", format!("The catalog has no plugin called {id}."), "Look again.")
        })?;
        let src = source();
        let url = file_url(&e.url, &src).ok_or_else(|| EngineError::Internal("an entry's address".into()))?;
        let bytes = fetch(&client(&src)?, url, e.size as usize).await?;
        let sum = hex(&sha2::Sha256::digest(&bytes));
        if bytes.len() as u64 != e.size || !sum.eq_ignore_ascii_case(&e.sha256) {
            tracing::warn!(id, "a catalog plugin's file isn't the one the catalog signed: refused");
            return Err(refused(
                502,
                "catalog_file_changed",
                format!("{}'s file isn't the one the catalog lists, so it wasn't installed.", e.name),
                "Try again later.",
            ));
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let tmp = std::env::temp_dir().join(format!("ferrocast-plugin-{}-{nanos}.zip", std::process::id()));
        std::fs::write(&tmp, &bytes).map_err(|e| EngineError::Internal(e.to_string()))?;
        let installed = async {
            // The file's own id must be the one listed: an entry can't install another plugin.
            let (m, _) = crate::plugins::read_zip(&tmp)?;
            if m.id != e.id {
                return Err(refused(
                    502,
                    "catalog_file_changed",
                    format!("{}'s file holds another plugin, so it wasn't installed.", e.name),
                    "Try again later.",
                ));
            }
            self.install_plugin(&tmp).await
        }
        .await;
        let _ = std::fs::remove_file(&tmp);
        installed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &[u8] = include_bytes!("../testdata/catalog/catalog.json");
    const SIG: &str = include_str!("../testdata/catalog/catalog.json.sig");
    const KEY: &str = include_str!("../testdata/catalog/test-key.pub");
    const ZIP: &[u8] = include_bytes!("../testdata/catalog/invert.zip");

    #[test]
    fn only_a_list_signed_with_the_key_is_taken() {
        assert_eq!(verify(LIST, SIG, KEY), Ok(()));
        let mut changed = LIST.to_vec();
        changed[20] ^= 1;
        assert!(verify(&changed, SIG, KEY).is_err(), "a changed byte");
        assert!(verify(LIST, "nonsense", KEY).is_err());
        assert!(verify(LIST, SIG, "").is_err(), "no key");
        // The app's own key isn't the tests'.
        assert_ne!(CATALOG_KEY.trim(), KEY.trim());
    }

    #[test]
    fn the_catalog_key_reads() {
        let b = base64::engine::general_purpose::STANDARD.decode(CATALOG_KEY.trim());
        let text = b.ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
        assert!(minisign_verify::PublicKey::decode(&text).is_ok(), "CATALOG_KEY isn't a minisign key");
    }

    /// Before pushing the catalog: `FERROCAST_CATALOG_DIR=~/…/ferrocast-plugins cargo test -p
    /// ferrocast-engine a_catalog_folder -- --ignored` checks its list is signed with the app's key.
    #[test]
    #[ignore]
    fn a_catalog_folder_is_signed_with_the_app_key() {
        let dir = std::path::PathBuf::from(std::env::var("FERROCAST_CATALOG_DIR").unwrap_or_default());
        let list = std::fs::read(dir.join("catalog.json")).unwrap_or_else(|e| panic!("catalog.json: {e}"));
        let sig = std::fs::read_to_string(dir.join("catalog.json.sig")).unwrap_or_else(|e| panic!("sig: {e}"));
        assert_eq!(verify(&list, &sig, CATALOG_KEY), Ok(()));
        let entries = entries(&list, &Source { url: CATALOG_URL.into(), key: CATALOG_KEY.into(), local: false });
        assert!(entries.as_ref().is_ok_and(|e| !e.is_empty()), "{entries:?}");
    }

    #[test]
    fn a_file_comes_from_github_or_beside_the_list() {
        let real = Source { url: CATALOG_URL.into(), key: String::new(), local: false };
        let ok = |u: &str| file_url(u, &real).map(|u| u.to_string());
        assert_eq!(
            ok("plugins/invert-1.2.0.zip").as_deref(),
            Some("https://raw.githubusercontent.com/ArshPunisher/ferrocast-plugins/main/plugins/invert-1.2.0.zip")
        );
        assert!(ok("https://github.com/ArshPunisher/ferrocast-plugins/releases/download/v1/x.zip").is_some());
        for bad in [
            "http://github.com/x.zip",
            "https://example.com/x.zip",
            "file:///etc/passwd",
            "../secrets.zip",
            "a/../../b.zip",
            "/abs.zip",
            "",
            "http://127.0.0.1:9/x.zip",
        ] {
            assert!(ok(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn entries_that_make_no_sense_are_left_out() {
        let src = Source { url: CATALOG_URL.into(), key: String::new(), local: false };
        let good = json!({ "id": "com.a.b", "name": "B", "version": "1", "kind": "effect", "url": "b.zip", "sha256": "a".repeat(64), "size": 10 });
        let mut list = vec![good.clone()];
        for (k, v) in [
            ("id", json!("Bad Id")),
            ("kind", json!("native")),
            ("sha256", json!("xyz")),
            ("size", json!(MAX_ZIP + 1)),
            ("url", json!("https://example.com/b.zip")),
        ] {
            let mut e = good.clone();
            e[k] = v;
            e["id"] = if k == "id" { e["id"].clone() } else { json!(format!("com.a.{k}")) };
            list.push(e);
        }
        list.push(good.clone());
        let got = entries(json!({ "plugins": list }).to_string().as_bytes(), &src).unwrap_or_default();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(newer("1.10.0", "1.9.2") && !newer("1.0", "1.0") && newer("2", "1.9") && !newer("1.0.0", "1.2"));
    }

    #[test]
    fn an_empty_source_address_allows_no_local_redirect() {
        let src = Source { url: String::new(), key: String::new(), local: true };
        let u = reqwest::Url::parse("http://127.0.0.1:9999/x").unwrap();
        assert!(!allowed(&u, &src), "an empty source let any local port through");
    }

    #[test]
    fn a_prerelease_or_older_version_is_never_an_update() {
        assert!(newer("1.0.0", "1.0.0-alpha.1"), "the release follows its prerelease");
        assert!(!newer("1.0.0-alpha.1", "1.0.0"), "a prerelease is not newer than the release");
        assert!(newer("1.0.0-beta.10", "1.0.0-beta.2"), "numbers in a prerelease compare as numbers");
        assert!(!newer("1.0.0-beta.2", "1.0.0-beta.10"));
        assert!(newer("1.0.0-beta", "1.0.0-alpha"), "words compare in order");
        assert!(!newer("1.0.0-alpha.1", "1.0.0-alpha.1"));
        assert!(!newer("garbage", "1.0.0"), "what can't be read is not offered as an update");
        assert!(newer("1.0.1+build5", "1.0.0"));
        assert!(!newer("1.0.0+build9", "1.0.0"), "build metadata doesn't order versions");
    }

    #[test]
    fn numbers_of_any_length_compare_as_numbers() {
        // Past u64: still numbers, by length and then digit by digit.
        assert!(newer("1.0.0-100000000000000000000", "1.0.0-99999999999999999999"));
        assert!(!newer("1.0.0-99999999999999999999", "1.0.0-100000000000000000000"));
        assert!(!newer("1.0.0-100000000000000000000", "1.0.0-100000000000000000000"));
        assert!(newer("1.0.0-100000000000000000001", "1.0.0-100000000000000000000"));
        assert!(newer("1.0.100000000000000000000", "1.0.99999999999999999999"), "core parts too");
        assert!(newer("1.0.0-alpha", "1.0.0-100000000000000000000"), "words follow numbers");
    }

    #[test]
    fn a_version_is_readable_or_not_by_the_one_grammar() {
        for ok in ["1", "1.0", "1.2.3", "10.20.30", "1.0.0-alpha.1", "1.0.0-0.3.7", "1.0.0+build.5", "1.0.0-rc-1+b"] {
            assert!(readable_version(ok), "{ok}");
        }
        for bad in
            ["", "2oops", "1.0.0-", "1..0", ".1", "1.0.", "v1.0", "1.0.0-a..b", "1.0.0+", "1.0.0 beta", "1.0.0-\u{e9}"]
        {
            assert!(!readable_version(bad), "{bad:?}");
            assert!(!newer(bad, "0.0.1") && !newer("9.9.9", bad), "{bad:?} orders nothing");
        }
    }

    /// Browse and install from a catalog served here: the list checked, the file checked, a
    /// changed file refused, installed plugins and updates marked.
    #[tokio::test]
    async fn browse_and_install_from_the_catalog() {
        use axum::{Router, routing::get};
        let served = std::sync::Arc::new(std::sync::Mutex::new(ZIP.to_vec()));
        let zip = served.clone();
        let app = Router::new()
            .route("/catalog.json", get(|| async { LIST.to_vec() }))
            .route("/catalog.json.sig", get(|| async { SIG.to_string() }))
            .route("/invert.zip", get(move || async move { zip.lock().map(|z| z.clone()).unwrap_or_default() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap_or_else(|e| panic!("{e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        tokio::spawn(async move { axum::serve(listener, app).await });
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: KEY.into(), local: true });
        let (e, _tmp, _studio, _chat) = crate::automation::tests::engine().await;
        let got = e.plugin_catalog(true).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(got["plugins"][0]["id"], "com.ferrocast.invert");
        assert_eq!(got["plugins"][0]["installed"], Value::Null);
        // A file that isn't the one listed is refused, and nothing is installed.
        *served.lock().unwrap_or_else(|e| e.into_inner()) = {
            let mut z = ZIP.to_vec();
            z[30] ^= 1;
            z
        };
        let no = e.install_from_catalog("com.ferrocast.invert").await;
        assert!(matches!(no, Err(EngineError::Refused { code: "catalog_file_changed", .. })), "{no:?}");
        assert!(e.plugins().await.is_empty());
        *served.lock().unwrap_or_else(|e| e.into_inner()) = ZIP.to_vec();
        let p = e.install_from_catalog("com.ferrocast.invert").await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((p.id.as_str(), p.version.as_str()), ("com.ferrocast.invert", "1.2.0"));
        let got = e.plugin_catalog(false).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (got["plugins"][0]["installed"].as_str(), got["plugins"][0]["update"].as_bool()),
            (Some("1.2.0"), Some(false))
        );
        assert!(matches!(
            e.install_from_catalog("com.nope.nope").await,
            Err(EngineError::Refused { code: "catalog_no_plugin", .. })
        ));
        // No key: closed, and said so.
        set_source(Source { url: format!("http://127.0.0.1:{port}/catalog.json"), key: String::new(), local: true });
        assert!(matches!(e.plugin_catalog(true).await, Err(EngineError::Refused { code: "catalog_not_open", .. })));
    }
}
