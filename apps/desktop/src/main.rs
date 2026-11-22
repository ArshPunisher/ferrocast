//! The Ferrocast desktop app: a tray icon, one window, and the engine + local server in
//! the same process.
//!
//! The window is a thin view of `http://127.0.0.1:<port>/app/`; it has no IPC permissions
//! and talks to the engine over the same HTTP API a browser uses (ARCHITECTURE.md §1).
//! Closing the window hides it (unless "close to tray" is off); the engine keeps running in
//! the tray. Launch at login, the panic hotkey and window size follow the dashboard's
//! settings, and warnings show as system notifications while the window is hidden.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Debug builds of the whole app are big enough that macOS's linker notes its unwind table can't
// be compact ("__eh_frame section too large"): harmless, and CI turns warnings into errors.
#![allow(linker_messages)]
// Proving a drawing job Send goes deep through wgpu's types (pictures drawn off the
// window's thread).
#![recursion_limit = "256"]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(any(windows, test))]
mod crash;
mod log_limit;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[cfg(target_os = "macos")]
mod mac_quit;
#[cfg(target_os = "macos")]
mod mac_update;
mod menu;
mod quit;
mod store;
mod studio;
mod studio_history;
mod studio_keys;
mod studio_library;
mod whisper;

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
/// Launch directly in Safe Mode (no third-party plugins, VSTs, auto-start, or auto-loaded collection).
const SAFE_MODE_ARG: &str = "--safe-mode";
/// How often the shell picks up settings changed in the dashboard.
const SETTINGS_EVERY: Duration = Duration::from_secs(2);

/// Settings the shell applies itself, shared with the window and shortcut handlers.
#[derive(Default)]
struct Shell {
    close_to_tray: AtomicBool,
    /// The tray icon was made. Without one, closing the window quits: hidden, nothing could
    /// bring it back.
    has_tray: AtomicBool,
    panic_shortcut: Mutex<Option<Shortcut>>,
}

/// What the running app holds on to.
struct Running {
    engine: Engine,
    base: String,
    token: String,
}

pub(crate) fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FERROCAST_HOME") {
        // Absolute: on Windows the Studio moves the working folder to OBS's (studio crate).
        let dir = PathBuf::from(dir);
        return std::path::absolute(&dir).unwrap_or(dir);
    }
    // Debug builds never touch the installed app's data.
    let name = if cfg!(debug_assertions) { "Ferrocast-dev" } else { "Ferrocast" };
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join(name)
}

/// Maximum bytes in `ferrocast.log` before it rotates to `ferrocast.1.log` while running.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Size-capped rotating log writer: keeps `ferrocast.log` under `max_bytes` with at most one
/// rotated backup (`ferrocast.1.log`) even across week-long sessions.
struct RotatingLogWriter {
    path: PathBuf,
    backup: PathBuf,
    max_bytes: u64,
    written: u64,
    file: Option<std::fs::File>,
}

impl RotatingLogWriter {
    fn open(dir: &std::path::Path, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("ferrocast.log");
        let backup = dir.join("ferrocast.1.log");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() >= max_bytes) {
            let _ = std::fs::remove_file(&backup);
            let _ = std::fs::rename(&path, &backup);
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self { path, backup, max_bytes: max_bytes.max(1), written, file: Some(file) })
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        if let Some(mut f) = self.file.take() {
            let _ = std::io::Write::flush(&mut f);
        }
        // Remove previous backup first so rename succeeds on Windows as well as Unix.
        let _ = std::fs::remove_file(&self.backup);
        let _ = std::fs::rename(&self.path, &self.backup);
        let file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(&self.path)?;
        self.written = 0;
        self.file = Some(file);
        Ok(())
    }
}

impl std::io::Write for RotatingLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written > 0 && self.written.saturating_add(buf.len() as u64) > self.max_bytes {
            let _ = self.rotate();
        }
        let f = match self.file.as_mut() {
            Some(f) => f,
            None => {
                let file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
                self.written = file.metadata().map(|m| m.len()).unwrap_or(0);
                self.file.insert(file)
            }
        };
        let n = f.write(buf)?;
        self.written = self.written.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            f.flush()?;
        }
        Ok(())
    }
}

/// The file log's writer, to flush it at quit.
static LOG_FILE: std::sync::OnceLock<NonBlockingLog> = std::sync::OnceLock::new();

/// How many log lines wait for the writer thread before new ones are dropped.
const LOG_QUEUE: usize = 8_192;

enum LogMsg {
    Line(Vec<u8>),
    /// Everything before this is written; then it answers.
    Flush(std::sync::mpsc::SyncSender<()>),
}

