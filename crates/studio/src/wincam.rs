//! The virtual camera on Windows (docs/plan/VIRTUAL-CAMERA-WINDOWS.md): what's on air (or one
//! source) drawn as NV12 at the camera's size by the engine, written to shared memory that
//! Ferrocast Camera (`ferrocast_camera.dll`) reads in other apps.
//!
//! - DirectShow: the camera registered for this user (no admin), both the 64-bit and the
//!   32-bit DLL. Zoom, Discord, OBS, browsers… list it.
//! - Windows 11, once set up for every app (one UAC prompt, `regsvr32` on the DLL): a Media
//!   Foundation camera made for this session while the camera is on. Every camera app lists
//!   it, so the DirectShow one is taken away then (never two "Ferrocast Camera"s).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ferrocast_camshare::regdata;
use ferrocast_camshare::win::{Root, View, delete_keys, read_text, write_entries};
use ferrocast_camshare::{CLSID_MF, FRIENDLY_NAME, NAME_GLOBAL, NAME_LOCAL, Writer, camera_size, guid_text, now_ms};
use ferrocore::Filter;
use ferrocore::engine::Canvas;
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HMODULE, WAIT_OBJECT_0};
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
    MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{GUID, HRESULT, HSTRING, Interface, PCWSTR, s, w};

use crate::own::Shared;

/// The camera's DLLs beside Ferrocast's program, named with Ferrocast's version: an app
/// holding the camera open keeps an older file locked, so an update can't overwrite it and
/// puts its own beside it instead (the registration then points at the new one).
fn dll_name(bits: u32) -> String {
    let stem = if bits == 32 { "ferrocast_camera32" } else { "ferrocast_camera" };
    format!("{stem}-{}.dll", env!("CARGO_PKG_VERSION"))
}

/// The DLL for 64-bit (or 32-bit) apps in `dir`: this version's, else an unversioned one (a
/// build run from its folder).
fn dll(dir: &Path, bits: u32) -> Option<PathBuf> {
    let plain = if bits == 32 { "ferrocast_camera32.dll" } else { "ferrocast_camera.dll" };
    [dir.join(dll_name(bits)), dir.join(plain)].into_iter().find(|p| p.exists())
}

/// Deletes other versions' DLLs no app holds any more.
fn tidy(dir: &Path) {
    let keep = [dll_name(64), dll_name(32)];
    let Ok(list) = std::fs::read_dir(dir) else { return };
    for f in list.flatten() {
        let name = f.file_name().to_string_lossy().into_owned();
        if (name.starts_with("ferrocast_camera-") || name.starts_with("ferrocast_camera32-"))
            && name.ends_with(".dll")
            && !keep.contains(&name)
        {
            // Still in use by an app: tried again next time.
            let _ = std::fs::remove_file(f.path());
        }
    }
}
/// Video calls take 30 frames a second: a faster canvas sends every second or third frame.
const MAX_FPS: u32 = 30;

/// What the Studio can say about the camera's setup on this PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CameraSetup {
    /// Windows 11's camera can be set up (it has `MFCreateVirtualCamera`, and this isn't the
    /// Store version).
    pub every_app_possible: bool,
    /// It's set up (the one-time step ran).
    pub every_app_on: bool,
    /// The Microsoft Store version: its registry is the package's own, so other apps can't see
    /// a camera it registers.
    pub store: bool,
}

/// The camera's setup here.
pub fn setup() -> CameraSetup {
    let store = packaged();
    CameraSetup { every_app_possible: !store && mf_create().is_some(), every_app_on: every_app_installed(), store }
}

/// Sets up (or takes away) the camera for every app: one UAC prompt, then `regsvr32` copies
/// the DLL where Windows' camera service can read it and registers it for the machine.
pub fn set_every_app(on: bool) -> Result<CameraSetup, String> {
    if packaged() {
        return Err(STORE.into());
    }
    let dll = dll(&dll_dir()?, 64).ok_or(MISSING)?;
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let regsvr = Path::new(&system).join("System32").join("regsvr32.exe");
    let args = format!("{}/s \"{}\"", if on { "" } else { "/u " }, dll.display());
    match run_elevated(&regsvr, &args)? {
        0 => Ok(setup()),
        code => Err(format!("Windows couldn't set up the camera for every app (regsvr32 said {code}).")),
    }
}

