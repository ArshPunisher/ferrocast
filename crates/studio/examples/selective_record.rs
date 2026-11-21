//! Selective recording, by hand: the Studio streams to a small RTMP server here (saved as an FLV)
//! and records to an MP4 at the same time, with a red box shown only in the recording, a green box
//! only on the stream and a yellow one in both, over a blue backdrop. `--plain` shows every box
//! in both. Then look at a frame of each file (the example prints what the two pictures hold).
//!
//!   cargo run -p ferrocast-studio --example selective_record -- <out dir> [--plain] [seconds]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrocast_studio::stage::Stage;
use ferrocast_studio::{Canvas, EncodeSettings, ItemEdit, RecordPlan, StreamSettings};
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
use rml_rtmp::sessions::{ServerSession, ServerSessionConfig, ServerSessionEvent, ServerSessionResult};
use serde_json::json;

/// Accepts one publisher and writes what it sends as an FLV at `file`.
fn serve(listener: TcpListener, file: PathBuf) -> Option<()> {
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
    let mut flv = ferrocore::flv::FlvFile::new(true, true);
    let mut queue = first;
    queue.extend(s.handle_input(&rest).ok()?);
    let save = |flv: &ferrocore::flv::FlvFile| std::fs::write(&file, &flv.bytes);
    loop {
        while !queue.is_empty() {
            for r in std::mem::take(&mut queue) {
                match r {
                    ServerSessionResult::OutboundResponse(p) => sock.write_all(&p.bytes).ok()?,
                    ServerSessionResult::RaisedEvent(ev) => match ev {
                        ServerSessionEvent::ConnectionRequested { request_id, .. }
                        | ServerSessionEvent::PublishStreamRequested { request_id, .. } => {
                            queue.extend(s.accept_request(request_id).ok()?)
                        }
                        ServerSessionEvent::VideoDataReceived { data, timestamp, .. } => {
                            flv.tag(9, timestamp.value, &data)
                        }
                        ServerSessionEvent::AudioDataReceived { data, timestamp, .. } => {
                            flv.tag(8, timestamp.value, &data)
                        }
                        _ => {}
                    },
                    ServerSessionResult::UnhandleableMessageReceived(_) => {}
                }
            }
        }
        let n = sock.read(&mut buf).unwrap_or(0);
        if n == 0 {
            save(&flv).ok()?;
            return Some(());
        }
        queue.extend(s.handle_input(&buf[..n]).ok()?);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("FERROCAST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().ok_or("an output folder")?);
    let mut plain = false;
    let mut secs = 5u64;
    for a in args {
        match a.parse::<u64>() {
            Ok(n) => secs = n,
            Err(_) if a == "--plain" => plain = true,
            Err(_) => return Err(format!("what is {a}?").into()),
        }
    }
    std::fs::create_dir_all(&out)?;
    let c = Stage::new(Canvas { width: 640, height: 360, fps: 30 }, "Live")?;
    c.add_source("Live", "color_source_v3", "Back", &json!({ "color": 0xFF_FF_00_00u32 }), true)?;
    let boxed = |name: &str, abgr: u32, x: f32| -> Result<i64, Box<dyn std::error::Error>> {
        let it = c.add_source(
            "Live",
            "color_source_v3",
            name,
            &json!({ "width": 160, "height": 90, "color": abgr }),
            false,
        )?;
        c.set_item_box("Live", it.id, x, 40.0, 160.0, 90.0)?;
        Ok(it.id)
    };
    let red = boxed("Red", 0xFF_00_00_FFu32, 40.0)?;
    let green = boxed("Green", 0xFF_00_FF_00u32, 440.0)?;
    let yellow = boxed("Yellow", 0xFF_00_FF_FFu32, 240.0)?;
    let _ = yellow;
    if !plain {
        c.edit_item("Live", red, ItemEdit::RecordingOnly)?;
        c.edit_item("Live", green, ItemEdit::StreamOnly)?;
    }
    println!("flagged items in the collection: {}", c.has_selective_items());

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let flv = out.join("stream.flv");
    let server = std::thread::spawn({
        let flv = flv.clone();
        move || serve(listener, flv)
    });
    let mut enc = EncodeSettings::with_encoder("com.apple.videotoolbox.videoencoder.ave.avc");
    enc.video_kbps = 1500;
    let settings = StreamSettings {
        encode: enc.clone(),
        reconnect: None,
        delay_s: 0,
        vod_track: None,
        own_encoder: true,
        video_tracks: Vec::new(),
    };
    let plan = RecordPlan {
        folder: out.clone(),
        name: "recording".into(),
        overwrite: true,
        encode: enc,
        ..Default::default()
    };
    let stream = c.stream(&format!("rtmp://127.0.0.1:{port}/live"), "key", &settings)?;
    let rec_path: PathBuf = out.join("recording.mp4");
    let recording = c.record_at(&rec_path, &plan, None)?;
    std::thread::sleep(Duration::from_secs(secs));
    drop(recording);
    drop(stream);
    let _ = server.join();
    drop(c);
    report(&rec_path);
    report(&flv);
    Ok(())
}

fn report(file: &Path) {
    println!("{}: {} bytes", file.display(), std::fs::metadata(file).map(|m| m.len()).unwrap_or(0));
}
