//! The encoders on this computer (the Mac's own, or FFmpeg's elsewhere and with
//! FERROCAST_AV_ENCODER=ffmpeg): frames in, a stream FFmpeg reads back out.

use std::process::Command;

use ferrocast_av::flv::{FlvFile, video_header, video_tag};
use ferrocast_av::video::{Frame, PixelFormat, RateControl, VideoCodec, VideoConfig, VideoEncoder};

/// Whether this computer encodes: the Mac always; elsewhere only with FFmpeg and a working H.264
/// encoder (the shared CI machines have none, the engine's Windows job brings FFmpeg).
fn encodes() -> bool {
    let native = cfg!(target_os = "macos") && std::env::var("FERROCAST_AV_ENCODER").as_deref() != Ok("ffmpeg");
    let ok = native || ferrocast_av::ffenc::h264_encoder().is_some();
    if !ok {
        eprintln!("skipped: no encoder here (FFmpeg isn't installed)");
    }
    ok
}

fn config(codec: VideoCodec, input: PixelFormat) -> VideoConfig {
    VideoConfig {
        codec,
        input,
        width: 640,
        height: 360,
        fps_num: 30,
        fps_den: 1,
        bitrate_kbps: 2500,
        rate: RateControl::Cbr,
        keyframe_secs: 1,
        software: false,
    }
}

/// A moving gradient, BGRA.
fn bgra(i: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(640 * 360 * 4);
    for y in 0..360u32 {
        for x in 0..640u32 {
            v.extend_from_slice(&[(x + i * 4) as u8, (y + i) as u8, 128, 255]);
        }
    }
    v
}

fn run(codec: VideoCodec, input: PixelFormat) -> (Vec<u8>, Vec<bool>) {
    let mut enc = VideoEncoder::new(config(codec, input)).unwrap_or_else(|e| panic!("{e}"));
    let mut packets = Vec::new();
    let (y, uv) = (vec![100u8; 640 * 360], vec![128u8; 640 * 180]);
    for i in 0..60u32 {
        let px = bgra(i);
        let frame = match input {
            PixelFormat::Bgra => Frame::Bgra { data: &px, stride: 640 * 4 },
            PixelFormat::Nv12 => Frame::Nv12 { y: &y, y_stride: 640, uv: &uv, uv_stride: 640 },
        };
        packets.extend(enc.encode(frame, i64::from(i) * 33_333, false).unwrap_or_else(|e| panic!("{e}")));
    }
    packets.extend(enc.flush().unwrap_or_else(|e| panic!("{e}")));
    let header = enc.header().unwrap_or_else(|| panic!("a codec header"));
    let mut flv = FlvFile::new(false, true);
    flv.tag(9, 0, &video_header(codec, &header));
    for p in &packets {
        let (ms, body) = video_tag(codec, p, 0);
        flv.tag(9, ms, &body);
    }
    (flv.bytes, packets.iter().map(|p| p.key).collect())
}

/// What ffprobe sees in an FLV: codec, size, and how many frames decode.
fn probe(flv: &[u8], name: &str) -> Option<String> {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, flv).ok()?;
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=codec_name,width,height,nb_read_frames")
        .args(["-of", "csv=p=0"])
        .arg(&path)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn h264_from_bgra() {
    if !encodes() {
        return;
    }
    let (flv, keys) = run(VideoCodec::H264, PixelFormat::Bgra);
    assert_eq!(keys.len(), 60, "one packet a frame");
    assert!(keys[0], "starts on a keyframe");
    assert!(keys.iter().filter(|k| **k).count() >= 2, "a keyframe every second");
    if let Some(p) = probe(&flv, "ferrocast-av-h264.flv") {
        assert_eq!(p, "h264,640,360,60");
    }
}

