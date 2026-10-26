//! The Ferrocast desktop app: a tray icon, one window, and the engine + local server in
//! the same process.
//!
//! The window is a thin view of `http://127.0.0.1:<port>/app/`; it has no IPC permissions
//! and talks to the engine over the same HTTP API a browser uses (ARCHITECTURE.md §1).
//! Closing the window hides it (unless "close to tray" is off); the engine keeps running in
//! the tray. Launch at login, the panic hotkey and window size follow the dashboard's
//! settings, and warnings show as system notifications while the window is hidden.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod studio;
mod studio_keys;

use ferrocast_engine::Engine;
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

const SHOW_SHORTCUT: &str = "CmdOrCtrl+Alt+Shift+F";
/// Passed by the login item: start in the tray without opening the window.
const MINIMIZED_ARG: &str = "--minimized";
/// How often the shell picks up settings changed in the dashboard.
const SETTINGS_EVERY: Duration = Duration::from_secs(2);

/// Settings the shell applies itself, shared with the window and shortcut handlers.
#[derive(Default)]
struct Shell {
    close_to_tray: AtomicBool,
    panic_shortcut: Mutex<Option<Shortcut>>,
}

/// What the running app holds on to.
struct Running {
    engine: Engine,
    base: String,
    token: String,
}

fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FERROCAST_HOME") {
        return PathBuf::from(dir);
    }
    // Debug builds never touch the installed app's data.
    let name = if cfg!(debug_assertions) { "Ferrocast-dev" } else { "Ferrocast" };
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join(name)
}

/// The log goes to the terminal and to `<data>/logs/ferrocast.log` (kept under 5 MB, one old
/// file beside it), so a restart or a crash can be looked into afterwards. A panic is written
/// there too.
fn init_tracing(dir: &std::path::Path) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tao=warn,wry=warn"));
    let logs = dir.join("logs");
    let file = std::fs::create_dir_all(&logs).ok().and_then(|()| {
        let path = logs.join("ferrocast.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
            let _ = std::fs::rename(&path, logs.join("ferrocast.1.log"));
        }
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
    });
    let to_file = file.map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f)));
    let _ = tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer()).with(to_file).try_init();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(%info, "Ferrocast stopped unexpectedly");
        default(info);
    }));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), pid = std::process::id(), "Ferrocast starting");
}

fn main() {
    let dir = data_dir();
    init_tracing(&dir);
    let minimized = std::env::args().any(|a| a == MINIMIZED_ARG);
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::Builder::new().args([MINIMIZED_ARG]).build())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    // The Studio's own shortcuts (push to talk also acts on letting go).
                    if studio_keys::handle(app, shortcut, event.state()) {
                        return;
                    }
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let is_panic = app.try_state::<Arc<Shell>>().is_some_and(|s| {
                        s.panic_shortcut.lock().unwrap_or_else(|p| p.into_inner()).as_ref() == Some(shortcut)
                    });
                    if is_panic {
                        toggle_panic(app);
                    } else if *shortcut == shortcut_of(SHOW_SHORTCUT) {
                        show_window(app);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let started = tauri::async_runtime::block_on(start(&dir));
            let running = match started {
                Ok(r) => r,
                Err(message) => {
                    tracing::error!(%message, "Ferrocast couldn't start");
                    show_startup_error(app.handle(), &message);
                    return Ok(());
                }
            };
            let engine = running.engine.clone();
            app.manage(running);
            app.manage(studio_keys::StudioKeys::new(engine.clone()));
            engine.set_studio_host(Arc::new(studio::DesktopStudio::new(app.handle().clone(), dir.clone())));
            // The Studio's saved shortcuts work from the start, Studio running or not.
            {
                let (app, engine) = (app.handle().clone(), engine.clone());
                tauri::async_runtime::spawn(async move {
                    if let Ok(s) = engine.studio_settings().await {
                        studio_keys::apply(&app, &s.hotkeys);
                    }
                });
            }
            let shell = Arc::new(Shell::default());
            shell.close_to_tray.store(engine.settings().close_to_tray, Ordering::Relaxed);
            app.manage(shell.clone());
            build_tray(app.handle())?;
            if let Err(e) = app.global_shortcut().register(shortcut_of(SHOW_SHORTCUT)) {
                tracing::warn!(error = %e, "couldn't register the show-window shortcut (another app may use it)");
            }
            tauri::async_runtime::spawn(follow_settings(app.handle().clone(), engine.clone(), shell));
            tauri::async_runtime::spawn(keep_updated(app.handle().clone(), engine.clone()));
            let pending = Pending::default();
            tauri::async_runtime::spawn(install_when_asked(app.handle().clone(), engine.clone(), pending.clone()));
            tauri::async_runtime::spawn(restart_when_asked(app.handle().clone(), engine.clone(), pending));
            tauri::async_runtime::spawn(notify_when_hidden(app.handle().clone(), engine));
            if !minimized {
                show_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
            {
                let app = window.app_handle();
                let _ = app.save_window_state(StateFlags::all());
                let to_tray = app.try_state::<Arc<Shell>>().is_none_or(|s| s.close_to_tray.load(Ordering::Relaxed));
                if to_tray {
                    // Closing hides: the engine keeps alerts and the bot running from the tray.
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    app.exit(0);
                }
            }
        })
        .build(tauri::generate_context!());
    let app = match app {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "the window system couldn't start");
            std::process::exit(1);
        }
    };
    app.run(|app, event| match event {
        // The last window closing must not quit a tray app; only "Quit" does (exit code
        // set). If startup failed there's no tray to quit from, so closing the error
        // window really exits.
        RunEvent::ExitRequested { api, code: None, .. } if app.try_state::<Running>().is_some() => {
            api.prevent_exit();
        }
        // macOS: clicking the Dock icon of a running app brings back the hidden window.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_window(app),
        RunEvent::Exit => tracing::info!("Ferrocast quitting"),
        _ => {}
    });
}