/// The log file's writer, off the threads that log: `write` hands the line to a thread of its own and
/// never waits (a full queue drops the line), so a slow disk, a rotation or a locked file can't park a
/// runtime worker or the audio thread that happens to log. (A log line through the file layer used to
/// be a `std` mutex and a write on the logging thread: with a single-worker runtime that was a way to
/// stall the Studio's sender with nothing logged.)
#[derive(Clone)]
struct NonBlockingLog {
    tx: std::sync::mpsc::SyncSender<LogMsg>,
    /// Lines dropped because the queue was full, not yet said in the log.
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl NonBlockingLog {
    fn spawn(mut inner: impl std::io::Write + Send + 'static, queue: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<LogMsg>(queue);
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let said = dropped.clone();
        let _ = std::thread::Builder::new().name("ferrocast-log".into()).spawn(move || {
            while let Ok(m) = rx.recv() {
                match m {
                    LogMsg::Line(l) => {
                        let _ = inner.write_all(&l);
                    }
                    LogMsg::Flush(done) => {
                        let _ = inner.flush();
                        let _ = done.send(());
                    }
                }
                // A gap in the log says so, once the writer has caught up enough to write it.
                let n = said.swap(0, std::sync::atomic::Ordering::AcqRel);
                if n > 0 {
                    let _ =
                        inner.write_all(format!("{n} log lines were dropped (the log couldn't keep up)\n").as_bytes());
                }
            }
        });
        Self { tx, dropped }
    }

    /// Waits (at most `wait`) until what was logged before is written: for a panic or a quit.
    fn flush(&self, wait: std::time::Duration) {
        let (done, rx) = std::sync::mpsc::sync_channel(1);
        let deadline = std::time::Instant::now() + wait;
        let mut msg = LogMsg::Flush(done);
        // A full queue makes room as the writer goes on: try until the deadline.
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => break,
                Err(std::sync::mpsc::TrySendError::Full(m)) if std::time::Instant::now() < deadline => {
                    msg = m;
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
        let _ = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
    }
}

impl std::io::Write for NonBlockingLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Lossy by design: the queue being full means the disk is far behind.
        if self.tx.try_send(LogMsg::Line(buf.to_vec())).is_err() {
            self.dropped.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for NonBlockingLog {
    type Writer = NonBlockingLog;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
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
    let writer = RotatingLogWriter::open(&logs, MAX_LOG_BYTES).ok().map(|w| NonBlockingLog::spawn(w, LOG_QUEUE));
    let flush_log = writer.clone();
    if let Some(w) = &writer {
        let _ = LOG_FILE.set(w.clone());
    }
    let to_file = writer.map(|w| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(w));
    // The terminal too: stdout may be a pipe nobody reads, and a write to it must not park a worker.
    let to_terminal = tracing_subscriber::fmt::layer().with_writer(NonBlockingLog::spawn(std::io::stdout(), LOG_QUEUE));
    // A line repeated many times a second (DeepFilterNet's clipping warning) passes once a minute.
    let _ =
        tracing_subscriber::registry().with(filter).with(log_limit::layer()).with(to_terminal).with(to_file).try_init();
    log_limit::report_in_background();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Where it came from too (function names in builds with symbols, addresses without).
        let trace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%info, %trace, "Ferrocast stopped unexpectedly");
        // The line is queued: give the writer a moment to put it in the file before the process goes.
        if let Some(l) = &flush_log {
            l.flush(std::time::Duration::from_secs(2));
        }
        default(info);
    }));
    tracing::info!(version = env!("CARGO_PKG_VERSION"), pid = std::process::id(), "Ferrocast starting");
}

