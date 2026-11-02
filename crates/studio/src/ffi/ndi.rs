//! NDI (video over the local network), through NDI's own library loaded when it's first wanted:
//! the NDI SDK or runtime the streamer installed (`brew install --cask libndi` on a Mac, NDI
//! Tools or the runtime elsewhere). Nothing of NDI ships with Ferrocast; without it, NDI says
//! so. Sources found on the network, a source read into a fed picture (with its sound), and
//! what's on air sent out under a name.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::OnceLock;

/// NDI's four-character codes.
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}
const RGBA: u32 = fourcc(b'R', b'G', b'B', b'A');
const RGBX: u32 = fourcc(b'R', b'G', b'B', b'X');
const FLTP: u32 = fourcc(b'F', b'L', b'T', b'p');
/// Receive in RGBX or RGBA (NDIlib_recv_color_format_RGBX_RGBA), at full quality.
const COLOR_RGBX_RGBA: i32 = 2;
const BANDWIDTH_HIGHEST: i32 = 100;
/// What `recv_capture_v3` got.
const FRAME_VIDEO: i32 = 1;
const FRAME_AUDIO: i32 = 2;
const FRAME_ERROR: i32 = 4;
/// Progressive frames.
const FORMAT_PROGRESSIVE: i32 = 1;
/// "Let NDI stamp it".
const TIMECODE_SYNTHESIZE: i64 = i64::MAX;

#[repr(C)]
#[derive(Clone, Copy)]
struct Source {
    name: *const c_char,
    url: *const c_char,
}

#[repr(C)]
struct FindCreate {
    show_local_sources: bool,
    groups: *const c_char,
    extra_ips: *const c_char,
}

#[repr(C)]
struct RecvCreate {
    source: Source,
    color_format: i32,
    bandwidth: i32,
    allow_video_fields: bool,
    name: *const c_char,
}

#[repr(C)]
struct SendCreate {
    name: *const c_char,
    groups: *const c_char,
    clock_video: bool,
    clock_audio: bool,
}

#[repr(C)]
struct VideoFrame {
    xres: i32,
    yres: i32,
    fourcc: u32,
    frame_rate_n: i32,
    frame_rate_d: i32,
    aspect: f32,
    format: i32,
    timecode: i64,
    data: *mut u8,
    line_stride: i32,
    metadata: *const c_char,
    timestamp: i64,
}

#[repr(C)]
struct AudioFrame {
    sample_rate: i32,
    channels: i32,
    samples: i32,
    timecode: i64,
    fourcc: u32,
    data: *mut u8,
    channel_stride: i32,
    metadata: *const c_char,
    timestamp: i64,
}

impl VideoFrame {
    fn empty() -> Self {
        VideoFrame {
            xres: 0,
            yres: 0,
            fourcc: 0,
            frame_rate_n: 0,
            frame_rate_d: 0,
            aspect: 0.0,
            format: 0,
            timecode: 0,
            data: std::ptr::null_mut(),
            line_stride: 0,
            metadata: std::ptr::null(),
            timestamp: 0,
        }
    }
}

impl AudioFrame {
    fn empty() -> Self {
        AudioFrame {
            sample_rate: 0,
            channels: 0,
            samples: 0,
            timecode: 0,
            fourcc: 0,
            data: std::ptr::null_mut(),
            channel_stride: 0,
            metadata: std::ptr::null(),
            timestamp: 0,
        }
    }
}

/// NDI's calls, from its library.
struct Lib {
    _lib: libloading::Library,
    find_create: unsafe extern "C" fn(*const FindCreate) -> *mut c_void,
    find_wait: unsafe extern "C" fn(*mut c_void, u32) -> bool,
    find_sources: unsafe extern "C" fn(*mut c_void, *mut u32) -> *const Source,
    find_destroy: unsafe extern "C" fn(*mut c_void),
    recv_create: unsafe extern "C" fn(*const RecvCreate) -> *mut c_void,
    recv_capture: unsafe extern "C" fn(*mut c_void, *mut VideoFrame, *mut AudioFrame, *mut c_void, u32) -> i32,
    recv_free_video: unsafe extern "C" fn(*mut c_void, *const VideoFrame),
    recv_free_audio: unsafe extern "C" fn(*mut c_void, *const AudioFrame),
    recv_destroy: unsafe extern "C" fn(*mut c_void),
    send_create: unsafe extern "C" fn(*const SendCreate) -> *mut c_void,
    send_video: unsafe extern "C" fn(*mut c_void, *const VideoFrame),
    send_destroy: unsafe extern "C" fn(*mut c_void),
}

