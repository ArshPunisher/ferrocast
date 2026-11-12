//! Ferrocast Camera, checked end to end on Windows (CI's Windows runner runs it):
//!
//! 1. Frames are written as Ferrocast writes them: the left half bright, the right half dark.
//! 2. DirectShow: the camera registered for this user is listed by name, with its formats.
//! 3. FFmpeg (`--ffmpeg <exe>`) opens it as any app does, and gets that picture; with the
//!    writer stopped, the placeholder.
//! 4. Media Foundation: the media source made in this process gives that picture.
//! 5. Windows 11's camera for every app (admin, where the frame server is): set up with
//!    regsvr32, made with MFCreateVirtualCamera, found among the cameras and read. Skipped,
//!    saying why, where Windows hasn't got it.
//!
//! `cargo run -p ferrocast-wincam --example camera_check -- --dll target\debug\ferrocast_camera.dll [--ffmpeg ffmpeg.exe]`

#[cfg(not(windows))]
fn main() {
    println!("camera_check runs on Windows only");
}

#[cfg(windows)]
fn main() {
    if let Err(e) = win::run() {
        eprintln!("FAIL: {e}");
        std::process::exit(1);
    }
    println!("all camera checks passed");
}

#[cfg(windows)]
#[allow(unsafe_code, clippy::undocumented_unsafe_blocks)]
mod win {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use ferrocast_camshare::regdata;
    use ferrocast_camshare::win::{Root, View, delete_keys, write_entries};
    use ferrocast_camshare::{CLSID_DSHOW, CLSID_MF, FRIENDLY_NAME, NAME_GLOBAL, NAME_LOCAL, Writer, now_ms};
    use windows::Win32::Media::DirectShow::{IAMStreamConfig, IBaseFilter, ICreateDevEnum, IPin};
    use windows::Win32::Media::MediaFoundation::{
        CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory, IMFActivate, IMFAttributes, IMFMediaSource,
        IMFMediaStream, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MEMediaSample, MENewStream,
        MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_FRAME_SIZE, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
        MFCreateAttributes, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFStartup,
    };
    use windows::Win32::System::Com::StructuredStorage::{IPropertyBag, PROPVARIANT};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, IEnumMoniker,
        IMoniker,
    };
    use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_UNKNOWN};
    use windows::core::{GUID, HSTRING, IUnknown, Interface, PCWSTR, w};

    const W: u32 = 1280;
    const H: u32 = 720;
    const BRIGHT: u8 = 200;
    const DARK: u8 = 40;

    type R<T> = Result<T, String>;

    fn arg(name: &str) -> Option<String> {
        let args: Vec<String> = std::env::args().collect();
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
    }

    /// The left half's and the right half's mean brightness of a frame's Y plane.
    fn halves(y: &[u8], w: usize, h: usize) -> (u8, u8) {
        let (mut l, mut r) = (0u64, 0u64);
        for row in h / 4..h * 3 / 4 {
            for x in w / 8..w * 3 / 8 {
                l += u64::from(y[row * w + x]);
            }
            for x in w * 5 / 8..w * 7 / 8 {
                r += u64::from(y[row * w + x]);
            }
        }
        let n = (h / 2 * w / 4) as u64;
        ((l / n) as u8, (r / n) as u8)
    }

    fn near(v: u8, want: u8) -> bool {
        (i32::from(v) - i32::from(want)).abs() <= 12
    }

    /// Writes the test picture into the session's region (and the frame server's once it's
    /// there), as Ferrocast does, until stopped.
    struct Sending {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Sending {
        fn start() -> R<Sending> {
            let (region, _) = ferrocast_camshare::win::create(NAME_LOCAL, None)?;
            let local = Arc::new(Mutex::new(Writer::new(region)));
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let thread = std::thread::spawn(move || {
                let mut y = vec![0u8; (W * H) as usize];
                for row in y.chunks_mut(W as usize) {
                    row[..(W / 2) as usize].fill(BRIGHT);
                    row[(W / 2) as usize..].fill(DARK);
                }
                let uv = vec![128u8; (W * H / 2) as usize];
                let mut global: Option<Writer> = None;
                let mut n = 0u64;
                while !s.load(Ordering::Acquire) {
                    let now = now_ms();
                    let _ = local.lock().map(|mut w| w.write((W, H), (30, 1), n, &y, &uv, now));
                    if global.is_none() && n.is_multiple_of(30) {
                        global = ferrocast_camshare::win::open(NAME_GLOBAL, true).map(Writer::new);
                    }
                    if let Some(g) = &mut global {
                        let _ = g.write((W, H), (30, 1), n, &y, &uv, now);
                    }
                    n += 1;
                    std::thread::sleep(Duration::from_millis(33));
                }
            });
            Ok(Sending { stop, thread: Some(thread) })
        }
    }

    impl Drop for Sending {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    pub fn run() -> R<()> {
        let dll = PathBuf::from(arg("--dll").ok_or("--dll <path to ferrocast_camera.dll>")?);
        let dll = std::fs::canonicalize(&dll).map_err(|e| format!("{}: {e}", dll.display()))?;
        let dll = PathBuf::from(dll.to_string_lossy().trim_start_matches(r"\\?\"));
        // SAFETY: COM for this thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().map_err(|e| e.to_string())?;
        let sending = Sending::start()?;
        std::thread::sleep(Duration::from_millis(300));

        // 2. Registered for this user, listed by DirectShow.
        let formats = ferrocast_camshare::caps::FORMATS.map(|f| f.fourcc());
        write_entries(Root::User, View::Bits64, &regdata::dshow_entries(&dll.to_string_lossy(), &formats))?;
        let result = (|| -> R<()> {
            dshow_listed()?;
            println!("ok: DirectShow lists \"{FRIENDLY_NAME}\" with its formats");
            if let Some(ff) = arg("--ffmpeg") {
                ffmpeg_reads(&ff, true)?;
                println!("ok: FFmpeg reads the picture through DirectShow");
            } else {
                println!("skip: FFmpeg (no --ffmpeg)");
            }
            // 4. The media source in this process.
            write_entries(Root::User, View::Bits64, &regdata::mf_entries(&dll.to_string_lossy()))?;
            mf_source_reads()?;
            println!("ok: the Media Foundation source gives the picture");
            // 5. Windows 11's camera for every app.
            match every_app(&dll) {
                Ok(()) => println!("ok: Windows 11's camera is listed and reads the picture"),
                Err(e) if e.starts_with("skip") => println!("{e}"),
                Err(e) => return Err(e),
            }
            Ok(())
        })();
        // 3b. Ferrocast stops: the placeholder (dark, not the picture).
        drop(sending);
        let after = match (&result, arg("--ffmpeg")) {
            (Ok(()), Some(ff)) => {
                ffmpeg_reads(&ff, false).map(|()| println!("ok: the placeholder once Ferrocast stops"))
            }
            _ => Ok(()),
        };
        for view in [View::Bits64, View::Bits32] {
            delete_keys(Root::User, view, &regdata::dshow_keys());
            delete_keys(Root::User, view, &regdata::mf_keys());
        }
        result.and(after)
    }

    /// DirectShow lists the camera by name; its pin offers types and stream caps.
    fn dshow_listed() -> R<()> {
        let e = |x: windows::core::Error| x.to_string();
        // SAFETY: plain COM calls on DirectShow's own objects.
        unsafe {
            let devs: ICreateDevEnum =
                CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER).map_err(e)?;
            let mut list: Option<IEnumMoniker> = None;
            devs.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut list, 0).map_err(e)?;
            let list = list.ok_or("no video capture sources at all")?;
            let mut names = Vec::new();
            loop {
                let mut one = [None];
                if list.Next(&mut one, None).0 != 0 {
                    break;
                }
                let Some(m): Option<IMoniker> = one[0].take() else { break };
                let bag: IPropertyBag = m.BindToStorage(None, None).map_err(e)?;
                let mut v = VARIANT::default();
                if bag.Read(w!("FriendlyName"), &mut v, None).is_err() {
                    continue;
                }
                let name = if v.Anonymous.Anonymous.vt == VT_BSTR {
                    v.Anonymous.Anonymous.Anonymous.bstrVal.to_string()
                } else {
                    String::new()
                };
                if name == FRIENDLY_NAME {
                    let filter: IBaseFilter = m.BindToObject(None, None).map_err(e)?;
                    let pin: IPin = filter.FindPin(w!("Capture")).map_err(e)?;
                    let config: IAMStreamConfig = pin.cast().map_err(e)?;
                    let (mut count, mut size) = (0, 0);
                    config.GetNumberOfCapabilities(&mut count, &mut size).map_err(e)?;
                    if count < 3 {
                        return Err(format!("only {count} formats offered"));
                    }
                    let mt = config.GetFormat().map_err(e)?;
                    if mt.is_null() || (*mt).lSampleSize == 0 {
                        return Err("no current format".into());
                    }
                    if !(*mt).pbFormat.is_null() {
                        CoTaskMemFree(Some((*mt).pbFormat.cast()));
                    }
                    CoTaskMemFree(Some(mt.cast()));
                    return Ok(());
                }
                names.push(name);
            }
            Err(format!("\"{FRIENDLY_NAME}\" isn't listed: {names:?}"))
        }
    }

    /// FFmpeg opens the camera as apps do and reads a frame: the picture, or the placeholder.
    fn ffmpeg_reads(ffmpeg: &str, picture: bool) -> R<()> {
        let out = std::env::temp_dir().join(format!("ferrocast-camera-{}.gray", std::process::id()));
        let run = std::process::Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "dshow", "-video_size"])
            .arg(format!("{W}x{H}"))
            .args(["-i", &format!("video={FRIENDLY_NAME}"), "-frames:v", "10", "-f", "rawvideo", "-pix_fmt", "gray"])
            .arg(&out)
            .output()
            .map_err(|e| format!("FFmpeg didn't run ({e})"))?;
        if !run.status.success() {
            return Err(format!("FFmpeg couldn't read the camera: {}", String::from_utf8_lossy(&run.stderr)));
        }
        let bytes = std::fs::read(&out).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&out);
        let frame = (W * H) as usize;
        if bytes.len() < frame {
            return Err(format!("FFmpeg gave {} bytes, not a frame", bytes.len()));
        }
        let last = &bytes[bytes.len() / frame * frame - frame..bytes.len() / frame * frame];
        let (l, r) = halves(last, W as usize, H as usize);
        // FFmpeg's grey is full range: Y 200 → 214, 40 → 28 (approximately).
        let full = |v: u8| ((f32::from(v) - 16.0) * 255.0 / 219.0).clamp(0.0, 255.0) as u8;
        if picture && !(near(l, full(BRIGHT)) && near(r, full(DARK))) {
            return Err(format!("the picture read was {l}/{r}, not {}/{}", full(BRIGHT), full(DARK)));
        }
        if !picture && (l > 60 || r > 60) {
            return Err(format!("not the placeholder after stopping: {l}/{r}"));
        }
        Ok(())
    }

    /// The first stream an event's value holds.
    fn unknown_of(pv: &PROPVARIANT) -> Option<IUnknown> {
        // SAFETY: reads the union only when it holds an IUnknown.
        unsafe {
            let inner = &pv.Anonymous.Anonymous;
            (inner.vt == VT_UNKNOWN).then(|| (*inner.Anonymous.punkVal).clone()).flatten()
        }
    }

    /// The media source, made by COM in this process: started, asked for a sample, which holds
    /// the picture.
    fn mf_source_reads() -> R<()> {
        let e = |x: windows::core::Error| x.to_string();
        // SAFETY: plain Media Foundation calls on our own source.
        unsafe {
            MFStartup(0x0002_0070, 0).map_err(e)?;
            let source: IMFMediaSource =
                CoCreateInstance(&GUID::from_u128(CLSID_MF), None, CLSCTX_INPROC_SERVER).map_err(e)?;
            let pd = source.CreatePresentationDescriptor().map_err(e)?;
            source.Start(&pd, &GUID::zeroed(), &PROPVARIANT::default()).map_err(e)?;
            let mut stream: Option<IMFMediaStream> = None;
            for _ in 0..4 {
                let ev = source.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)).map_err(e)?;
                if ev.GetType().map_err(e)? == MENewStream.0 as u32 {
                    stream = ev.GetValue().ok().and_then(|v| unknown_of(&v)).and_then(|u| u.cast().ok());
                    break;
                }
            }
            let stream = stream.ok_or("no stream from the source")?;
            let mut got = None;
            for _ in 0..30 {
                stream.RequestSample(None).map_err(e)?;
                let ev = stream.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)).map_err(e)?;
                if ev.GetType().map_err(e)? == MEMediaSample.0 as u32 {
                    got = ev.GetValue().ok().and_then(|v| unknown_of(&v));
                    let s: windows::Win32::Media::MediaFoundation::IMFSample =
                        got.as_ref().ok_or("no sample")?.cast().map_err(e)?;
                    let buf = s.ConvertToContiguousBuffer().map_err(e)?;
                    let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
                    buf.Lock(&mut p, None, Some(&mut len)).map_err(e)?;
                    let y = std::slice::from_raw_parts(p, len as usize).to_vec();
                    buf.Unlock().map_err(e)?;
                    // The source's first type is Ferrocast's own size (1280×720 here).
                    let (l, r) = halves(&y, W as usize, H as usize);
                    if near(l, BRIGHT) && near(r, DARK) {
                        got = Some(s.cast().map_err(e)?);
                        break;
                    }
                    got = None;
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            let _ = source.Stop();
            let _ = source.Shutdown();
            got.map(|_| ()).ok_or_else(|| "the source never gave the picture".into())
        }
    }

    /// Windows 11's camera for every app, end to end (needs admin and the frame server).
    fn every_app(dll: &Path) -> R<()> {
        type Create = unsafe extern "system" fn(
            i32,
            i32,
            i32,
            PCWSTR,
            PCWSTR,
            *const GUID,
            u32,
            *mut *mut std::ffi::c_void,
        ) -> windows::core::HRESULT;
        let create: Create = unsafe {
            let lib = windows::Win32::System::LibraryLoader::LoadLibraryW(w!("mfsensorgroup.dll"))
                .map_err(|_| "skip: no mfsensorgroup.dll here")?;
            let f =
                windows::Win32::System::LibraryLoader::GetProcAddress(lib, windows::core::s!("MFCreateVirtualCamera"))
                    .ok_or("skip: this Windows has no MFCreateVirtualCamera")?;
            std::mem::transmute(f)
        };
        // What this check needs from the machine; without it, a skip that says why. With it,
        // every failure below is a real one.
        let quiet = |cmd: &str, args: &[&str]| {
            std::process::Command::new(cmd)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if !quiet("net", &["session"]) {
            return Err("skip: not running as administrator".into());
        }
        if !quiet("sc", &["query", "FrameServer"]) {
            return Err("skip: no Windows Camera Frame Server service here".into());
        }
        let reg = std::process::Command::new("regsvr32").args(["/s"]).arg(dll).status().map_err(|e| e.to_string())?;
        if !reg.success() {
            return Err(format!("regsvr32 couldn't set the camera up for every app ({reg})"));
        }
        let result = (|| -> R<()> {
            let (name, id) = (HSTRING::from(FRIENDLY_NAME), HSTRING::from(ferrocast_camshare::guid_text(CLSID_MF)));
            let mut raw = std::ptr::null_mut();
            let hr =
                unsafe { create(0, 0, 0, PCWSTR(name.as_ptr()), PCWSTR(id.as_ptr()), std::ptr::null(), 0, &mut raw) };
            if hr.is_err() || raw.is_null() {
                return Err(format!("MFCreateVirtualCamera failed ({hr:?})"));
            }
            let cam = unsafe { windows::Win32::Media::MediaFoundation::IMFVirtualCamera::from_raw(raw) };
            if let Err(x) = unsafe { cam.Start(None) } {
                let _ = unsafe { cam.Shutdown() };
                return Err(format!("Windows 11's camera didn't start ({x})"));
            }
            let read = read_every_app_camera();
            unsafe {
                let _ = cam.Remove();
                let _ = cam.Shutdown();
            }
            read
        })();
        let _ = std::process::Command::new("regsvr32").args(["/s", "/u"]).arg(dll).status();
        result
    }

    /// Finds "Ferrocast Camera" among Media Foundation's cameras and reads the picture.
    fn read_every_app_camera() -> R<()> {
        let e = |x: windows::core::Error| x.to_string();
        let until = Instant::now() + Duration::from_secs(20);
        // SAFETY: plain Media Foundation calls; the device list is freed below.
        unsafe {
            loop {
                let mut attrs: Option<IMFAttributes> = None;
                MFCreateAttributes(&mut attrs, 1).map_err(e)?;
                let attrs = attrs.ok_or("no attributes")?;
                attrs
                    .SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)
                    .map_err(e)?;
                let (mut list, mut n) = (std::ptr::null_mut::<Option<IMFActivate>>(), 0u32);
                MFEnumDeviceSources(&attrs, &mut list, &mut n).map_err(e)?;
                let devices: Vec<Option<IMFActivate>> = (0..n as usize).map(|i| list.add(i).read()).collect();
                CoTaskMemFree(Some(list.cast()));
                for d in devices.into_iter().flatten() {
                    let mut buf = [0u16; 256];
                    let mut len = 0u32;
                    if d.GetString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut buf, Some(&mut len)).is_err() {
                        continue;
                    }
                    if String::from_utf16_lossy(&buf[..len as usize]) != FRIENDLY_NAME {
                        continue;
                    }
                    let source: IMFMediaSource = d.ActivateObject().map_err(e)?;
                    let reader = MFCreateSourceReaderFromMediaSource(&source, None).map_err(e)?;
                    // The size the frame server agreed (the picture is fitted into it).
                    let size = reader
                        .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
                        .and_then(|t| t.GetUINT64(&MF_MT_FRAME_SIZE))
                        .map_err(e)?;
                    let (fw, fh) = ((size >> 32) as usize, (size & 0xFFFF_FFFF) as usize);
                    for _ in 0..60 {
                        let (mut flags, mut sample) = (0u32, None);
                        reader
                            .ReadSample(
                                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                                0,
                                None,
                                Some(&mut flags),
                                None,
                                Some(&mut sample),
                            )
                            .map_err(e)?;
                        let Some(s) = sample else { continue };
                        let buf = s.ConvertToContiguousBuffer().map_err(e)?;
                        let (mut p, mut l) = (std::ptr::null_mut(), 0u32);
                        buf.Lock(&mut p, None, Some(&mut l)).map_err(e)?;
                        let y = std::slice::from_raw_parts(p, l as usize).to_vec();
                        buf.Unlock().map_err(e)?;
                        if y.len() < fw * fh {
                            continue;
                        }
                        let (lv, rv) = halves(&y, fw, fh);
                        if near(lv, BRIGHT) && near(rv, DARK) {
                            let _ = d.ShutdownObject();
                            return Ok(());
                        }
                    }
                    let _ = d.ShutdownObject();
                    return Err("Windows 11's camera never gave the picture".into());
                }
                if Instant::now() > until {
                    return Err("Windows 11's camera wasn't listed".into());
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    #[allow(dead_code)]
    fn _ids() -> (u128, u128) {
        (CLSID_DSHOW, CLSID_MF)
    }
}
