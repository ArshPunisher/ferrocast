//! Windows' own capture (Windows.Graphics.Capture, as OBS's "Windows 10 (1903 and up)" method):
//! a display or a window, games in full screen included, straight from the compositor. Each
//! frame is copied from the GPU into a picture (fitted to the canvas). A Direct3D device on the
//! GPU, else Windows' CPU one (WARP); where it can't run at all, the caller falls back to FFmpeg.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RESOURCE_MISC_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::Interface;

use crate::win_logic::{WgcPacer, is_fatal_wgc_hresult};
use crate::{AvError, Picture};

struct WinRtGuard(bool);

impl Drop for WinRtGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the single successful `RoInitialize` call on this thread.
            unsafe { RoUninitialize() };
        }
    }
}

thread_local! {
    static WINRT_THREAD: WinRtGuard = {
        // SAFETY: initializes WinRT MTA once per thread; `S_OK`/`S_FALSE` return `Ok(())` and are
        // balanced by `RoUninitialize` when the thread exits, while `RPC_E_CHANGED_MODE` returns
        // `Err` (apartment already initialized differently) and must not be uninitialized.
        WinRtGuard(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok())
    };
}

fn ensure_winrt_init() {
    let _ = WINRT_THREAD.try_with(|_| ());
}

/// What the frames leave for the engine.
#[derive(Debug, Default)]
struct Shared {
    latest: Mutex<Option<Surface>>,
    frames: AtomicU64,
    /// True while `Reader` holds a GPU staging copy of a paced-out frame waiting to flush on still.
    deferred_pending: AtomicBool,
    /// The window closed (or the display went).
    closed: AtomicBool,
    /// A frame callback or Direct3D read hit a fatal error (e.g. DXGI device loss).
    failed: AtomicBool,
    error: Mutex<Option<String>>,
}

/// The GPU side a frame is read through (used under `Mutex<Reader>`).
struct Reader {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// A CPU-readable copy of the last size seen.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Latest paced-out frame copied on the GPU so a burst ending on a rejected frame flushes once due.
    deferred_staging: Option<(ID3D11Texture2D, u32, u32)>,
    deferred_size: Option<(u32, u32)>,
    max: (u32, u32),
    pacer: WgcPacer,
    /// Pixel buffers handed back by the engine, used again (no 8 MB of fresh memory a frame).
    spare: Spare,
}

// SAFETY: Every D3D11 immediate-context and staging-texture operation on `Reader` runs under
// `Arc<Mutex<Reader>>`, and `ID3D11Device` is free-threaded per Microsoft's D3D11 specification.
unsafe impl Send for Reader {}

/// Thread-safe handle to the D3D11 device so `Wgc::ended()` can poll `GetDeviceRemovedReason`.
struct D3dDevice(ID3D11Device);

// SAFETY: ID3D11Device is free-threaded per Microsoft's D3D11 specification; `ended()` only calls
// `GetDeviceRemovedReason()` which is safe from any thread.
unsafe impl Send for D3dDevice {}
// SAFETY: Same free-threaded guarantee as Send for read-only status inspection.
unsafe impl Sync for D3dDevice {}

/// The WinRT device the pool is recreated with on its own thread.
struct PoolDevice(IDirect3DDevice);

// SAFETY: a WinRT Direct3D device is agile (usable from any thread); the pool's thread only
// passes it back to the pool.
unsafe impl Send for PoolDevice {}

impl PoolDevice {
    fn get(&self) -> &IDirect3DDevice {
        &self.0
    }
}

/// A running capture; dropping it stops.
pub struct Wgc {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    d3d: D3dDevice,
    token: i64,
    shared: Arc<Shared>,
    reader: Arc<Mutex<Reader>>,
}

impl std::fmt::Debug for Wgc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wgc").field("frames", &self.frames()).finish_non_exhaustive()
    }
}

