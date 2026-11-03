//! Setup's window: a fixed 720 x 470 frameless window showing `ui/index.html` in WebView2.
//!
//! The page asks for things with `window.ipc.postMessage(JSON)`; setup answers by calling
//! `setup.event(...)` on it. The page is setup's own, served from inside this program; it loads
//! nothing from the internet.

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event as TaoEvent, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::{IconExtWindows, WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Icon, WindowBuilder};
use wry::WebViewBuilder;
use wry::http::{Request, Response, StatusCode};

use super::{Event, INSTALL_STEPS, InstallJob, Package, UNINSTALL_STEPS, UninstallJob, install_dir_for, log, open_app, sys, tail_log};
use crate::args::Install;
use crate::flow;

const PAGE: &str = include_str!("../../ui/index.html");
const FONTS: [(&str, &[u8]); 3] = [
    ("bricolage.woff2", include_bytes!("../../ui/fonts/bricolage.woff2")),
    ("hanken.woff2", include_bytes!("../../ui/fonts/hanken.woff2")),
    ("jetbrains-mono.woff2", include_bytes!("../../ui/fonts/jetbrains-mono.woff2")),
];

/// Links the page may open; anything else is ignored.
const LINKS: [(&str, &str); 2] =
    [("site", "https://ferrocast.clearcraftlabs.com"), ("feedback", "https://github.com/ArshPunisher/ferrocast/issues/new")];

pub enum Start {
    Install { pkg: Arc<Package>, dir: PathBuf, old_version: Option<String>, opts: Install },
    Uninstall { dir: PathBuf, main: String, extras: u64 },
}

enum Ui {
    Script(String),
    Exit(u8),
}

/// What the page sends.
#[derive(Debug, Deserialize)]
#[serde(tag = "do", rename_all = "camelCase")]
enum Ask {
    Install { dir: String, desktop: bool },
    Browse { dir: String },
    Space { dir: String },
    Uninstall { data: bool, extras: bool },
    Cancel,
    Finish { open: bool },
    Close,
    Minimize,
    Drag,
    Link { to: String },
}

struct Session {
    start: Start,
    busy: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// Where the last finished install went (for "Open Ferrocast now").
    installed: Mutex<Option<PathBuf>>,
    hwnd: Mutex<isize>,
}

fn send(proxy: &EventLoopProxy<Ui>, call: &str, value: &serde_json::Value) {
    let _ = proxy.send_event(Ui::Script(format!("window.setup && window.setup.{call}({value})")));
}

fn space_info(dir: &std::path::Path, need: u64, main: &str) -> serde_json::Value {
    let free = sys::free_space(dir);
    json!({
        "dir": dir.display().to_string(),
        "drive": sys::drive_of(dir),
        "free": free,
        "need": need,
        "enough": free.is_none_or(|f| f >= need),
        "ours": flow::ours_or_empty(dir, main),
    })
}

/// The first state the page draws, given before it loads.
fn boot(start: &Start) -> serde_json::Value {
    match start {
        Start::Install { pkg, dir, old_version, opts } => {
            let m = &pkg.manifest;
            let need = m.total + m.total / 10;
            json!({
                "flow": if opts.update { "update" } else { "install" },
                "passive": opts.passive,
                "version": m.version,
                "oldVersion": old_version,
                "size": m.total,
                "space": space_info(dir, need, &m.main),
                "defaultDir": dir.display().to_string(),
                "running": !sys::running_in(dir).is_empty(),
                "desktopExists": sys::desktop_dir().is_some_and(|d| d.join("Ferrocast.lnk").exists()),
                "steps": INSTALL_STEPS,
            })
        }
        Start::Uninstall { dir, extras, .. } => json!({
            "flow": "uninstall",
            "dir": dir.display().to_string(),
            "extras": extras,
            "running": !sys::running_in(dir).is_empty(),
            "steps": UNINSTALL_STEPS,
        }),
    }
}