#[test]
fn hevc_from_nv12() {
    if !encodes() {
        return;
    }
    if !cfg!(target_os = "macos") || std::env::var("FERROCAST_AV_ENCODER").is_ok_and(|v| v == "ffmpeg") {
        return; // HEVC is the Mac's own encoder only so far.
    }
    let (flv, keys) = run(VideoCodec::Hevc, PixelFormat::Nv12);
    assert!(keys[0]);
    if let Some(p) = probe(&flv, "ferrocast-av-hevc.flv") {
        assert_eq!(p, "hevc,640,360,60");
    }
}

#[test]
fn wrong_frames_are_refused() {
    if !encodes() {
        return;
    }
    let mut enc = VideoEncoder::new(config(VideoCodec::H264, PixelFormat::Bgra)).unwrap_or_else(|e| panic!("{e}"));
    assert!(enc.encode(Frame::Bgra { data: &[0; 16], stride: 640 * 4 }, 0, false).is_err());
    assert!(VideoEncoder::new(VideoConfig { width: 641, ..config(VideoCodec::H264, PixelFormat::Bgra) }).is_err());
}

#[test]
fn aac_from_a_tone() {
    if !encodes() {
        return;
    }
    use ferrocast_av::audio::{AudioConfig, AudioEncoder};
    use ferrocast_av::flv::{audio_header, audio_tag};
    let mut enc = AudioEncoder::new(AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 160 })
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(enc.header(), vec![0x11, 0x90]);
    // Two seconds of 440 Hz, fed in uneven pieces as a sound card would.
    let mut pcm = Vec::new();
    for i in 0..96_000 {
        let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.5;
        pcm.extend_from_slice(&[v, v]);
    }
    let mut packets = Vec::new();
    for chunk in pcm.chunks(960) {
        packets.extend(enc.encode(chunk).unwrap_or_else(|e| panic!("{e}")));
    }
    packets.extend(enc.flush().unwrap_or_else(|e| panic!("{e}")));
    assert!(packets.len() >= 90, "about 93 frames, got {}", packets.len());
    assert!(packets.windows(2).all(|w| w[1].pts_us > w[0].pts_us));
    let mut flv = FlvFile::new(true, false);
    flv.tag(8, 0, &audio_header(&enc.header()));
    for p in &packets {
        flv.tag(8, (p.pts_us / 1000) as u32, &audio_tag(&p.data));
    }
    let path = std::env::temp_dir().join("ferrocast-av-aac.flv");
    std::fs::write(&path, &flv.bytes).unwrap_or_else(|e| panic!("{e}"));
    if let Ok(out) = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,profile,sample_rate,channels", "-of", "csv=p=0"])
        .arg(&path)
        .output()
    {
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "aac,LC,48000,2");
    }
}

/// `frames` of picture (30 fps) and their sound, encoded, with their tracks' shapes.
fn encoded(
    frames: u32,
) -> (
    ferrocast_av::mp4::VideoTrack,
    ferrocast_av::mp4::AudioTrack,
    Vec<ferrocast_av::video::VideoPacket>,
    Vec<ferrocast_av::audio::AudioPacket>,
) {
    use ferrocast_av::audio::{AudioConfig, AudioEncoder};
    use ferrocast_av::mp4::{AudioTrack, VideoTrack};
    let mut venc = VideoEncoder::new(config(VideoCodec::H264, PixelFormat::Bgra)).unwrap_or_else(|e| panic!("{e}"));
    let acfg = AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 };
    let mut aenc = AudioEncoder::new(acfg).unwrap_or_else(|e| panic!("{e}"));
    let mut vpk = Vec::new();
    let mut apk = Vec::new();
    for i in 0..frames {
        let px = bgra(i);
        vpk.extend(
            venc.encode(Frame::Bgra { data: &px, stride: 640 * 4 }, i64::from(i) * 33_333, false)
                .unwrap_or_else(|e| panic!("{e}")),
        );
        let pcm: Vec<f32> = (0..1600)
            .flat_map(|n| {
                let v = ((i * 1600 + n) as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.3;
                [v, v]
            })
            .collect();
        apk.extend(aenc.encode(&pcm).unwrap_or_else(|e| panic!("{e}")));
    }
    vpk.extend(venc.flush().unwrap_or_else(|e| panic!("{e}")));
    apk.extend(aenc.flush().unwrap_or_else(|e| panic!("{e}")));
    let video =
        VideoTrack { codec: VideoCodec::H264, width: 640, height: 360, config: venc.header().unwrap_or_default() };
    let audio = AudioTrack { sample_rate: 48000, channels: 2, config: aenc.header(), bitrate_kbps: 128 };
    (video, audio, vpk, apk)
}