fn shortcut_of(s: &str) -> Shortcut {
    s.parse().unwrap_or_else(|_| Shortcut::new(None, tauri_plugin_global_shortcut::Code::F24))
}

/// The first check soon after start (the dashboard shows "checking" until then), then every
/// few hours; a check that failed (offline at login, say) tries again sooner.
const UPDATE_FIRST: Duration = Duration::from_secs(15);
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const UPDATE_RETRY: Duration = Duration::from_secs(15 * 60);
/// Download progress reaches the dashboard at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
/// Where to read about a release.
const RELEASES_URL: &str = "https://github.com/ArshPunisher/ferrocast/releases";
/// What latest.json says when a release has no notes of its own.
const PLACEHOLDER_NOTES: &str = "See the release notes on GitHub.";
/// Release notes longer than this are cut (the full text is on the release page).
const NOTES_MAX: usize = 6_000;

/// The signed feed for this build: a prerelease version reads the beta feed (betas and newer
/// stable releases), a stable version only the stable one, so the installed build decides.
fn update_feed() -> Option<String> {
    match ferrocast_engine::update_channel(env!("CARGO_PKG_VERSION")) {
        "beta" => Some("https://github.com/ArshPunisher/ferrocast/releases/download/beta/latest.json".into()),
        _ => None, // stable: the endpoint in tauri.conf.json
    }
}

/// A check gives up after this; a download after the long one (the plugin uses the same
/// timeout for the whole download, body included).
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// Time to open a connection, split across the addresses a host resolves to. Some networks
/// can't reach one of GitHub's download addresses; without this the system waits about 75
/// seconds on it before trying the next, longer than a check is allowed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

async fn check_update<R: tauri::Runtime>(
    app: &AppHandle<R>,
    timeout: Duration,
) -> Result<Option<tauri_plugin_updater::Update>, tauri_plugin_updater::Error> {
    let mut builder = app.updater_builder();
    if let Some(feed) = update_feed() {
        let url = feed.parse().map_err(tauri_plugin_updater::Error::UrlParse)?;
        builder = builder.endpoints(vec![url])?;
    }
    builder.timeout(timeout).configure_client(|c| c.connect_timeout(CONNECT_TIMEOUT)).build()?.check().await
}

/// What the feed's notes say, as plain text, without the "see GitHub" placeholder.
fn release_notes(body: Option<&str>) -> Option<String> {
    let text = body?.trim();
    if text.is_empty() || text == PLACEHOLDER_NOTES {
        return None;
    }
    let mut out: String = text.chars().take(NOTES_MAX).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    Some(out)
}

fn found_update(u: &tauri_plugin_updater::Update) -> ferrocast_engine::AvailableUpdate {
    ferrocast_engine::AvailableUpdate {
        notes_url: Some(format!("{RELEASES_URL}/tag/v{}", u.version)),
        notes: release_notes(u.body.as_deref()),
        published_at: u.raw_json["pub_date"].as_str().map(str::to_string),
        version: u.version.clone(),
    }
}

