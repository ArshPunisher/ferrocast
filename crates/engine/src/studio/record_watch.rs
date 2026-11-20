//! The recording's watch: a recording that stopped by itself (a full disk, a drive taken away)
//! is said once, in one line with what to do behind its (i), and room running low is said once
//! before it runs out (M7-01). Looks every couple of seconds with the live watch's loop.

use serde_json::Value;

use crate::Engine;
use crate::frames::NoticeLevel;

/// Free space below which a recording is warned about, in MB (2 GB).
const LOW_MB: u64 = 2 * 1024;
/// Or room for fewer minutes than this at the rate it has been writing.
const LOW_MINUTES: f64 = 10.0;
/// How long a recording must have run before its rate says anything.
const RATE_AFTER_S: u64 = 30;

/// What the watch has said for the recording it is looking at (its file), so each thing is said
/// once and not again at every look.
#[derive(Debug, Default)]
pub(crate) struct RecordWatch {
    file: String,
    stopped_said: bool,
    low_said: bool,
    /// Automatic new files already said (the count is the recording's, over its files).
    splits_said: u64,
}

/// What was said about broken sidechain cycles so the user is told once per unique loop.
#[derive(Debug, Default)]
pub(crate) struct SidechainWatch {
    reported: Vec<(String, String)>,
}

/// Checks broken sidechain cycles reported by the Studio and emits one concise notice when a new
/// cycle appears, clearing once the loop is fixed.
pub(crate) fn look_sidechains(w: &mut SidechainWatch, v: &Value) -> Option<Say> {
    let cycles: Vec<(String, String)> = v
        .get("sidechain_cycles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let src = c.get("source").and_then(Value::as_str)?.to_string();
            let key = c.get("key").and_then(Value::as_str)?.to_string();
            Some((src, key))
        })
        .collect();
    if cycles.is_empty() {
        w.reported.clear();
        return None;
    }
    let fresh = cycles.iter().find(|pair| !w.reported.contains(pair)).cloned();
    w.reported = cycles;
    let (src, key) = fresh?;
    let (line, detail) = if src == key {
        (
            format!("Sidechain loop on {src} was broken"),
            format!(
                "{src} can't duck under itself. Its sidechain compressor gets silence until you pick another source."
            ),
        )
    } else {
        (
            format!("Sidechain loop between {src} and {key} was broken"),
            format!(
                "{src} and {key} can't duck under each other. {src}'s sidechain compressor gets silence until you change one."
            ),
        )
    };
    Some(Say { key: "sidechain-cycle", level: NoticeLevel::Warn, line, detail })
}

/// A line for the dashboard: how bad, the line, and what is behind its (i).
#[derive(Debug, PartialEq)]
pub(crate) struct Say {
    pub key: &'static str,
    pub level: NoticeLevel,
    pub line: String,
    pub detail: String,
}

/// One look at the recording's stats → what to say now (nothing, mostly).
pub(crate) fn look(w: &mut RecordWatch, v: &Value) -> Vec<Say> {
    let mut out = Vec::new();
    let Some(file) = v.get("file").and_then(Value::as_str) else {
        // No recording: the next one starts afresh.
        *w = RecordWatch::default();
        return out;
    };
    if w.file != file {
        *w = RecordWatch { file: file.to_string(), splits_said: w.splits_said, ..RecordWatch::default() };
    }
    // The encoder was rebuilt and the recording went on in a new file: said once each time.
    let splits = v["auto_splits"].as_u64().unwrap_or(0);
    if splits > w.splits_said {
        w.splits_said = splits;
        out.push(Say {
            key: "recording-split",
            level: NoticeLevel::Info,
            line: "Recording continued in a new file".into(),
            detail: "The video encoder was restarted (the display woke, or the system reset it), \
                     so a new file keeps every part playable."
                .into(),
        });
    }
    let active = v["active"].as_bool() == Some(true);
    if !active {
        if let Some(why) = v["error"].as_str()
            && !w.stopped_said
        {
            w.stopped_said = true;
            // The stats' line is "The recording stopped: the disk is full."
            let line = why.trim_end_matches('.').replace("The recording stopped", "Recording stopped");
            out.push(Say {
                key: "record-stopped",
                level: NoticeLevel::Error,
                line,
                detail: v["error_detail"].as_str().unwrap_or("What was recorded until now is saved.").into(),
            });
        }
        return out;
    }
    if w.low_said {
        return out;
    }
    let free_mb = v["disk_free_mb"].as_u64();
    let (bytes, seconds) = (v["bytes"].as_u64().unwrap_or(0), v["seconds"].as_u64().unwrap_or(0));
    let minutes_left = free_mb.filter(|_| seconds >= RATE_AFTER_S && bytes > 0).map(|mb| {
        let per_s = bytes as f64 / seconds as f64;
        mb as f64 * 1024.0 * 1024.0 / per_s / 60.0
    });
    let low = free_mb.is_some_and(|mb| mb < LOW_MB) || minutes_left.is_some_and(|m| m < LOW_MINUTES);
    if low {
        w.low_said = true;
        let line = match minutes_left {
            Some(m) => format!("Low disk space: about {} min of recording left", m.max(1.0).round()),
            None => format!("Low disk space: {} left", gb(free_mb.unwrap_or(0))),
        };
        out.push(Say {
            key: "record-space",
            level: NoticeLevel::Warn,
            line,
            detail: format!(
                "{} is free where it records. When it runs out the recording stops and keeps what it has. \
                 Free some space or stop and choose another drive.",
                gb(free_mb.unwrap_or(0))
            ),
        });
    }
    out
}

