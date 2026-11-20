//! Quitting while something is on air asks first, as OBS's "Show active outputs warning on exit"
//! does: the stream, a recording and the replay buffer all end with the app. Every way of
//! quitting that the person chooses goes through [`request`]: the menu's Quit (⌘Q), the tray's
//! Quit, the Dock's Quit, and closing the window when it isn't set to hide to the tray.
//! (A signal from the system, `kill` or a logout, can't be asked and still quits cleanly.)

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager};
#[cfg(not(target_os = "macos"))]
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// One question at a time: a second Quit while the first is open does nothing.
static ASKING: AtomicBool = AtomicBool::new(false);

/// Set when the app is to start again in Safe Mode once it has quit cleanly (`main`'s exit
/// handler does it, after the run markers are cleared).
#[cfg(target_os = "macos")]
pub(crate) static RELAUNCH_SAFE: AtomicBool = AtomicBool::new(false);

/// Set once the app is on its way out (confirmed, or nothing was on): the system's own
/// terminate request that follows must not be asked about again.
static LEAVING: AtomicBool = AtomicBool::new(false);

/// macOS: a Quit from outside the app (the Dock, an AppleEvent) reached `applicationShouldTerminate:`
/// → says what to answer the system now. `Later` starts the question; the answer goes back with
/// `replyToApplicationShouldTerminate:`.
#[cfg(target_os = "macos")]
pub(crate) fn terminate_requested(app: &AppHandle) -> crate::mac_quit::Answer {
    let answer = crate::mac_quit::terminate_answer(LEAVING.load(Ordering::SeqCst), ASKING.load(Ordering::SeqCst));
    if answer == crate::mac_quit::Answer::Later {
        request_inner(app, Mode::Terminate);
    }
    answer
}

/// Quits, asking first when the stream, a recording or the replay buffer is on.
pub(crate) fn request(app: &AppHandle) {
    request_inner(app, Mode::Quit);
}

/// Help > Restart in Safe Mode: as Quit (asks first when something is on air), then the app
/// starts again with `--safe-mode`.
#[cfg(target_os = "macos")]
pub(crate) fn request_safe_restart(app: &AppHandle) {
    request_inner(app, Mode::SafeRestart);
}

/// Where the question came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Our own Quit (menu, tray, a closing window): the app exits itself when it is to go.
    Quit,
    /// Help > Restart in Safe Mode.
    SafeRestart,
    /// macOS: the system asked (the Dock's Quit, an AppleEvent): the answer goes back to it.
    #[cfg(target_os = "macos")]
    Terminate,
}

fn request_inner(app: &AppHandle, mode: Mode) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let active = match app.try_state::<crate::Running>() {
            Some(r) => {
                // Not waited for long: a system quit (logout, the Dock) is held up while this
                // runs, and a Studio busy with a long edit must not hold it up for good.
                let stats = tokio::time::timeout(std::time::Duration::from_secs(3), r.engine.studio_stats())
                    .await
                    .ok()
                    .and_then(Result::ok);
                Active::from(r.engine.is_live(), stats.as_ref())
            }
            None => Active::default(),
        };
        match decide(&active) {
            Decision::Quit => go(&app, mode),
            Decision::Ask { title, text } => {
                if ASKING.swap(true, Ordering::SeqCst) {
                    // One question at a time. The system, if it asked, is told no.
                    #[cfg(target_os = "macos")]
                    if mode == Mode::Terminate {
                        run_on_main(&app, || crate::mac_quit::reply_terminate(false));
                    }
                    return;
                }
                crate::show_window(&app);
                let restart = mode == Mode::SafeRestart;
                let (title, text) = if restart {
                    ("Restart in Safe Mode?", text.replace("Quit anyway?", "Restart anyway?"))
                } else {
                    (title, text)
                };
                ask(&app, mode, title, text);
            }
        }
    });
}