/// An updater error in words the streamer can act on (never a raw Rust error alone).
fn update_problem(e: &tauri_plugin_updater::Error) -> ferrocast_engine::UpdateProblem {
    use ferrocast_engine::UpdateProblem as P;
    use tauri_plugin_updater::Error as E;
    const LATER: &str = "Try again in a few minutes. If it keeps happening, download the newest version from ferrocast.clearcraftlabs.com/download.";
    match e {
        E::Reqwest(r) if r.is_connect() || r.is_timeout() || r.is_request() => P::new(
            "offline",
            "Ferrocast couldn't reach the update server.",
            "Check that this computer is online (and that a firewall or VPN allows github.com), then press Check for updates.",
        ),
        E::Reqwest(r) if r.is_decode() || r.is_body() => {
            P::new("download_failed", "The download stopped part way.", "Check your connection, then try again.")
        }
        E::Reqwest(_) | E::Network(_) => P::new("download_failed", "The update server refused the download.", LATER),
        E::ReleaseNotFound | E::Serialization(_) | E::Semver(_) | E::EmptyEndpoints | E::UrlParse(_) => {
            P::new("feed_unavailable", "The update feed isn't available right now.", LATER)
        }
        E::TargetNotFound(_) | E::TargetsNotFound(_) | E::UnsupportedOs | E::UnsupportedArch => P::new(
            "no_build",
            "The newest release has no automatic update for this computer.",
            "Download it from ferrocast.clearcraftlabs.com/download instead.",
        ),
        E::Minisign(_)
        | E::Base64(_)
        | E::SignatureUtf8(_)
        | E::SignedVersionMismatch { .. }
        | E::MissingSignedVersion => P::new(
            "bad_signature",
            "The download failed its signature check, so it was not installed. Nothing on this computer changed.",
            "It may have been damaged on the way. Try again later; if it keeps happening, download from ferrocast.clearcraftlabs.com/download.",
        ),
        E::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied => P::new(
            "no_permission",
            "Ferrocast isn't allowed to replace itself where it's installed.",
            if cfg!(target_os = "macos") {
                "Move Ferrocast into Applications (and open it from there), then try again."
            } else {
                "Install the update from ferrocast.clearcraftlabs.com/download with an account that may change the app."
            },
        ),
        E::AuthenticationFailed => P::new(
            "install_failed",
            "The password prompt was cancelled, so the update wasn't installed.",
            "Press Update now again and enter your computer's password.",
        ),
        other => P::new(
            "install_failed",
            format!("Something went wrong while installing ({other})."),
            "Ferrocast keeps running the current version. Try again, or download from ferrocast.clearcraftlabs.com/download.",
        ),
    }
}

/// A failed check: an answer that isn't a feed means the feed is unavailable, not a broken
/// download.
fn check_problem(e: &tauri_plugin_updater::Error) -> ferrocast_engine::UpdateProblem {
    match e {
        tauri_plugin_updater::Error::Reqwest(r) if r.is_decode() || r.status().is_some() => {
            update_problem(&tauri_plugin_updater::Error::ReleaseNotFound)
        }
        other => update_problem(other),
    }
}

/// One check: tells the engine it started and what it found (or why it failed). `true`
/// when the feed answered.
async fn run_check<R: tauri::Runtime>(app: &AppHandle<R>, engine: &Engine) -> bool {
    engine.update_check_started();
    match check_update(app, CHECK_TIMEOUT).await {
        Ok(found) => {
            if let Some(u) = &found {
                tracing::info!(version = %u.version, "an update is available");
            }
            engine.update_checked(Ok(found.as_ref().map(found_update)));
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "update check failed");
            engine.update_checked(Err(check_problem(&e)));
            false
        }
    }
}

/// Checks the signed update feed on a timer and whenever the dashboard asks, and tells the
/// engine what it found, so the dashboard can offer it (R-60). Signatures are checked
/// against the public key in tauri.conf.json when it downloads.
async fn keep_updated(app: AppHandle, engine: Engine) {
    engine.attach_updater();
    tokio::select! {
        () = tokio::time::sleep(UPDATE_FIRST) => {}
        () = engine.update_check_requested() => {}
    }
    loop {
        let wait = if run_check(&app, &engine).await { UPDATE_EVERY } else { UPDATE_RETRY };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = engine.update_check_requested() => {}
        }
    }
}

/// A downloaded, verified update waiting for "Restart to finish" (Windows: the installer
/// runs then, because it closes Ferrocast).
type Pending = Arc<Mutex<Option<(tauri_plugin_updater::Update, Vec<u8>)>>>;

