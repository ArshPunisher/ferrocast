//! The phone's dashboard: the same build the desktop app embeds (`ferrocast-web/dist/
//! dashboard`), served from `RELAY_DASHBOARD_DIR` at `/app/` on the remote host only.
//!
//! `index.html` gets one script tag before its own, `/app/ferrocast-remote.js`, which sets
//! `window.__FERROCAST__ = { api: "<origin>/r", remote: true }`: the dashboard then calls
//! `/r/api/v1/*` on this same origin with the device token. Scripts only from this origin
//! (a file, not an inline script), no framing, no CORS anywhere.

use std::path::{Component, Path as FsPath, PathBuf};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};

use super::AppState;
use super::proxy::on_remote_host;

/// The config script's file name (never a file of the build: the build has no such name).
const CONFIG_JS: &str = "ferrocast-remote.js";
/// Files larger than this aren't served (the build's biggest chunk is well under 2 MB).
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// The phone page's CSP: everything from this origin, images also from the platforms' CDNs
/// (avatars, emotes), the live socket to this host.
fn csp(origin: &str) -> String {
    let ws = origin.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: https:; \
         media-src 'self' data: blob:; font-src 'self' data:; connect-src 'self' {ws}; frame-src 'none'; \
         object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
    )
}

fn headers(res: &mut Response, origin: &str, cache: &'static str, html: bool) {
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    if html && let Ok(v) = HeaderValue::from_str(&csp(origin)) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
}

fn plain(status: StatusCode, text: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>Ferrocast</title>\
         <body style=\"font:17px system-ui;background:#0a0b0d;color:#eef0f4;display:grid;place-items:center;min-height:100vh;margin:0;padding:24px;text-align:center\"><p>{text}</p>"
    );
    let mut res = (status, Html(body)).into_response();
    res.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'"),
    );
    res
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "webm" => "video/webm",
        "mp4" => "video/mp4",
        "ogg" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A path inside the build folder, or `None` for anything that could leave it.
fn inside(root: &FsPath, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.len() > 300 || rel.contains('\\') || rel.contains('\0') {
        return None;
    }
    let p = FsPath::new(rel);
    let plain = p.components().all(|c| matches!(c, Component::Normal(s) if !s.to_string_lossy().starts_with('.')));
    plain.then(|| root.join(p))
}

fn ready(st: &AppState, h: &HeaderMap) -> Result<(String, PathBuf), Box<Response>> {
    if !super::available(st) || !on_remote_host(st, h) {
        return Err(Box::new(plain(StatusCode::NOT_FOUND, "There's nothing here.")));
    }
    let origin = st.remote.cfg.origin.clone().unwrap_or_default();
    let Some(dir) = st.remote.cfg.dashboard_dir.clone() else {
        return Err(Box::new(plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "The dashboard isn't on this server yet. Try again later.",
        )));
    };
    Ok((origin, dir))
}

/// `GET /app` → `/app/` on the remote host; elsewhere it isn't a page.
pub(crate) async fn app_root(State(st): State<AppState>, h: HeaderMap) -> Response {
    match ready(&st, &h) {
        Ok(_) => Redirect::permanent("/app/").into_response(),
        Err(r) => *r,
    }
}

/// Where `tag` goes: right after the `<head …>` tag (attributes and any case), so it runs
/// before the theme script and the app. Without a head, after `<html …>`, else after the
/// doctype; never before the doctype (that would put the page in quirks mode).
fn inject(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let tag_end = |name: &str| -> Option<usize> {
        let mut from = 0;
        while let Some(i) = lower[from..].find(name).map(|i| i + from) {
            let after = lower.as_bytes().get(i + name.len()).copied();
            // `<head>` or `<head lang=…>`, not `<header>`.
            if matches!(after, Some(b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/')) {
                return lower[i..].find('>').map(|j| i + j + 1);
            }
            from = i + name.len();
        }
        None
    };
    let at = tag_end("<head")
        .or_else(|| tag_end("<html"))
        .or_else(|| lower.trim_start().starts_with("<!doctype").then(|| lower.find('>').map(|j| j + 1)).flatten())
        .unwrap_or(0);
    format!("{}{tag}{}", &html[..at], &html[at..])
}

/// `GET /app/`: the build's `index.html` with the config script added.
pub(crate) async fn index(State(st): State<AppState>, h: HeaderMap) -> Response {
    let (origin, dir) = match ready(&st, &h) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let html = match tokio::fs::read_to_string(dir.join("index.html")).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, dir = %dir.display(), "the remote dashboard build can't be read");
            return plain(StatusCode::SERVICE_UNAVAILABLE, "The dashboard isn't on this server yet. Try again later.");
        }
    };
    let html = inject(&html, &format!("<script src=\"/app/{CONFIG_JS}\"></script>"));
    let mut res = Html(html).into_response();
    headers(&mut res, &origin, "no-cache", true);
    res
}

/// `GET /app/{*path}`: the config script, a file of the build, or (for a path without a
/// file extension) the app itself.
pub(crate) async fn file(State(st): State<AppState>, h: HeaderMap, Path(path): Path<String>) -> Response {
    let (origin, dir) = match ready(&st, &h) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    if path == CONFIG_JS {
        let api = format!("{origin}/r");
        let js = format!(
            "window.__FERROCAST__ = Object.freeze({{ api: {}, remote: true }});\n",
            serde_json::to_string(&api).unwrap_or_else(|_| "\"\"".into())
        );
        let mut res = ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], js).into_response();
        headers(&mut res, &origin, "no-cache", false);
        return res;
    }
    let Some(full) = inside(&dir, &path) else {
        return plain(StatusCode::NOT_FOUND, "There's nothing here.");
    };
    match tokio::fs::metadata(&full).await {
        Ok(m) if m.is_file() && m.len() <= MAX_FILE => match tokio::fs::read(&full).await {
            Ok(bytes) => {
                let mut res = Response::new(Body::from(bytes));
                if let Ok(v) = HeaderValue::from_str(mime(&path)) {
                    res.headers_mut().insert(header::CONTENT_TYPE, v);
                }
                let cache =
                    if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
                headers(&mut res, &origin, cache, path.ends_with(".html"));
                res
            }
            Err(_) => plain(StatusCode::NOT_FOUND, "There's nothing here."),
        },
        _ => {
            let looks_like_file = path.rsplit('/').next().is_some_and(|last| last.contains('.'));
            if looks_like_file {
                plain(StatusCode::NOT_FOUND, "There's nothing here.")
            } else {
                index(State(st), h).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::inject;

    #[test]
    fn the_config_script_goes_right_after_the_head_tag_never_before_the_doctype() {
        let t = "<s>";
        assert_eq!(inject("<!doctype html><html><head><title>", t), "<!doctype html><html><head><s><title>");
        assert_eq!(
            inject("<!DOCTYPE html>\n<html lang=\"en\">\n<HEAD data-x=\"1\">\n<meta>", t),
            "<!DOCTYPE html>\n<html lang=\"en\">\n<HEAD data-x=\"1\"><s>\n<meta>"
        );
        // `<header>` in the body isn't the head.
        assert_eq!(inject("<!doctype html><html><body><header>", t), "<!doctype html><html><s><body><header>");
        assert_eq!(inject("<!doctype html><body>", t), "<!doctype html><s><body>");
        assert_eq!(inject("<p>bare", t), "<s><p>bare");
    }
}