/// Five seconds of picture and sound into a fragmented MP4, read back by ffprobe; then the same
/// file cut short mid-fragment (a crash) still plays up to its last whole fragment.
#[test]
fn mp4_recording_survives_a_crash() {
    if !encodes() {
        return;
    }
    use ferrocast_av::mp4::Mp4Writer;
    let (video, audio, vpk, apk) = encoded(150);
    let mut w = Mp4Writer::new(Vec::new(), Some(video), Some(audio)).unwrap_or_else(|e| panic!("{e}"));
    // Interleaved by time, as they'd arrive live.
    let (mut vi, mut ai) = (0, 0);
    while vi < vpk.len() || ai < apk.len() {
        if ai >= apk.len() || (vi < vpk.len() && vpk[vi].dts_us <= apk[ai].pts_us) {
            w.video(&vpk[vi]).unwrap_or_else(|e| panic!("{e}"));
            vi += 1;
        } else {
            w.audio(&apk[ai]).unwrap_or_else(|e| panic!("{e}"));
            ai += 1;
        }
    }
    let bytes = w.finish().unwrap_or_else(|e| panic!("{e}"));
    let probe = |name: &str, data: &[u8]| -> Option<String> {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, data).ok()?;
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_frames",
                "-show_entries",
                "stream=codec_name,nb_read_frames",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().replace('\n', " "))
    };
    if let Some(p) = probe("ferrocast-av.mp4", &bytes) {
        let n = apk.len();
        assert_eq!(p, format!("h264,150 aac,{n}"));
    }
    // Cut 3 KB before the end: the last fragment is broken; the rest still reads.
    if let Some(p) = probe("ferrocast-av-cut.mp4", &bytes[..bytes.len() - 3000]) {
        let frames: u32 =
            p.split(' ').next().and_then(|v| v.split(',').nth(1)).and_then(|n| n.parse().ok()).unwrap_or(0);
        assert!(frames >= 120, "plays up to the last whole fragment: {p}");
    }
}