/// Where NDI's library may be (NDI_RUNTIME_DIR_V6/V5 first, as NDI's own tools look).
fn candidates() -> Vec<std::path::PathBuf> {
    let file = if cfg!(windows) {
        "Processing.NDI.Lib.x64.dll"
    } else if cfg!(target_os = "macos") {
        "libndi.dylib"
    } else {
        "libndi.so.6"
    };
    let mut out: Vec<std::path::PathBuf> = ["NDI_RUNTIME_DIR_V6", "NDI_RUNTIME_DIR_V5"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|d| std::path::PathBuf::from(d).join(file))
        .collect();
    if cfg!(target_os = "macos") {
        out.extend(
            [
                "/Library/NDI SDK for Apple/lib/macOS/libndi.dylib",
                "/usr/local/lib/libndi.dylib",
                "/opt/homebrew/lib/libndi.dylib",
                "/Applications/NDI Video Monitor.app/Contents/Frameworks/libndi.dylib",
            ]
            .map(std::path::PathBuf::from),
        );
    } else if cfg!(windows) {
        out.push(r"C:\Program Files\NDI\NDI 6 Runtime\v6\Processing.NDI.Lib.x64.dll".into());
        out.push(r"C:\Program Files\NDI\NDI 6 Tools\Runtime\Processing.NDI.Lib.x64.dll".into());
    } else {
        for d in ["/usr/lib", "/usr/local/lib", "/usr/lib/x86_64-linux-gnu"] {
            out.push(std::path::Path::new(d).join("libndi.so.6"));
            out.push(std::path::Path::new(d).join("libndi.so.5"));
        }
    }
    out
}

fn load() -> Result<Lib, String> {
    let path = candidates()
        .into_iter()
        .find(|p| p.is_file())
        .ok_or("NDI isn't installed: install NDI Tools (or on a Mac `brew install --cask libndi`) and try again.")?;
    // SAFETY: loading NDI's own library, whose calls are declared above as its SDK has them
    // (Processing.NDI.*.h, v5 and v6 alike); each symbol is looked up by name and its type
    // matches the header's.
    unsafe {
        let lib = libloading::Library::new(&path).map_err(|e| format!("NDI's library didn't load ({e})"))?;
        macro_rules! sym {
            ($name:literal) => {
                *lib.get($name).map_err(|e| format!("NDI's library has no {} ({e})", stringify!($name)))?
            };
        }
        let init: unsafe extern "C" fn() -> bool = sym!(b"NDIlib_initialize\0");
        if !init() {
            return Err("NDI didn't start on this computer (its CPU isn't supported)".into());
        }
        Ok(Lib {
            find_create: sym!(b"NDIlib_find_create_v2\0"),
            find_wait: sym!(b"NDIlib_find_wait_for_sources\0"),
            find_sources: sym!(b"NDIlib_find_get_current_sources\0"),
            find_destroy: sym!(b"NDIlib_find_destroy\0"),
            recv_create: sym!(b"NDIlib_recv_create_v3\0"),
            recv_capture: sym!(b"NDIlib_recv_capture_v3\0"),
            recv_free_video: sym!(b"NDIlib_recv_free_video_v2\0"),
            recv_free_audio: sym!(b"NDIlib_recv_free_audio_v3\0"),
            recv_destroy: sym!(b"NDIlib_recv_destroy\0"),
            send_create: sym!(b"NDIlib_send_create\0"),
            send_video: sym!(b"NDIlib_send_send_video_v2\0"),
            send_destroy: sym!(b"NDIlib_send_destroy\0"),
            _lib: lib,
        })
    }
}

/// Kept once it loads; a failure isn't kept, so installing NDI works without a restart.
static LIB: OnceLock<Lib> = OnceLock::new();

fn lib() -> Result<&'static Lib, String> {
    if let Some(l) = LIB.get() {
        return Ok(l);
    }
    let l = load()?;
    Ok(LIB.get_or_init(|| l))
}

/// Whether NDI is here, or why not.
pub(crate) fn available() -> Result<(), String> {
    lib().map(|_| ())
}

/// The NDI sources on the network now (looked for up to `wait_ms`), by name.
pub(crate) fn sources(wait_ms: u32) -> Result<Vec<String>, String> {
    let l = lib()?;
    let opts = FindCreate { show_local_sources: true, groups: std::ptr::null(), extra_ips: std::ptr::null() };
    // SAFETY: a finder made and destroyed here; the list it gives is read before it's
    // destroyed, and each name is a C string NDI owns until then.
    unsafe {
        let f = (l.find_create)(&opts);
        if f.is_null() {
            return Err("NDI couldn't look for sources".into());
        }
        // Sources answer over a moment; wait in steps until the list stops growing.
        let mut n = 0u32;
        let until = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(wait_ms));
        while std::time::Instant::now() < until {
            let _ = (l.find_wait)(f, 300);
            let mut now = 0u32;
            let _ = (l.find_sources)(f, &mut now);
            if now > 0 && now == n {
                break;
            }
            n = now;
        }
        let mut count = 0u32;
        let list = (l.find_sources)(f, &mut count);
        let names = if list.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(list, count as usize)
                .iter()
                .filter(|s| !s.name.is_null())
                .map(|s| CStr::from_ptr(s.name).to_string_lossy().into_owned())
                .collect()
        };
        (l.find_destroy)(f);
        Ok(names)
    }
}

