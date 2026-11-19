//! Ferrocast's Web page source where WebKit isn't (Windows): the page runs in a hidden Edge or
//! Chrome (every Windows has Edge) with its own empty profile, and its pictures come back over
//! the DevTools protocol's screencast (a PNG each time the page changes, transparent where the
//! page is) into our engine's fed source. Same calls as `ffi::web`; OBS's Browser source keys.
//! The page's sound isn't heard yet (the browser is muted).
// On other systems it's built only for its tests.
#![cfg_attr(not(windows), allow(dead_code))]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use ferrocore::engine::Feed;
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

/// What a page is asked to do (as `ffi::web::Do`).
pub(crate) enum Do {
    /// Its window's title on a Mac; clicking in isn't here yet.
    Interact(#[allow(dead_code)] String),
    Stop,
    Reload,
}

/// How a page is shown: its address, size, CSS and frame rate.
#[derive(Debug, Clone, PartialEq)]
struct Look {
    url: String,
    w: u32,
    h: u32,
    css: String,
    fps: u32,
}

enum Cmd {
    Look(Look),
    Reload,
}

/// A web page shown in a fed source. Dropping it closes the browser.
pub(crate) struct FedPage {
    tx: Option<Sender<Cmd>>,
    feed: Arc<Mutex<Feed>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for FedPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FedPage").finish_non_exhaustive()
    }
}

impl FedPage {
    /// Opens `url` at `w`×`h` with `css`, at most `fps` pictures a second into `feed`. None when
    /// there's no Edge or Chrome to show it with.
    pub(crate) fn open(url: &str, w: u32, h: u32, css: &str, fps: u32, feed: Feed) -> Option<Self> {
        let Some(browser) = browser() else {
            tracing::warn!("a web page source needs Microsoft Edge or Google Chrome, and neither was found");
            return None;
        };
        let look = Look { url: file_url(url), w: w.max(1), h: h.max(1), css: css.to_string(), fps: fps.clamp(1, 60) };
        let (tx, rx) = channel();
        let feed = Arc::new(Mutex::new(feed));
        let thread = {
            let feed = feed.clone();
            std::thread::Builder::new()
                .name("ferrocast-web-page".into())
                .spawn(move || run(&browser, look, &rx, &feed))
                .ok()?
        };
        Some(Self { tx: Some(tx), feed, thread: Some(thread) })
    }

    /// Our engine's source changed (the collection was opened again): its new feed.
    pub(crate) fn set_feed(&self, feed: Feed) {
        *self.feed.lock().unwrap_or_else(|e| e.into_inner()) = feed;
    }

    /// A new address, size or CSS (the source's settings changed).
    pub(crate) fn update(&self, url: &str, w: u32, h: u32, css: &str, fps: u32) {
        let look = Look { url: file_url(url), w: w.max(1), h: h.max(1), css: css.to_string(), fps: fps.clamp(1, 60) };
        if let Some(tx) = &self.tx {
            let _ = tx.send(Cmd::Look(look));
        }
    }

    /// Reloads the page. Clicking into one ("Interact") isn't here yet.
    pub(crate) fn act(&self, what: Do) -> Result<(), String> {
        match what {
            Do::Reload => {
                if let Some(tx) = &self.tx {
                    let _ = tx.send(Cmd::Reload);
                }
                Ok(())
            }
            Do::Stop => Ok(()),
            Do::Interact(_) => Err("Clicking into a web page works on a Mac for now.".into()),
        }
    }
}

impl Drop for FedPage {
    fn drop(&mut self) {
        // The thread sees the channel close and ends its browser.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A local file's address the way a browser wants it on Windows (`file:///C:/…`).
fn file_url(url: &str) -> String {
    match url.strip_prefix("file://") {
        Some(rest) if !rest.starts_with('/') => format!("file:///{}", rest.replace('\\', "/")),
        _ => url.to_string(),
    }
}

/// Edge or Chrome (FERROCAST_BROWSER first).
fn browser() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut list: Vec<PathBuf> = env("FERROCAST_BROWSER").into_iter().collect();
    if cfg!(windows) {
        for base in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"].into_iter().filter_map(env) {
            list.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
            list.push(base.join(r"Google\Chrome\Application\chrome.exe"));
        }
    } else if cfg!(target_os = "macos") {
        list.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        list.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
    } else {
        for name in ["google-chrome", "chromium", "chromium-browser", "microsoft-edge"] {
            list.extend(
                std::env::var_os("PATH")
                    .into_iter()
                    .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
                    .map(|d| d.join(name)),
            );
        }
    }
    list.into_iter().find(|p| p.is_file())
}