fn gb(mb: u64) -> String {
    if mb >= 1024 { format!("{:.1} GB", mb as f64 / 1024.0) } else { format!("{mb} MB") }
}

impl Engine {
    /// One look at the Studio's recording; what it finds is said on the dashboard.
    pub(crate) async fn record_watch_tick(&self) {
        let Ok(host) = self.studio_host() else { return };
        // What a capture couldn't say itself (a microphone chosen instead of a Bluetooth one):
        // this Studio's engine's, not another's.
        for (key, line, detail) in host.take_notices().await {
            self.notice_detail(&key, NoticeLevel::Info, &line, &detail);
        }
        // A browser source whose page stopped drawing: one notice each, with a Reload, and a calm
        // line when it draws again (the same key replaces the first).
        let now = host.stalled_sources().await;
        let says = {
            let mut noted = self.studio().stalled_noted.lock().unwrap_or_else(|p| p.into_inner());
            stall_says(&mut noted, &now)
        };
        for s in says {
            if let Some(path) = s.reload {
                self.notice_post(
                    &s.key,
                    s.level,
                    &s.line,
                    &s.detail,
                    ("Reload", &path, serde_json::json!({ "do": "reload" })),
                );
            } else {
                self.notice_detail(&s.key, s.level, &s.line, &s.detail);
            }
        }
        let Ok(v) = host.record_stats().await else { return };
        let says = {
            let mut w = self.studio().record_watch.lock().unwrap_or_else(|p| p.into_inner());
            look(&mut w, &v)
        };
        for s in says {
            self.notice_detail(s.key, s.level, &s.line, &s.detail);
        }
    }

    /// Observes `sidechain_cycles` in Studio stats and notifies the user once per unique loop.
    pub(crate) fn observe_sidechain_cycles(&self, stats: &Value) {
        let say = {
            let mut w = self.studio().sidechain_watch.lock().unwrap_or_else(|p| p.into_inner());
            look_sidechains(&mut w, stats)
        };
        if let Some(s) = say {
            self.notice_detail(s.key, s.level, &s.line, &s.detail);
        }
    }
}

/// One line about a browser source: said when it stops drawing, and when it draws again.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StallSay {
    pub key: String,
    pub level: NoticeLevel,
    pub line: String,
    pub detail: String,
    /// The route that reloads the source, while it is stalled.
    pub reload: Option<String>,
}