impl Drop for Wgc {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// A Direct3D 11 device: the GPU's, else WARP (Windows drawing on the CPU).
fn device() -> Result<(ID3D11Device, ID3D11DeviceContext), AvError> {
    let mut last = None;
    for kind in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        match try_device(kind) {
            Ok(d) => return Ok(d),
            Err(e) => last = Some(e),
        }
    }
    Err(AvError::Source(format!(
        "no Direct3D device for the capture ({})",
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}

fn try_device(kind: D3D_DRIVER_TYPE) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let (mut device, mut context) = (None, None);
    // SAFETY: out pointers to our own Options, no adapter (the default one) and no software module.
    unsafe {
        D3D11CreateDevice(
            None,
            kind,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    match (device, context) {
        (Some(d), Some(c)) => Ok((d, c)),
        _ => Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)),
    }
}

impl Wgc {
    /// Whether this Windows has the capture (10 version 1903 and later).
    pub fn supported() -> bool {
        GraphicsCaptureSession::IsSupported().unwrap_or(false)
    }

    /// A display by its monitor handle.
    pub fn display(monitor: HMONITOR, max: (u32, u32), fps: u32, cursor: bool) -> Result<Self, AvError> {
        Self::start(
            // SAFETY: a monitor handle Windows listed; the interop only reads it.
            |i| unsafe { i.CreateForMonitor(monitor) },
            max,
            fps,
            cursor,
        )
    }

    /// A window by its handle.
    pub fn window(hwnd: HWND, max: (u32, u32), fps: u32, cursor: bool) -> Result<Self, AvError> {
        Self::start(
            // SAFETY: a window handle; a closed one is an error, not a fault.
            |i| unsafe { i.CreateForWindow(hwnd) },
            max,
            fps,
            cursor,
        )
    }

    fn start(
        item: impl FnOnce(&IGraphicsCaptureItemInterop) -> windows::core::Result<GraphicsCaptureItem>,
        max: (u32, u32),
        fps: u32,
        cursor: bool,
    ) -> Result<Self, AvError> {
        let fail = |what: &str, e: windows::core::Error| AvError::Source(format!("Windows' capture {what} ({e})"));
        // WinRT once per thread (already set up a different way is fine too; uninitialized on exit).
        ensure_winrt_init();
        if !Self::supported() {
            return Err(AvError::Source("this Windows has no capture of its own (it needs 10, 1903 or later)".into()));
        }
        let (d3d, context) = device()?;
        let dxgi: IDXGIDevice = d3d.cast().map_err(|e| fail("had no DXGI device", e))?;
        // SAFETY: a DXGI device from the device just made.
        let inspectable =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(|e| fail("had no device", e))?;
        let device: IDirect3DDevice = inspectable.cast().map_err(|e| fail("had no device", e))?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| fail("isn't there", e))?;
        let item = item(&interop).map_err(|e| fail("couldn't take that", e))?;
        let size = item.Size().map_err(|e| fail("has no size", e))?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(|e| fail("didn't start", e))?;
        let session = pool.CreateCaptureSession(&item).map_err(|e| fail("didn't start", e))?;
        // Newer Windows only: the mouse as asked, and no yellow border round what's captured.
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        let _ = session.SetIsBorderRequired(false);
        let shared = Arc::new(Shared::default());
        let d3d_handle = D3dDevice(d3d.clone());
        let reader = Arc::new(Mutex::new(Reader {
            device: d3d,
            context,
            staging: None,
            deferred_staging: None,
            deferred_size: None,
            max: (max.0.max(16), max.1.max(16)),
            pacer: WgcPacer::new(fps),
            spare: Spare::default(),
        }));
        let pool_size = Mutex::new(size);
        let (s, dev, reader_cb) = (shared.clone(), PoolDevice(device.clone()), reader.clone());
        let token = pool
            .FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
                move |pool, _| {
                    let Some(pool) = pool.as_ref() else { return Ok(()) };
                    let res: windows::core::Result<()> = (|| {
                        let frame = pool.TryGetNextFrame()?;
                        // A window resized: the pool follows (the frame after is the new size).
                        let content = frame.ContentSize()?;
                        {
                            let mut ps = pool_size.lock().unwrap_or_else(|p| p.into_inner());
                            if content.Width != ps.Width || content.Height != ps.Height {
                                *ps = SizeInt32 { Width: content.Width.max(1), Height: content.Height.max(1) };
                                pool.Recreate(dev.get(), DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, *ps)?;
                            }
                        }
                        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                        // SAFETY: the frame's surface is a D3D11 texture on our device.
                        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
                        let w = (content.Width.max(1) as u32).min(texture_width(&texture));
                        let h = (content.Height.max(1) as u32).min(texture_height(&texture));
                        let now = Instant::now();
                        let mut r = reader_cb.lock().unwrap_or_else(|p| p.into_inner());
                        if !r.pacer.accept(now) {
                            // Keep only the newest rejected texture on the GPU so a burst ending on a
                            // paced-out frame flushes when quiet without pinning pool buffers.
                            r.defer(&texture, w, h)?;
                            r.pacer.defer(now);
                            s.deferred_pending.store(true, Ordering::Release);
                            return Ok(());
                        }
                        s.deferred_pending.store(false, Ordering::Release);
                        if let Some(p) = r.read(&texture, w, h)? {
                            *s.latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(p);
                            s.frames.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(())
                    })();
                    if let Err(e) = res {
                        if !is_fatal_wgc_hresult(e.code().0) {
                            return Ok(());
                        }
                        if let Ok(mut err) = s.error.lock() {
                            *err = Some(format!("Windows' capture stopped ({e})"));
                        }
                        s.failed.store(true, Ordering::Release);
                        return Err(e);
                    }
                    Ok(())
                },
            ))
            .map_err(|e| fail("didn't start", e))?;
        let closed = shared.clone();
        let _ =
            item.Closed(&TypedEventHandler::<GraphicsCaptureItem, windows::core::IInspectable>::new(move |_, _| {
                closed.closed.store(true, Ordering::Release);
                Ok(())
            }));
        session.StartCapture().map_err(|e| fail("didn't start", e))?;
        Ok(Wgc { session, pool, d3d: d3d_handle, token, shared, reader })
    }

