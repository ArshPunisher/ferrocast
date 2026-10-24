//! ffmpeg for cutting the vertical picture (A2.4), fetched the first time someone picks "Cut it
//! from your stream" rather than shipped with every install (it's 20 to 30 MB, and most
//! streamers never need it). One pinned build per system from ffmpeg-static's GitHub release,
//! checked against its SHA-256 before it's unpacked, kept in `<data>/tools/`.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{Engine, EngineError};

/// The ffmpeg-static release (ffmpeg 6, GPL: the same licence as Ferrocast).
pub const RELEASE: &str = "b6.1.1";
const BASE: &str = "https://github.com/eugeneware/ffmpeg-static/releases/download";
/// Bigger than any build in the release; a bigger download is refused.
const MAX_BYTES: usize = 40 * 1024 * 1024;

/// This system's build: the file name in the release and its SHA-256 (checked 24 Oct 2026).
fn asset() -> Option<(&'static str, &'static str)> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(("ffmpeg-darwin-arm64.gz", "8923876afa8db5585022d7860ec7e589af192f441c56793971276d450ed3bbfa"))
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(("ffmpeg-darwin-x64.gz", "929b375c1182d956c51f7ac25e0b2b0411fb01f6f407aa15c9758efeb4242106"))
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(("ffmpeg-win32-x64.gz", "8883a3dffbd0a16cf4ef95206ea05283f78908dbfb118f73c83f4951dcc06d77"))
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(("ffmpeg-linux-x64.gz", "bfe8a8fc511530457b528c48d77b5737527b504a3797a9bc4866aeca69c2dffa"))
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(("ffmpeg-linux-arm64.gz", "754a678672298bc68156adff58aa7385a592c2b30b1d0ae8750c45c915c4bac0"))
    } else {
        None
    }
}

const EXE: &str = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };

/// Whether ffmpeg is here, being fetched, or can't be had.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FfmpegState {
    /// Found (downloaded, next to the app, or installed on the system).
    Ready,
    Missing,
    Downloading {
        percent: u8,
    },
    Failed {
        reason: String,
    },
    /// No build for this system: the streamer installs ffmpeg themselves.
    Unsupported,
}

/// Where the fetched ffmpeg lives in the data folder.
pub fn tools_path(data: &Path) -> PathBuf {
    data.join("tools").join(format!("ffmpeg-{RELEASE}")).join(EXE)
}

/// Downloads `url` (at most `MAX_BYTES`), checks its SHA-256, unpacks the gzip into `dest`
/// (written beside it and renamed, so a half-done file is never taken for a whole one), and
/// makes it runnable. `progress` gets 0 to 100.
pub async fn download_verified(
    url: &str,
    sha256: &str,
    dest: &Path,
    progress: impl Fn(u8) + Send,
) -> Result<(), String> {
    let http =
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(300)).build().map_err(|e| e.to_string())?;
    let mut res = http.get(url).send().await.map_err(|e| format!("the download didn't start ({e})"))?;
    if !res.status().is_success() {
        return Err(format!("the download answered {}", res.status().as_u16()));
    }
    let total = res.content_length().unwrap_or(0) as usize;
    if total > MAX_BYTES {
        return Err("the download is bigger than ffmpeg should be".into());
    }
    let mut body = Vec::with_capacity(total);
    while let Some(chunk) = res.chunk().await.map_err(|e| format!("the download stopped ({e})"))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            return Err("the download is bigger than ffmpeg should be".into());
        }
        if let Some(pct) = (body.len() * 100).checked_div(total) {
            progress(pct.min(99) as u8);
        }
    }
    let got = Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect::<String>();
    if got != sha256 {
        return Err("the downloaded file isn't the expected one (its checksum differs), so it wasn't used".into());
    }
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || unpack(&body, &dest)).await.map_err(|e| e.to_string())??;
    progress(100);
    Ok(())
}

