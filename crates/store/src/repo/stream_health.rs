//! Local persistence of the last 5 streams' health snapshots (settings key
//! `stream_health_history`).

use ferrocast_core::diagnostics::StreamHealthRecord;

use crate::Store;
use crate::error::Result;
use crate::repo::settings;

const SETTINGS_KEY: &str = "stream_health_history";
/// Number of most recent streams retained locally for support reports.
pub const MAX_STREAMS: usize = 5;

/// Inserts or updates a stream health record and keeps the newest [`MAX_STREAMS`] entries.
pub fn record(store: &Store, mut entry: StreamHealthRecord) -> Result<()> {
    entry.destinations.sort_by(|a, b| a.platform.cmp(&b.platform).then(a.target.cmp(&b.target)));
    let mut list: Vec<StreamHealthRecord> = settings::get_or_default(store, SETTINGS_KEY)?;
    if let Some(pos) = list.iter().position(|x| x.id == entry.id) {
        list[pos] = entry;
    } else {
        list.push(entry);
    }
    list.sort_by(|a, b| b.started_at.0.cmp(&a.started_at.0).then(b.id.cmp(&a.id)));
    list.truncate(MAX_STREAMS);
    settings::set(store, SETTINGS_KEY, &list)
}

/// Returns up to `limit` (capped at [`MAX_STREAMS`]) recent stream health records, newest first.
pub fn last(store: &Store, limit: usize) -> Result<Vec<StreamHealthRecord>> {
    let mut list: Vec<StreamHealthRecord> = settings::get_or_default(store, SETTINGS_KEY)?;
    list.sort_by(|a, b| b.started_at.0.cmp(&a.started_at.0).then(b.id.cmp(&a.id)));
    list.truncate(limit.min(MAX_STREAMS));
    Ok(list)
}

/// Marks any still-`live` stream record as ended uncleanly (called when startup finds an unclean
/// exit marker).
pub fn mark_unended_as_unclean(store: &Store) -> Result<()> {
    let mut list: Vec<StreamHealthRecord> = settings::get_or_default(store, SETTINGS_KEY)?;
    let mut changed = false;
    for item in &mut list {
        if item.live {
            item.live = false;
            item.unclean_end = true;
            if !item.issues.iter().any(|i| i == "unclean_end") {
                item.issues.push("unclean_end".into());
            }
            changed = true;
        }
    }
    if changed {
        settings::set(store, SETTINGS_KEY, &list)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ferrocast_core::event::{Millis, Platform};

    use super::*;

    fn sample_stream(id: &str, started_at: i64, live: bool) -> StreamHealthRecord {
        StreamHealthRecord {
            id: id.into(),
            started_at: Millis(started_at),
            ended_at: (!live).then_some(Millis(started_at + 60_000)),
            duration_s: 60,
            platforms: vec![Platform::Twitch],
            live,
            unclean_end: false,
            fps_target: 60,
            canvas: "1920x1080".into(),
            encoder_id: Some("obs_x264".into()),
            sample_rate_hz: 48_000,
            dropped_frames: 10,
            skipped_frames: 0,
            lagged_frames: 0,
            rendered_frames: 3600,
            avg_fps: 60.0,
            cpu_pct: 24.5,
            memory_mb: 640,
            frame_ms: 8.2,
            destinations: vec![],
            issues: vec![],
        }
    }

    #[test]
    fn keeps_only_the_newest_five_and_updates_existing_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for i in 1..=7 {
            record(&store, sample_stream(&format!("s{i}"), i * 1000, false)).unwrap();
        }
        let got = last(&store, 10).unwrap();
        assert_eq!(got.len(), 5);
        assert_eq!(got.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["s7", "s6", "s5", "s4", "s3"]);

        let mut updated = sample_stream("s7", 7000, true);
        updated.dropped_frames = 99;
        record(&store, updated).unwrap();
        assert_eq!(last(&store, 5).unwrap().len(), 5);
        assert_eq!(last(&store, 5).unwrap()[0].dropped_frames, 99);

        mark_unended_as_unclean(&store).unwrap();
        let after = last(&store, 5).unwrap();
        assert!(!after[0].live);
        assert!(after[0].unclean_end);
        assert!(after[0].issues.contains(&"unclean_end".to_string()));
    }
}