fn serve(req: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let path = req.uri().path().trim_start_matches('/');
    let (body, kind): (&'static [u8], &str) = match path {
        "" | "index.html" => (PAGE.as_bytes(), "text/html; charset=utf-8"),
        p => match FONTS.iter().find(|(name, _)| p == format!("fonts/{name}")) {
            Some((_, bytes)) => (bytes, "font/woff2"),
            None => {
                return Response::builder().status(StatusCode::NOT_FOUND).body(Cow::Borrowed(&b""[..])).unwrap_or_default();
            }
        },
    };
    Response::builder()
        .header("Content-Type", kind)
        // Only this page's own files and inline code run in it.
        .header("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; font-src http://setup.localhost; img-src data:")
        .body(Cow::Borrowed(body))
        .unwrap_or_default()
}

pub fn show(start: Start) -> ExitCode {
    let mut event_loop = EventLoopBuilder::<Ui>::with_user_event();
    let event_loop = event_loop.build();
    let proxy = event_loop.create_proxy();
    let title = match &start {
        Start::Install { opts, .. } if opts.update => "Ferrocast Update",
        Start::Install { .. } => "Ferrocast Setup",
        Start::Uninstall { .. } => "Uninstall Ferrocast",
    };
    let light = sys::light_theme();
    let window = match WindowBuilder::new()
        .with_title(title)
        .with_inner_size(LogicalSize::new(720.0, 470.0))
        .with_resizable(false)
        .with_maximizable(false)
        .with_decorations(false)
        .with_undecorated_shadow(true)
        .with_visible(false)
        .with_window_icon(Icon::from_resource(1, None).ok())
        .build(&event_loop)
    {
        Ok(w) => w,
        Err(e) => {
            super::complain(&format!("Setup couldn't open its window: {e}"));
            return ExitCode::FAILURE;
        }
    };
    if let Some(m) = window.current_monitor() {
        let (ms, ws, mp) = (m.size(), window.outer_size(), m.position());
        let x = mp.x + (ms.width.saturating_sub(ws.width) / 2) as i32;
        let y = mp.y + (ms.height.saturating_sub(ws.height) / 2) as i32;
        window.set_outer_position(PhysicalPosition::new(x, y));
    }

    let boot_json = boot(&start);
    let session = Arc::new(Session {
        start,
        busy: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
        installed: Mutex::new(None),
        hwnd: Mutex::new(window.hwnd()),
    });
    let bg = if light { (242, 243, 246, 255) } else { (16, 18, 22, 255) };
    let s = session.clone();
    let p = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_custom_protocol("setup".into(), |_, req| serve(req))
        .with_url("http://setup.localhost/index.html")
        .with_initialization_script(format!("window.__SETUP__ = {boot_json};"))
        .with_background_color(bg)
        .with_devtools(false)
        .with_ipc_handler(move |req: Request<String>| match serde_json::from_str::<Ask>(req.body()) {
            Ok(ask) => handle(&s, &p, ask),
            Err(e) => log(&format!("page sent {:?}: {e}", req.body())),
        })
        .build(&window);
    let webview = match webview {
        Ok(w) => w,
        Err(e) => {
            super::complain(&format!("Setup couldn't draw its window (WebView2): {e}"));
            return ExitCode::FAILURE;
        }
    };
    window.set_visible(true);

    // Dragging and minimizing come from the page's own title bar.
    let busy = session.busy.clone();
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            TaoEvent::UserEvent(Ui::Script(js)) => {
                let _ = webview.evaluate_script(&js);
            }
            TaoEvent::UserEvent(Ui::Exit(code)) => {
                *flow = ControlFlow::ExitWithCode(i32::from(code));
            }
            TaoEvent::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // Alt+F4 while working asks the page to offer Cancel instead.
                if busy.load(Ordering::Relaxed) {
                    let _ = webview.evaluate_script("window.setup && window.setup.closeAsked()");
                } else {
                    *flow = ControlFlow::ExitWithCode(1);
                }
            }
            _ => {}
        }
        if let ControlFlow::ExitWithCode(_) = *flow
            && std::env::current_exe().is_ok_and(|e| e.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ferrocast-uninstall-")))
                && let Ok(me) = std::env::current_exe() {
                    sys::delete_after_exit(&me);
                }
        // The window lives as long as the loop.
        let _ = &window;
    })
}