/// Downloads (with progress), checks the signature and installs when the dashboard asks
/// (the engine already refused it while live). The new version runs after "Restart to
/// finish", never before: on macOS and Linux it is put in place now; on Windows the
/// installer would close Ferrocast at once, so it waits for the restart.
async fn install_when_asked(app: AppHandle, engine: Engine, pending: Pending) {
    loop {
        engine.update_install_requested().await;
        let update = match check_update(&app, DOWNLOAD_TIMEOUT).await {
            Ok(Some(u)) => u,
            Ok(None) => {
                engine.update_checked(Ok(None));
                engine.update_install_failed(ferrocast_engine::UpdateProblem::new(
                    "gone",
                    "That update is no longer offered.",
                    "You're on the newest version; nothing changed.",
                ));
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "update check before installing failed");
                engine.update_install_failed(check_problem(&e));
                continue;
            }
        };
        let version = update.version.clone();
        let mut got = 0u64;
        let mut last: Option<std::time::Instant> = None;
        let progress = engine.clone();
        let v = version.clone();
        // The plugin checks the signature as part of the download.
        let downloaded = update
            .download(
                move |chunk, total| {
                    got += chunk as u64;
                    if last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                        last = Some(std::time::Instant::now());
                        progress.update_progress(&v, got, total);
                    }
                },
                || {},
            )
            .await;
        let bytes = match downloaded {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "update download failed");
                engine.update_install_failed(update_problem(&e));
                continue;
            }
        };
        let size = bytes.len() as u64;
        engine.update_progress(&version, size, Some(size));
        // A stream may have started during the download: check again right before applying.
        if engine.update_blocked() {
            engine.update_install_failed(ferrocast_engine::UpdateProblem::new(
                "went_live",
                "You went live during the download, so it waits.",
                "Press Update now again after your stream.",
            ));
            continue;
        }
        engine.update_installing(&version);
        if cfg!(windows) {
            *pending.lock().unwrap_or_else(|p| p.into_inner()) = Some((update, bytes));
            engine.update_ready(&version);
            continue;
        }
        // Unpacking and moving the app takes a moment: off the async workers.
        let installed = tokio::task::spawn_blocking(move || update.install(bytes)).await;
        match installed {
            Ok(Ok(())) => {
                tracing::info!(%version, "update installed; waiting for the restart");
                engine.update_ready(&version);
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "update install failed");
                engine.update_install_failed(update_problem(&e));
            }
            Err(e) => {
                tracing::error!(error = %e, "the install task stopped");
                engine.update_failed("the install stopped unexpectedly");
            }
        }
    }
}

/// "Restart to finish" (the engine refuses it while live; checked again here). On Windows
/// this is when the installer runs: it closes Ferrocast and opens the new version.
async fn restart_when_asked(app: AppHandle, engine: Engine, pending: Pending) {
    loop {
        engine.update_restart_requested().await;
        if engine.update_blocked() {
            continue;
        }
        let _ = app.save_window_state(StateFlags::all());
        let waiting = pending.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some((update, bytes)) = waiting {
            // Returns only if the installer couldn't start (it exits the app otherwise).
            if let Err(e) = update.install(bytes) {
                tracing::error!(error = %e, "update install failed");
                engine.update_install_failed(update_problem(&e));
                continue;
            }
        }
        tracing::info!("restarting to finish the update");
        app.restart();
    }
}

/// Applies the settings the shell owns whenever the dashboard changes them: launch at
/// login, close to tray, and the panic hotkey (PanicConfig.hotkey).
async fn follow_settings(app: AppHandle, engine: Engine, shell: Arc<Shell>) {
    let mut login: Option<bool> = None;
    let mut hotkey: Option<String> = None;
    loop {
        let settings = engine.settings();
        shell.close_to_tray.store(settings.close_to_tray, Ordering::Relaxed);
        if login != Some(settings.launch_at_login) {
            let al = app.autolaunch();
            let result = if settings.launch_at_login { al.enable() } else { al.disable() };
            match result {
                Ok(()) => login = Some(settings.launch_at_login),
                Err(e) => {
                    tracing::warn!(error = %e, "couldn't change launch at login");
                    login = Some(settings.launch_at_login); // don't retry every two seconds
                }
            }
        }
        let wanted = engine.panic_state().1.hotkey.trim().to_string();
        if hotkey.as_deref() != Some(wanted.as_str()) {
            set_panic_hotkey(&app, &shell, &wanted);
            hotkey = Some(wanted);
        }
        tokio::time::sleep(SETTINGS_EVERY).await;
    }
}

fn set_panic_hotkey(app: &AppHandle, shell: &Shell, wanted: &str) {
    let mut slot = shell.panic_shortcut.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(old) = slot.take() {
        let _ = app.global_shortcut().unregister(old);
    }
    if wanted.is_empty() {
        return;
    }
    // Registered exactly as written ("Ctrl" stays Control on a Mac): it's global, and ⌘
    // combinations belong to the app in front.
    match wanted.parse::<Shortcut>() {
        Ok(sc) => match app.global_shortcut().register(sc) {
            Ok(()) => *slot = Some(sc),
            Err(e) => {
                tracing::warn!(hotkey = wanted, error = %e, "couldn't register the panic hotkey (another app may use it)")
            }
        },
        Err(e) => {
            tracing::warn!(hotkey = wanted, error = %e, "the panic hotkey isn't a shortcut this computer understands")
        }
    }
}

