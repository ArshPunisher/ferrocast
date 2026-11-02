//! Windows' own capture (Windows.Graphics.Capture, as OBS's "Windows 10 (1903 and up)" method):
//! a display or a window, games in full screen included, straight from the compositor. Each
//! frame is copied from the GPU into a picture (fitted to the canvas). A Direct3D device on the
//! GPU, else Windows' CPU one (WARP); where it can't run at all, the caller falls back to FFmpeg.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::Interface;

use crate::{AvError, Picture};

/// What the frames leave for the engine.
#[derive(Debug, Default)]
struct Shared {
    latest: Mutex<Option<Picture>>,
    frames: AtomicU64,
    /// The window closed (or the display went).
    closed: AtomicBool,
}

/// The GPU side a frame is read through (used on the pool's thread, one frame at a time).
struct Reader {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// A CPU-readable copy of the last size seen.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    max: (u32, u32),
    every: Duration,
    last: Option<Instant>,
}

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
    token: i64,
    shared: Arc<Shared>,
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
        // WinRT on this thread (already set up a different way is fine too).
        // SAFETY: initialises this thread's apartment; a second call is harmless.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
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
        let reader = Mutex::new(Reader {
            device: d3d,
            context,
            staging: None,
            max: (max.0.max(16), max.1.max(16)),
            every: Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 240))),
            last: None,
        });
        let pool_size = Mutex::new(size);
        let (s, dev) = (shared.clone(), PoolDevice(device.clone()));
        let token = pool
            .FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
                move |pool, _| {
                    let Some(pool) = pool.as_ref() else { return Ok(()) };
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
                    let mut r = reader.lock().unwrap_or_else(|p| p.into_inner());
                    // At the canvas's rate at most.
                    if r.last.is_some_and(|t| t.elapsed() < r.every) {
                        return Ok(());
                    }
                    r.last = Some(Instant::now());
                    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                    // SAFETY: the frame's surface is a D3D11 texture on our device.
                    let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
                    let w = (content.Width.max(1) as u32).min(texture_width(&texture));
                    let h = (content.Height.max(1) as u32).min(texture_height(&texture));
                    if let Some(p) = r.read(&texture, w, h)? {
                        *s.latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(p);
                        s.frames.fetch_add(1, Ordering::Relaxed);
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
        Ok(Wgc { session, pool, token, shared })
    }

    /// The newest picture (taken).
    pub fn take(&self) -> Option<Picture> {
        self.shared.latest.lock().ok()?.take()
    }

    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Why it ended, if it did.
    pub fn ended(&self) -> Option<String> {
        self.shared.closed.load(Ordering::Acquire).then(|| "what it captured closed".to_string())
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

impl Reader {
    /// The texture's top-left `w`×`h` as a picture, fitted into `max` (nearest pixel when it's
    /// bigger: only what's kept is read).
    fn read(&mut self, texture: &ID3D11Texture2D, w: u32, h: u32) -> windows::core::Result<Option<Picture>> {
        let full = desc(texture);
        if self.staging.as_ref().is_none_or(|(_, sw, sh)| (*sw, *sh) != (full.Width, full.Height)) {
            let d = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: D3D11_BIND_FLAG(0).0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
                ..full
            };
            let mut t = None;
            // SAFETY: a description copied from a real texture, an out pointer of ours.
            unsafe { self.device.CreateTexture2D(&d, None, Some(&mut t))? };
            let Some(t) = t else { return Ok(None) };
            self.staging = Some((t, full.Width, full.Height));
        }
        let Some((staging, ..)) = self.staging.as_ref() else { return Ok(None) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both textures are on this device with the same size and format; the map is
        // read and let go before this returns.
        unsafe {
            self.context.CopyResource(staging, texture);
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let (ow, oh) = crate::ffsrc::fit((w, h), self.max);
        let mut rgba = vec![0u8; ow as usize * oh as usize * 4];
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData as *const u8;
        // Which source column each output column reads, worked out once a frame (not per pixel).
        let cols: Vec<usize> = (0..ow as usize).map(|x| x * w as usize / ow as usize * 4).collect();
        for y in 0..oh as usize {
            let sy = y * h as usize / oh as usize;
            // SAFETY: row `sy` < the texture's height, inside the mapped data (RowPitch apart).
            let row = unsafe { std::slice::from_raw_parts(base.add(sy * pitch), w as usize * 4) };
            let out = &mut rgba[y * ow as usize * 4..(y + 1) * ow as usize * 4];
            if ow == w {
                // Same size: a straight swap of blue and red, which the compiler vectorises.
                bgra_to_rgba(row, out);
            } else {
                for (o, &sx) in out.as_chunks_mut::<4>().0.iter_mut().zip(&cols) {
                    // BGRA in, RGBA out; Windows' desktop is opaque.
                    *o = [row[sx + 2], row[sx + 1], row[sx], 255];
                }
            }
        }
        // SAFETY: the map made above.
        unsafe { self.context.Unmap(staging, 0) };
        Ok(Some(Picture { width: ow, height: oh, rgba, premultiplied: false }))
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
    fn a_row_turns_from_bgra_to_opaque_rgba() {
        let mut out = [0u8; 8];
        super::bgra_to_rgba(&[1, 2, 3, 0, 10, 20, 30, 7], &mut out);
        assert_eq!(out, [3, 2, 1, 255, 30, 20, 10, 255]);
    }
}
