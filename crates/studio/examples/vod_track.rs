//! Twitch's VOD track on a real engine (macOS: FERROCAST_OBS_DIR, else an installed OBS.app):
//! the Studio streams with `vod_track` to a small RTMP server here, which sees the main audio
//! (track 0) and the VOD track (Enhanced RTMP multitrack audio, track 1), as OBS sends them.
//!
//!   cargo run -p ferrocast-studio --example vod_track -- <out dir>

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ferrocast_studio::{Canvas, Collection, EncodeSettings, Paths, StreamSettings, Studio};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use serde_json::json;

/// Accepts one publisher and sends the first bytes of every audio message it gets.
fn serve(listener: TcpListener, tx: mpsc::Sender<Vec<u8>>) -> Option<()> {
    let (mut sock, _) = listener.accept().ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
    let mut hs = Handshake::new(PeerType::Server);
    let mut buf = vec![0u8; 65536];
    let rest = loop {
        let n = sock.read(&mut buf).ok()?;
        match hs.process_bytes(&buf[..n]).ok()? {
            HandshakeProcessResult::InProgress { response_bytes } => sock.write_all(&response_bytes).ok()?,
            HandshakeProcessResult::Completed { response_bytes, remaining_bytes } => {
                sock.write_all(&response_bytes).ok()?;
                break remaining_bytes;
            }
        }
    };
    let (mut s, first) = ServerSession::new(ServerSessionConfig::new()).ok()?;
    let mut queue = first;
    queue.extend(s.handle_input(&rest).ok()?);
    loop {
        // Until nothing's left to answer (accepting a request queues its replies).
        while !queue.is_empty() {
            for r in std::mem::take(&mut queue) {
                match r {
                    ServerSessionResult::OutboundResponse(p) => sock.write_all(&p.bytes).ok()?,
                    ServerSessionResult::RaisedEvent(ev) => match ev {
                        ServerSessionEvent::ConnectionRequested { request_id, .. }
                        | ServerSessionEvent::PublishStreamRequested { request_id, .. } => {
                            queue.extend(s.accept_request(request_id).ok()?)
                        }
                        ServerSessionEvent::AudioDataReceived { data, .. } => {
                            tx.send(data[..data.len().min(8)].to_vec()).ok()?;
                        }
                        _ => {}
                    },
                    ServerSessionResult::UnhandleableMessageReceived(_) => {}
                }
            }
        }
        let n = sock.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        queue.extend(s.handle_input(&buf[..n]).ok()?);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERROCAST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&out)?;
    let tone = out.join("tone.wav");
    if !tone.exists() {
        std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=600:duration=10", "-ac", "2"])
            .arg(&tone)
            .status()?;
    }
    let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
    let studio = Studio::start(
        &Paths::from_obs_app(Path::new(&dir), true),
        Canvas { width: 640, height: 360, fps: 30 },
        &out.join("plugin-config"),
    )?;
    let c = Collection::new(&studio, "Live")?;
    c.add_source("Live", "color_source_v3", "Back", &json!({ "color": 0xFF_30_30_30u32 }), true)?;
    c.add_source(
        "Live",
        "ffmpeg_source",
        "Voice",
        &json!({ "local_file": tone.display().to_string(), "looping": true }),
        false,
    )?;
    // The voice on both tracks; music would be on track 1 only.
    c.set_tracks("Voice", &[1, 2])?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || serve(listener, tx));
    let mut enc = EncodeSettings::with_encoder("obs_x264");
    enc.video_kbps = 800;
    let settings = StreamSettings { encode: enc, reconnect: None, delay_s: 0, vod_track: Some(2) };
    let stream = studio.stream(&format!("rtmp://127.0.0.1:{port}/live"), "key", &settings)?;
    let (mut main, mut vod, mut vod_header) = (0, 0, false);
    let until = Instant::now() + Duration::from_secs(8);
    while Instant::now() < until && (main < 20 || vod < 20) {
        let Ok(a) = rx.recv_timeout(Duration::from_secs(1)) else { continue };
        // 0x95: Enhanced RTMP multitrack audio; then one track + packet type, FourCC, track id.
        if a.first() == Some(&0x95) && a.get(6) == Some(&1) {
            vod += 1;
            vod_header |= a.get(1) == Some(&0x00);
        } else {
            main += 1;
        }
    }
    println!("main track: {main} packets, VOD track: {vod} packets (header seen: {vod_header})");
    drop(stream);
    assert!(main >= 20, "the main audio arrives");
    assert!(vod >= 20 && vod_header, "the VOD track arrives as track 1 with its header");
    println!("vod track: ok");
    drop(c);
    Ok(())
}
