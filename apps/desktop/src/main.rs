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

use ferrocast_engine::Engine;
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
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

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tao=warn,wry=warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn main() {
    init_tracing();
    let dir = data_dir();
    let minimized = std::env::args().any(|a| a == MINIMIZED_ARG);
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::Builder::new().args([MINIMIZED_ARG]).build())
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
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
            let shell = Arc::new(Shell::default());
            shell.close_to_tray.store(engine.settings().close_to_tray, Ordering::Relaxed);
            app.manage(shell.clone());
            build_tray(app.handle())?;
            if let Err(e) = app.global_shortcut().register(shortcut_of(SHOW_SHORTCUT)) {
                tracing::warn!(error = %e, "couldn't register the show-window shortcut (another app may use it)");
            }
            tauri::async_runtime::spawn(follow_settings(app.handle().clone(), engine.clone(), shell));
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
    app.run(|app, event| {
        // The last window closing must not quit a tray app; only "Quit" does (exit code
        // set). If startup failed there's no tray to quit from, so closing the error
        // window really exits.
        if let RunEvent::ExitRequested { api, code: None, .. } = event
            && app.try_state::<Running>().is_some()
        {
            api.prevent_exit();
        }
    });
}

fn shortcut_of(s: &str) -> Shortcut {
    s.parse().unwrap_or_else(|_| Shortcut::new(None, tauri_plugin_global_shortcut::Code::F24))
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
    Ok(Running { engine, base: server.base_url(), token: server.sessions.desktop_token().to_string() })
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
    let Ok(url) = format!("{}/app/", running.base).parse() else {
        return;
    };
    let api = serde_json::to_string(&running.base).unwrap_or_default();
    let token = serde_json::to_string(&running.token).unwrap_or_default();
    let script = format!("window.__FERROCAST__ = Object.freeze({{ api: {api}, token: {token}, desktop: true }});");
    let origin = running.base.clone();
    let opener = app.clone();
    let built = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title("Ferrocast")
        .inner_size(1240.0, 800.0)
        .min_inner_size(380.0, 600.0)
        .initialization_script(&script)
        .on_navigation(move |u| {
            // The window only ever shows our dashboard; links to the web open in the browser.
            let ours = u.as_str().starts_with(&format!("{origin}/"));
            if !ours && matches!(u.scheme(), "http" | "https") {
                let _ = opener.opener().open_url(u.as_str(), None::<&str>);
            }
            ours
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
    let menu = MenuBuilder::new(app)
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