    /// The newest picture (taken), as RGBA: a copy (the engine uses `take_surface`).
    pub fn take(&self) -> Option<Picture> {
        self.take_surface().map(|s| s.to_picture())
    }

    /// The newest frame as BGRA, for sending to the card as it is (taken).
    pub fn take_surface(&self) -> Option<Surface> {
        if let Some(surface) = self.shared.latest.lock().ok().and_then(|mut g| g.take()) {
            return Some(surface);
        }
        if self.shared.deferred_pending.load(Ordering::Acquire)
            && let Ok(mut r) = self.reader.try_lock()
            && r.pacer.flush_due(Instant::now())
        {
            self.shared.deferred_pending.store(false, Ordering::Release);
            match r.read_deferred() {
                Ok(Some(surface)) => {
                    self.shared.frames.fetch_add(1, Ordering::Relaxed);
                    return Some(surface);
                }
                Ok(None) => {}
                Err(e) if is_fatal_wgc_hresult(e.code().0) => {
                    if let Ok(mut err) = self.shared.error.lock() {
                        *err = Some(format!("Windows' capture stopped ({e})"));
                    }
                    self.shared.failed.store(true, Ordering::Release);
                }
                Err(_) => {}
            }
        }
        None
    }

    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Why it ended, if it did: item closed, frame callback error, or D3D11 device loss/TDR.
    pub fn ended(&self) -> Option<String> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Some("what it captured closed".to_string());
        }
        if self.shared.failed.load(Ordering::Acquire) {
            return Some(
                self.shared
                    .error
                    .lock()
                    .ok()
                    .and_then(|e| e.clone())
                    .unwrap_or_else(|| "Windows' capture failed".to_string()),
            );
        }
        // SAFETY: ID3D11Device::GetDeviceRemovedReason is thread-safe and read-only.
        if let Err(e) = unsafe { self.d3d.0.GetDeviceRemovedReason() } {
            return Some(format!("the graphics device reset ({e})"));
        }
        None
    }
}