fn main() {
    let dir = data_dir();
    init_tracing(&dir);
    let safe_mode = std::env::args().any(|a| a == SAFE_MODE_ARG);
    let guarded = safe_mode || ferrocast_engine::safe_mode::has_unclean_exit(&dir);
    // VST plugins run in their own locked-down helper, which may never read this folder (stream
    // keys, sign-ins); the Studio's sound filters then reach them. Deferred in Safe Mode.
    ferrocast_vsthost::client::keep_private(vec![dir.clone()]);
    if !guarded {
        ferrocast_vsthost::install();
    } else {
        tracing::warn!(safe_mode, "Safe Mode active at launch: VST plugins and third-party plugins held off");
    }
    #[cfg(windows)]
    crash::install(&dir);
    #[cfg(all(windows, debug_assertions))]
    crash::test_crash_if_asked();
    // The Store package's startup task can't pass `--minimized`: it's asked instead.
    let minimized = std::env::args().any(|a| a == MINIMIZED_ARG) || store::started_at_login();
    let startup_deeplink = std::env::args().find(|a| is_ferrocast_deeplink(a));
    if store::packaged() {
        tracing::info!("running from the Microsoft Store package: the Store updates it");
    }
    let app = tauri::Builder::default()
        .manage(PendingDeepLink(std::sync::Mutex::new(startup_deeplink)))
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if let Some(uri) = args.iter().find(|a| is_ferrocast_deeplink(a)) {
                dispatch_deeplink(app, uri);
            } else {
                show_window(app);
            }
        }))
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
            let started = tauri::async_runtime::block_on(start(&dir, safe_mode));
            let running = match started {
                Ok(r) => r,
                Err(message) => {
                    tracing::error!(%message, "Ferrocast couldn't start");
                    show_startup_error(app.handle(), &message);
                    return Ok(());
                }
            };
            let engine = running.engine.clone();
            engine.on_exit_safe_mode(Arc::new(ferrocast_vsthost::install));
            let show_for_safe_mode = engine.safe_mode_status().active || engine.safe_mode_status().offer;
            app.manage(running);
            app.manage(studio_keys::StudioKeys::new(engine.clone()));
            engine.watch_saves();
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
            watch_threads(app.handle().clone());
            // A desktop with no tray, or its icon file not writable (Linux, 19 Nov: a second
            // user's start panicked on the first one's /tmp/tray-icon), runs without the tray.
            // macOS: the menu bar (Settings, Undo, Quit… where a Mac user looks for them).
            #[cfg(target_os = "macos")]
            {
                match menu::build(app.handle()) {
                    Ok(bar) => {
                        if let Err(e) = app.set_menu(bar) {
                            tracing::warn!(error = %e, "the menu bar didn't show");
                        }
                        app.on_menu_event(|app, event| menu::on_event(app, event.id().as_ref()));
                    }
                    Err(e) => tracing::warn!(error = %e, "the menu bar couldn't be made"),
                }
            }
            match build_tray(app.handle()) {
                Ok(()) => shell.has_tray.store(true, Ordering::Relaxed),
                Err(e) => tracing::warn!(error = %e, "no tray icon: Ferrocast runs without it, and closing quits"),
            }
            if let Err(e) = app.global_shortcut().register(shortcut_of(SHOW_SHORTCUT)) {
                tracing::warn!(error = %e, "couldn't register the show-window shortcut (another app may use it)");
            }
            let has_tray = shell.has_tray.load(Ordering::Relaxed);
            tauri::async_runtime::spawn(follow_settings(app.handle().clone(), engine.clone(), shell));
            if store::packaged() {
                // Store installs get new versions from the Store (its rules, and the package
                // folder is read-only), so the dashboard says that instead of offering ours.
                engine.updates_from_store();
            } else {
                tauri::async_runtime::spawn(keep_updated(app.handle().clone(), engine.clone()));
            }
            let pending = Pending::default();
            tauri::async_runtime::spawn(install_when_asked(app.handle().clone(), engine.clone(), pending.clone()));
            tauri::async_runtime::spawn(restart_when_asked(app.handle().clone(), engine.clone(), pending));
            tauri::async_runtime::spawn(notify_when_hidden(app.handle().clone(), engine));
            #[cfg(any(unix, windows))]
            tauri::async_runtime::spawn(quit_on_signal(app.handle().clone()));
            // Started minimized (at login) with no tray, or offering/running Safe Mode: shown so
            // the prompt is never hidden in the tray.
            if !minimized || !has_tray || show_for_safe_mode {
                show_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::DragDrop(drop) = event
                && window.label() == "main"
            {
                forward_drop(window, drop);
            }
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
            {
                let app = window.app_handle();
                let _ = app.save_window_state(StateFlags::all());
                let to_tray = app
                    .try_state::<Arc<Shell>>()
                    .is_none_or(|s| s.close_to_tray.load(Ordering::Relaxed) && s.has_tray.load(Ordering::Relaxed));
                if to_tray {
                    // Closing hides: the engine keeps alerts and the bot running from the tray.
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    // Closing the window quits: asked first when something is on air.
                    api.prevent_close();
                    quit::request(app);
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
            // macOS: the Dock's Quit (and a Quit from Activity Monitor or a logout) arrives here;
            // it is a quit request like any other, asked about when something is on air.
            #[cfg(target_os = "macos")]
            quit::request(app);
        }
        // macOS: the app delegate gets the answer to a Quit from the Dock or an AppleEvent.
        #[cfg(target_os = "macos")]
        RunEvent::Ready => mac_quit::install(app),
        // macOS: clicking the Dock icon of a running app brings back the hidden window.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_window(app),
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        RunEvent::Opened { urls } => {
            if let Some(uri) = urls.iter().map(|u| u.as_str()).find(|u| is_ferrocast_deeplink(u)) {
                dispatch_deeplink(app, uri);
            } else {
                show_window(app);
            }
        }
        RunEvent::Exit => {
            tracing::info!("Ferrocast quitting");
            let running = app.try_state::<Running>();
            clean_exit_sequence(
                studio::mark_quitting,
                studio::close,
                || {
                    if let Some(r) = running.as_ref() {
                        r.engine.shutdown();
                    }
                },
                || clear_run_markers(running.as_ref().map(|r| &r.engine)),
            );
            tracing::info!("the Studio closed");
            // The file's writer is a thread of its own: the last lines are written before the process goes.
            if let Some(l) = LOG_FILE.get() {
                l.flush(std::time::Duration::from_secs(2));
            }
            // Help > Restart in Safe Mode: now that the quit is clean, start again with the flag.
            #[cfg(target_os = "macos")]
            if quit::RELAUNCH_SAFE.load(Ordering::SeqCst) {
                let mut env = app.env();
                if !env.args_os.iter().any(|a| a == SAFE_MODE_ARG) {
                    env.args_os.push(SAFE_MODE_ARG.into());
                }
                tauri::process::restart(&env);
            }
        }
        _ => {}
    });
}