fn unpack(gz: &[u8], dest: &Path) -> Result<(), String> {
    use std::io::Read;
    let dir = dest.parent().ok_or("no folder for ffmpeg")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't make {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gz)
        .take(200 * 1024 * 1024)
        .read_to_end(&mut out)
        .map_err(|_| "the download couldn't be unpacked".to_string())?;
    let part = dest.with_extension("part");
    std::fs::write(&part, &out).map_err(|e| format!("couldn't save ffmpeg: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&part, dest).map_err(|e| format!("couldn't save ffmpeg: {e}"))?;
    Ok(())
}

impl Engine {
    /// ffmpeg for the cutter: `FERROCAST_FFMPEG`, the fetched one, then one next to the app or
    /// installed on the system.
    pub(crate) fn ffmpeg_path(&self) -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("FERROCAST_FFMPEG").map(PathBuf::from).filter(|p| p.is_file()) {
            return Some(p);
        }
        let data = self.golive_data_dir();
        data.map(|d| tools_path(&d)).filter(|p| p.is_file()).or_else(crate::golive::find_ffmpeg)
    }

    pub(crate) fn ffmpeg_state(&self) -> FfmpegState {
        if self.ffmpeg_path().is_some() {
            return FfmpegState::Ready;
        }
        match self.golive_ffmpeg_fetch() {
            Some(s) => s,
            None if asset().is_none() => FfmpegState::Unsupported,
            None => FfmpegState::Missing,
        }
    }

    /// Starts fetching ffmpeg (once at a time); the cut starts by itself when it's ready.
    pub async fn get_ffmpeg(&self) -> Result<FfmpegState, EngineError> {
        if self.ffmpeg_path().is_some() {
            return Ok(FfmpegState::Ready);
        }
        let Some((name, sha)) = asset() else {
            return Err(crate::golive::refused_pub(
                "ffmpeg_unsupported",
                "Ferrocast can't fetch ffmpeg for this kind of computer.",
                "Install ffmpeg yourself (from ffmpeg.org), then try again.",
            ));
        };
        let Some(data) = self.golive_data_dir() else {
            return Err(EngineError::Unavailable("the data folder isn't known yet".into()));
        };
        if !self.claim_ffmpeg_fetch() {
            return Ok(self.ffmpeg_state());
        }
        self.send_golive_pub();
        let engine = self.clone();
        tokio::spawn(async move {
            let url = format!("{BASE}/{RELEASE}/{name}");
            let progress = {
                let e = engine.clone();
                move |p: u8| e.set_golive_ffmpeg_fetch(Some(FfmpegState::Downloading { percent: p }))
            };
            match download_verified(&url, sha, &tools_path(&data), progress).await {
                Ok(()) => {
                    tracing::info!("ffmpeg fetched for cutting the vertical picture");
                    engine.set_golive_ffmpeg_fetch(None);
                    engine.reconcile().await;
                }
                Err(why) => {
                    tracing::warn!(%why, "ffmpeg couldn't be fetched");
                    engine.set_golive_ffmpeg_fetch(Some(FfmpegState::Failed {
                        reason: format!("Ferrocast couldn't get ffmpeg: {why}."),
                    }));
                    engine.send_golive_pub();
                }
            }
        });
        Ok(FfmpegState::Downloading { percent: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    async fn serve(body: Vec<u8>) -> String {
        let app = axum::Router::new().route(
            "/ffmpeg.gz",
            axum::routing::get(move || {
                let b = body.clone();
                async move { b }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await });
        format!("{base}/ffmpeg.gz")
    }

    /// The real file from GitHub for this system (run by hand: it downloads 20 to 30 MB).
    #[tokio::test]
    #[ignore = "downloads the real ffmpeg"]
    async fn the_pinned_build_downloads_checks_out_and_runs() {
        let (name, sha) = asset().expect("a build for this system");
        let dir = tempfile::tempdir().unwrap();
        let dest = tools_path(dir.path());
        download_verified(&format!("{BASE}/{RELEASE}/{name}"), sha, &dest, |_| {}).await.unwrap();
        let out = std::process::Command::new(&dest).args(["-hide_banner", "-encoders"]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("libx264"), "it runs and can encode H.264");
    }

    #[tokio::test]
    async fn fetched_only_when_the_checksum_matches() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"#!/bin/sh\necho ffmpeg\n").unwrap();
        let gz = gz.finish().unwrap();
        let sha = Sha256::digest(&gz).iter().map(|b| format!("{b:02x}")).collect::<String>();
        let url = serve(gz).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = tools_path(dir.path());
        let wrong = "0".repeat(64);
        let err = download_verified(&url, &wrong, &dest, |_| {}).await.unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert!(!dest.exists(), "a file that didn't check out is never kept");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        download_verified(&url, &sha, &dest, move |p| s2.lock().unwrap().push(p)).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"#!/bin/sh\necho ffmpeg\n");
        assert_eq!(seen.lock().unwrap().last(), Some(&100));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o111, 0o111, "runnable");
        }
        assert!(asset().is_some(), "this system has a pinned build");
    }
}