/// A connection to one NDI source; dropping it lets go.
pub(crate) struct Receiver {
    recv: *mut c_void,
}

// SAFETY: an NDI receiver may be used from any one thread at a time (NDI's docs); this one is
// moved to its reading thread and used only there.
unsafe impl Send for Receiver {}

/// What a receiver got.
pub(crate) enum Got {
    /// RGBA, straight alpha.
    Picture(ferrocore::Picture),
    /// Interleaved stereo at this rate.
    Sound(Vec<f32>, u32),
    Nothing,
    Lost,
}

impl Receiver {
    /// Connects to the source called `name` (as NDI lists it: "MACHINE (Source)").
    pub(crate) fn connect(name: &str) -> Result<Receiver, String> {
        let l = lib()?;
        let cname = CString::new(name).map_err(|_| "an NDI name can't hold a zero byte".to_string())?;
        let me = CString::new("Ferrocast").unwrap_or_default();
        let opts = RecvCreate {
            source: Source { name: cname.as_ptr(), url: std::ptr::null() },
            color_format: COLOR_RGBX_RGBA,
            bandwidth: BANDWIDTH_HIGHEST,
            allow_video_fields: false,
            name: me.as_ptr(),
        };
        // SAFETY: the options and the strings they point to live through the call (NDI copies
        // them).
        let recv = unsafe { (l.recv_create)(&opts) };
        if recv.is_null() {
            return Err(format!("NDI couldn't connect to \"{name}\""));
        }
        Ok(Receiver { recv })
    }

    /// The next picture or sound, waiting up to `ms`.
    pub(crate) fn next(&mut self, ms: u32) -> Got {
        let Ok(l) = lib() else { return Got::Lost };
        let (mut v, mut a) = (VideoFrame::empty(), AudioFrame::empty());
        // SAFETY: the receiver is live; NDI fills one of the frames, read here and given back
        // with its own free call before returning.
        unsafe {
            match (l.recv_capture)(self.recv, &mut v, &mut a, std::ptr::null_mut(), ms) {
                FRAME_VIDEO => {
                    let got = picture_of(&v);
                    (l.recv_free_video)(self.recv, &v);
                    got.map_or(Got::Nothing, Got::Picture)
                }
                FRAME_AUDIO => {
                    let got = sound_of(&a);
                    (l.recv_free_audio)(self.recv, &a);
                    got.map_or(Got::Nothing, |(pcm, rate)| Got::Sound(pcm, rate))
                }
                FRAME_ERROR => Got::Lost,
                _ => Got::Nothing,
            }
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if let Ok(l) = lib() {
            // SAFETY: the receiver this owns, destroyed once.
            unsafe { (l.recv_destroy)(self.recv) };
        }
    }
}

/// An RGBA or RGBX frame → a picture (rows packed).
///
/// # Safety
/// `v` is a frame NDI just gave, not yet freed.
unsafe fn picture_of(v: &VideoFrame) -> Option<ferrocore::Picture> {
    if v.data.is_null() || v.xres <= 0 || v.yres <= 0 || !(v.fourcc == RGBA || v.fourcc == RGBX) {
        return None;
    }
    let (w, h) = (v.xres as usize, v.yres as usize);
    let stride = if v.line_stride > 0 { v.line_stride as usize } else { w * 4 };
    if stride < w * 4 {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        // SAFETY: NDI's frame holds `yres` rows of `line_stride` bytes.
        let row = unsafe { std::slice::from_raw_parts(v.data.add(y * stride), w * 4) };
        rgba.extend_from_slice(row);
    }
    if v.fourcc == RGBX {
        rgba.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
    }
    Some(ferrocore::Picture { width: w as u32, height: h as u32, rgba, premultiplied: false })
}

/// Planar float sound → interleaved stereo (mono doubled, more channels: the first two).
///
/// # Safety
/// `a` is a frame NDI just gave, not yet freed.
unsafe fn sound_of(a: &AudioFrame) -> Option<(Vec<f32>, u32)> {
    if a.data.is_null() || a.fourcc != FLTP || a.samples <= 0 || a.channels <= 0 || a.sample_rate <= 0 {
        return None;
    }
    let (n, ch) = (a.samples as usize, a.channels as usize);
    let stride = if a.channel_stride > 0 { a.channel_stride as usize } else { n * 4 };
    let plane = |c: usize| {
        // SAFETY: NDI's frame holds `channels` planes of `channel_stride` bytes, `samples` floats each.
        unsafe { std::slice::from_raw_parts(a.data.add(c.min(ch - 1) * stride).cast::<f32>(), n) }
    };
    let (l, r) = (plane(0), plane(1));
    Some((l.iter().zip(r).flat_map(|(a, b)| [*a, *b]).collect(), a.sample_rate as u32))
}

/// What's on air, sent out on the network as an NDI source; dropping it stops.
pub(crate) struct Sender {
    send: *mut c_void,
    fps: u32,
}

// SAFETY: an NDI sender may be used from any one thread at a time; this one lives on its
// sending thread.
unsafe impl Send for Sender {}

impl Sender {
    /// A new NDI source called `name` (shown as "THIS-COMPUTER (name)").
    pub(crate) fn new(name: &str, fps: u32) -> Result<Sender, String> {
        let l = lib()?;
        let cname = CString::new(name).map_err(|_| "an NDI name can't hold a zero byte".to_string())?;
        let opts = SendCreate { name: cname.as_ptr(), groups: std::ptr::null(), clock_video: true, clock_audio: false };
        // SAFETY: the options and the name live through the call (NDI copies them).
        let send = unsafe { (l.send_create)(&opts) };
        if send.is_null() {
            return Err(format!("NDI couldn't make a source called \"{name}\""));
        }
        Ok(Sender { send, fps: fps.clamp(1, 120) })
    }