/// Shows the page until the channel closes, starting the browser again when it ends or can't be
/// reached: after 3 s, twice as long each time it fails in a row (a minute at most).
fn run(browser: &Path, mut look: Look, rx: &Receiver<Cmd>, feed: &Mutex<Feed>) {
    let mut pause = Duration::from_secs(3);
    loop {
        let ended = match Session::start(browser, &look) {
            Ok(mut s) => {
                pause = Duration::from_secs(3);
                s.serve(&mut look, rx, feed)
            }
            Err(e) => {
                tracing::warn!("a web page source's browser didn't start: {e}");
                Ended::Failed
            }
        };
        if matches!(ended, Ended::Closed) {
            return;
        }
        // A new size or CSS starts afresh at once; a lost browser after a pause (commands kept).
        if matches!(ended, Ended::Lost | Ended::Failed) {
            let until = Instant::now() + pause;
            if matches!(ended, Ended::Failed) {
                pause = (pause * 2).min(Duration::from_secs(60));
            }
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                match rx.recv_timeout(left) {
                    Ok(Cmd::Look(l)) => look = l,
                    Ok(Cmd::Reload) => {}
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }
}

enum Ended {
    /// The page was closed.
    Closed,
    /// The browser went away.
    Lost,
    /// The browser couldn't be started or reached.
    Failed,
    /// A new size or CSS: a fresh browser.
    Again,
}

/// One browser with the page open.
struct Session {
    child: Child,
    dir: PathBuf,
    ws: WebSocket<TcpStream>,
    next: u64,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.ws.close(None);
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Its profile was made for it alone.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Session {
    fn start(browser: &Path, look: &Look) -> Result<Session, String> {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ferrocast-web-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("no profile folder ({e})"))?;
        let mut cmd = Command::new(browser);
        cmd.args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-sync",
            "--hide-scrollbars",
            "--mute-audio",
            "--force-device-scale-factor=1",
            "--autoplay-policy=no-user-gesture-required",
        ])
        .arg(format!("--user-data-dir={}", dir.display()))
        .arg(format!("--window-size={},{}", look.w, look.h))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| format!("couldn't start {} ({e})", browser.display()))?;
        let page = (|| {
            let port = devtools_port(&dir, &mut child)?;
            page_socket(port)
        })();
        let url = match page {
            Ok(u) => u,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let mut s = Session { child, dir, ws: connect(&url)?, next: 0 };
        s.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": look.w, "height": look.h, "deviceScaleFactor": 1, "mobile": false }),
        )?;
        s.call("Emulation.setDefaultBackgroundColorOverride", json!({ "color": { "r": 0, "g": 0, "b": 0, "a": 0 } }))?;
        s.call("Page.enable", json!({}))?;
        if !look.css.trim().is_empty() {
            s.call("Page.addScriptToEvaluateOnNewDocument", json!({ "source": css_script(&look.css) }))?;
        }
        s.call("Page.navigate", json!({ "url": look.url }))?;
        s.call(
            "Page.startScreencast",
            json!({ "format": "png", "maxWidth": look.w, "maxHeight": look.h, "everyNthFrame": 1 }),
        )?;
        Ok(s)
    }

    /// Sends a command (its answer isn't waited for).
    fn call(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.next += 1;
        let msg = json!({ "id": self.next, "method": method, "params": params }).to_string();
        self.ws.send(Message::text(msg)).map_err(|e| format!("the browser stopped listening ({e})"))
    }

    /// Hands the page's pictures on, and follows commands, until it's closed or lost.
    fn serve(&mut self, look: &mut Look, rx: &Receiver<Cmd>, feed: &Mutex<Feed>) -> Ended {
        let mut last = None::<Instant>;
        loop {
            match rx.try_recv() {
                Ok(Cmd::Look(l)) => {
                    let again = (l.w, l.h, &l.css) != (look.w, look.h, &look.css);
                    let go = l.url != look.url;
                    *look = l;
                    if again {
                        return Ended::Again;
                    }
                    if go && self.call("Page.navigate", json!({ "url": look.url })).is_err() {
                        return Ended::Lost;
                    }
                }
                Ok(Cmd::Reload) => {
                    if self.call("Page.reload", json!({})).is_err() {
                        return Ended::Lost;
                    }
                }
                Err(TryRecvError::Disconnected) => return Ended::Closed,
                Err(TryRecvError::Empty) => {}
            }
            if !matches!(self.child.try_wait(), Ok(None)) {
                tracing::warn!("a web page source's browser ended");
                return Ended::Lost;
            }
            let text = match self.ws.read() {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => return Ended::Lost,
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    continue;
                }
                Err(e) => {
                    tracing::warn!("a web page source lost its browser: {e}");
                    return Ended::Lost;
                }
            };
            let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
            if v.get("method").and_then(Value::as_str) != Some("Page.screencastFrame") {
                continue;
            }
            let p = &v["params"];
            // Each picture is acknowledged, or the browser sends no more.
            let session = p.get("sessionId").cloned().unwrap_or(Value::Null);
            if self.call("Page.screencastFrameAck", json!({ "sessionId": session })).is_err() {
                return Ended::Lost;
            }
            // At most `fps` a second.
            let every = Duration::from_secs(1) / look.fps.max(1);
            if last.is_some_and(|t| t.elapsed() < every) {
                continue;
            }
            let Some(pic) = p.get("data").and_then(Value::as_str).and_then(decode) else { continue };
            last = Some(Instant::now());
            feed.lock().unwrap_or_else(|e| e.into_inner()).clone().picture(pic);
        }
    }
}