const STORE: &str = "The Microsoft Store version can't add a camera other apps see yet: \
                     install Ferrocast from its website for the virtual camera.";
const MISSING: &str = "Ferrocast Camera's files are missing: install Ferrocast again.";

/// A running camera; dropping it stops sending (apps show the placeholder) and takes the
/// Windows 11 camera away.
#[derive(Debug)]
pub(crate) struct Camera {
    engine: Weak<Shared>,
    tap: u64,
    _mf: Option<MfCamera>,
    /// Whether it's the camera for every app (Windows 11) rather than DirectShow's.
    pub(crate) every_app: bool,
}

impl Drop for Camera {
    fn drop(&mut self) {
        if let Some(e) = self.engine.upgrade() {
            e.engine.stop_tap(self.tap);
        }
    }
}

impl Camera {
    pub(crate) fn active(&self) -> bool {
        self.engine.strong_count() > 0
    }

    /// Starts sending `scene` (None: what's on air) drawn at `size`, scaled into the camera's
    /// size, at the canvas's rate (30 at most).
    pub(crate) fn start(
        engine: Weak<Shared>,
        scene: Option<String>,
        size: (u32, u32),
        fps: u32,
    ) -> Result<Camera, String> {
        if packaged() {
            return Err(STORE.into());
        }
        let dir = dll_dir()?;
        if dll(&dir, 64).is_none() {
            return Err(MISSING.into());
        }
        let (region, _) = ferrocast_camshare::win::create(NAME_LOCAL, None)?;
        // Windows 11 set up for every app: its camera, and no DirectShow one beside it.
        let mf = if every_app_installed() {
            match MfCamera::start() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "Windows 11's camera didn't start: DirectShow's instead");
                    None
                }
            }
        } else {
            None
        };
        if mf.is_some() {
            forget_dshow();
        } else {
            register_dshow(&dir)?;
        }
        let (cw, ch) = camera_size(size.0, size.1);
        let every = fps.div_ceil(MAX_FPS).max(1);
        let rate = (fps / every).max(1);
        let canvas = Canvas {
            width: cw,
            height: ch,
            scene,
            every,
            track: 0,
            scaled_from: ((cw, ch) != size).then_some((size.0, size.1, Filter::Bicubic)),
            more_tracks: Vec::new(),
            video_tracks: Vec::new(),
        };
        let writers = Arc::new(Mutex::new(Writers {
            local: Writer::new(region),
            global: None,
            looked: None,
            every_app: mf.is_some(),
        }));
        let sink = {
            let writers = writers.clone();
            Box::new(move |f: &ferrocore::Nv12, ts: u64| {
                let mut w = writers.lock().unwrap_or_else(|p| p.into_inner());
                w.write(f, (rate, 1), ts);
            })
        };
        let shared = engine.upgrade().ok_or("The Studio isn't running.")?;
        let tap = shared.engine.tap_frames(canvas, sink).map_err(|e| e.to_string())?;
        tracing::info!(size = %format!("{cw}x{ch}"), fps = rate, every_app = mf.is_some(), "the virtual camera is on");
        Ok(Camera { engine, tap, every_app: mf.is_some(), _mf: mf })
    }
}

/// Where the frames go: the session's region (DirectShow apps), and the frame server's
/// (Windows 11's camera) once it has made it.
struct Writers {
    local: Writer,
    global: Option<Writer>,
    looked: Option<Instant>,
    every_app: bool,
}

impl Drop for Writers {
    fn drop(&mut self) {
        // The frame server's region is open to every signed-in account: leave no picture in it.
        if let Some(g) = &self.global {
            g.wipe();
        }
    }
}