/// The signals that mean "end now": `kill` and `pkill` (SIGTERM), Ctrl-C in a terminal (SIGINT), a
/// terminal or login ending (SIGHUP). By default each ends the process on the spot, with no
/// `RunEvent::Exit`: the run markers stayed and the next launch opened in Safe Mode (20 Nov, a
/// plain `kill` of the test app). They are caught instead and asked to quit as Quit does.
#[cfg(unix)]
struct QuitSignals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl QuitSignals {
    /// Must run inside the async runtime.
    fn register() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    /// The first of them to arrive.
    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.int.recv() => "SIGINT",
            _ = self.hup.recv() => "SIGHUP",
        }
    }
}

/// Windows console / session termination events (`SetConsoleCtrlHandler` via Tokio): Ctrl-C,
/// Ctrl-Break, closing a console window, user logoff, and system shutdown. Tokio parks the OS
/// handler thread on `CTRL_CLOSE`/`CTRL_LOGOFF`/`CTRL_SHUTDOWN` so `app.exit(0)` has Windows'
/// shutdown grace period to run `RunEvent::Exit` and clear the run markers instead of triggering
/// Safe Mode on the next launch (GUI logoff/shutdown also arrives via `WM_ENDSESSION` → `tao` →
/// `RunEvent::Exit`).
#[cfg(windows)]
struct QuitSignals {
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_break: tokio::signal::windows::CtrlBreak,
    ctrl_close: tokio::signal::windows::CtrlClose,
    ctrl_logoff: tokio::signal::windows::CtrlLogoff,
    ctrl_shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl QuitSignals {
    /// Must run inside the async runtime.
    fn register() -> std::io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
        Ok(Self {
            ctrl_c: ctrl_c()?,
            ctrl_break: ctrl_break()?,
            ctrl_close: ctrl_close()?,
            ctrl_logoff: ctrl_logoff()?,
            ctrl_shutdown: ctrl_shutdown()?,
        })
    }

    /// The first of them to arrive.
    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.ctrl_c.recv() => "CTRL_C",
            _ = self.ctrl_break.recv() => "CTRL_BREAK",
            _ = self.ctrl_close.recv() => "CTRL_CLOSE",
            _ = self.ctrl_logoff.recv() => "CTRL_LOGOFF",
            _ = self.ctrl_shutdown.recv() => "CTRL_SHUTDOWN",
        }
    }
}

/// Quits through the normal path (`RunEvent::Exit`, the clean-exit sequence) on the first signal.
/// A second one means the quit is stuck (a drain that won't end, a window that won't close): the
/// handlers stay installed, so without this only SIGKILL / TerminateProcess would end the process.
#[cfg(any(unix, windows))]
async fn quit_on_signal(app: AppHandle) {
    let mut signals = match QuitSignals::register() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't listen for quit signals: termination may skip clean quit");
            return;
        }
    };
    quit_then_force(&mut signals, || app.exit(0), || std::process::exit(1)).await;
}

/// `quit` on the first signal, `force` on the next.
#[cfg(any(unix, windows))]
async fn quit_then_force(signals: &mut QuitSignals, quit: impl FnOnce(), force: impl FnOnce()) {
    let which = signals.next().await;
    tracing::info!(signal = which, "asked to quit by a signal");
    quit();
    let again = signals.next().await;
    tracing::warn!(signal = again, "asked again while quitting: ending now, without the clean quit");
    force();
}

/// The "Ferrocast is running" notes (the Studio's session and the engine's `running.json`) that
/// the next launch reads as a crash and answers with Safe Mode. Removed at the end of a clean quit,
/// and by the Windows updater just before its installer ends the process.
fn clear_run_markers(engine: Option<&Engine>) {
    studio::end_session();
    if let Some(engine) = engine {
        engine.clear_running_marker();
    }
}