/// Runs first in every page: adds the source's CSS once the page has a document.
fn css_script(css: &str) -> String {
    let css = serde_json::to_string(css).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(() => {{ const add = () => {{ const s = document.createElement('style'); s.textContent = {css}; \
         (document.head || document.documentElement).appendChild(s); }}; \
         if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', add); else add(); }})();"
    )
}

/// The port the browser listens on, from the file it writes into its profile (15 s at most).
fn devtools_port(dir: &Path, child: &mut Child) -> Result<u16, String> {
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        if let Ok(text) = std::fs::read_to_string(dir.join("DevToolsActivePort"))
            && let Some(port) = text.lines().next().and_then(|l| l.trim().parse().ok())
        {
            return Ok(port);
        }
        if !matches!(child.try_wait(), Ok(None)) {
            return Err("the browser closed as it started".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("the browser didn't open its DevTools port".into())
}

/// The open page's DevTools address, from the browser's list (asked a few times while it opens).
fn page_socket(port: u16) -> Result<String, String> {
    for _ in 0..50 {
        if let Ok(list) = http_get(port, "/json/list")
            && let Ok(v) = serde_json::from_str::<Value>(&list)
            && let Some(url) = v
                .as_array()
                .into_iter()
                .flatten()
                .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                .and_then(|t| t.get("webSocketDebuggerUrl").and_then(Value::as_str))
        {
            return Ok(url.to_string());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("the browser didn't list its page".into())
}

/// A GET to the browser on this computer → its body.
fn http_get(port: u16, path: &str) -> Result<String, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
        .map_err(|e| e.to_string())?;
    // Read up to the body's length (the browser may keep the connection open).
    let mut got = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&got[..at]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok());
            if let Some(len) = len
                && got.len() >= at + 4 + len
            {
                return Ok(String::from_utf8_lossy(&got[at + 4..at + 4 + len]).into_owned());
            }
        }
        let n = s.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 || got.len() > 1 << 20 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&got);
    text.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).ok_or_else(|| "no answer".into())
}

/// Connects to a DevTools address on this computer, reading in short steps (so commands are seen).
fn connect(url: &str) -> Result<WebSocket<TcpStream>, String> {
    let rest = url.strip_prefix("ws://").ok_or("not a local DevTools address")?;
    let host = rest.split('/').next().unwrap_or_default();
    if !host.starts_with("127.0.0.1:") && !host.starts_with("localhost:") {
        return Err("not a local DevTools address".into());
    }
    let stream = TcpStream::connect(host).map_err(|e| e.to_string())?;
    let (ws, _) = tungstenite::client(url, stream).map_err(|e| format!("DevTools didn't answer ({e})"))?;
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(100))).map_err(|e| e.to_string())?;
    Ok(ws)
}

