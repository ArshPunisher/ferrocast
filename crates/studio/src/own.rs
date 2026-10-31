//! Streaming and recording with Ferrocast's own encoders (ENGINE.md E1, behind the "own
//! encoder" switch): libobs still draws and mixes, but its finished frames and sound go through
//! ferrocast-av's encoders, out over RTMP by Go live's own sender to the same server libobs
//! would send to (Go live's ingest on this computer), or into a crash-safe MP4. Everything after
//! looks the same to the rest of the app.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ferrocast_av::audio::AudioConfig;
use ferrocast_av::pipeline::{Pipeline, Tag};
use ferrocast_av::video::{Frame, PixelFormat, RateControl, VideoCodec, VideoConfig};
use ferrocast_golive::push::{self, Destination, Health, PushState, Retry};
use ferrocast_golive::{Hub, Target};

use crate::{EncodeSettings, ProgramFeed, RecordPlan, RecordStats, StreamSettings, StreamStats, Studio, StudioError};

/// Why this stream can't use our encoders (it then uses libobs's), or None when it can.
pub(crate) fn unsupported(s: &StreamSettings, main_canvas: bool) -> Option<&'static str> {
    if !main_canvas {
        Some("the vertical picture's stream")
    } else if s.delay_s > 0 {
        Some("a stream delay")
    } else if s.vod_track.is_some() {
        Some("Twitch's VOD track")
    } else if s.encode.scale.is_some() {
        Some("a scaled stream")
    } else if s.encode.quality.is_some() {
        Some("constant quality")
    } else if !s.encode.extra.is_empty() {
        Some("an encoder's own options")
    } else if !own_codec_ok(&s.encode.video_encoder) {
        Some("this encoder (ours is H.264 or HEVC on the Mac's hardware)")
    } else if !cfg!(target_os = "macos") {
        Some("this system (our encoders are Mac-only so far)")
    } else {
        None
    }
}

/// Why this recording can't use our encoders, or None when it can.
pub(crate) fn unsupported_recording(plan: &RecordPlan) -> Option<&'static str> {
    let tracks_one = plan.tracks.is_empty() || plan.tracks == [1];
    if !matches!(plan.format.as_str(), "mp4" | "fmp4") {
        Some("this format (ours writes MP4)")
    } else if plan.split_minutes > 0 || plan.split_mb > 0 {
        Some("splitting a recording")
    } else if !tracks_one {
        Some("more than one sound track")
    } else if !plan.muxer.trim().is_empty() {
        Some("custom muxer settings")
    } else if plan.encode.quality.is_some() {
        Some("constant quality")
    } else if !plan.encode.extra.is_empty() {
        Some("an encoder's own options")
    } else if plan.encode.scale.is_some() {
        Some("a scaled recording")
    } else if !own_codec_ok(&plan.encode.video_encoder) {
        Some("this encoder (ours is H.264 or HEVC on the Mac's hardware)")
    } else if !cfg!(target_os = "macos") {
        Some("this system (our encoders are Mac-only so far)")
    } else {
        None
    }
}

/// Whether the picked encoder is one ours stands in for: the Mac's hardware H.264 or HEVC.
fn own_codec_ok(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    !(id.contains("av1") || id.contains("aom") || id.contains("svt") || id.contains("x264") || id.contains("prores"))
}

#[derive(Debug, Default)]
struct Counts {
    bytes: AtomicU64,
    frames: AtomicU32,
    running: AtomicBool,
    error: Mutex<Option<String>>,
}

/// Where a stream goes: the server, the key, and how it reconnects.
pub(crate) struct Send<'a> {
    pub server: &'a str,
    pub key: &'a str,
    pub reconnect: Option<(u32, u32)>,
}

/// A stream or recording through our encoders; dropping it ends it (the encoders flushed, the
/// server told, the file closed).
#[derive(Debug)]
pub(crate) struct OwnStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    counts: Arc<Counts>,
    /// The sender's health and its stop, when streaming.
    health: Option<tokio::sync::watch::Receiver<Health>>,
    stop_push: Option<tokio::sync::watch::Sender<bool>>,
    runtime: Option<tokio::runtime::Runtime>,
    /// libobs's side of the hand-over (kept on this thread: it holds the Studio); its
    /// channels are with the encoder thread.
    feed: Option<ProgramFeed>,
}

