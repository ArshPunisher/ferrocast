//! Quitting while something is on air asks first, as OBS's "Show active outputs warning on exit"
//! does: the stream, a recording and the replay buffer all end with the app. Every way of
//! quitting that the person chooses goes through [`request`]: the menu's Quit (⌘Q), the tray's
//! Quit, the Dock's Quit, and closing the window when it isn't set to hide to the tray.
//! (A signal from the system, `kill` or a logout, can't be asked and still quits cleanly.)

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// One question at a time: a second Quit while the first is open does nothing.
static ASKING: AtomicBool = AtomicBool::new(false);

/// Quits, asking first when the stream, a recording or the replay buffer is on.
pub(crate) fn request(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let active = match app.try_state::<crate::Running>() {
            Some(r) => {
                let stats = r.engine.studio_stats().await.ok();
                Active::from(r.engine.is_live(), stats.as_ref())
            }
            None => Active::default(),
        };
        match decide(&active) {
            Decision::Quit => app.exit(0),
            Decision::Ask { title, text } => {
                if ASKING.swap(true, Ordering::SeqCst) {
                    return;
                }
                crate::show_window(&app);
                let quit = app.clone();
                // "Cancel" first: it is the default button and what Esc picks. The answer is
                // true for the first button.
                app.dialog()
                    .message(text)
                    .title(title)
                    .kind(MessageDialogKind::Warning)
                    .buttons(MessageDialogButtons::OkCancelCustom("Cancel".into(), "Quit".into()))
                    .show(move |cancelled| {
                        ASKING.store(false, Ordering::SeqCst);
                        if !cancelled {
                            quit.exit(0);
                        }
                    });
            }
        }
    });
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