fn clean_exit_sequence(
    mark_quitting: impl FnOnce(),
    close_studio: impl FnOnce() -> bool,
    shutdown_engine: impl FnOnce(),
    clear_markers: impl FnOnce(),
) -> bool {
    mark_quitting();
    // Before the main thread's own end, which can't run the engine's GPU cleanup.
    let closed = close_studio();
    shutdown_engine();
    if closed {
        clear_markers();
    } else {
        tracing::warn!("clean quit markers preserved: Studio did not finish closing cleanly");
    }
    closed
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
    // Windows: the updater runs the installer and ends the process itself (`process::exit`, no
    // `RunEvent::Exit`), so the clean-quit notes would stay behind and the first launch of every
    // new version would open in Safe Mode with the owner's scenes held back. The hook runs once the
    // installer is ready to start, so a failed install leaves the notes where they were. (It does
    // nothing on the other systems, where the restart goes through the clean exit.)
    let handle = app.clone();
    builder = builder.on_before_exit(move || {
        let running = handle.try_state::<Running>();
        clear_run_markers(running.as_ref().map(|r| &r.engine));
    });
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
        let installed = tokio::task::spawn_blocking(move || {
            // A Mac keeps the same Ferrocast.app folder, so the Dock's pin survives the update;
            // the plugin's own install (it swaps the whole folder) is the fallback, and asks
            // for an admin password where the folder needs one.
            #[cfg(target_os = "macos")]
            if let Some(app) = std::env::current_exe().ok().and_then(|e| mac_update::bundle_of(&e)) {
                match mac_update::install(&bytes, &app) {
                    Ok(()) => return Ok(()),
                    Err(e) => tracing::warn!(error = %e, "couldn't update inside the app folder; replacing it whole"),
                }
            }
            update.install(bytes)
        })
        .await;
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
            let on = settings.launch_at_login;
            let result = if store::packaged() {
                // The package's startup task (a Run key written from inside a package is never
                // read by Windows).
                tauri::async_runtime::spawn_blocking(move || store::set_launch_at_login(on))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            } else {
                let al = app.autolaunch();
                if on { al.enable() } else { al.disable() }.map_err(|e| e.to_string())
            };
            match result {
                Ok(()) => {
                    if store::packaged() {
                        tracing::info!(on, "launch at login set through the package's startup task");
                    }
                    login = Some(on);
                }
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

/// The show-window key and the panic key put back, after a shortcut field let every global
/// shortcut go (see `studio_keys::capture`).
pub(crate) fn restore_app_shortcuts(app: &AppHandle) {
    if let Err(e) = app.global_shortcut().register(shortcut_of(SHOW_SHORTCUT)) {
        tracing::warn!(error = %e, "couldn't register the show-window shortcut again (another app may use it)");
    }
    if let (Some(shell), Some(running)) = (app.try_state::<Arc<Shell>>(), app.try_state::<Running>()) {
        let wanted = running.engine.panic_state().1.hotkey.trim().to_string();
        set_panic_hotkey(app, &shell, &wanted);
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
        // The line, then its detail (the dashboard keeps that behind an (i); a system
        // notification has no such place).
        let line = v["data"]["message"].as_str().unwrap_or("");
        let body = match v["data"]["detail"].as_str() {
            Some(d) if !d.is_empty() => {
                let stop = if line.ends_with(['.', '!', '?']) { "" } else { "." };
                format!("{line}{stop} {d}")
            }
            _ => line.to_string(),
        }
        .chars()
        .take(300)
        .collect::<String>();
        if let Err(e) = app.notification().builder().title("Ferrocast").body(body).show() {
            tracing::debug!(error = %e, "couldn't show a notification");
        }
    }
}

async fn start(dir: &std::path::Path, safe_mode: bool) -> Result<Running, String> {
    let engine = Engine::start_with_mode(dir, safe_mode).await.map_err(|e| e.to_string())?;
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

/// Files dragged onto the window, passed to the page as a `ferrocast:drop` event (the
/// window takes drops itself, which is how their paths are known; the page then adds them as
/// sources, as OBS does). `detail`: `{ kind: "enter" | "over" | "drop" | "leave", paths?,
/// x?, y? }`, the position in the page's CSS pixels.
fn forward_drop(window: &tauri::Window, drop: &tauri::DragDropEvent) {
    let Some(view) = window.app_handle().get_webview_window("main") else { return };
    tracing::debug!(?drop, "files dragged onto the window");
    let scale = window.scale_factor().unwrap_or(1.0);
    let at = |p: &tauri::PhysicalPosition<f64>| (p.x / scale, p.y / scale);
    // Only paths that are text: one that isn't (invalid UTF-8) can't be a source's file, and
    // serializing it would fail (CodeRabbit).
    fn text(paths: &[PathBuf]) -> Vec<&str> {
        paths.iter().filter_map(|p| p.to_str()).collect()
    }
    let detail = match drop {
        tauri::DragDropEvent::Enter { paths, position } => {
            let (x, y) = at(position);
            serde_json::json!({ "kind": "enter", "paths": text(paths), "x": x, "y": y })
        }
        tauri::DragDropEvent::Over { position } => {
            let (x, y) = at(position);
            serde_json::json!({ "kind": "over", "x": x, "y": y })
        }
        tauri::DragDropEvent::Drop { paths, position } => {
            let (x, y) = at(position);
            serde_json::json!({ "kind": "drop", "paths": text(paths), "x": x, "y": y })
        }
        _ => serde_json::json!({ "kind": "leave" }),
    };
    // JSON is a JavaScript expression; paths are only ever data here.
    let _ = view.eval(format!("window.dispatchEvent(new CustomEvent('ferrocast:drop', {{ detail: {detail} }}))"));
}

/// Shows the main window, creating it if it was never opened.
struct PendingDeepLink(std::sync::Mutex<Option<String>>);

impl PendingDeepLink {
    /// The link waiting, left in place.
    fn peek(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Clears the link, but only if it is still `used`: one that arrived since stays for its own turn.
    fn clear_if(&self, used: &str) -> bool {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_deref() == Some(used) {
            *slot = None;
            true
        } else {
            false
        }
    }
}

fn is_ferrocast_deeplink(s: &str) -> bool {
    let lower = s.get(..11).map(str::to_ascii_lowercase);
    lower.as_deref() == Some("ferrocast:/")
        && s.get(11..12) == Some("/")
        && s.len() <= 512
        && s.is_ascii()
        && !s.chars().any(char::is_control)
        && !s.contains('\\')
}

fn store_deeplink_route(uri: &str) -> String {
    let mut encoded = String::with_capacity(uri.len());
    for b in uri.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            encoded.push(*b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("#/store?link={encoded}")
}

fn dispatch_deeplink(app: &AppHandle, uri: &str) {
    if !is_ferrocast_deeplink(uri) {
        return;
    }
    if let Some(pending) = app.try_state::<PendingDeepLink>() {
        *pending.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(uri.to_string());
    }
    show_window(app);
    if let Some(w) = app.get_webview_window("main") {
        let detail = serde_json::json!({ "uri": uri });
        let _ = w.eval(format!("window.dispatchEvent(new CustomEvent('ferrocast:deeplink', {{ detail: {detail} }}))"));
        if let Some(pending) = app.try_state::<PendingDeepLink>() {
            *pending.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
    }
}

pub(crate) fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let Some(running) = app.try_state::<Running>() else {
        return;
    };
    // Read, not taken: it is cleared below only once the window is really there.
    let pending_uri = app.try_state::<PendingDeepLink>().and_then(|p| p.peek());
    let pending_route = pending_uri.as_deref().map(store_deeplink_route);
    // Dev builds only: FERROCAST_DEBUG_ROUTE (e.g. #/studio/golive) opens there, for testing.
    #[cfg(debug_assertions)]
    let route = pending_route
        .or_else(|| std::env::var("FERROCAST_DEBUG_ROUTE").ok().filter(|r| r.starts_with("#/")))
        .unwrap_or_default();
    #[cfg(not(debug_assertions))]
    let route = pending_route.unwrap_or_default();
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
    match built {
        Ok(_) => {
            if let (Some(used), Some(pending)) = (&pending_uri, app.try_state::<PendingDeepLink>()) {
                pending.clear_if(used);
            }
        }
        // The link stays: the next time the window is asked for it opens where the link said.
        Err(e) => tracing::error!(error = %e, "couldn't open the window"),
    }
}

/// Says in the log when the window's thread or the async workers stop getting to their work
/// (5 s), and when they're back: on GitHub's Windows runner every call through the window's
/// thread waited forever and nothing said which part was stuck (19 Nov).
fn watch_threads(app: AppHandle) {
    use std::sync::atomic::AtomicU64;
    let start = std::time::Instant::now();
    let now = move || start.elapsed().as_millis() as u64;
    let main_seen = Arc::new(AtomicU64::new(0));
    let async_seen = Arc::new(AtomicU64::new(0));
    let spawned = std::thread::Builder::new().name("ferrocast-liveness".into()).spawn(move || {
        let mut stuck = [false, false];
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let m = main_seen.clone();
            if app.run_on_main_thread(move || m.store(now(), Ordering::Relaxed)).is_err() {
                return;
            }
            let a = async_seen.clone();
            tauri::async_runtime::spawn(async move { a.store(now(), Ordering::Relaxed) });
            let t = now();
            for (i, (seen, what)) in
                [(&main_seen, "the window's thread"), (&async_seen, "the async workers")].into_iter().enumerate()
            {
                let quiet = t.saturating_sub(seen.load(Ordering::Relaxed));
                if quiet > 5_000 && !stuck[i] {
                    stuck[i] = true;
                    tracing::warn!(seconds = quiet / 1000, "nothing ran on {what} for a while");
                } else if quiet <= 5_000 && stuck[i] {
                    stuck[i] = false;
                    tracing::info!("{what}: running again");
                }
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "the thread that watches for stalls didn't start");
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
    // Linux writes the tray's icon to a file: ours under this user's data, never the shared
    // /tmp/tray-icon (owned by whoever started an app with a tray first) when there's no
    // XDG_RUNTIME_DIR.
    #[cfg(target_os = "linux")]
    {
        let dir = data_dir().join("tray");
        let _ = std::fs::create_dir_all(&dir);
        tray = tray.temp_dir_path(dir);
    }
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
        "quit" => quit::request(app),
        _ => {}
    })
    .build(app)?;
    Ok(())
}

pub(crate) fn with_engine<F, Fut>(app: &AppHandle, f: F)
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

    /// The link a window is opened for stays until the window is really there, and a newer link
    /// that arrived meanwhile is never cleared by the older one's success.
    #[test]
    fn a_pending_link_is_cleared_only_after_its_window_opened_and_only_that_link() {
        let p = PendingDeepLink(std::sync::Mutex::new(Some("ferrocast://store/install/a".into())));
        // Reading it for the route leaves it: if the window fails to build, the next try has it.
        assert_eq!(p.peek().as_deref(), Some("ferrocast://store/install/a"));
        assert_eq!(p.peek().as_deref(), Some("ferrocast://store/install/a"));
        // A newer link arrives while the window is being built: the older one's success keeps it.
        *p.0.lock().unwrap() = Some("ferrocast://store/install/b".into());
        assert!(!p.clear_if("ferrocast://store/install/a"));
        assert_eq!(p.peek().as_deref(), Some("ferrocast://store/install/b"));
        // The one that was used is cleared.
        assert!(p.clear_if("ferrocast://store/install/b"));
        assert_eq!(p.peek(), None);
        assert!(!p.clear_if("ferrocast://store/install/b"), "nothing left to clear");
    }

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

    /// The Windows updater ends the process after its `on_before_exit` hook, which calls this: with
    /// the notes left behind, the next launch reads the update as a crash and opens in Safe Mode.
    #[tokio::test]
    async fn the_run_markers_the_updater_clears_are_the_ones_that_start_safe_mode() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let engine = Engine::start_with_mode(dir.path(), false).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(dir.path().join("running.json").exists(), "a running launch leaves its note");
        assert!(
            ferrocast_engine::safe_mode::has_unclean_exit(dir.path()),
            "and the next launch would take it for a crash"
        );
        clear_run_markers(Some(&engine));
        assert!(!dir.path().join("running.json").exists());
        assert!(
            !ferrocast_engine::safe_mode::has_unclean_exit(dir.path()),
            "a launch after the update is a normal one"
        );
        engine.shutdown();
    }

    /// A signal goes to every listener in the process: the tests that send one take turns.
    #[cfg(unix)]
    static SIGNAL_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// `kill` (SIGTERM) used to end Ferrocast with the run markers left behind; it is caught now.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_kill_signal_is_caught_so_the_quit_can_run_clean() {
        let _turn = SIGNAL_TESTS.lock().await;
        let mut signals = QuitSignals::register().unwrap_or_else(|e| panic!("{e}"));
        let pid = std::process::id().to_string();
        let sent = std::process::Command::new("kill").args(["-TERM", &pid]).status().unwrap_or_else(|e| panic!("{e}"));
        assert!(sent.success());
        let which = tokio::time::timeout(Duration::from_secs(5), signals.next()).await;
        assert_eq!(which.ok(), Some("SIGTERM"));
    }

    /// A second signal while the quit is stuck ends the process (it used to be swallowed).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_second_signal_forces_the_exit_when_the_quit_is_stuck() {
        let _turn = SIGNAL_TESTS.lock().await;
        let mut signals = QuitSignals::register().unwrap_or_else(|e| panic!("{e}"));
        let log = std::sync::Mutex::new(Vec::new());
        let pid = std::process::id().to_string();
        let term = || {
            let sent =
                std::process::Command::new("kill").args(["-TERM", &pid]).status().unwrap_or_else(|e| panic!("{e}"));
            assert!(sent.success());
        };
        let task = quit_then_force(
            &mut signals,
            || log.lock().unwrap_or_else(|e| e.into_inner()).push("quit"),
            || log.lock().unwrap_or_else(|e| e.into_inner()).push("force"),
        );
        tokio::pin!(task);
        term();
        // One signal: asked to quit, nothing forced.
        assert!(tokio::time::timeout(Duration::from_millis(500), &mut task).await.is_err());
        assert_eq!(*log.lock().unwrap_or_else(|e| e.into_inner()), vec!["quit"]);
        term();
        assert!(tokio::time::timeout(Duration::from_secs(5), &mut task).await.is_ok());
        assert_eq!(*log.lock().unwrap_or_else(|e| e.into_inner()), vec!["quit", "force"]);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_quit_signals_register_cleanly() {
        let mut signals = QuitSignals::register().unwrap_or_else(|e| panic!("SetConsoleCtrlHandler failed: {e}"));
        // No signal sent: polling times out cleanly rather than firing spuriously.
        assert!(tokio::time::timeout(Duration::from_millis(50), signals.next()).await.is_err());
    }

    #[test]
    fn clean_exit_sequence_deletes_markers_only_after_clean_close() {
        let log = std::cell::RefCell::new(Vec::new());
        let ok = clean_exit_sequence(
            || log.borrow_mut().push("mark_quitting"),
            || {
                log.borrow_mut().push("close_studio");
                true
            },
            || log.borrow_mut().push("shutdown_engine"),
            || log.borrow_mut().push("clear_markers"),
        );
        assert!(ok);
        assert_eq!(*log.borrow(), vec!["mark_quitting", "close_studio", "shutdown_engine", "clear_markers"]);

        let partial = std::cell::RefCell::new(Vec::new());
        let failed = clean_exit_sequence(
            || partial.borrow_mut().push("mark_quitting"),
            || {
                partial.borrow_mut().push("close_studio");
                false
            },
            || partial.borrow_mut().push("shutdown_engine"),
            || partial.borrow_mut().push("clear_markers"),
        );
        assert!(!failed);
        assert_eq!(*partial.borrow(), vec!["mark_quitting", "close_studio", "shutdown_engine"]);
    }

    /// A log writer whose disk is slow (or locked, or rotating) must not slow the threads that log:
    /// writes return at once and a full queue drops lines; what was queued is written, in order.
    #[test]
    fn the_log_writer_never_makes_the_logging_thread_wait() {
        use std::io::Write;
        struct Slow(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Slow {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(std::time::Duration::from_millis(100));
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut log = NonBlockingLog::spawn(Slow(got.clone()), 16);
        let t = std::time::Instant::now();
        for i in 0..1_000 {
            log.write_all(format!("line {i}\n").as_bytes()).unwrap();
        }
        assert!(t.elapsed() < std::time::Duration::from_millis(200), "1000 writes took {:?}", t.elapsed());
        log.flush(std::time::Duration::from_secs(10));
        let text = String::from_utf8(got.lock().unwrap().clone()).unwrap();
        assert!(text.starts_with("line 0\n"), "the first lines are kept, in order");
        assert!(text.lines().count() < 1_000, "a full queue drops lines instead of waiting");
        assert!(text.contains("log lines were dropped"), "the gap is said in the log: {text}");
    }

    #[test]
    fn rotating_log_writer_caps_active_and_backup_while_running() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("ferrocast-log-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cap = 64 * 1024u64;
        let mut w = RotatingLogWriter::open(&dir, cap).unwrap();
        let line = vec![b'a'; 1024];
        // Write 1 MB through a 64 KB cap (~16 rotations during a single live session).
        for _ in 0..1024 {
            w.write_all(&line).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        let cur_len = std::fs::metadata(dir.join("ferrocast.log")).unwrap().len();
        let bak_len = std::fs::metadata(dir.join("ferrocast.1.log")).unwrap().len();
        let file_count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(file_count, 2, "only active log and one backup");
        assert!(cur_len <= cap, "active log exceeded cap: {cur_len} > {cap}");
        assert!(bak_len <= cap, "backup log exceeded cap: {bak_len} > {cap}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deeplinks_validate_strictly_and_route_to_store() {
        assert!(is_ferrocast_deeplink("ferrocast://store/install/aurora-pulse"));
        assert!(!is_ferrocast_deeplink("https://evil.com/store/install/x"));
        assert!(!is_ferrocast_deeplink("ferrocast://store/install/x\\y"));
        assert!(!is_ferrocast_deeplink("ferrocast://store/install/x\ny"));
        assert_eq!(
            store_deeplink_route("ferrocast://store/install/aurora-pulse"),
            "#/store?link=ferrocast%3A%2F%2Fstore%2Finstall%2Faurora-pulse"
        );
    }
}