fn handle(s: &Arc<Session>, proxy: &EventLoopProxy<Ui>, ask: Ask) {
    match ask {
        Ask::Install { dir, desktop } => {
            let Start::Install { pkg, opts, .. } = &s.start else { return };
            if s.busy.swap(true, Ordering::SeqCst) {
                return;
            }
            s.stop.store(false, Ordering::SeqCst);
            let dir = PathBuf::from(dir);
            let job = InstallJob { dir: dir.clone(), desktop, opts: opts.clone() };
            let (pkg, proxy, busy, stop, s2) = (pkg.clone(), proxy.clone(), s.busy.clone(), s.stop.clone(), s.clone());
            std::thread::spawn(move || {
                let result = job.run(&pkg, &stop, &mut |e| send(&proxy, "event", &json!(e)));
                busy.store(false, Ordering::SeqCst);
                match result {
                    Ok(()) => {
                        if let Ok(mut i) = s2.installed.lock() {
                            *i = Some(dir.join(&pkg.manifest.main));
                        }
                        // An update reopens the app and closes itself.
                        if job.opts.passive || job.opts.update {
                            if let Some(args) = &job.opts.relaunch {
                                open_app(&dir.join(&pkg.manifest.main), args);
                            }
                            std::thread::sleep(Duration::from_millis(2200));
                            let _ = proxy.send_event(Ui::Exit(0));
                        }
                    }
                    Err(e) => {
                        log(&format!("install failed: {e}"));
                        let (title, detail) = e.words();
                        send(&proxy, "event", &json!(Event::Error { title, detail, log: tail_log(12) }));
                    }
                }
            });
        }
        Ask::Uninstall { data, extras } => {
            let Start::Uninstall { dir, main, .. } = &s.start else { return };
            if s.busy.swap(true, Ordering::SeqCst) {
                return;
            }
            let job = UninstallJob { dir: dir.clone(), main: main.clone(), delete_data: data, remove_extras: extras };
            let (proxy, busy) = (proxy.clone(), s.busy.clone());
            std::thread::spawn(move || {
                let result = job.run(&mut |e| send(&proxy, "event", &json!(e)));
                busy.store(false, Ordering::SeqCst);
                if let Err(e) = result {
                    log(&format!("uninstall failed: {e}"));
                    let (title, detail) = e.words();
                    send(&proxy, "event", &json!(Event::Error { title, detail, log: tail_log(12) }));
                }
            });
        }
        Ask::Browse { dir } => {
            let owner = s.hwnd.lock().map(|h| *h).unwrap_or_default();
            if let Some(picked) = sys::pick_folder(owner, std::path::Path::new(&dir)) {
                send_space(s, proxy, &install_dir_for(&picked));
            }
        }
        Ask::Space { dir } => send_space(s, proxy, std::path::Path::new(&dir)),
        Ask::Cancel => {
            if s.busy.load(Ordering::SeqCst) {
                s.stop.store(true, Ordering::SeqCst);
            } else {
                let _ = proxy.send_event(Ui::Exit(1));
            }
        }
        Ask::Finish { open } => {
            if open
                && let Some(exe) = s.installed.lock().ok().and_then(|i| i.clone()) {
                    open_app(&exe, &[]);
                }
            let _ = proxy.send_event(Ui::Exit(0));
        }
        Ask::Close => {
            if !s.busy.load(Ordering::SeqCst) {
                let _ = proxy.send_event(Ui::Exit(0));
            }
        }
        Ask::Minimize => send_window(s, WindowAsk::Minimize),
        Ask::Drag => send_window(s, WindowAsk::Drag),
        Ask::Link { to } => {
            if let Some((_, url)) = LINKS.iter().find(|(k, _)| *k == to) {
                sys::open_url(url);
            }
        }
    }
}

fn send_space(s: &Session, proxy: &EventLoopProxy<Ui>, dir: &std::path::Path) {
    let Start::Install { pkg, .. } = &s.start else { return };
    let need = pkg.manifest.total + pkg.manifest.total / 10;
    send(proxy, "space", &space_info(dir, need, &pkg.manifest.main));
}

enum WindowAsk {
    Minimize,
    Drag,
}

/// The page's title bar: dragging starts the window's own move loop; minimizing is a
/// plain Windows call, both on the window's thread (the IPC handler runs there).
fn send_window(s: &Session, ask: WindowAsk) {
    let hwnd = s.hwnd.lock().map(|h| *h).unwrap_or_default();
    match ask {
        WindowAsk::Minimize => sys::minimize(hwnd),
        WindowAsk::Drag => sys::start_drag(hwnd),
    }
}
