//! Finding OBS's own WebSocket settings, so the streamer never types the port or password.
//!
//! obs-websocket 5 (built into OBS 28+) keeps them in
//! `<OBS config>/plugin_config/obs-websocket/config.json`:
//! `{ "server_enabled": true, "server_port": 4455, "auth_required": true, "server_password": "…" }`.
//! A missing file means OBS isn't installed (or never started); a damaged one is reported,
//! never guessed at.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Bigger than any real config; a huge file is a damaged one.
const MAX_BYTES: u64 = 64 * 1024;

/// What was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub path: PathBuf,
    pub port: u16,
    pub server_enabled: bool,
    pub auth_required: bool,
    /// Only when `auth_required` and set.
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    Found(Detected),
    /// No settings file in any usual place.
    Missing,
    /// A settings file is there but can't be read; the message says why, in plain words.
    Unreadable(String),
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default = "yes")]
    server_enabled: bool,
    #[serde(default)]
    server_port: Option<i64>,
    #[serde(default)]
    auth_required: bool,
    #[serde(default)]
    server_password: Option<String>,
}

fn yes() -> bool {
    true
}

/// Where OBS keeps its config on this OS, most likely first. Built from `home`, `appdata`
/// (Windows `%APPDATA%`) and `xdg` (`$XDG_CONFIG_HOME`), so tests can point anywhere.
pub fn candidate_paths(home: Option<&Path>, appdata: Option<&Path>, xdg: Option<&Path>) -> Vec<PathBuf> {
    let tail = Path::new("obs-studio").join("plugin_config").join("obs-websocket").join("config.json");
    let mut out = Vec::new();
    if cfg!(target_os = "windows") {
        if let Some(a) = appdata {
            out.push(a.join(&tail));
        }
    } else if cfg!(target_os = "macos") {
        if let Some(h) = home {
            out.push(h.join("Library").join("Application Support").join(&tail));
        }
    } else {
        if let Some(x) = xdg {
            out.push(x.join(&tail));
        }
        if let Some(h) = home {
            out.push(h.join(".config").join(&tail));
            // The Flatpak build keeps its own config.
            out.push(h.join(".var").join("app").join("com.obsproject.Studio").join("config").join(&tail));
        }
    }
    out
}

/// The usual places on this computer, from the environment.
pub fn default_paths() -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = env("HOME").or_else(|| env("USERPROFILE"));
    candidate_paths(home.as_deref(), env("APPDATA").as_deref(), env("XDG_CONFIG_HOME").as_deref())
}

/// Reads the first settings file that exists. Blocking file I/O: call it off the async runtime.
pub fn detect(paths: &[PathBuf]) -> Detection {
    let Some(path) = paths.iter().find(|p| p.is_file()) else {
        return Detection::Missing;
    };
    match read(path) {
        Ok(d) => Detection::Found(d),
        Err(why) => Detection::Unreadable(why),
    }
}

fn read(path: &Path) -> Result<Detected, String> {
    let damaged = "OBS's WebSocket settings file couldn't be read. Open OBS once (Tools, WebSocket Server Settings) to rewrite it, or type the port and password below.";
    let size = std::fs::metadata(path).map_err(|_| damaged.to_string())?.len();
    if size > MAX_BYTES {
        return Err(damaged.into());
    }
    let bytes = std::fs::read(path).map_err(|_| damaged.to_string())?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| damaged.to_string())?;
    if !value.is_object() {
        return Err(damaged.into());
    }
    let raw: Raw = serde_json::from_value(value).map_err(|_| damaged.to_string())?;
    let port = match raw.server_port {
        None => 4455,
        Some(p) => u16::try_from(p).ok().filter(|p| *p > 0).ok_or_else(|| damaged.to_string())?,
    };
    let password = raw.server_password.filter(|p| raw.auth_required && !p.is_empty() && p.chars().count() <= 256);
    Ok(Detected {
        path: path.to_path_buf(),
        port,
        server_enabled: raw.server_enabled,
        auth_required: raw.auth_required,
        password,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &[u8]) -> PathBuf {
        let p = dir.join("config.json");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn reads_port_and_password_from_obs() {
        let d = tempfile::tempdir().unwrap();
        let p = write(
            d.path(),
            br#"{"alerts_enabled":false,"auth_required":true,"first_load":false,"server_enabled":true,"server_password":"s3cret","server_port":4460}"#,
        );
        let missing = d.path().join("nope.json");
        match detect(&[missing, p.clone()]) {
            Detection::Found(f) => {
                assert_eq!((f.port, f.auth_required, f.server_enabled), (4460, true, true));
                assert_eq!(f.password.as_deref(), Some("s3cret"));
                assert_eq!(f.path, p);
            }
            other => panic!("{other:?}"),
        }
        let open = write(d.path(), br#"{"auth_required":false,"server_password":"left-over","server_enabled":false}"#);
        match detect(&[open]) {
            Detection::Found(f) => {
                assert_eq!(f.port, 4455, "the default port");
                assert_eq!(f.password, None, "no password when OBS doesn't ask for one");
                assert!(!f.server_enabled);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_and_damaged_files_are_reported_not_guessed() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(detect(&[d.path().join("config.json")]), Detection::Missing);
        assert_eq!(detect(&[]), Detection::Missing);
        for junk in [
            &b"{not json"[..],
            b"[]",
            br#"{"server_port":70000}"#,
            br#"{"server_port":-1}"#,
            br#"{"server_port":"4455"}"#,
        ] {
            let p = write(d.path(), junk);
            assert!(matches!(detect(&[p]), Detection::Unreadable(m) if m.contains("couldn't be read")), "{junk:?}");
        }
        let big = write(d.path(), &vec![b' '; 70 * 1024]);
        assert!(matches!(detect(&[big]), Detection::Unreadable(_)));
        // A directory named like the file isn't a settings file.
        let dir = d.path().join("dir.json");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(detect(&[dir]), Detection::Missing);
    }

    #[test]
    fn looks_in_the_right_place_for_this_os() {
        let paths = candidate_paths(Some(Path::new("/home/u")), Some(Path::new("C:/AppData")), Some(Path::new("/xdg")));
        let first = paths.first().unwrap().to_string_lossy().replace('\\', "/");
        assert!(first.ends_with("obs-studio/plugin_config/obs-websocket/config.json"));
        if cfg!(target_os = "macos") {
            assert!(first.starts_with("/home/u/Library/Application Support/"));
        }
        assert!(candidate_paths(None, None, None).is_empty());
    }
}