/// Warnings and errors (a platform needs signing in, panic mode, a refund) become system
/// notifications while the window is hidden, so the streamer sees them mid-game.
async fn notify_when_hidden(app: AppHandle, engine: Engine) {
    let mut live = engine.subscribe_live();
    loop {
        let frame = match live.recv().await {
            Ok(f) => f,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&frame.text) else { continue };
        if v["type"] != "notice" || !matches!(v["data"]["level"].as_str(), Some("warn" | "error")) {
            continue;
        }
        let visible = app.get_webview_window("main").and_then(|w| w.is_visible().ok()).unwrap_or(false);
        if visible {
            continue;
        }
        let body = v["data"]["message"].as_str().unwrap_or("").chars().take(300).collect::<String>();
        if let Err(e) = app.notification().builder().title("Ferrocast").body(body).show() {
            tracing::debug!(error = %e, "couldn't show a notification");
        }
    }
}

async fn start(dir: &std::path::Path) -> Result<Running, String> {
    let engine = Engine::start(dir).await.map_err(|e| e.to_string())?;
    // LAN access is read once here: changing it takes effect the next time Ferrocast starts.
    let opts = ferrocast_server::ServeOptions { lan: engine.settings().lan_access, ..Default::default() };
    let server = ferrocast_server::serve(engine.clone(), opts)
        .await
        .map_err(|e| format!("The local server couldn't start: {e}"))?;
    let token = server.sessions.desktop_token().to_string();
    // Dev builds only: tests drive this window's engine from outside (never in a release).
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("FERROCAST_DEBUG_TOKEN_FILE") {
        let _ = std::fs::write(path, format!("{} {}", server.base_url(), token));
    }
    Ok(Running { engine, base: server.base_url(), token })
}

/// Only web pages leave the app; `file:`, `javascript:` and custom schemes never do.
fn is_web(u: &tauri::Url) -> bool {
    matches!(u.scheme(), "http" | "https")
}

/// What the window does with a navigation: `(load it here, open it in the browser)`.
/// The window only ever shows our dashboard; links to the web open in the browser. A pair
/// link links a browser, never this window: "Open in browser" makes one.
fn navigation(u: &tauri::Url, origin: &str) -> (bool, bool) {
    let ours = u.as_str().starts_with(&format!("{origin}/"));
    let pair = ours && u.fragment().is_some_and(|f| f.starts_with("pair="));
    (ours && !pair, (!ours || pair) && is_web(u))
}

/// Tries at handing a page to the browser: the system can refuse one while the browser is
/// still starting or quitting.
const OPEN_ATTEMPTS: u32 = 3;
const OPEN_RETRY: Duration = Duration::from_millis(700);

/// Hands a web page to the default browser, off the window's thread: WebKit waits on the
/// navigation handler, and starting a browser that isn't running can take a while. When
/// every try fails the dashboard says so (the connect dialog keeps a link to copy) instead
/// of a log line nobody sees.
fn open_in_browser(app: &AppHandle, u: &tauri::Url) {
    let app = app.clone();
    let url = u.to_string();
    std::thread::spawn(move || {
        let opened = open_with_retry(|| app.opener().open_url(&url, None::<&str>), OPEN_ATTEMPTS, OPEN_RETRY);
        if let Err(e) = opened {
            tracing::error!(error = %e, "couldn't open the browser");
            if let Some(r) = app.try_state::<Running>() {
                r.engine.browser_didnt_open();
            }
        }
    });
}

/// Calls `open` until it works, up to `attempts` times with `wait` between; the last error.
fn open_with_retry<E>(mut open: impl FnMut() -> Result<(), E>, attempts: u32, wait: Duration) -> Result<(), E> {
    let mut tried = 1;
    loop {
        match open() {
            Ok(()) => return Ok(()),
            Err(e) if tried >= attempts => return Err(e),
            Err(_) => {
                tried += 1;
                std::thread::sleep(wait);
            }
        }
    }
}

