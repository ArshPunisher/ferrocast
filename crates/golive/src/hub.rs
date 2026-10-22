//! The one stream coming in, shared with every destination. The publisher's packets go out on a
//! broadcast channel; the codec headers and metadata are kept so a destination that connects (or
//! reconnects) mid-stream can start with them. A destination that falls behind loses its oldest
//! packets, never slowing the publisher or the others.

use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use rml_rtmp::sessions::StreamMetadata;
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
}

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

    pub fn subscribe(&self) -> Subscription {
        let st = self.lock();
        Subscription { live: st.live, headers: st.headers.clone(), rx: self.tx.subscribe() }
    }

    pub(crate) fn start(&self) {
        let mut st = self.lock();
        st.headers = Headers::default();
        st.live = Live { stream: st.live.stream + 1, on: true };
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
        st.headers.metadata = Some(m.clone());
        let _ = self.tx.send(Frame::Metadata(m));
    }

    pub(crate) fn video(&self, ts: u32, data: Bytes) {
        let mut st = self.lock();
        if flv::video_is_header(&data) {
            st.headers.video = Some(data.clone());
        }
        let key = flv::video_is_keyframe(&data);
        let _ = self.tx.send(Frame::Video { ts, data, key });
    }

    pub(crate) fn audio(&self, ts: u32, data: Bytes) {
        let mut st = self.lock();
        if flv::audio_is_header(&data) {
            st.headers.audio = Some(data.clone());
        }
        let _ = self.tx.send(Frame::Audio { ts, data });
    }

    /// Headers and the live state change, and packets are sent, under one lock, so `subscribe`
    /// never sees one without the other. A poisoned lock still holds a valid state.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}
