//! The hardware encoder on this computer: frames in, a stream FFmpeg reads back out.
#![cfg(target_os = "macos")]

use std::process::Command;

use ferrocast_av::flv::{FlvFile, video_header, video_tag};
use ferrocast_av::video::{Frame, PixelFormat, RateControl, VideoCodec, VideoConfig, VideoEncoder};

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
    let (flv, keys) = run(VideoCodec::Hevc, PixelFormat::Nv12);
    assert!(keys[0]);
    if let Some(p) = probe(&flv, "ferrocast-av-hevc.flv") {
        assert_eq!(p, "hevc,640,360,60");
    }
}

#[test]
fn wrong_frames_are_refused() {
    let mut enc = VideoEncoder::new(config(VideoCodec::H264, PixelFormat::Bgra)).unwrap_or_else(|e| panic!("{e}"));
    assert!(enc.encode(Frame::Bgra { data: &[0; 16], stride: 640 * 4 }, 0, false).is_err());
    assert!(VideoEncoder::new(VideoConfig { width: 641, ..config(VideoCodec::H264, PixelFormat::Bgra) }).is_err());
}

#[test]
fn aac_from_a_tone() {
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

/// Five seconds of picture and sound into a fragmented MP4, read back by ffprobe; then the same
/// file cut short mid-fragment (a crash) still plays up to its last whole fragment.
#[test]
fn mp4_recording_survives_a_crash() {
    use ferrocast_av::audio::{AudioConfig, AudioEncoder};
    use ferrocast_av::mp4::{AudioTrack, Mp4Writer, VideoTrack};
    let mut venc = VideoEncoder::new(config(VideoCodec::H264, PixelFormat::Bgra)).unwrap_or_else(|e| panic!("{e}"));
    let acfg = AudioConfig { sample_rate: 48000, channels: 2, bitrate_kbps: 128 };
    let mut aenc = AudioEncoder::new(acfg).unwrap_or_else(|e| panic!("{e}"));
    let mut vpk = Vec::new();
    let mut apk = Vec::new();
    for i in 0..150u32 {
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
    let video =
        VideoTrack { codec: VideoCodec::H264, width: 640, height: 360, config: venc.header().unwrap_or_default() };
    let audio = AudioTrack { sample_rate: 48000, channels: 2, config: aenc.header(), bitrate_kbps: 128 };
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

/// Constant bitrate keeps sending about the bitrate even when the picture doesn't change, as
/// platforms expect (an idle black screen still streams steadily).
#[test]
fn cbr_holds_the_rate_on_a_still_picture() {
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
    // 3 s at 2500 kbps is about 937 KB; Apple silicon pads to it, Intel Macs fall back to an
    // average (no padding), so only an upper bound is sure there.
    let kbps = bytes * 8 / 3 / 1000;
    assert!(kbps <= 3000, "{kbps} kbps");
    if cfg!(target_arch = "aarch64") {
        assert!(kbps >= 1500, "{kbps} kbps on a still picture");
    }
}