    /// Sends one RGBA picture (straight alpha) of `w`×`h`; NDI paces it to the frame rate.
    pub(crate) fn send(&mut self, rgba: &[u8], w: u32, h: u32) {
        if rgba.len() != (w * h * 4) as usize {
            return;
        }
        let Ok(l) = lib() else { return };
        let frame = VideoFrame {
            xres: w as i32,
            yres: h as i32,
            fourcc: RGBA,
            frame_rate_n: self.fps as i32,
            frame_rate_d: 1,
            aspect: w as f32 / h.max(1) as f32,
            format: FORMAT_PROGRESSIVE,
            timecode: TIMECODE_SYNTHESIZE,
            data: rgba.as_ptr().cast_mut(),
            line_stride: (w * 4) as i32,
            metadata: std::ptr::null(),
            timestamp: 0,
        };
        // SAFETY: the sender is live; the synchronous send reads `rgba` (which outlives the
        // call) and returns once done with it.
        unsafe { (l.send_video)(self.send, &frame) };
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        if let Ok(l) = lib() {
            // SAFETY: the sender this owns, destroyed once.
            unsafe { (l.send_destroy)(self.send) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_character_codes_are_ndis() {
        assert_eq!(RGBA, 0x4142_4752);
        assert_eq!(FLTP, 0x7054_4c46);
    }

    #[test]
    fn frames_have_the_sdks_layout() {
        // The sizes NDI's headers give on a 64-bit system.
        assert_eq!(std::mem::size_of::<VideoFrame>(), 72);
        assert_eq!(std::mem::size_of::<AudioFrame>(), 64);
        assert_eq!(std::mem::size_of::<RecvCreate>(), 40);
        assert_eq!(std::mem::size_of::<SendCreate>(), 24);
        assert_eq!(std::mem::size_of::<FindCreate>(), 24);
    }

    /// With NDI installed: what one sender sends, a receiver on this computer gets.
    #[test]
    fn a_picture_goes_round_through_ndi() {
        if available().is_err() {
            return;
        }
        let name = format!("Ferrocast test {}", std::process::id());
        let mut tx = Sender::new(&name, 30).unwrap_or_else(|e| panic!("{e}"));
        let full = sources(5000)
            .unwrap_or_default()
            .into_iter()
            .find(|s| s.ends_with(&format!("({name})")))
            .unwrap_or_else(|| panic!("the sender isn't listed"));
        let mut rx = Receiver::connect(&full).unwrap_or_else(|e| panic!("{e}"));
        let red = [255u8, 0, 0, 255].repeat(64 * 36);
        let mut got = None;
        for _ in 0..150 {
            tx.send(&red, 64, 36);
            if let Got::Picture(p) = rx.next(20) {
                got = Some(p);
                break;
            }
        }
        let p = got.unwrap_or_else(|| panic!("nothing came back"));
        assert_eq!((p.width, p.height), (64, 36));
        assert_eq!(&p.rgba[..4], &[255, 0, 0, 255]);
    }
}
