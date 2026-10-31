//! The one stream coming in, shared with every destination. The publisher's packets go out on a
//! broadcast channel; the codec headers and metadata are kept so a destination that connects (or
//! reconnects) mid-stream can start with them. A destination that falls behind loses its oldest
//! packets, never slowing the publisher or the others.

use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use rml_rtmp::sessions::StreamMetadata;
use serde::Serialize;
use tokio::sync::{broadcast, watch};

use crate::flv;

/// How many packets a destination may fall behind (video and audio together, about ten seconds
/// at 60 fps) before it skips ahead to the next keyframe.
pub const BACKLOG: usize = 1024;

/// One packet of the stream, as the publisher sent it.
#[derive(Debug, Clone)]
pub enum Frame {
    Video {
        ts: u32,
        data: Bytes,
        key: bool,
    },
    Audio {
        ts: u32,
        data: Bytes,
    },
    Metadata(Arc<StreamMetadata>),
    /// The publisher stopped: destinations end their stream.
    End,
}

/// What a destination needs before the first packet.
#[derive(Debug, Clone, Default)]
pub struct Headers {
    pub metadata: Option<Arc<StreamMetadata>>,
    pub video: Option<Bytes>,
    pub audio: Option<Bytes>,
    /// Extra audio tracks' codec headers, by track (Twitch's VOD track).
    pub extra_audio: Vec<(u8, Bytes)>,
}

/// Which stream is on. Every new publish gets the next number, so a destination can tell a
/// stream that ended and started again apart from the one it was sending.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Live {
    pub stream: u64,
    pub on: bool,
}

/// What a destination gets when it starts sending: the stream it joined, the headers so far
/// and every packet from then on, with nothing missed in between.
#[derive(Debug)]
pub struct Subscription {
    pub live: Live,
    pub headers: Headers,
    pub rx: broadcast::Receiver<Frame>,
}

/// The shared stream. Cheap to clone through `Arc`.
#[derive(Debug)]
pub struct Hub {
    tx: broadcast::Sender<Frame>,
    state: Mutex<State>,
    live: watch::Sender<Live>,
}

#[derive(Debug, Default)]
struct State {
    headers: Headers,
    live: Live,
    stats: StreamStats,
    /// Bitrate window: its first packet's timestamp and the bytes since.
    window: Option<(u32, u64)>,
    last_key: Option<u32>,
}

/// What the incoming stream really is, measured from its packets (not OBS's settings), for
/// the upload check and the platform rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct StreamStats {
    /// Audio and video together over the last two seconds.
    pub kbps: u32,
    /// The latest gap between two keyframes.
    pub keyframe_interval_ms: Option<u32>,
    pub video_codec: Option<flv::VideoCodec>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
}

/// The bitrate window's length, in stream milliseconds.
const RATE_WINDOW_MS: u32 = 2_000;

impl Hub {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(BACKLOG);
        let (live, _) = watch::channel(Live::default());
        Arc::new(Self { tx, state: Mutex::new(State::default()), live })
    }

    /// Which stream is on; changes when a publisher starts or stops.
    pub fn live(&self) -> watch::Receiver<Live> {
        self.live.subscribe()
    }

    /// The incoming stream's measured numbers (all empty while nothing streams).
    pub fn stats(&self) -> StreamStats {
        let st = self.lock();
        if st.live.on { st.stats } else { StreamStats::default() }
    }

    pub fn subscribe(&self) -> Subscription {
        let st = self.lock();
        Subscription { live: st.live, headers: st.headers.clone(), rx: self.tx.subscribe() }
    }

    pub(crate) fn start(&self) {
        let mut st = self.lock();
        let stream = st.live.stream + 1;
        *st = State { live: Live { stream, on: true }, ..State::default() };
        self.live.send_replace(st.live);
    }

    pub(crate) fn end(&self) {
        let mut st = self.lock();
        st.live.on = false;
        let _ = self.tx.send(Frame::End);
        self.live.send_replace(st.live);
    }

    pub(crate) fn metadata(&self, m: StreamMetadata) {
        let m = Arc::new(m);
        let mut st = self.lock();
        st.stats.width = m.video_width;
        st.stats.height = m.video_height;
        st.stats.fps = m.video_frame_rate;
        st.headers.metadata = Some(m.clone());
        let _ = self.tx.send(Frame::Metadata(m));
    }

    pub(crate) fn video(&self, ts: u32, data: Bytes) {
        let mut st = self.lock();
        let key = flv::video_is_keyframe(&data);
        if flv::video_is_header(&data) {
            st.stats.video_codec = flv::video_codec(&data);
            st.headers.video = Some(data.clone());
        } else if key {
            if let Some(prev) = st.last_key {
                st.stats.keyframe_interval_ms = Some(ts.wrapping_sub(prev));
            }
            st.last_key = Some(ts);
        }
        st.count(ts, data.len());
        let _ = self.tx.send(Frame::Video { ts, data, key });
    }

    pub(crate) fn audio(&self, ts: u32, data: Bytes) {
        let mut st = self.lock();
        if flv::audio_is_header(&data) {
            st.headers.audio = Some(data.clone());
        } else if let Some((track, true)) = flv::audio_extra_track(&data) {
            st.headers.extra_audio.retain(|(t, _)| *t != track);
            st.headers.extra_audio.push((track, data.clone()));
        }
        st.count(ts, data.len());
        let _ = self.tx.send(Frame::Audio { ts, data });
    }

    /// Headers and the live state change, and packets are sent, under one lock, so `subscribe`
    /// never sees one without the other. A poisoned lock still holds a valid state.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl State {
    /// Adds a packet to the bitrate window; every two seconds of stream time, the rate.
    fn count(&mut self, ts: u32, len: usize) {
        let (start, bytes) = self.window.get_or_insert((ts, 0));
        *bytes += len as u64;
        let span = ts.wrapping_sub(*start);
        if (RATE_WINDOW_MS..u32::MAX / 2).contains(&span) {
            self.stats.kbps = (*bytes * 8 / u64::from(span)) as u32;
            self.window = Some((ts, 0));
        } else if span >= u32::MAX / 2 {
            // Time went backwards (a timestamp reset): start again.
            self.window = Some((ts, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_measure_the_real_stream() {
        let hub = Hub::new();
        hub.start();
        let mut m = StreamMetadata::new();
        m.video_width = Some(1920);
        m.video_height = Some(1080);
        m.video_frame_rate = Some(60.0);
        hub.metadata(m);
        hub.video(0, Bytes::from_static(&[0x17, 0x00, 1, 2]));
        // 750 kB/s of video for 4 s, a keyframe every 2 s: 6000 kbps.
        for i in 0..=40u32 {
            let ts = i * 100;
            let mut d = vec![if i % 20 == 0 { 0x17 } else { 0x27 }, 0x01];
            d.resize(75_000, 0);
            hub.video(ts, Bytes::from(d));
        }
        let s = hub.stats();
        assert_eq!(s.video_codec, Some(flv::VideoCodec::H264));
        assert_eq!(s.keyframe_interval_ms, Some(2000));
        assert!((5900..=6100).contains(&s.kbps), "{}", s.kbps);
        assert_eq!((s.width, s.height, s.fps), (Some(1920), Some(1080), Some(60.0)));
        // Ended: nothing to report. A new stream starts with empty numbers.
        hub.end();
        assert_eq!(hub.stats(), StreamStats::default());
        hub.start();
        assert_eq!(hub.stats(), StreamStats::default());
    }
}