/// Chapter marks (OBS's "Add Chapter Marker") in an MP4 and an MKV, as ffprobe reads them:
/// named where asked, "Chapter n" where not, at the time they were added.
#[test]
fn chapter_marks_are_in_the_file() {
    if !encodes() {
        return;
    }
    use ferrocast_av::mkv::MkvWriter;
    use ferrocast_av::mp4::Mp4Writer;
    use std::io::Cursor;
    let (video, audio, vpk, apk) = encoded(90);
    let chapters = |name: &str, data: &[u8]| -> Option<String> {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, data).ok()?;
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "chapter=start_time:chapter_tags=title", "-of", "csv=p=0"])
            .arg(&path)
            .output()
            .ok()?;
        let _ = std::fs::remove_file(&path);
        Some(String::from_utf8_lossy(&out.stdout).trim().replace('\n', "|"))
    };
    // A mark at the start, one named a second in, one unnamed two seconds in.
    let marks = |i: usize, mark: &mut dyn FnMut(&str)| match i {
        0 => mark("Intro"),
        30 => mark("Game"),
        60 => mark(""),
        _ => {}
    };
    let mut mp4 = Mp4Writer::new(Cursor::new(Vec::new()), Some(video.clone()), Some(audio.clone()))
        .unwrap_or_else(|e| panic!("{e}"));
    let mut mkv = MkvWriter::new(Cursor::new(Vec::new()), Some(video), Some(audio)).unwrap_or_else(|e| panic!("{e}"));
    let mut ai = 0;
    for (i, v) in vpk.iter().enumerate() {
        while ai < apk.len() && apk[ai].pts_us <= v.dts_us {
            mp4.audio(&apk[ai]).unwrap_or_else(|e| panic!("{e}"));
            mkv.audio(&apk[ai]).unwrap_or_else(|e| panic!("{e}"));
            ai += 1;
        }
        mp4.video(v).unwrap_or_else(|e| panic!("{e}"));
        mkv.video(v).unwrap_or_else(|e| panic!("{e}"));
        marks(i, &mut |n| assert!(mp4.chapter(n)));
        marks(i, &mut |n| assert!(mkv.chapter(n)));
    }
    let mp4 = mp4.finish_with_chapters().unwrap_or_else(|e| panic!("{e}")).into_inner();
    let mkv = mkv.finish_with_chapters().unwrap_or_else(|e| panic!("{e}")).into_inner();
    for (name, data) in [("ferrocast-chapters.mp4", &mp4), ("ferrocast-chapters.mkv", &mkv)] {
        let Some(c) = chapters(name, data) else { continue };
        let got: Vec<(f64, String)> = c
            .split('|')
            .filter_map(|l| {
                let (t, n) = l.split_once(',')?;
                Some((t.parse().ok()?, n.to_string()))
            })
            .collect();
        let names: Vec<&str> = got.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, ["Intro", "Game", "Chapter 3"], "{name}: {c}");
        for ((t, _), want) in got.iter().zip([0.0, 1.0, 2.0]) {
            assert!((t - want).abs() < 0.1, "{name}: a mark at {t}, not {want}");
        }
    }
}

/// Constant bitrate keeps sending about the bitrate even when the picture doesn't change, as
/// platforms expect (an idle black screen still streams steadily).
#[test]
fn cbr_holds_the_rate_on_a_still_picture() {
    if !encodes() {
        return;
    }
    let mut enc = VideoEncoder::new(config(VideoCodec::H264, PixelFormat::Bgra)).unwrap_or_else(|e| panic!("{e}"));
    let black = vec![0u8; 640 * 360 * 4];
    let mut bytes = 0usize;
    for i in 0..90u32 {
        for p in enc
            .encode(Frame::Bgra { data: &black, stride: 640 * 4 }, i64::from(i) * 33_333, false)
            .unwrap_or_else(|e| panic!("{e}"))
        {
            bytes += p.data.len();
        }
    }
    for p in enc.flush().unwrap_or_else(|e| panic!("{e}")) {
        bytes += p.data.len();
    }
    // 3 s at 2500 kbps is about 937 KB; Apple silicon pads to it, Intel Macs fall back to an
    // average (no padding), so only an upper bound is sure there.
    let kbps = bytes * 8 / 3 / 1000;
    assert!(kbps <= 3000, "{kbps} kbps");
    // Apple silicon pads; so do x264 and the GPUs' encoders through FFmpeg.
    if cfg!(target_arch = "aarch64") || !cfg!(target_os = "macos") {
        assert!(kbps >= 1500, "{kbps} kbps on a still picture");
    }
}

#[test]
fn a_new_bitrate_takes_while_running() {
    if !encodes() {
        return;
    }
    let mut enc = VideoEncoder::new(config(VideoCodec::H264, PixelFormat::Bgra)).unwrap_or_else(|e| panic!("{e}"));
    let mut sizes = [0usize; 2];
    for i in 0..180u32 {
        if i == 90 && !enc.set_kbps(500).unwrap_or_else(|e| panic!("{e}")) {
            eprintln!("skipped: this encoder takes a new bitrate only at the next start");
            return;
        }
        // A moving picture, so the encoder has something to spend bits on.
        let px = bgra(i);
        for p in enc
            .encode(Frame::Bgra { data: &px, stride: 640 * 4 }, i64::from(i) * 33_333, false)
            .unwrap_or_else(|e| panic!("{e}"))
        {
            sizes[usize::from(i >= 90)] += p.data.len();
        }
    }
    let kbps = sizes.map(|b| b * 8 / 3 / 1000);
    assert_eq!(enc.config().bitrate_kbps, 500);
    assert!(kbps[1] * 2 < kbps[0], "2500 kbps then 500: {kbps:?}");
}