/// A screencast picture (base64 PNG) → RGBA.
fn decode(b64: &str) -> Option<ferrocore::Picture> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    d.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = d.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        _ => return None,
    };
    Some(ferrocore::Picture { width: w, height: h, rgba, premultiplied: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_files_become_browser_addresses() {
        assert_eq!(file_url(r"file://C:\Users\me\page.html"), "file:///C:/Users/me/page.html");
        assert_eq!(file_url("file:///Users/me/page.html"), "file:///Users/me/page.html");
        assert_eq!(file_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn css_is_added_as_text() {
        let s = css_script("body { color: red } </style><script>x()</script>");
        assert!(s.contains(r#"s.textContent = "body { color: red } </style><script>x()</script>""#), "{s}");
    }

    #[test]
    fn only_local_devtools_addresses() {
        assert!(connect("ws://example.com:9222/devtools/page/x").is_err());
        assert!(connect("wss://127.0.0.1:9222/x").is_err());
    }

    /// With Chrome or Edge here: a red page reaches the engine, transparent outside the page's box.
    #[test]
    fn a_page_reaches_the_engine() {
        use ferrocore::engine::{Engine, EngineConfig, SceneItem, SourceKind};
        if browser().is_none() {
            return;
        }
        let video = ferrocore::video::VideoConfig {
            codec: ferrocore::video::VideoCodec::H264,
            input: ferrocore::video::PixelFormat::Nv12,
            width: 320,
            height: 180,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 1000,
            rate: ferrocore::video::RateControl::Cbr,
            keyframe_secs: 2,
            software: false,
        };
        let audio = ferrocore::audio::AudioConfig { sample_rate: 48_000, channels: 2, bitrate_kbps: 128 };
        let Ok(e) = Engine::new(EngineConfig { width: 320, height: 180, fps: 30, ..Default::default() }, video, audio)
        else {
            return;
        };
        e.set_source(1, SourceKind::Fed { width: 320, height: 180 }).unwrap_or_else(|x| panic!("{x}"));
        e.set_scene("Main", vec![SceneItem::new(1, ferrocore::Placement::default())]);
        e.go("Main").unwrap_or_else(|x| panic!("{x}"));
        let feed = e.fed(1).unwrap_or_else(|| panic!("fed"));
        let html = "data:text/html,<body style='margin:0'><div style='width:160px;height:180px;background:rgb(255,0,0)'></div></body>";
        let page = FedPage::open(html, 320, 180, "body { background: transparent }", 30, feed)
            .unwrap_or_else(|| panic!("no page"));
        let at = |px: &[u8], x: usize, y: usize| px.get((y * 320 + x) * 4..(y * 320 + x) * 4 + 3).map(<[u8]>::to_vec);
        let mut px = Vec::new();
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(250));
            px = e.snapshot().unwrap_or_default();
            if at(&px, 40, 90) == Some(vec![255, 0, 0]) {
                break;
            }
        }
        page.act(Do::Reload).unwrap_or_else(|x| panic!("{x}"));
        drop(page);
        assert_eq!(at(&px, 40, 90), Some(vec![255, 0, 0]), "the page's red box");
        assert_eq!(at(&px, 280, 90), Some(vec![0, 0, 0]), "nothing drawn where the page is clear");
    }
}
