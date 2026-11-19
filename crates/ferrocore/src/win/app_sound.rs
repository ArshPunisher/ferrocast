//! One app's sound on Windows (10 version 2004 and later): WASAPI's process loopback, the way
//! OBS's "Application Audio Capture" and its window capture's "Capture Audio" hear a single app
//! (its child processes too: a browser plays its tabs' sound from helper processes).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HWND, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::core::{Interface, implement};

/// 48 kHz stereo floats: what the engine mixes (Windows converts the app's sound to it).
const RATE: u32 = 48_000;
/// WAVE_FORMAT_IEEE_FLOAT (mmreg.h).
const FLOAT: u16 = 3;

/// The process that owns a window (0 when it's gone).
pub fn process_of(window: u32) -> u32 {
    let mut pid = 0u32;
    // SAFETY: reads the owner of a window handle; a closed one leaves `pid` 0.
    unsafe { GetWindowThreadProcessId(HWND(window as usize as *mut _), Some(&mut pid)) };
    pid
}

/// Told when Windows has made (or failed to make) the audio client.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Mutex<Option<mpsc::Sender<()>>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _op: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        if let Some(tx) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum LoopbackTarget {
    IncludeTree(u32),
    ExcludeTree(u32),
}

/// An audio client hearing (or excluding) `target` and its child processes.
fn client_for(target: LoopbackTarget) -> Result<IAudioClient, String> {
    let (pid, mode) = match target {
        LoopbackTarget::IncludeTree(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        LoopbackTarget::ExcludeTree(pid) => (pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
    };
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    };
    // Never dropped: PROPVARIANT's Drop is PropVariantClear, which frees a VT_BLOB's data with
    // CoTaskMemFree. Ours points at `params` on this stack, so freeing it corrupted the heap
    // and Windows closed the app at once (seen on Windows 11 with a Chrome window source).
    let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw mut params).cast(),
                    },
                },
            }),
        },
    });
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(Mutex::new(Some(tx))).into();
    // SAFETY: `prop` points at `params`, both alive until the activation has answered below
    // (Windows copies the blob during the call); the handler is a COM object of ours.
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&raw const *prop),
            &handler,
        )
    }
    .map_err(|e| format!("Windows can't hear one app here ({e}); it needs Windows 10 version 2004 or later"))?;
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "Windows didn't answer for the app's sound".to_string())?;
    let mut hr = windows::core::HRESULT(0);
    let mut unknown = None;
    // SAFETY: out pointers of ours; the operation has completed (the handler was called).
    unsafe { op.GetActivateResult(&mut hr, &mut unknown) }.map_err(|e| e.to_string())?;
    hr.ok().map_err(|e| format!("the app's sound couldn't be opened ({e})"))?;
    unknown.ok_or("the app's sound couldn't be opened")?.cast::<IAudioClient>().map_err(|e| e.to_string())
}

/// Hears process `pid` until `stop`, handing 48 kHz stereo pieces to `keep`. `ready` is told
/// once whether it started. It also returns when the app quits or Windows lets go of its sound
/// (a read fails): its caller says so, and the watchdog starts it again (before 20 Nov it went on
/// forever, silent, after the app was closed and opened again).
pub fn run(pid: u32, stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>, keep: impl FnMut(&[f32])) {
    run_target(LoopbackTarget::IncludeTree(pid), stop, ready, keep);
}

/// Hears everything the computer plays except this process (and its children), until `stop`.
/// Used for Desktop Audio on Windows 10 version 2004+ so Ferrocast's own monitoring playback
/// never feeds back or leaks onto the stream.
pub fn run_excluding_self(stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>, keep: impl FnMut(&[f32])) {
    // SAFETY: GetCurrentProcessId reads the current process identifier with no preconditions.
    let pid = unsafe { GetCurrentProcessId() };
    run_target(LoopbackTarget::ExcludeTree(pid), stop, ready, keep);
}

fn run_target(
    target: LoopbackTarget,
    stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), String>>,
    mut keep: impl FnMut(&[f32]),
) {
    // SAFETY: COM for this thread (its own; undone below).
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let started = (|| -> Result<(IAudioClient, IAudioCaptureClient), String> {
        if matches!(target, LoopbackTarget::IncludeTree(0)) {
            return Err("that window is closed".into());
        }
        let client = client_for(target)?;
        let format = WAVEFORMATEX {
            wFormatTag: FLOAT,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // SAFETY: a format of ours, alive for the call; 200 ms of room (in 100 ns units).
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                2_000_000,
                0,
                &format,
                None,
            )
        }
        .map_err(|e| format!("the app's sound couldn't be opened ({e})"))?;
        // SAFETY: the client was initialised above.
        let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|e| e.to_string())?;
        // SAFETY: as above.
        unsafe { client.Start() }.map_err(|e| e.to_string())?;
        Ok((client, capture))
    })();
    let (client, capture) = match started {
        Ok(c) => {
            let _ = ready.send(Ok(()));
            c
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            if com {
                // SAFETY: undoes this thread's CoInitializeEx.
                unsafe { CoUninitialize() };
            }
            return;
        }
    };
    let mut silence = Vec::new();
    // SAFETY: a handle to wait on the app's end (None if it can't be had: then only a failed
    // read ends this); closed below.
    let app = match target {
        // SAFETY: a handle to wait on the app's end, closed below.
        LoopbackTarget::IncludeTree(pid) => unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.ok(),
        LoopbackTarget::ExcludeTree(_) => None,
    };
    let mut ticks = 0u32;
    'hear: while !stop.load(Ordering::Acquire) {
        // Windows hands sound over every 10 ms or so; nothing comes while the app is quiet.
        std::thread::sleep(Duration::from_millis(5));
        ticks = ticks.wrapping_add(1);
        // SAFETY: a live process handle of ours; a zero wait only looks.
        if ticks.is_multiple_of(40) && app.is_some_and(|h| unsafe { WaitForSingleObject(h, 0) } == WAIT_OBJECT_0) {
            break;
        }
        loop {
            // SAFETY: a started capture client, read on the thread that made it.
            let Ok(n) = (unsafe { capture.GetNextPacketSize() }) else { break 'hear };
            if n == 0 {
                break;
            }
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            // SAFETY: out pointers of ours; the buffer is read and given back before the next one.
            if unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.is_err() {
                break 'hear;
            }
            let samples = frames as usize * 2;
            if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                silence.resize(samples, 0.0);
                keep(&silence);
            } else {
                // SAFETY: Windows lent `frames` frames of the format asked for (two f32 each).
                keep(unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) });
            }
            // SAFETY: gives back the buffer just read.
            unsafe {
                let _ = capture.ReleaseBuffer(frames);
            }
        }
    }
    if let Some(h) = app {
        // SAFETY: the handle opened above, closed once.
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    // SAFETY: stops the client started above.
    unsafe {
        let _ = client.Stop();
    }
    drop((capture, client));
    if com {
        // SAFETY: undoes this thread's CoInitializeEx.
        unsafe { CoUninitialize() };
    }
}