/// Constant quality (recordings, Mac only): the bitrate follows the picture, little for a still
/// one and much more for a busy one, and every frame decodes.
#[test]
fn constant_quality_follows_the_picture() {
    if !cfg!(target_os = "macos") || std::env::var("FERROCAST_AV_ENCODER").as_deref() == Ok("ffmpeg") {
        return;
    }
    let size = |busy: bool| {
        let cfg = VideoConfig { rate: RateControl::Quality(0.69), ..config(VideoCodec::H264, PixelFormat::Bgra) };
        let mut enc = VideoEncoder::new(cfg).unwrap_or_else(|e| panic!("{e}"));
        let mut packets = Vec::new();
        let black = vec![0u8; 640 * 360 * 4];
        for i in 0..60u32 {
            // Noise for the busy picture: nothing an encoder can predict.
            let px: Vec<u8> = if busy {
                (0..640 * 360 * 4)
                    .map(|n: u32| (n.wrapping_mul(2_654_435_761).wrapping_add(i * 7919) >> 13) as u8)
                    .collect()
            } else {
                black.clone()
            };
            packets.extend(
                enc.encode(Frame::Bgra { data: &px, stride: 640 * 4 }, i64::from(i) * 33_333, false)
                    .unwrap_or_else(|e| panic!("{e}")),
            );
        }
        packets.extend(enc.flush().unwrap_or_else(|e| panic!("{e}")));
        let header = enc.header().unwrap_or_else(|| panic!("a codec header"));
        let mut flv = FlvFile::new(false, true);
        flv.tag(9, 0, &video_header(VideoCodec::H264, &header));
        for p in &packets {
            let (ms, body) = video_tag(VideoCodec::H264, p, 0);
            flv.tag(9, ms, &body);
        }
        if let Some(p) = probe(&flv.bytes, &format!("ferrocast-av-cq-{busy}.flv")) {
            assert_eq!(p, "h264,640,360,60", "every frame decodes");
        }
        packets.iter().map(|p| p.data.len()).sum::<usize>() * 8 / 2 / 1000
    };
    let (still, busy) = (size(false), size(true));
    eprintln!("constant quality: {still} kbps still, {busy} kbps busy");
    // A CBR encoder would sit near 2500 for both.
    assert!(still < 1000 && busy > still * 5, "{still} kbps still, {busy} kbps busy");
}

