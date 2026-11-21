//! The app's menu bar on a Mac (OBS has one; a Mac user looks for Settings under the app's name,
//! Undo under Edit, and Quit with Cmd+Q). Every item is one of four things that already exist:
//! a page of the dashboard (`Nav`), an action the dashboard already has (`Page`, run by the
//! page), a web address (`Url`), or something only the app can do (`Native`: show a folder,
//! full screen, quit). Nothing here is new behaviour; the table below is the whole menu, and its
//! tests keep it honest. Windows and Linux keep their windows without a menu bar (a menu would
//! change the window's frame there), so this is macOS only.

/// What choosing an item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Act {
    /// The dashboard goes to this route (`#/settings`).
    Nav(&'static str),
    /// The dashboard runs this action (`undo`, `lock-preview`…), which it already has.
    Page(&'static str),
    /// This address opens in the browser.
    Url(&'static str),
    /// Something only the app does.
    Native(Native),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Native {
    Quit,
    ShowRecordings,
    ShowSettingsFolder,
    ShowLogs,
    FullScreen,
    AlwaysOnTop,
    /// Quits (asking first when something is on air) and starts again with `--safe-mode`.
    RestartSafeMode,
}

pub(crate) struct Item {
    pub id: &'static str,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub label: &'static str,
    /// Tauri's accelerator syntax ("CmdOrCtrl+,").
    pub accel: Option<&'static str>,
    pub act: Act,
}

/// The system's own items (they work on whatever has focus).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Std {
    About,
    Services,
    Hide,
    HideOthers,
    ShowAll,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Minimize,
    Maximize,
    CloseWindow,
}

pub(crate) enum Entry {
    Item(Item),
    Sep,
    Std(Std),
}

pub(crate) struct Menu {
    pub title: &'static str,
    pub entries: &'static [Entry],
}

const RELEASES: &str = "https://github.com/ArshPunisher/ferrocast/releases";
const DOCS: &str = "https://ferrocast.clearcraftlabs.com/docs";

const fn item(id: &'static str, label: &'static str, accel: Option<&'static str>, act: Act) -> Entry {
    Entry::Item(Item { id, label, accel, act })
}

pub(crate) const MENUS: &[Menu] = &[
    Menu {
        title: "Ferrocast",
        entries: &[
            Entry::Std(Std::About),
            item("check-updates", "Check for Updates…", None, Act::Page("check-updates")),
            Entry::Sep,
            item("settings", "Settings…", Some("CmdOrCtrl+,"), Act::Nav("#/settings")),
            Entry::Sep,
            Entry::Std(Std::Services),
            Entry::Sep,
            Entry::Std(Std::Hide),
            Entry::Std(Std::HideOthers),
            Entry::Std(Std::ShowAll),
            Entry::Sep,
            item("quit", "Quit Ferrocast", Some("CmdOrCtrl+Q"), Act::Native(Native::Quit)),
        ],
    },
    Menu {
        title: "File",
        entries: &[
            item("show-recordings", "Show Recordings", None, Act::Native(Native::ShowRecordings)),
            item("remux", "Remux Recordings…", None, Act::Page("remux")),
            item("show-settings-folder", "Show Settings Folder", None, Act::Native(Native::ShowSettingsFolder)),
            item("show-logs", "Show Log Files", None, Act::Native(Native::ShowLogs)),
            Entry::Sep,
            Entry::Std(Std::CloseWindow),
        ],
    },
    Menu {
        title: "Edit",
        entries: &[
            item("undo", "Undo", Some("CmdOrCtrl+Z"), Act::Page("undo")),
            item("redo", "Redo", Some("CmdOrCtrl+Shift+Z"), Act::Page("redo")),
            Entry::Sep,
            Entry::Std(Std::Cut),
            Entry::Std(Std::Copy),
            Entry::Std(Std::Paste),
            Entry::Std(Std::SelectAll),
            Entry::Sep,
            item("lock-preview", "Lock Preview", None, Act::Page("lock-preview")),
        ],
    },
    Menu {
        title: "View",
        entries: &[
            item("zoom-fit", "Fit Preview", Some("CmdOrCtrl+0"), Act::Page("zoom-fit")),
            item("zoom-in", "Zoom In", Some("CmdOrCtrl+="), Act::Page("zoom-in")),
            item("zoom-out", "Zoom Out", Some("CmdOrCtrl+-"), Act::Page("zoom-out")),
            Entry::Sep,
            item("full-screen", "Full Screen", Some("Ctrl+CmdOrCtrl+F"), Act::Native(Native::FullScreen)),
            item("always-on-top", "Always on Top", None, Act::Native(Native::AlwaysOnTop)),
        ],
    },
    Menu {
        title: "Go",
        entries: &[
            item("go-home", "Home", Some("CmdOrCtrl+1"), Act::Nav("#/home")),
            item("go-studio", "Studio", Some("CmdOrCtrl+2"), Act::Nav("#/studio")),
            item("go-chat", "Chat", Some("CmdOrCtrl+3"), Act::Nav("#/chat")),
            item("go-alerts", "Alerts", Some("CmdOrCtrl+4"), Act::Nav("#/alerts")),
            item("go-overlays", "Overlays", Some("CmdOrCtrl+5"), Act::Nav("#/overlays")),
        ],
    },
    Menu {
        title: "Tools",
        entries: &[
            item("automations", "Automations", None, Act::Nav("#/settings/automations")),
            item("plugins", "Plugins", None, Act::Nav("#/settings/plugins")),
            item("updates", "Updates", None, Act::Nav("#/settings/updates")),
        ],
    },
    Menu { title: "Window", entries: &[Entry::Std(Std::Minimize), Entry::Std(Std::Maximize)] },
    Menu {
        title: "Help",
        entries: &[
            item("docs", "Ferrocast Documentation", None, Act::Url(DOCS)),
            item("release-notes", "Release Notes", None, Act::Url(RELEASES)),
            item("help-logs", "Show Log Files", None, Act::Native(Native::ShowLogs)),
            Entry::Sep,
            item("restart-safe", "Restart in Safe Mode…", None, Act::Native(Native::RestartSafeMode)),
        ],
    },
];

/// The prefix of every item's id, so the tray's own items (`open`, `quit`…) are never taken for these.
pub(crate) const PREFIX: &str = "menu:";

/// What an item (by id, with or without the prefix) does.
pub(crate) fn action(id: &str) -> Option<Act> {
    let id = id.strip_prefix(PREFIX).unwrap_or(id);
    MENUS.iter().flat_map(|m| m.entries).find_map(|e| match e {
        Entry::Item(i) if i.id == id => Some(i.act),
        _ => None,
    })
}

/// The message the page is sent for an item: it goes to the dashboard as the detail of a
/// `ferrocast:menu` event. None: the app does this itself.
pub(crate) fn page_message(act: Act) -> Option<serde_json::Value> {
    match act {
        Act::Nav(to) => Some(serde_json::json!({ "kind": "nav", "to": to })),
        Act::Page(name) => Some(serde_json::json!({ "kind": "action", "name": name })),
        Act::Url(_) | Act::Native(_) => None,
    }
}

/// The script that hands a message to the dashboard.
pub(crate) fn page_script(msg: &serde_json::Value) -> String {
    format!("window.dispatchEvent(new CustomEvent('ferrocast:menu', {{ detail: {msg} }}))")
}

#[cfg(target_os = "macos")]
pub(crate) use mac::{build, on_event};

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tauri::menu::{CheckMenuItemBuilder, Menu as TauriMenu, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
    use tauri::{AppHandle, Manager};
    use tauri_plugin_opener::OpenerExt;

    static ON_TOP: AtomicBool = AtomicBool::new(false);

    pub(crate) fn build(app: &AppHandle) -> tauri::Result<TauriMenu<tauri::Wry>> {
        let mut bar = tauri::menu::MenuBuilder::new(app);
        for m in MENUS {
            let mut sub = SubmenuBuilder::new(app, m.title);
            for e in m.entries {
                sub = match e {
                    Entry::Sep => sub.separator(),
                    Entry::Std(s) => sub.item(&match s {
                        Std::About => PredefinedMenuItem::about(app, None, None)?,
                        Std::Services => PredefinedMenuItem::services(app, None)?,
                        Std::Hide => PredefinedMenuItem::hide(app, None)?,
                        Std::HideOthers => PredefinedMenuItem::hide_others(app, None)?,
                        Std::ShowAll => PredefinedMenuItem::show_all(app, None)?,
                        Std::Cut => PredefinedMenuItem::cut(app, None)?,
                        Std::Copy => PredefinedMenuItem::copy(app, None)?,
                        Std::Paste => PredefinedMenuItem::paste(app, None)?,
                        Std::SelectAll => PredefinedMenuItem::select_all(app, None)?,
                        Std::Minimize => PredefinedMenuItem::minimize(app, None)?,
                        Std::Maximize => PredefinedMenuItem::maximize(app, None)?,
                        Std::CloseWindow => PredefinedMenuItem::close_window(app, None)?,
                    }),
                    Entry::Item(i) => {
                        let id = format!("{PREFIX}{}", i.id);
                        if i.act == Act::Native(Native::AlwaysOnTop) {
                            let mut b = CheckMenuItemBuilder::with_id(id, i.label).checked(false);
                            if let Some(a) = i.accel {
                                b = b.accelerator(a);
                            }
                            sub.item(&b.build(app)?)
                        } else {
                            let mut b = MenuItemBuilder::with_id(id, i.label);
                            if let Some(a) = i.accel {
                                b = b.accelerator(a);
                            }
                            sub.item(&b.build(app)?)
                        }
                    }
                };
            }
            bar = bar.item(&sub.build()?);
        }
        bar.build()
    }

    /// A menu item was chosen (the tray's items don't carry the prefix and are ignored here).
    pub(crate) fn on_event(app: &AppHandle, id: &str) {
        if !id.starts_with(PREFIX) {
            return;
        }
        let Some(act) = action(id) else {
            tracing::warn!(id, "a menu item with no action");
            return;
        };
        if let Some(msg) = page_message(act) {
            // The window comes forward (it may be hidden to the tray), then the page acts.
            crate::show_window(app);
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.eval(page_script(&msg));
            }
            return;
        }
        match act {
            Act::Url(url) => {
                if let Err(e) = app.opener().open_url(url, None::<&str>) {
                    tracing::warn!(url, error = %e, "the menu's link didn't open");
                }
            }
            Act::Native(n) => native(app, n),
            Act::Nav(_) | Act::Page(_) => {}
        }
    }

    fn native(app: &AppHandle, n: Native) {
        match n {
            Native::Quit => crate::quit::request(app),
            Native::RestartSafeMode => crate::quit::request_safe_restart(app),
            Native::ShowSettingsFolder => reveal(app, crate::data_dir()),
            Native::ShowLogs => reveal(app, crate::data_dir().join("logs")),
            Native::ShowRecordings => {
                let app = app.clone();
                crate::with_engine(&app.clone(), move |engine| async move {
                    match engine.studio_settings().await {
                        Ok(s) => reveal(&app, crate::studio::record_folder(&s)),
                        Err(e) => tracing::warn!(error = %e, "the recordings folder isn't known"),
                    }
                });
            }
            Native::FullScreen => {
                if let Some(w) = app.get_webview_window("main") {
                    let on = w.is_fullscreen().unwrap_or(false);
                    let _ = w.set_fullscreen(!on);
                }
            }
            Native::AlwaysOnTop => {
                let on = !ON_TOP.fetch_xor(true, Ordering::Relaxed);
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.set_always_on_top(on);
                }
            }
        }
    }

    /// Opens a folder in Finder (made first if it isn't there yet: no logs before the first run).
    fn reveal(app: &AppHandle, dir: std::path::PathBuf) {
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = app.opener().open_path(dir.to_string_lossy(), None::<&str>) {
            tracing::warn!(dir = %dir.display(), error = %e, "the folder didn't open");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn items() -> Vec<&'static Item> {
        MENUS
            .iter()
            .flat_map(|m| m.entries)
            .filter_map(|e| if let Entry::Item(i) = e { Some(i) } else { None })
            .collect()
    }

    #[test]
    fn every_item_has_one_id_and_one_action() {
        let all = items();
        let ids: HashSet<_> = all.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), all.len(), "an id repeats: {:?}", all.iter().map(|i| i.id).collect::<Vec<_>>());
        for i in &all {
            assert_eq!(action(i.id), Some(i.act), "{}", i.id);
            assert_eq!(action(&format!("{PREFIX}{}", i.id)), Some(i.act));
        }
        assert_eq!(action("open"), None, "the tray's items aren't the menu's");
        assert_eq!(action("nonsense"), None);
    }

    #[test]
    fn routes_and_links_are_the_dashboards_and_the_projects() {
        for i in items() {
            match i.act {
                Act::Nav(to) => assert!(to.starts_with("#/"), "{}: {to}", i.id),
                Act::Url(u) => assert!(u.starts_with("https://"), "{}: {u}", i.id),
                _ => {}
            }
        }
        assert_eq!(action("settings"), Some(Act::Nav("#/settings")));
        assert_eq!(action("automations"), Some(Act::Nav("#/settings/automations")));
        assert_eq!(action("plugins"), Some(Act::Nav("#/settings/plugins")));
        assert_eq!(action("remux"), Some(Act::Page("remux")));
        assert_eq!(action("restart-safe"), Some(Act::Native(Native::RestartSafeMode)));
    }

    #[test]
    fn the_standard_shortcuts_are_where_a_mac_user_expects_them() {
        let accel = |id: &str| items().into_iter().find(|i| i.id == id).and_then(|i| i.accel);
        assert_eq!(accel("settings"), Some("CmdOrCtrl+,"));
        assert_eq!(accel("quit"), Some("CmdOrCtrl+Q"));
        assert_eq!(accel("undo"), Some("CmdOrCtrl+Z"));
        assert_eq!(accel("redo"), Some("CmdOrCtrl+Shift+Z"));
        // No two items share a shortcut.
        let keys: Vec<_> = items().iter().filter_map(|i| i.accel).collect();
        let set: HashSet<_> = keys.iter().collect();
        assert_eq!(keys.len(), set.len(), "{keys:?}");
        // Menus: the app's first, then the ones a Mac has.
        let titles: Vec<_> = MENUS.iter().map(|m| m.title).collect();
        assert_eq!(titles, ["Ferrocast", "File", "Edit", "View", "Go", "Tools", "Window", "Help"]);
    }

    #[test]
    fn the_page_is_told_what_to_do_and_the_app_does_the_rest_itself() {
        assert_eq!(page_message(Act::Nav("#/chat")), Some(serde_json::json!({ "kind": "nav", "to": "#/chat" })));
        assert_eq!(page_message(Act::Page("undo")), Some(serde_json::json!({ "kind": "action", "name": "undo" })));
        assert_eq!(page_message(Act::Native(Native::Quit)), None);
        assert_eq!(page_message(Act::Url("https://x.test")), None);
        let js = page_script(&serde_json::json!({ "kind": "nav", "to": "#/chat" }));
        assert!(js.contains("ferrocast:menu") && js.contains("#/chat"), "{js}");
    }
}