/// Puts the question: an `NSAlert` on a Mac (Return = Cancel, Esc = Cancel), the dialog plugin's
/// message elsewhere. Whatever the answer, `ASKING` is cleared.
#[cfg(target_os = "macos")]
fn ask(app: &AppHandle, mode: Mode, title: &'static str, text: String) {
    let quit = app.clone();
    run_on_main(app, move || {
        let go_on = crate::mac_quit::ask_modal(title, &text, mode == Mode::SafeRestart);
        ASKING.store(false, Ordering::SeqCst);
        if go_on {
            go(&quit, mode);
        } else if mode == Mode::Terminate {
            crate::mac_quit::reply_terminate(false);
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn ask(app: &AppHandle, mode: Mode, title: &'static str, text: String) {
    let quit = app.clone();
    // "Cancel" first: it is the default button and what Esc picks. The answer is true for the
    // first button.
    app.dialog()
        .message(text)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom("Cancel".into(), "Quit".into()))
        .show(move |cancelled| {
            ASKING.store(false, Ordering::SeqCst);
            if !cancelled {
                go(&quit, mode);
            }
        });
}

/// On the main thread (an `NSAlert` and the reply to the system must be).
#[cfg(target_os = "macos")]
fn run_on_main(app: &AppHandle, f: impl FnOnce() + Send + 'static) {
    if app.run_on_main_thread(f).is_err() {
        tracing::warn!("the quit question couldn't be put on the main thread");
        ASKING.store(false, Ordering::SeqCst);
    }
}

/// The answer is yes (or nothing was on): leave.
fn go(app: &AppHandle, mode: Mode) {
    LEAVING.store(true, Ordering::SeqCst);
    match mode {
        Mode::Quit => leave(app, false),
        Mode::SafeRestart => leave(app, true),
        // The system asked: it is told yes, and quits through its own path (tao's
        // `applicationWillTerminate:` runs the clean exit).
        #[cfg(target_os = "macos")]
        Mode::Terminate => run_on_main(app, || crate::mac_quit::reply_terminate(true)),
    }
}

/// Exits, and starts again in Safe Mode after when asked to.
fn leave(app: &AppHandle, relaunch_safe: bool) {
    #[cfg(target_os = "macos")]
    RELAUNCH_SAFE.store(relaunch_safe, Ordering::SeqCst);
    #[cfg(not(target_os = "macos"))]
    let _ = relaunch_safe;
    app.exit(0);
}

/// What is running that quitting would end.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Active {
    /// Streaming to a platform (the Studio's stream, or Go live's status).
    pub stream: bool,
    pub record: bool,
    pub replay: bool,
}

impl Active {
    /// From Go live's "live on any platform" and the Studio's stats (`stream`, `record` and
    /// `replay` each `{ active }`, or null).
    pub(crate) fn from(live: bool, stats: Option<&Value>) -> Active {
        let on = |k: &str| stats.is_some_and(|s| s[k]["active"] == true);
        Active { stream: live || on("stream"), record: on("record"), replay: on("replay") }
    }

    fn any(&self) -> bool {
        self.stream || self.record || self.replay
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Nothing on air: quit now.
    Quit,
    /// Ask first, with these words.
    Ask { title: &'static str, text: String },
}

/// Quit at once, or ask? The words say what is on and what quitting does to it.
pub(crate) fn decide(a: &Active) -> Decision {
    if !a.any() {
        return Decision::Quit;
    }
    let mut on = Vec::new();
    let mut ends = Vec::new();
    if a.stream {
        on.push("live");
        ends.push("This ends your stream");
    }
    if a.record {
        on.push("recording");
        ends.push("The recording stops and is saved");
    }
    if a.replay {
        on.push("the replay buffer is on");
        ends.push("The replay buffer's last seconds are lost");
    }
    // "You're live and recording." / "The replay buffer is on." / "You're recording."
    let head = if on.len() == 1 && a.replay && !a.stream && !a.record {
        "The replay buffer is on.".to_string()
    } else {
        let people: Vec<&str> = on.iter().copied().filter(|s| *s != "the replay buffer is on").collect();
        let mut s = format!("You're {}", people.join(" and "));
        if a.replay {
            s.push_str(", and the replay buffer is on");
        }
        s.push('.');
        s
    };
    Decision::Ask { title: "Quit Ferrocast?", text: format!("{head} Quit anyway? {}.", ends.join(". ")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(a: Active) -> String {
        match decide(&a) {
            Decision::Ask { text, .. } => text,
            Decision::Quit => "quit".into(),
        }
    }

    #[test]
    fn nothing_on_quits_at_once() {
        assert_eq!(decide(&Active::default()), Decision::Quit);
        assert_eq!(Active::from(false, None), Active::default());
        assert_eq!(
            Active::from(false, Some(&json!({ "stream": null, "record": null, "replay": { "active": false } }))),
            Active::default()
        );
    }

    #[test]
    fn anything_on_asks_in_words_that_match() {
        assert_eq!(
            text(Active { stream: true, ..Default::default() }),
            "You're live. Quit anyway? This ends your stream."
        );
        assert_eq!(
            text(Active { record: true, ..Default::default() }),
            "You're recording. Quit anyway? The recording stops and is saved."
        );
        assert_eq!(
            text(Active { replay: true, ..Default::default() }),
            "The replay buffer is on. Quit anyway? The replay buffer's last seconds are lost."
        );
        assert_eq!(
            text(Active { stream: true, record: true, replay: false }),
            "You're live and recording. Quit anyway? This ends your stream. The recording stops and is saved."
        );
        let all = text(Active { stream: true, record: true, replay: true });
        assert!(all.starts_with("You're live and recording, and the replay buffer is on. Quit anyway?"), "{all}");
    }

    #[test]
    fn it_reads_what_is_on_from_go_live_and_the_studio_stats() {
        let stats = json!({ "stream": { "active": true }, "record": { "active": true }, "replay": null });
        assert_eq!(Active::from(false, Some(&stats)), Active { stream: true, record: true, replay: false });
        // Live on a platform (Go live) counts even when the Studio's stats aren't there.
        assert_eq!(Active::from(true, None), Active { stream: true, record: false, replay: false });
        // A recording that stopped by itself is not on.
        assert_eq!(
            Active::from(false, Some(&json!({ "record": { "active": false, "error": "disk full" } }))),
            Active::default()
        );
    }
}