impl Writers {
    fn write(&mut self, f: &ferrocore::Nv12, fps: (u32, u32), ts: u64) {
        let now = now_ms();
        let _ = self.local.write((f.width, f.height), fps, ts, &f.y, &f.uv, now);
        // The frame server makes its region when an app first opens the camera: look once a
        // second until it's there.
        if self.every_app && self.global.is_none() && self.looked.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            self.looked = Some(Instant::now());
            self.global = ferrocast_camshare::win::open(NAME_GLOBAL, true).map(Writer::new);
        }
        if let Some(g) = &mut self.global {
            let _ = g.write((f.width, f.height), fps, ts, &f.y, &f.uv, now);
        }
    }
}

// ------------------------------------------------------------------ registration

/// Where the camera's DLLs are: beside the program (the installer puts them there).
fn dll_dir() -> Result<PathBuf, String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| "Ferrocast's folder isn't known.".to_string())
}

/// The DirectShow camera for this user, pointing at our DLLs (written again when they moved:
/// an update to another folder).
fn register_dshow(dir: &Path) -> Result<(), String> {
    let formats = ferrocast_camshare::caps::FORMATS.map(|f| f.fourcc());
    let class = format!("CLSID\\{}\\InprocServer32", guid_text(ferrocast_camshare::CLSID_DSHOW));
    for (view, bits) in [(View::Bits64, 64), (View::Bits32, 32)] {
        let Some(file) = dll(dir, bits) else { continue };
        let path = file.to_string_lossy().into_owned();
        let listed = read_text(Root::User, view, &regdata::dshow_instance_key(), Some("FriendlyName"));
        if read_text(Root::User, view, &class, None).as_deref() != Some(path.as_str())
            || listed.as_deref() != Some(FRIENDLY_NAME)
        {
            write_entries(Root::User, view, &regdata::dshow_entries(&path, &formats))?;
        }
    }
    tidy(dir);
    Ok(())
}

/// Takes the DirectShow camera away for this user (Windows 11's is used instead, or Ferrocast
/// is being removed).
pub fn forget_dshow() {
    for view in [View::Bits64, View::Bits32] {
        delete_keys(Root::User, view, &regdata::dshow_keys());
    }
}

/// The one-time step ran: the media source's class is registered for the machine, and its
/// copy is there.
fn every_app_installed() -> bool {
    let key = format!("CLSID\\{}\\InprocServer32", guid_text(CLSID_MF));
    read_text(Root::Machine, View::Bits64, &key, None).is_some_and(|p| Path::new(&p).exists())
}

/// Ferrocast runs packaged (the Microsoft Store version).
fn packaged() -> bool {
    let mut len = 0u32;
    // SAFETY: asks only for the name's length; APPMODEL_ERROR_NO_PACKAGE (15700) when unpackaged.
    let err = unsafe { GetCurrentPackageFullName(&mut len, None) };
    err.0 != 15700
}

/// Runs `exe args` as administrator (Windows asks first) and waits for it → its exit code.
fn run_elevated(exe: &Path, args: &str) -> Result<u32, String> {
    let (file, params) = (HSTRING::from(exe.as_os_str()), HSTRING::from(args));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: `info` and its strings live through the call.
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        return Err(if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
            "Windows didn't allow it: the prompt was closed.".into()
        } else {
            format!("Windows couldn't ask for permission ({e}).")
        });
    }
    let process = info.hProcess;
    // SAFETY: waits on the process we started, then reads its exit code and closes it.
    unsafe {
        if WaitForSingleObject(process, 120_000) != WAIT_OBJECT_0 {
            let _ = CloseHandle(process);
            return Err("Setting up the camera took too long.".into());
        }
        let mut code = 0u32;
        let got = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
        got.map_err(|e| e.to_string())?;
        Ok(code)
    }
}

// ------------------------------------------------------------------ Windows 11's camera

type CreateVirtualCamera =
    unsafe extern "system" fn(i32, i32, i32, PCWSTR, PCWSTR, *const GUID, u32, *mut *mut std::ffi::c_void) -> HRESULT;
type MfStartup = unsafe extern "system" fn(u32, u32) -> HRESULT;
type MfShutdown = unsafe extern "system" fn() -> HRESULT;