/// A name as a URL path piece (JavaScript's encodeURIComponent: the dashboard decodes it).
fn percent(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// What to say about the sources now (`(name, stalled)`), given the ones already said to be
/// stalled: each stalled source once, each recovered one once, nothing while nothing changes.
pub(crate) fn stall_says(noted: &mut std::collections::HashSet<String>, now: &[(String, bool)]) -> Vec<StallSay> {
    let mut out = Vec::new();
    for (name, stalled) in now {
        let key = format!("web-stalled:{name}");
        if *stalled && noted.insert(name.clone()) {
            out.push(StallSay {
                key,
                level: NoticeLevel::Warn,
                line: format!("{name} stopped drawing"),
                detail: "Its browser stopped sending pictures and starting it again didn't help. Reload starts the page over."
                    .into(),
                reload: Some(format!("/api/v1/studio/engine/sources/{}/web", percent(name))),
            });
        } else if !*stalled && noted.remove(name) {
            out.push(StallSay {
                key,
                level: NoticeLevel::Info,
                line: format!("{name} is drawing again"),
                detail: String::new(),
                reload: None,
            });
        }
    }
    // A source that went away while stalled: its notice is not left behind.
    noted.retain(|n| now.iter().any(|(name, _)| name == n));
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_stalled_source_is_said_once_with_a_reload_and_again_when_it_recovers() {
        let mut noted = std::collections::HashSet::new();
        let stalled = vec![("Alerts".to_string(), true), ("Chat".to_string(), false)];
        let first = super::stall_says(&mut noted, &stalled);
        assert_eq!(first.len(), 1, "one notice, for the stalled one only");
        assert_eq!(first[0].key, "web-stalled:Alerts");
        assert!(first[0].line.contains("Alerts stopped drawing"));
        assert!(first[0].reload.as_deref().is_some_and(|p| p.ends_with("/sources/Alerts/web")));
        assert_eq!(super::percent("Chat & Alerts/1"), "Chat%20%26%20Alerts%2F1");
        assert!(super::stall_says(&mut noted, &stalled).is_empty(), "said once, not every tick");
        let back = super::stall_says(&mut noted, &[("Alerts".to_string(), false), ("Chat".to_string(), false)]);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].key, "web-stalled:Alerts", "the same key replaces the first line");
        assert!(back[0].reload.is_none());
        assert!(super::stall_says(&mut noted, &[("Alerts".to_string(), false)]).is_empty());
    }

    use serde_json::json;

    use super::*;

    fn rec(active: bool, error: Option<&str>, free_mb: u64, bytes: u64, seconds: u64) -> Value {
        json!({ "file": "/r/a.mp4", "active": active, "error": error,
                "disk_free_mb": free_mb, "bytes": bytes, "seconds": seconds,
                "error_detail": "Free some space or choose another drive, then record again." })
    }

    #[test]
    fn a_new_file_for_a_rebuilt_encoder_is_said_once_each_time() {
        let mut w = RecordWatch::default();
        let v = |file: &str, splits: u64| json!({ "file": file, "active": true, "auto_splits": splits, "bytes": 0, "seconds": 0 });
        assert!(look(&mut w, &v("/r/a.mp4", 0)).is_empty());
        let first = look(&mut w, &v("/r/a-2.mp4", 1));
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].key, first[0].level), ("recording-split", NoticeLevel::Info));
        assert_eq!(first[0].line, "Recording continued in a new file");
        assert!(look(&mut w, &v("/r/a-2.mp4", 1)).is_empty(), "not again at the next look");
        assert!(look(&mut w, &v("/r/a-3.mp4", 1)).is_empty(), "the new file name doesn't say it again");
        assert_eq!(look(&mut w, &v("/r/a-4.mp4", 2)).len(), 1, "a second time is said");
    }

    #[test]
    fn a_stopped_recording_is_said_once() {
        let mut w = RecordWatch::default();
        let stopped = rec(false, Some("The recording stopped: the disk is full."), 0, 1_000_000, 60);
        let first = look(&mut w, &stopped);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].line, "Recording stopped: the disk is full");
        assert!(first[0].line.len() <= 60);
        assert_eq!(first[0].level, NoticeLevel::Error);
        assert!(first[0].detail.contains("Free some space"));
        for _ in 0..50 {
            assert!(look(&mut w, &stopped).is_empty(), "not again at every look");
        }
        // A new recording that stops the same way is said again.
        let mut other = stopped.clone();
        other["file"] = json!("/r/b.mp4");
        assert_eq!(look(&mut w, &other).len(), 1);
        // No recording at all: nothing, and the watch forgets.
        assert!(look(&mut w, &Value::Null).is_empty());
        assert_eq!(look(&mut w, &stopped).len(), 1);
    }

    #[test]
    fn a_recording_that_runs_is_quiet_with_room_to_spare() {
        let mut w = RecordWatch::default();
        // 2 MB/s with 50 GB free: hours.
        assert!(look(&mut w, &rec(true, None, 50_000, 120_000_000, 60)).is_empty());
        // Stopped by the person (no error): nothing.
        assert!(look(&mut w, &rec(false, None, 50_000, 120_000_000, 60)).is_empty());
    }

    #[test]
    fn low_room_is_said_once_by_size_or_by_minutes() {
        // Under 2 GB: said at once, with no rate yet.
        let mut w = RecordWatch::default();
        let few = look(&mut w, &rec(true, None, 1500, 1_000_000, 5));
        assert_eq!(few.len(), 1);
        assert_eq!(few[0].line, "Low disk space: 1.5 GB left");
        assert_eq!(few[0].level, NoticeLevel::Warn);
        assert!(look(&mut w, &rec(true, None, 1400, 9_000_000, 20)).is_empty(), "once");
        // Plenty by size, but 4 MB/s with 8 GB free is 35 min: quiet; with 2.2 GB it is under 10 min: about 10.
        let mut w = RecordWatch::default();
        assert!(look(&mut w, &rec(true, None, 8000, 240_000_000, 60)).is_empty());
        let soon = look(&mut w, &rec(true, None, 2200, 240_000_000, 60));
        assert_eq!(soon.len(), 1, "{soon:?}");
        assert_eq!(soon[0].line, "Low disk space: about 10 min of recording left");
        // The rate is not trusted in the first seconds.
        let mut w = RecordWatch::default();
        assert!(look(&mut w, &rec(true, None, 2500, 50_000_000, 2)).is_empty());
    }

    #[test]
    fn a_sidechain_loop_is_reported_once_until_resolved() {
        let mut w = SidechainWatch::default();
        let loop_stats = json!({ "sidechain_cycles": [{ "source": "Voice", "key": "Music" }] });
        let first = look_sidechains(&mut w, &loop_stats).expect("first notice");
        assert_eq!(first.key, "sidechain-cycle");
        assert_eq!(first.level, NoticeLevel::Warn);
        assert!(first.line.contains("Voice") && first.line.contains("Music"));
        assert!(first.detail.contains("silence"));
        for _ in 0..50 {
            assert!(look_sidechains(&mut w, &loop_stats).is_none(), "not repeated");
        }
        // Cleared -> forgets, so recreating it later tells the user once again.
        assert!(look_sidechains(&mut w, &json!({})).is_none());
        assert!(look_sidechains(&mut w, &loop_stats).is_some());
    }
}