/// Shows the main window, creating it if it was never opened.
fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let Some(running) = app.try_state::<Running>() else {
        return;
    };
    // Dev builds only: FERROCAST_DEBUG_ROUTE (e.g. #/studio/golive) opens there, for testing.
    #[cfg(debug_assertions)]
    let route = std::env::var("FERROCAST_DEBUG_ROUTE").ok().filter(|r| r.starts_with("#/")).unwrap_or_default();
    #[cfg(not(debug_assertions))]
    let route = String::new();
    let Ok(url) = format!("{}/app/{route}", running.base).parse() else {
        return;
    };
    let api = serde_json::to_string(&running.base).unwrap_or_default();
    let token = serde_json::to_string(&running.token).unwrap_or_default();
    let script = format!("window.__FERROCAST__ = Object.freeze({{ api: {api}, token: {token}, desktop: true }});");
    let origin = running.base.clone();
    let opener = app.clone();
    let popup_opener = app.clone();
    let built = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title("Ferrocast")
        .inner_size(1240.0, 800.0)
        .min_inner_size(380.0, 600.0)
        .initialization_script(&script)
        .on_navigation(move |u| {
            let (allow, browser) = navigation(u, &origin);
            if browser {
                open_in_browser(&opener, u);
            }
            allow
        })
        // `target="_blank"` links and `window.open` ask for a new window. The webview makes
        // none on its own (the click simply did nothing, 1.0.0), so web pages go to the
        // browser and the window stays the dashboard.
        .on_new_window(move |u, _features| {
            if is_web(&u) {
                open_in_browser(&popup_opener, &u);
            }
            tauri::webview::NewWindowResponse::Deny
        })
        .build();
    if let Err(e) = built {
        tracing::error!(error = %e, "couldn't open the window");
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItemBuilder::with_id("open", "Open Ferrocast").build(app)?;
    let test = MenuItemBuilder::with_id("test", "Test an alert").build(app)?;
    let pause = MenuItemBuilder::with_id("pause", "Pause or resume alerts").build(app)?;
    let panic = MenuItemBuilder::with_id("panic", "Panic mode on/off").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit Ferrocast").build(app)?;
    let version = MenuItemBuilder::with_id("version", concat!("Ferrocast ", env!("CARGO_PKG_VERSION")))
        .enabled(false)
        .build(app)?;
    let menu = MenuBuilder::new(app)
        .item(&version)
        .item(&PredefinedMenuItem::separator(app)?)
        .items(&[&open, &test, &pause])
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&panic)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&quit)
        .build()?;
    let mut tray = TrayIconBuilder::with_id("main").tooltip("Ferrocast").menu(&menu).show_menu_on_left_click(true);
    // macOS: a black template image of the mark, so the menu bar tints it for light, dark and
    // highlighted states. Elsewhere the tray shows the app icon.
    #[cfg(target_os = "macos")]
    {
        tray = tray.icon(tauri::include_image!("icons/tray-template.png")).icon_as_template(true);
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.on_menu_event(|app, event| match event.id().as_ref() {
        "open" => show_window(app),
        "test" => with_engine(app, |e| async move {
            let _ = e.test("follow", None).await;
        }),
        "pause" => with_engine(app, |e| async move {
            if e.queue().await.paused {
                e.queue_resume().await;
            } else {
                e.queue_pause().await;
            }
        }),
        "panic" => toggle_panic(app),
        "quit" => app.exit(0),
        _ => {}
    })
    .build(app)?;
    Ok(())
}

fn with_engine<F, Fut>(app: &AppHandle, f: F)
where
    F: FnOnce(Engine) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    if let Some(r) = app.try_state::<Running>() {
        let engine = r.engine.clone();
        tauri::async_runtime::spawn(f(engine));
    }
}

fn toggle_panic(app: &AppHandle) {
    with_engine(app, |e| async move {
        let on = !e.panic_state().0;
        if let Err(err) = e.panic(on).await {
            tracing::warn!(%err, "panic mode failed");
        }
    });
}