/// Looks up a function of a system DLL (None where this Windows doesn't have it).
fn lookup(dll: PCWSTR, name: windows::core::PCSTR) -> Option<unsafe extern "system" fn() -> isize> {
    // SAFETY: loads a system DLL by name (kept loaded) and finds an export.
    unsafe {
        let lib: HMODULE = LoadLibraryW(dll).ok()?;
        GetProcAddress(lib, name)
    }
}

/// `MFCreateVirtualCamera` (Windows 11 build 22000 and later).
fn mf_create() -> Option<CreateVirtualCamera> {
    // SAFETY: the export's documented signature.
    lookup(w!("mfsensorgroup.dll"), s!("MFCreateVirtualCamera")).map(|f| unsafe { std::mem::transmute(f) })
}

/// Windows 11's camera, owned by a thread of its own (COM's multithreaded apartment), removed
/// when dropped.
#[derive(Debug)]
struct MfCamera {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for MfCamera {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A COM pointer kept on its own thread.
struct Held(IMFVirtualCamera);

impl MfCamera {
    fn start() -> Result<MfCamera, String> {
        let create = mf_create().ok_or("This Windows has no camera for every app (Windows 11 has).")?;
        // SAFETY: the exports' documented signatures.
        let startup: Option<MfStartup> =
            lookup(w!("mfplat.dll"), s!("MFStartup")).map(|f| unsafe { std::mem::transmute(f) });
        // SAFETY: as above.
        let shutdown: Option<MfShutdown> =
            lookup(w!("mfplat.dll"), s!("MFShutdown")).map(|f| unsafe { std::mem::transmute(f) });
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("ferrocast-camera-mf".into())
            .spawn(move || {
                // SAFETY: COM for this thread, undone at its end.
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                // SAFETY: Media Foundation for this process (counted; MFShutdown below).
                let mf_on = startup.is_some_and(|f| unsafe { f(0x0002_0070, 0) }.is_ok());
                let made = (|| -> Result<Held, String> {
                    let (name, id) = (HSTRING::from(FRIENDLY_NAME), HSTRING::from(guid_text(CLSID_MF)));
                    let mut raw = std::ptr::null_mut();
                    // SAFETY: the documented call: a software camera for this session, seen
                    // by this user, made from our media source's class.
                    let hr = unsafe {
                        create(
                            MFVirtualCameraType_SoftwareCameraSource.0,
                            MFVirtualCameraLifetime_Session.0,
                            MFVirtualCameraAccess_CurrentUser.0,
                            PCWSTR(name.as_ptr()),
                            PCWSTR(id.as_ptr()),
                            std::ptr::null(),
                            0,
                            &mut raw,
                        )
                    };
                    hr.ok().map_err(|e| format!("Windows 11's camera couldn't be made ({e})"))?;
                    if raw.is_null() {
                        return Err("Windows 11's camera couldn't be made".into());
                    }
                    // SAFETY: a counted IMFVirtualCamera from the call above.
                    let cam = unsafe { IMFVirtualCamera::from_raw(raw) };
                    // SAFETY: starts it (apps can see it now).
                    unsafe { cam.Start(None) }.map_err(|e| format!("Windows 11's camera couldn't start ({e})"))?;
                    Ok(Held(cam))
                })();
                match made {
                    Ok(Held(cam)) => {
                        let _ = ready_tx.send(Ok(()));
                        // Until Ferrocast turns it off (the sender is dropped).
                        let _ = stop_rx.recv();
                        // SAFETY: takes the camera away and lets go of it, on its own thread.
                        unsafe {
                            let _ = cam.Remove();
                            let _ = cam.Shutdown();
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
                if mf_on && let Some(f) = shutdown {
                    // SAFETY: paired with MFStartup above.
                    let _ = unsafe { f() };
                }
                if com {
                    // SAFETY: paired with CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(())) => Ok(MfCamera { stop: Some(stop_tx), thread: Some(thread) }),
            // It said why; its thread ends by itself.
            Ok(Err(e)) => Err(e),
            // Still inside Windows' call: not waited for. The thread finds the stop sender gone
            // and takes the camera away as soon as Windows returns.
            Err(_) => {
                drop(stop_tx);
                drop(thread);
                Err("Windows 11's camera took too long to start".into())
            }
        }
    }
}
