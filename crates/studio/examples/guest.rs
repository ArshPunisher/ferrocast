//! A guest's camera in the Studio, end to end on this computer: a page (served here) takes a
//! browser's camera and mic and sends them over WebRTC; ferrocast-guest answers and receives them;
//! libobs's FFmpeg source shows them. Open the page in a browser (headless Chrome with a fake
//! camera works) and the example screenshots the guest into <out dir>.
//!
//!   cargo run -p ferrocast-studio --example guest -- <out dir> [port]
//!
//! FERROCAST_OBS_DIR names the engine (macOS: its Contents, else an installed OBS.app).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ferrocast_guest::Guest;
use ferrocast_studio::{Canvas, Collection, Paths, Studio};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>Guest</title>
<video id="me" autoplay muted playsinline width="320"></video><pre id="log"></pre>
<script>
const log = (m) => (document.getElementById('log').textContent += m + '\n');
(async () => {
  const media = await navigator.mediaDevices.getUserMedia({ video: { width: 1280, height: 720 }, audio: true });
  document.getElementById('me').srcObject = media;
  const pc = new RTCPeerConnection();
  for (const t of media.getTracks()) {
    const x = pc.addTransceiver(t, { direction: 'sendonly', streams: [media] });
    if (t.kind === 'video') {
      // Keep the size, lower the frame rate on a poor network; up to 4 Mbps.
      const p = x.sender.getParameters();
      p.degradationPreference = 'maintain-resolution';
      p.encodings = [{ ...(p.encodings?.[0] ?? {}), maxBitrate: 4_000_000 }];
      await x.sender.setParameters(p).catch(() => {});
    }
  }
  await pc.setLocalDescription(await pc.createOffer());
  // Every way to reach this browser goes in the offer.
  await new Promise((r) => {
    if (pc.iceGatheringState === 'complete') return r();
    pc.addEventListener('icegatheringstatechange', () => pc.iceGatheringState === 'complete' && r());
  });
  const res = await fetch('/offer', { method: 'POST', body: JSON.stringify(pc.localDescription) });
  await pc.setRemoteDescription(await res.json());
  pc.addEventListener('connectionstatechange', () => log('connection: ' + pc.connectionState));
  log('answered');
})().catch((e) => log('error: ' + e));
</script>"#;

/// Serves the page and takes one offer: its JSON goes to `offers`, the answer comes back on `answers`.
fn serve(listener: TcpListener, offers: mpsc::Sender<String>, answers: mpsc::Receiver<String>) {
    for stream in listener.incoming().flatten() {
        let mut r = BufReader::new(&stream);
        let mut first = String::new();
        if r.read_line(&mut first).is_err() {
            continue;
        }
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                break;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let (kind, body) = if first.starts_with("POST /offer") {
            let mut body = vec![0; len.min(1 << 20)];
            if r.read_exact(&mut body).is_err() || offers.send(String::from_utf8_lossy(&body).into()).is_err() {
                continue;
            }
            ("application/json", answers.recv_timeout(Duration::from_secs(10)).unwrap_or_default())
        } else {
            ("text/html", PAGE.to_string())
        };
        let mut w = &stream;
        let _ = write!(
            w,
            "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8899);
    std::fs::create_dir_all(&out)?;
    let paths = if cfg!(windows) {
        let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| r"C:\Program Files\obs-studio".into());
        Paths::from_obs_windows(Path::new(&dir))
    } else {
        let dir = std::env::var("FERROCAST_OBS_DIR").unwrap_or_else(|_| "/Applications/OBS.app/Contents".into());
        Paths::from_obs_app(Path::new(&dir), true)
    };
    let studio = Studio::start(&paths, Canvas { width: 1280, height: 720, fps: 30 }, &out.join("plugin-config"))?;
    let c = Collection::new(&studio, "Main")?;

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!("open http://127.0.0.1:{port}/ in a browser");
    let (offer_tx, offer_rx) = mpsc::channel();
    let (answer_tx, answer_rx) = mpsc::channel();
    std::thread::spawn(move || serve(listener, offer_tx, answer_rx));
    let offer = offer_rx.recv_timeout(Duration::from_secs(60))?;
    let (answer, guest) = Guest::accept(&offer, &out.join("guest.sdp"), None)?;
    answer_tx.send(answer)?;
    let item = c.add_source("Main", "ffmpeg_source", "Guest", &ferrocast_guest::source_settings(guest.sdp()), false)?;
    // GUEST_HIDDEN: added hidden, as the app adds a guest.
    if std::env::var_os("GUEST_HIDDEN").is_some() {
        c.edit_item("Main", item.id, ferrocast_studio::ItemEdit::Hide)?;
    }

    let start = Instant::now();
    let mut seen = false;
    while start.elapsed() < Duration::from_secs(25) {
        std::thread::sleep(Duration::from_secs(1));
        let s = guest.status();
        let shot = out.join("guest.png");
        // No picture yet: nothing to screenshot.
        let Ok((w, h, lit)) = c.screenshot("Guest", &shot).map_err(|e| e.into()).and_then(|()| brightness(&shot))
        else {
            println!("{:>2}s: {s:?}, no picture yet", start.elapsed().as_secs());
            continue;
        };
        println!("{:>2}s: {s:?}, picture {w}×{h}, lit {lit:.0}%", start.elapsed().as_secs());
        if s.video && w >= 1280 && lit > 20.0 {
            seen = true;
            break;
        }
    }
    assert!(seen, "the guest's picture arrived");
    // GUEST_HOLD: stay this many seconds more (to watch the sound keep time in the log).
    let hold = std::env::var("GUEST_HOLD").ok().and_then(|h| h.parse().ok()).unwrap_or(0);
    std::thread::sleep(Duration::from_secs(hold));
    println!("guest camera: ok");
    drop(c);
    Ok(())
}

/// A screenshot's size and how much of it isn't dark, in percent.
fn brightness(path: &Path) -> Result<(u32, u32, f64)> {
    let mut d = png::Decoder::new(BufReader::new(std::fs::File::open(path)?));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut r = d.read_info()?;
    let mut px = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut px)?;
    let lit =
        px[..info.buffer_size()].chunks(4).filter(|p| u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2]) > 90).count();
    let all = (info.width * info.height).max(1) as f64;
    Ok((info.width, info.height, lit as f64 * 100.0 / all))
}