/// Startup failed (a newer or damaged database, no port): say why instead of vanishing.
fn show_startup_error(app: &AppHandle, message: &str) {
    let text = serde_json::to_string(message).unwrap_or_default();
    let html = format!(
        "document.body.innerHTML = ''; const p = document.createElement('p'); \
         p.textContent = 'Ferrocast couldn\\'t start: ' + {text}; document.body.appendChild(p);"
    );
    let _ = WebviewWindowBuilder::new(app, "error", WebviewUrl::App("index.html".into()))
        .title("Ferrocast")
        .inner_size(520.0, 220.0)
        .initialization_script(format!("window.addEventListener('DOMContentLoaded', () => {{ {html} }});"))
        .build();
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "http://127.0.0.1:7878";

    fn nav(u: &str) -> (bool, bool) {
        navigation(&u.parse().unwrap(), BASE)
    }

    #[test]
    fn the_dashboard_loads_in_the_window() {
        assert_eq!(nav("http://127.0.0.1:7878/app/"), (true, false));
        assert_eq!(nav("http://127.0.0.1:7878/app/#/alerts"), (true, false));
    }

    #[test]
    fn web_pages_go_to_the_browser() {
        assert_eq!(nav("https://www.twitch.tv/activate"), (false, true));
        assert_eq!(nav("https://rzp.io/rzp/abc"), (false, true));
        // Another port on the same host is not ours.
        assert_eq!(nav("http://127.0.0.1:7879/app/"), (false, true));
        // A look-alike that starts with our origin but goes to another host.
        assert_eq!(nav("http://127.0.0.1:7878@evil.test/app/"), (false, true));
    }

    /// What `oauth::begin` makes: long, form-encoded (`+` between scopes), and with our own
    /// address inside the query as the redirect.
    const GOOGLE: &str = "https://accounts.google.com/o/oauth2/v2/auth?client_id=701866487620-p0npjp4t20ivffdcn89s5jhro9vik5cl.apps.googleusercontent.com&redirect_uri=http%3A%2F%2F127.0.0.1%3A53211%2Fcallback&response_type=code&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fyoutube.force-ssl+https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fyoutube.readonly&code_challenge=N07XXTGhamNvXHT0LS_emRx0ieA0Ua8QfloveAbcdefg&code_challenge_method=S256&state=abcdefghijklmnopqrstuvwxyzAB_-&access_type=offline&prompt=consent";

    #[test]
    fn the_youtube_sign_in_page_goes_to_the_browser() {
        assert_eq!(nav(GOOGLE), (false, true));
        let u: tauri::Url = GOOGLE.parse().unwrap();
        assert_eq!(u.as_str(), GOOGLE, "parsing keeps the URL as the page sent it");
        // Our origin in the query (unencoded, as some pages send it) doesn't make it ours.
        assert_eq!(nav("https://accounts.google.com/auth?redirect_uri=http://127.0.0.1:7878/app/"), (false, true));
        // Kick's sign-in goes through the relay.
        assert_eq!(nav("https://relay.ferrocast.app/kick/start?state=a+b%20c"), (false, true));
    }

    #[test]
    fn opening_the_browser_tries_again_then_gives_up() {
        let mut calls = 0;
        let r = open_with_retry(
            || {
                calls += 1;
                if calls < 2 { Err("busy") } else { Ok(()) }
            },
            3,
            Duration::ZERO,
        );
        assert_eq!((r, calls), (Ok(()), 2), "the second try worked");
        let mut calls = 0;
        let r = open_with_retry(
            || {
                calls += 1;
                Err::<(), _>(calls)
            },
            OPEN_ATTEMPTS,
            Duration::ZERO,
        );
        assert_eq!(r, Err(OPEN_ATTEMPTS), "the last error, after every try");
        assert_eq!(calls, OPEN_ATTEMPTS);
        let mut calls = 0;
        assert_eq!(
            open_with_retry(
                || {
                    calls += 1;
                    Ok::<_, ()>(())
                },
                3,
                Duration::ZERO
            ),
            Ok(())
        );
        assert_eq!(calls, 1, "no second try after it worked");
    }

    #[test]
    fn a_pair_link_opens_in_the_browser() {
        assert_eq!(nav("http://127.0.0.1:7878/app/?browser=1#pair=abc"), (false, true));
    }

    #[test]
    fn other_schemes_never_leave_the_app() {
        assert_eq!(nav("file:///etc/passwd"), (false, false));
        assert_eq!(nav("javascript:alert(1)"), (false, false));
        assert!(!is_web(&"mailto:a@b.c".parse().unwrap()));
        assert!(is_web(&"https://example.com".parse().unwrap()));
    }
}

/// The real updater plugin in a mock app, against feeds served on this computer: the
/// published 1.0.1 feed is a fixture, so a feed or plugin change that hides updates fails here.
#[cfg(test)]
mod update_tests {
    use super::*;
    use tauri::test::{MockRuntime, mock_builder, mock_context, noop_assets};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FEED: &str = include_str!("../tests/fixtures/latest-1.0.1.json");
    const CONF: &str = include_str!("../tauri.conf.json");

    fn pubkey() -> String {
        let conf: serde_json::Value = serde_json::from_str(CONF).unwrap();
        conf["plugins"]["updater"]["pubkey"].as_str().unwrap().to_string()
    }

    fn app(version: &str, endpoint: &str) -> tauri::App<MockRuntime> {
        let mut ctx = mock_context(noop_assets());
        ctx.config_mut().plugins.0.insert(
            "updater".into(),
            serde_json::json!({
                "pubkey": pubkey(),
                "endpoints": [endpoint],
                // Only here: the test feeds are plain http on 127.0.0.1.
                "dangerousInsecureTransportProtocol": true,
            }),
        );
        ctx.package_info_mut().version = version.parse().unwrap();
        mock_builder().plugin(tauri_plugin_updater::Builder::new().build()).build(ctx).unwrap()
    }

    /// Serves one answer to every request; returns its address.
    async fn serve(status: &'static str, body: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        format!("http://{addr}/latest.json")
    }

    async fn check(version: &str, endpoint: &str) -> Result<Option<tauri_plugin_updater::Update>, String> {
        let app = app(version, endpoint);
        // The stable feed is the configured one; a beta build would swap it for the beta feed.
        let updater = app.handle().updater_builder().timeout(CHECK_TIMEOUT).build().map_err(|e| e.to_string())?;
        updater.check().await.map_err(|e| check_problem(&e).code)
    }

    /// The problem's code from a check that should have failed.
    async fn problem(version: &str, endpoint: &str) -> String {
        match check(version, endpoint).await {
            Ok(found) => panic!("expected a problem, found {:?}", found.map(|u| u.version)),
            Err(code) => code,
        }
    }