impl OwnStream {
    /// A stream as `s` says.
    pub(crate) fn stream(studio: &Studio, server: &str, key: &str, s: &StreamSettings) -> Result<Self, StudioError> {
        Self::start(studio, Some(Send { server, key, reconnect: s.reconnect }), None, &s.encode)
    }

    /// A recording to `path` (a fragmented MP4: it plays even after a crash).
    pub(crate) fn record(studio: &Studio, path: &std::path::Path, plan: &RecordPlan) -> Result<Self, StudioError> {
        Self::start(studio, None, Some(path), &plan.encode)
    }

    fn start(studio: &Studio, send_to: Option<Send<'_>>, record: Option<&std::path::Path>, e: &EncodeSettings) -> Result<Self, StudioError> {
        let fail = |e: String| StudioError::Failed(format!("Ferrocast's own encoder couldn't start: {e}"));
        let target = match &send_to {
            Some(d) => Some(Target::parse(d.server).map_err(|e| fail(e.to_string()))?),
            None => None,
        };
        let mut feed = studio.program_feed()?;
        // The channels go to the encoder thread; the feed itself stays here.
        let frames = std::mem::replace(&mut feed.video, std::sync::mpsc::sync_channel(0).1);
        let sound = std::mem::replace(&mut feed.sound, std::sync::mpsc::sync_channel(0).1);
        let codec = if e.video_encoder.to_ascii_lowercase().contains("hevc") { VideoCodec::Hevc } else { VideoCodec::H264 };
        let video = VideoConfig {
            codec,
            input: PixelFormat::Nv12,
            width: feed.width,
            height: feed.height,
            fps_num: feed.fps_num,
            fps_den: feed.fps_den.max(1),
            bitrate_kbps: e.video_kbps.max(100),
            rate: if e.vbr { RateControl::Average } else { RateControl::Cbr },
            keyframe_secs: if e.keyframe_s == 0 { 2 } else { e.keyframe_s },
        };
        let audio = AudioConfig { sample_rate: feed.sample_rate, channels: 2, bitrate_kbps: e.audio_kbps.clamp(64, 320) };
        let mut pipe = Pipeline::new(video, audio, record).map_err(|e| fail(e.to_string()))?;

        let hub = Hub::new();
        let (mut runtime, mut health, mut stop_push) = (None, None, None);
        if let (Some(d), Some(target)) = (send_to, target) {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .thread_name("ferrocast-own-stream")
                .build()
                .map_err(|e| fail(e.to_string()))?;
            let retry = match d.reconnect {
                Some((_, every)) => Retry { first: Duration::from_secs(u64::from(every.max(1))), ..Retry::default() },
                None => Retry::default(),
            };
            let dest = Destination { id: "studio".into(), target, key: d.key.to_string(), retry, extra_audio: false };
            let (health_tx, h) = tokio::sync::watch::channel(Health { state: PushState::Waiting, kbps: 0, dropped: 0, since_ms: 0 });
            let (sp, stop_rx) = tokio::sync::watch::channel(false);
            rt.spawn(push::run(dest, hub.clone(), health_tx, stop_rx));
            (runtime, health, stop_push) = (Some(rt), Some(h), Some(sp));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let counts = Arc::new(Counts { running: AtomicBool::new(true), ..Default::default() });
        let thread = {
            let (stop, counts) = (stop.clone(), counts.clone());
            std::thread::Builder::new()
                .name("ferrocast-own-encoder".into())
                .spawn(move || {
                    hub.start();
                    let send = |tags: Vec<Tag>, counts: &Counts| {
                        for t in tags {
                            match t {
                                Tag::Video { ms, body } => {
                                    counts.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                                    hub.video(ms, body.into());
                                }
                                Tag::Audio { ms, body } => {
                                    counts.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                                    hub.audio(ms, body.into());
                                }
                            }
                        }
                    };
                    // The first error shows at once (the app polls last_error) and ends the stream.
                    let fail_now = |e: String| {
                        if let Ok(mut err) = counts.error.lock()
                            && err.is_none()
                        {
                            *err = Some(e);
                        }
                    };
                    while !stop.load(Ordering::Acquire) {
                        if let Ok(f) = frames.recv_timeout(Duration::from_millis(100)) {
                            let frame = Frame::Nv12 { y: &f.y, y_stride: f.y_stride, uv: &f.uv, uv_stride: f.uv_stride };
                            match pipe.video(frame, f.timestamp_ns) {
                                Ok(t) => {
                                    send(t, &counts);
                                    counts.frames.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    fail_now(e.to_string());
                                    break;
                                }
                            }
                        }
                        let mut sound_failed = false;
                        while let Ok(snd) = sound.try_recv() {
                            match pipe.audio(&snd.samples, snd.timestamp_ns) {
                                Ok(t) => send(t, &counts),
                                Err(e) => {
                                    fail_now(e.to_string());
                                    sound_failed = true;
                                    break;
                                }
                            }
                        }
                        if sound_failed {
                            break;
                        }
                        if pipe.recording_failed() {
                            fail_now("the recording couldn't be written (is the disk full?)".into());
                            break;
                        }
                    }
                    if let Ok(t) = pipe.finish() {
                        send(t, &counts);
                    }
                    hub.end();
                    counts.running.store(false, Ordering::Release);
                })
                .map_err(|e| fail(e.to_string()))?
        };
        tracing::info!(?codec, kbps = video.bitrate_kbps, recording = record.is_some(), "the Studio uses Ferrocast's own encoder");
        Ok(Self { stop, thread: Some(thread), counts, health, stop_push, runtime, feed: Some(feed) })
    }

    pub(crate) fn stats(&self) -> StreamStats {
        let state = self.health.as_ref().map(|h| h.borrow().state.clone());
        StreamStats {
            active: self.counts.running.load(Ordering::Acquire),
            reconnecting: matches!(state, Some(PushState::Reconnecting { .. } | PushState::Connecting)),
            bytes: self.counts.bytes.load(Ordering::Relaxed),
            frames: self.counts.frames.load(Ordering::Relaxed),
            dropped: self.feed.as_ref().map_or(0, |f| f.dropped()).min(u64::from(u32::MAX)) as u32,
        }
    }

    pub(crate) fn record_stats(&self) -> RecordStats {
        let s = self.stats();
        RecordStats { active: s.active, paused: false, bytes: s.bytes, frames: s.frames }
    }

    pub(crate) fn last_error(&self) -> Option<String> {
        if let Some(e) = self.counts.error.lock().ok().and_then(|e| e.clone()) {
            return Some(e);
        }
        match self.health.as_ref().map(|h| h.borrow().state.clone()) {
            Some(PushState::Reconnecting { reason, .. }) => Some(reason),
            _ => None,
        }
    }
}

impl Drop for OwnStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // libobs stops handing frames over.
        self.feed = None;
        // A moment for the last packets to leave, then the sender stops.
        if let Some(sp) = self.stop_push.take() {
            std::thread::sleep(Duration::from_millis(300));
            let _ = sp.send(true);
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EncodeSettings;

    fn settings(id: &str) -> StreamSettings {
        StreamSettings { encode: EncodeSettings::with_encoder(id), reconnect: None, delay_s: 0, vod_track: None, own_encoder: true }
    }

    #[test]
    fn only_what_ours_does_goes_through_ours() {
        let hw = "com.apple.videotoolbox.videoencoder.ave.avc";
        if cfg!(target_os = "macos") {
            assert_eq!(unsupported(&settings(hw), true), None);
            assert_eq!(unsupported(&settings("com.apple.videotoolbox.videoencoder.ave.hevc"), true), None);
        }
        assert!(unsupported(&settings(hw), false).is_some(), "vertical");
        assert!(unsupported(&settings("obs_x264"), true).is_some());
        assert!(unsupported(&settings("av1_texture_amf"), true).is_some());
        let mut extra = settings(hw);
        extra.encode.extra.insert("preset".into(), serde_json::json!("fast"));
        assert!(unsupported(&extra, true).is_some());
        assert!(unsupported(&StreamSettings { delay_s: 5, ..settings(hw) }, true).is_some());
        assert!(unsupported(&StreamSettings { vod_track: Some(2), ..settings(hw) }, true).is_some());
    }

    #[test]
    fn recordings_ours_can_write() {
        let plan = RecordPlan { encode: EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc"), ..RecordPlan::default() };
        if cfg!(target_os = "macos") {
            assert_eq!(unsupported_recording(&plan), None);
        }
        assert!(unsupported_recording(&RecordPlan { format: "mkv".into(), ..plan.clone() }).is_some());
        assert!(unsupported_recording(&RecordPlan { split_minutes: 10, ..plan.clone() }).is_some());
        assert!(unsupported_recording(&RecordPlan { tracks: vec![1, 2], ..plan.clone() }).is_some());
    }
}