/// x264 through FFmpeg (the software encoder, and Lossless recordings): rate factor 0 gives back
/// exactly the pixels that went in.
#[test]
fn x264_lossless_gives_back_the_picture() {
    if !ferrocast_av::ffenc::available() {
        eprintln!("skipped: FFmpeg isn't here");
        return;
    }
    let cfg = VideoConfig { rate: RateControl::Crf(0), software: true, ..config(VideoCodec::H264, PixelFormat::Nv12) };
    let mut enc = VideoEncoder::new(cfg).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(enc.name(), "libx264");
    // A gradient in NV12 (already 4:2:0, so nothing is lost before the encoder).
    let y: Vec<u8> = (0..640 * 360).map(|n: u32| ((n % 640) * 255 / 639) as u8).collect();
    let uv: Vec<u8> = (0..640 * 180).map(|n: u32| if n.is_multiple_of(2) { 90 } else { 200 }).collect();
    let mut packets = Vec::new();
    for i in 0..10u32 {
        packets.extend(
            enc.encode(Frame::Nv12 { y: &y, y_stride: 640, uv: &uv, uv_stride: 640 }, i64::from(i) * 33_333, false)
                .unwrap_or_else(|e| panic!("{e}")),
        );
    }
    packets.extend(enc.flush().unwrap_or_else(|e| panic!("{e}")));
    assert_eq!(packets.len(), 10, "one packet a frame");
    let header = enc.header().unwrap_or_else(|| panic!("a codec header"));
    let mut flv = FlvFile::new(false, true);
    flv.tag(9, 0, &video_header(VideoCodec::H264, &header));
    for p in &packets {
        let (ms, body) = video_tag(VideoCodec::H264, p, 0);
        flv.tag(9, ms, &body);
    }
    let dir = std::env::temp_dir();
    let (file, raw) = (dir.join("ferrocast-av-lossless.flv"), dir.join("ferrocast-av-lossless.nv12"));
    std::fs::write(&file, &flv.bytes).unwrap_or_else(|e| panic!("{e}"));
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(&file)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "nv12"])
        .arg(&raw)
        .status()
        .unwrap_or_else(|e| panic!("ffmpeg to decode it: {e}"))
        .success();
    assert!(ok, "the lossless file decodes");
    let back = std::fs::read(&raw).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back.len(), y.len() + uv.len());
    assert!(back[..y.len()] == y[..] && back[y.len()..] == uv[..], "lossless gives back every pixel");
}

/// A bitrate change while encoding through FFmpeg: another encoder takes over at a keyframe,
/// and the stream plays straight through, every frame there once, in order.
#[test]
fn ffmpegs_bitrate_changes_on_the_run() {
    if !ferrocast_av::ffenc::available() {
        eprintln!("skipped: FFmpeg isn't here");
        return;
    }
    let cfg = VideoConfig { rate: RateControl::Cbr, software: true, ..config(VideoCodec::H264, PixelFormat::Nv12) };
    let mut enc = VideoEncoder::new(cfg).unwrap_or_else(|e| panic!("{e}"));
    let mut packets = Vec::new();
    for i in 0..90u32 {
        if i == 30 {
            assert!(enc.set_kbps(1200).unwrap_or_else(|e| panic!("{e}")), "taken on the run");
        }
        if i == 60 {
            assert!(enc.set_kbps(4000).unwrap_or_else(|e| panic!("{e}")));
        }
        // A moving bar, so frames differ.
        let y: Vec<u8> = (0..640 * 360).map(|n: u32| if (n % 640 + i * 7) % 640 < 60 { 235 } else { 40 }).collect();
        let uv = vec![128u8; 640 * 180];
        packets.extend(
            enc.encode(Frame::Nv12 { y: &y, y_stride: 640, uv: &uv, uv_stride: 640 }, i64::from(i) * 33_333, false)
                .unwrap_or_else(|e| panic!("{e}")),
        );
    }
    packets.extend(enc.flush().unwrap_or_else(|e| panic!("{e}")));
    assert_eq!(packets.len(), 90, "every frame once");
    assert!(packets.windows(2).all(|w| w[0].pts_us < w[1].pts_us), "in order");
    assert!(packets[30].key && packets[60].key, "each new encoder starts on a keyframe");
    let header = enc.header().unwrap_or_else(|| panic!("a codec header"));
    let mut flv = FlvFile::new(false, true);
    flv.tag(9, 0, &video_header(VideoCodec::H264, &header));
    for p in &packets {
        let (ms, body) = video_tag(VideoCodec::H264, p, 0);
        flv.tag(9, ms, &body);
    }
    let file = std::env::temp_dir().join("ferrocast-av-rate-change.flv");
    std::fs::write(&file, &flv.bytes).unwrap_or_else(|e| panic!("{e}"));
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(&file)
        .output()
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "90",
        "it decodes whole: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty(), "no decoding errors: {}", String::from_utf8_lossy(&out.stderr));
}
