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
        // What a capture couldn't say itself (a microphone chosen instead of a Bluetooth one).
        for n in ferrocore::notices::take() {
            self.notice_detail(n.key, NoticeLevel::Info, &n.line, &n.detail);
        }
        let Ok(host) = self.studio_host() else { return };
        let Ok(v) = host.record_stats().await else { return };
        let says = {
            let mut w = self.studio().record_watch.lock().unwrap_or_else(|p| p.into_inner());
            look(&mut w, &v)
        };
        for s in says {
            self.notice_detail(s.key, s.level, &s.line, &s.detail);
        }
    }
}

#[cfg(test)]
mod tests {
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
}
