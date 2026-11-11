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
/// generate` writes it). Empty: no catalog yet, and Browse says so.
pub const CATALOG_KEY: &str = "";

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

static SOURCE: Mutex<Option<Source>> = Mutex::new(None);
static KEPT: Mutex<Option<(Instant, Vec<Entry>)>> = Mutex::new(None);

/// The tests' catalog in place of the real one.
pub fn set_source(s: Source) {
    *SOURCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
    *KEPT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn source() -> Source {
    SOURCE.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| Source {
        url: CATALOG_URL.into(),
        key: CATALOG_KEY.into(),
        local: false,
    })
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

/// Checks `list` against its minisign signature (`sig`, base64 as `cargo tauri signer sign`
/// writes it) and the key (base64 too).
pub fn verify(list: &[u8], sig: &str, key: &str) -> Result<(), String> {
    let b64 = |s: &str| {
        base64::engine::general_purpose::STANDARD.decode(s.trim()).ok().and_then(|b| String::from_utf8(b).ok())
    };
    let key = b64(key).ok_or("the catalog key isn't readable")?;
    let key = minisign_verify::PublicKey::decode(&key).map_err(|e| format!("the catalog key: {e}"))?;
    let sig = b64(sig).ok_or("the catalog's signature isn't readable")?;
    let sig = minisign_verify::Signature::decode(&sig).map_err(|e| format!("the catalog's signature: {e}"))?;
    key.verify(list, &sig, false).map_err(|_| "the catalog's signature doesn't match".to_string())
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
    match u.scheme() {
        "https" => HOSTS.contains(&host) || (src.local && host == "127.0.0.1"),
        "http" => src.local && host == "127.0.0.1",
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

/// Whether `new` is a later version than `old` (numbers dot by dot; anything else: differs).
pub fn newer(new: &str, old: &str) -> bool {
    let nums = |v: &str| v.split(['.', '-', '+']).map(|p| p.parse::<u64>().ok()).collect::<Option<Vec<_>>>();
    match (nums(new), nums(old)) {
        (Some(a), Some(b)) => a > b,
        _ => new != old,
    }
}

/// A client that follows redirects only to the allowed hosts, over https (or the tests' local
/// server).
fn client(src: &Source) -> Result<reqwest::Client, EngineError> {
    let local = src.local;
    let policy = reqwest::redirect::Policy::custom(move |a| {
        let s = Source { url: String::new(), key: String::new(), local };
        if a.previous().len() >= 5 || !allowed(a.url(), &s) { a.stop() } else { a.follow() }
    });
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(policy)
        .build()
        .map_err(|e| EngineError::Internal(e.to_string()))
}

/// Reads at most `max` bytes from `url`; more is refused (never all read).
async fn fetch(http: &reqwest::Client, url: reqwest::Url, max: usize) -> Result<Vec<u8>, EngineError> {
    let unreachable = |e: String| {
        refused(
            502,
            "catalog_unreachable",
            format!("The plugin catalog didn't answer: {e}"),
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

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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