fn desc(t: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: fills our own struct.
    unsafe { t.GetDesc(&mut d) };
    d
}

fn texture_width(t: &ID3D11Texture2D) -> u32 {
    desc(t).Width
}

fn texture_height(t: &ID3D11Texture2D) -> u32 {
    desc(t).Height
}

fn ensure_staging_tex<'a>(
    device: &ID3D11Device,
    slot: &'a mut Option<(ID3D11Texture2D, u32, u32)>,
    full: &D3D11_TEXTURE2D_DESC,
) -> windows::core::Result<Option<&'a ID3D11Texture2D>> {
    if slot.as_ref().is_none_or(|(_, sw, sh)| (*sw, *sh) != (full.Width, full.Height)) {
        let d = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: D3D11_BIND_FLAG(0).0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
            ..*full
        };
        let mut t = None;
        // SAFETY: a description copied from a real texture, an out pointer of ours.
        unsafe { device.CreateTexture2D(&d, None, Some(&mut t))? };
        let Some(t) = t else { return Ok(None) };
        *slot = Some((t, full.Width, full.Height));
    }
    Ok(slot.as_ref().map(|(t, ..)| t))
}

fn read_from_staging(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    w: u32,
    h: u32,
    max: (u32, u32),
    spare: &Spare,
) -> windows::core::Result<Surface> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: `staging` is a staging texture on `device`; the map is unmapped before returning.
    unsafe { context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
    let (ow, oh) = crate::ffsrc::fit((w, h), max);
    let mut bgra = spare.get(ow as usize * oh as usize * 4);
    let pitch = mapped.RowPitch as usize;
    let base = mapped.pData as *const u8;
    let out_row = ow as usize * 4;
    // Which source column each output column reads, worked out once a frame (not per pixel).
    let cols: Vec<usize> =
        if ow == w { Vec::new() } else { (0..ow as usize).map(|x| x * w as usize / ow as usize * 4).collect() };
    for y in 0..oh as usize {
        let sy = y * h as usize / oh as usize;
        // SAFETY: row `sy` < the texture's height, inside the mapped data (RowPitch apart).
        let row = unsafe { std::slice::from_raw_parts(base.add(sy * pitch), w as usize * 4) };
        let out = &mut bgra[y * out_row..(y + 1) * out_row];
        if ow == w {
            out.copy_from_slice(row);
        } else {
            for (o, &sx) in out.as_chunks_mut::<4>().0.iter_mut().zip(&cols) {
                o.copy_from_slice(&row[sx..sx + 4]);
            }
        }
        opaque(out);
    }
    // SAFETY: the map made above.
    unsafe { context.Unmap(staging, 0) };
    // SAFETY: verify the D3D11 device wasn't removed mid-readback before accepting pixels.
    unsafe { device.GetDeviceRemovedReason()? };
    Ok(Surface {
        bytes: bgra,
        width: ow,
        height: oh,
        spare: spare.clone(),
        #[cfg(test)]
        copy_counter: None,
    })
}

impl Reader {
    /// Copies a paced-out texture on the GPU without CPU mapping so it can flush when a burst ends.
    fn defer(&mut self, texture: &ID3D11Texture2D, w: u32, h: u32) -> windows::core::Result<()> {
        let full = desc(texture);
        let Some(staging) = ensure_staging_tex(&self.device, &mut self.deferred_staging, &full)? else {
            return Ok(());
        };
        // SAFETY: both textures share `self.device` with identical dimensions and format.
        unsafe { self.context.CopyResource(staging, texture) };
        self.deferred_size = Some((w, h));
        Ok(())
    }

    /// Reads back the previously deferred GPU copy once the pacer allows it.
    fn read_deferred(&mut self) -> windows::core::Result<Option<Surface>> {
        let Some((w, h)) = self.deferred_size.take() else { return Ok(None) };
        let Some((staging, ..)) = self.deferred_staging.as_ref() else { return Ok(None) };
        Ok(Some(read_from_staging(&self.device, &self.context, staging, w, h, self.max, &self.spare)?))
    }