    #[tokio::test]
    async fn the_published_feed_offers_1_0_1_to_1_0_0_and_older_betas() {
        let url = serve("200 OK", FEED.into()).await;
        for installed in ["1.0.0", "0.1.0-beta.10", "0.1.0-beta.11"] {
            let found = check(installed, &url).await.unwrap().unwrap_or_else(|| panic!("{installed} saw nothing"));
            assert_eq!(found.version, "1.0.1", "{installed}");
            let shown = found_update(&found);
            assert_eq!(
                shown.notes_url.as_deref(),
                Some("https://github.com/ArshPunisher/ferrocast/releases/tag/v1.0.1")
            );
            assert_eq!(shown.notes, None, "the placeholder isn't release notes");
            assert_eq!(shown.published_at.as_deref(), Some("2026-10-20T11:16:05Z"));
        }
        assert!(check("1.0.1", &url).await.unwrap().is_none(), "up to date");
        assert!(check("1.1.0", &url).await.unwrap().is_none(), "never a downgrade");
    }

    #[test]
    fn every_signature_in_the_feed_is_from_our_key() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let key_id = |text: &str| {
            let raw = b64.decode(text.lines().nth(1).unwrap()).unwrap();
            raw[2..10].to_vec()
        };
        let pk = String::from_utf8(b64.decode(pubkey()).unwrap()).unwrap();
        let ours = key_id(&pk);
        let feed: serde_json::Value = serde_json::from_str(FEED).unwrap();
        let platforms = feed["platforms"].as_object().unwrap();
        for (target, entry) in platforms {
            let sig = String::from_utf8(b64.decode(entry["signature"].as_str().unwrap()).unwrap()).unwrap();
            assert_eq!(key_id(&sig), ours, "{target} is signed with another key than tauri.conf.json's");
            assert!(sig.contains("version:1.0.1"), "{target}: the signature names its version");
        }
        // What the updater asks for on each computer we ship (`{os}-{arch}`): one universal
        // Mac build serves both chips, and deb/rpm installs find their own entry.
        for target in [
            "darwin-aarch64",
            "darwin-x86_64",
            "windows-x86_64",
            "linux-x86_64",
            "linux-x86_64-deb",
            "linux-x86_64-rpm",
        ] {
            assert!(platforms.contains_key(target), "the feed has no {target}");
        }
    }

    #[tokio::test]
    async fn a_missing_feed_says_so_instead_of_up_to_date() {
        let url = serve("404 Not Found", "Not Found".into()).await;
        assert_eq!(problem("1.0.0", &url).await, "feed_unavailable");
        let url = serve("200 OK", "<html>not json</html>".into()).await;
        assert_eq!(problem("1.0.0", &url).await, "feed_unavailable");
    }

    #[tokio::test]
    async fn offline_says_so() {
        // A port nothing listens on.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert_eq!(problem("1.0.0", &format!("http://{free}/latest.json")).await, "offline");
    }

    #[tokio::test]
    async fn a_release_without_this_computer_says_so() {
        let mut feed: serde_json::Value = serde_json::from_str(FEED).unwrap();
        feed["platforms"] = serde_json::json!({ "plan9-mips": feed["platforms"]["linux-x86_64"].clone() });
        let url = serve("200 OK", feed.to_string()).await;
        assert_eq!(problem("1.0.0", &url).await, "no_build");
        assert!(check("1.0.1", &url).await.unwrap().is_none(), "not an update: no error either");
    }

    #[test]
    fn signature_and_permission_problems_have_their_own_words() {
        use tauri_plugin_updater::Error as E;
        let sig = update_problem(&E::SignatureUtf8("x".into()));
        assert_eq!(sig.code, "bad_signature");
        assert!(sig.message.contains("signature"));
        let mismatch = update_problem(&E::SignedVersionMismatch { signed: "1.0.0".into(), announced: "9.0.0".into() });
        assert_eq!(mismatch.code, "bad_signature");
        let perm = update_problem(&E::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
        assert_eq!(perm.code, "no_permission");
        assert_eq!(update_problem(&E::Network("502".into())).code, "download_failed");
        let other = update_problem(&E::BinaryNotFoundInArchive);
        assert_eq!(other.code, "install_failed");
        for p in [sig, perm, other] {
            assert!(!p.message.is_empty() && !p.hint.is_empty(), "{p:?}");
        }
    }

    #[test]
    fn release_notes_skip_the_placeholder_and_stay_short() {
        assert_eq!(release_notes(None), None);
        assert_eq!(release_notes(Some("  ")), None);
        assert_eq!(release_notes(Some(PLACEHOLDER_NOTES)), None);
        assert_eq!(release_notes(Some("\n- Faster alerts\n")).as_deref(), Some("- Faster alerts"));
        let long = "é".repeat(NOTES_MAX + 10);
        let cut = release_notes(Some(&long)).unwrap();
        assert_eq!(cut.chars().count(), NOTES_MAX + 1);
        assert!(cut.ends_with('…'));
    }
}