    /// The texture's top-left `w`×`h` as BGRA, fitted into `max` (nearest pixel when it's
    /// bigger: only what's kept is read). Opaque: a window's empty corners would otherwise show
    /// through (Windows leaves their alpha 0).
    fn read(&mut self, texture: &ID3D11Texture2D, w: u32, h: u32) -> windows::core::Result<Option<Surface>> {
        self.deferred_size = None;
        let full = desc(texture);
        let Some(staging) = ensure_staging_tex(&self.device, &mut self.staging, &full)? else {
            return Ok(None);
        };
        // SAFETY: both textures are on this device with the same size and format.
        unsafe { self.context.CopyResource(staging, texture) };
        Ok(Some(read_from_staging(&self.device, &self.context, staging, w, h, self.max, &self.spare)?))
    }
}

/// Buffers going round between the capture and the engine.
#[derive(Debug, Default, Clone)]
struct Spare(Arc<Mutex<Vec<Vec<u8>>>>);

impl Spare {
    /// A buffer of `len` bytes (its old contents are all written over).
    fn get(&self, len: usize) -> Vec<u8> {
        let mut free = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match free.iter().position(|b| b.len() == len) {
            Some(i) => free.swap_remove(i),
            None => {
                // Another size (the window was resized): the old ones are let go.
                free.clear();
                vec![0; len]
            }
        }
    }

    fn give(&self, b: Vec<u8>) {
        let mut free = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if free.len() < 3 {
            free.push(b);
        }
    }
}

/// A frame of the screen as BGRA rows (opaque), handed back for reuse when dropped.
pub struct Surface {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    spare: Spare,
    #[cfg(test)]
    copy_counter: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Surface({}x{})", self.width, self.height)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.spare.give(std::mem::take(&mut self.bytes));
    }
}

impl Surface {
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Its bytes (BGRA, `stride` bytes a row), as the Mac's surface gives them.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&[u8], usize) -> R) -> Option<R> {
        Some(f(&self.bytes, self.width as usize * 4))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn with_copy_counter(mut self, counter: std::sync::Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.copy_counter = Some(counter);
        self
    }

    /// As an RGBA picture (a copy).
    pub fn to_picture(&self) -> Picture {
        #[cfg(test)]
        if let Some(c) = &self.copy_counter {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let mut rgba = vec![0u8; self.bytes.len()];
        bgra_to_rgba(&self.bytes, &mut rgba);
        Picture { width: self.width, height: self.height, rgba, premultiplied: false }
    }
}

/// Every pixel's alpha set full (BGRA rows).
fn opaque(row: &mut [u8]) {
    for px in row.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
}

/// One row of BGRA as opaque RGBA.
fn bgra_to_rgba(src: &[u8], out: &mut [u8]) {
    for (o, i) in out.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
        *o = [i[2], i[1], i[0], 255];
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rows_become_opaque_and_buffers_go_round() {
        let mut row = [1, 2, 3, 0, 10, 20, 30, 7];
        super::opaque(&mut row);
        assert_eq!(row, [1, 2, 3, 255, 10, 20, 30, 255]);
        let spare = super::Spare::default();
        let b = spare.get(16);
        let p = b.as_ptr();
        spare.give(b);
        // Each buffer bound first: a pointer into a temporary is a hard error on Windows CI.
        let again = spare.get(16);
        assert_eq!(again.as_ptr(), p, "the same buffer again");
        let other = spare.get(8);
        assert_eq!(other.len(), 8, "another size: a new one");
    }

    #[test]
    fn a_row_turns_from_bgra_to_opaque_rgba() {
        let mut out = [0u8; 8];
        super::bgra_to_rgba(&[1, 2, 3, 0, 10, 20, 30, 7], &mut out);
        assert_eq!(out, [3, 2, 1, 255, 30, 20, 10, 255]);
    }
}
