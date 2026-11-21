//! NV12 frames the card draws straight into IOSurface-backed pixel buffers (no CPU copy): a pool
//! of them per canvas size, the two planes wrapped as Metal render targets for wgpu, and a
//! refcounted handle ([`PixelFrame`]) that every encoder (and anything else reading the frame)
//! shares. VideoToolbox takes the buffer as it is; whoever needs host bytes locks it read-only.
//!
//! The pool is CoreVideo's own, so a buffer comes back to it only when the last holder lets go
//! (VideoToolbox keeps its input across its reorder window; a retain count of ours could not see
//! that). The pool is capped: at the cap [`SurfacePool::take`] says so, and the caller draws the
//! frame the copying way instead (counted, never waited for).

use std::ptr;

use super::{CFStringRef, CFTypeRef, Dict, Owned, int, kCFBooleanTrue};

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    static kCVPixelBufferMetalCompatibilityKey: CFStringRef;
    static kCVPixelBufferPoolAllocationThresholdKey: CFStringRef;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
    static kCVImageBufferColorPrimariesKey: CFStringRef;
    static kCVImageBufferTransferFunctionKey: CFStringRef;
    static kCVImageBufferYCbCrMatrixKey: CFStringRef;
    fn CVPixelBufferPoolCreate(
        alloc: CFTypeRef,
        pool_attrs: CFTypeRef,
        buffer_attrs: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> i32;
    fn CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(
        alloc: CFTypeRef,
        pool: CFTypeRef,
        aux: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> i32;
    fn CVPixelBufferGetIOSurface(buf: CFTypeRef) -> CFTypeRef;
    fn CVPixelBufferLockBaseAddress(buf: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buf: CFTypeRef, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(buf: CFTypeRef, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(buf: CFTypeRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(buf: CFTypeRef, plane: usize) -> usize;
    fn CVBufferSetAttachment(buf: CFTypeRef, key: CFStringRef, value: CFTypeRef, mode: u32);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
}

/// '420v', the format VideoToolbox's own pool hands out for NV12 (video range).
const NV12: i32 = i32::from_be_bytes(*b"420v");
/// kCVAttachmentMode_ShouldPropagate.
const PROPAGATE: u32 = 1;
/// kCVReturnPixelBufferNotOpenGLCompatible is unrelated; this is kCVReturnWouldExceedAllocationThreshold.
const WOULD_EXCEED: i32 = -6689;
/// kCVPixelBufferLock_ReadOnly.
const READ_ONLY: u64 = 1;

/// One IOSurface-backed NV12 frame, shared by reference count: `clone` retains the same buffer
/// (no pixel is copied) and the pool gets it back when the last clone is dropped.
#[derive(Debug)]
pub struct PixelFrame {
    buf: Owned,
    width: u32,
    height: u32,
}

// SAFETY: a CoreVideo pixel buffer is reference counted and the system passes it between its
// own queues (VideoToolbox, Metal); here it is written by the card before it is shared, and
// read under CoreVideo's lock afterwards.
unsafe impl Send for PixelFrame {}
// SAFETY: as above; every read goes through `with_planes`'s read-only lock.
unsafe impl Sync for PixelFrame {}

impl Clone for PixelFrame {
    fn clone(&self) -> Self {
        // SAFETY: the buffer is live (we hold a retain); CFRetain adds the clone's.
        let r = unsafe { CFRetain(self.buf.0) };
        Self { buf: Owned(r), width: self.width, height: self.height }
    }
}

impl PixelFrame {
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The CVPixelBufferRef, for the encoder (not retained).
    pub(crate) fn raw(&self) -> CFTypeRef {
        self.buf.0
    }

    /// The planes as they sit in memory, read-only: `f` gets Y, its stride, CbCr, its stride.
    /// None when the system won't let the buffer be read.
    pub fn with_planes<R>(&self, f: impl FnOnce(&[u8], usize, &[u8], usize) -> R) -> Option<R> {
        // SAFETY: the buffer is live; locked read-only until the guard's drop (a panic in `f` too).
        if unsafe { CVPixelBufferLockBaseAddress(self.buf.0, READ_ONLY) } != 0 {
            return None;
        }
        struct Unlock(CFTypeRef);
        impl Drop for Unlock {
            fn drop(&mut self) {
                // SAFETY: locked with the same flags by `with_planes`; the buffer outlives the guard.
                unsafe { CVPixelBufferUnlockBaseAddress(self.0, READ_ONLY) };
            }
        }
        let _unlock = Unlock(self.buf.0);
        // SAFETY: locked; NV12 has planes 0 (Y) and 1 (CbCr), each `stride * rows` bytes.
        let planes = unsafe {
            let plane = |i: usize| {
                let base = CVPixelBufferGetBaseAddressOfPlane(self.buf.0, i);
                let stride = CVPixelBufferGetBytesPerRowOfPlane(self.buf.0, i);
                let rows = CVPixelBufferGetHeightOfPlane(self.buf.0, i);
                (!base.is_null()).then(|| (std::slice::from_raw_parts(base, stride * rows), stride))
            };
            plane(0).zip(plane(1))
        };
        planes.map(|((y, ys), (uv, uvs))| f(y, ys, uv, uvs))
    }

    /// Packed host planes (`width` bytes a row), for a consumer that needs them (a tap, FFmpeg).
    /// A copy: counted as materialised bytes.
    pub fn to_host(&self) -> Option<crate::Nv12> {
        let (w, h) = (self.width as usize, self.height as usize);
        self.with_planes(|y, ys, uv, uvs| {
            let pack = |src: &[u8], stride: usize, rows: usize| {
                let mut out = Vec::with_capacity(w * rows);
                for r in 0..rows {
                    out.extend_from_slice(&src[r * stride..r * stride + w]);
                }
                out
            };
            crate::video::note_materialised_bytes((w * h * 3 / 2) as u64);
            crate::Nv12 { width: self.width, height: self.height, y: pack(y, ys, h), uv: pack(uv, uvs, h / 2) }
        })
    }

    /// The two planes as textures the card can draw into (Y as R8, CbCr as RG8). Each keeps the
    /// buffer retained until wgpu lets it go, which is after the card is done with it.
    pub(crate) fn textures(&self, device: &wgpu::Device) -> Option<(wgpu::Texture, wgpu::Texture)> {
        // SAFETY: the buffer is live; the IOSurface is only looked at ("Get": not retained), and
        // it stays alive while `self` (or a clone in a texture's keep-alive) is.
        let io = unsafe { CVPixelBufferGetIOSurface(self.buf.0) };
        if io.is_null() {
            return None;
        }
        // SAFETY: a non-null IOSurfaceRef; the objc2 type is a transparent CF object handle.
        let io = unsafe { &*io.cast::<objc2_io_surface::IOSurfaceRef>() };
        // SAFETY: `as_hal` is only used to reach the Metal device, which wgpu keeps alive while
        // the guard is held.
        let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
        let (w, h) = (self.width, self.height);
        let make = |plane: usize, mtl: objc2_metal::MTLPixelFormat, fmt: wgpu::TextureFormat, pw: u32, ph: u32| {
            use objc2_metal::{MTLDevice, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};
            // SAFETY: a plain 2D descriptor of the plane's own size.
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    mtl,
                    pw as usize,
                    ph as usize,
                    false,
                )
            };
            desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            let raw = hal.raw_device().newTextureWithDescriptor_iosurface_plane(&desc, io, plane)?;
            let keep = std::sync::Mutex::new(Some(self.clone()));
            // SAFETY: `raw` is a 2D texture of `fmt`, pw x ph, one mip and layer, made just above
            // from the live IOSurface's plane; the buffer stays retained (`keep`) until wgpu
            // destroys the texture, after the card has finished with it.
            let hal_tex = unsafe {
                wgpu::hal::metal::Device::texture_from_raw(
                    raw,
                    fmt,
                    MTLTextureType::Type2D,
                    1,
                    1,
                    wgpu::hal::CopyExtent { width: pw, height: ph, depth: 1 },
                    Some(Box::new(move || drop(keep))),
                )
            };
            Some((hal_tex, fmt, pw, ph))
        };
        let y = make(0, objc2_metal::MTLPixelFormat::R8Unorm, wgpu::TextureFormat::R8Unorm, w, h)?;
        let uv = make(1, objc2_metal::MTLPixelFormat::RG8Unorm, wgpu::TextureFormat::Rg8Unorm, w / 2, h / 2)?;
        drop(hal);
        let wrap = |(t, fmt, pw, ph): (wgpu::hal::metal::Texture, wgpu::TextureFormat, u32, u32)| {
            // SAFETY: the descriptor matches the hal texture made above (size, format, one mip, 2D).
            unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                    t,
                    &wgpu::TextureDescriptor {
                        label: Some("nv12 surface"),
                        size: wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: fmt,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::UNINITIALIZED,
                )
            }
        };
        Some((wrap(y), wrap(uv)))
    }
}

/// Why no frame came out of the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Empty {
    /// Every buffer the pool may make is out (encoders still hold them).
    Full,
    /// The system refused (out of memory, no IOSurface).
    Failed,
}

/// A pool of NV12 buffers of one size.
#[derive(Debug)]
pub(crate) struct SurfacePool {
    pool: Owned,
    aux: Dict,
    width: u32,
    height: u32,
}

// SAFETY: CVPixelBufferPool is thread-safe by CoreVideo's contract.
unsafe impl Send for SurfacePool {}
// SAFETY: as above.
unsafe impl Sync for SurfacePool {}

impl SurfacePool {
    /// At most `max` buffers at once.
    pub(crate) fn new(width: u32, height: u32, max: u32) -> Option<Self> {
        let attrs = Dict::new()?;
        let fmt = int(NV12)?;
        let w = int(i32::try_from(width).ok()?)?;
        let h = int(i32::try_from(height).ok()?)?;
        let surface = Dict::new()?;
        // SAFETY: CoreVideo's constant keys, valid for the process's life; the values are live CF
        // objects the dictionaries retain.
        unsafe {
            attrs.set(kCVPixelBufferPixelFormatTypeKey, fmt.0);
            attrs.set(kCVPixelBufferWidthKey, w.0);
            attrs.set(kCVPixelBufferHeightKey, h.0);
            attrs.set(kCVPixelBufferIOSurfacePropertiesKey, surface.0.0);
            attrs.set(kCVPixelBufferMetalCompatibilityKey, kCFBooleanTrue);
        }
        let aux = Dict::new()?;
        let cap = int(i32::try_from(max).ok()?)?;
        // SAFETY: as above.
        unsafe { aux.set(kCVPixelBufferPoolAllocationThresholdKey, cap.0) };
        let mut pool: CFTypeRef = ptr::null();
        // SAFETY: the attribute dictionary is live; the new pool is +1.
        let s = unsafe { CVPixelBufferPoolCreate(ptr::null(), ptr::null(), attrs.0.0, &raw mut pool) };
        if s != 0 {
            return None;
        }
        Some(Self { pool: Owned::new(pool)?, aux, width, height })
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// A free buffer, colour-tagged as the encoder's own are; never waits.
    pub(crate) fn take(&self) -> Result<PixelFrame, Empty> {
        let mut buf: CFTypeRef = ptr::null();
        // SAFETY: the pool and the aux dictionary are live; the new buffer is +1.
        let s = unsafe {
            CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(ptr::null(), self.pool.0, self.aux.0.0, &raw mut buf)
        };
        if s == WOULD_EXCEED {
            return Err(Empty::Full);
        }
        let Some(buf) = Owned::new(buf).filter(|_| s == 0) else { return Err(Empty::Failed) };
        // Before it is shared: the colour tags the encoder's own buffers get.
        // SAFETY: CoreVideo's constant keys and values; the buffer is live and ours alone.
        unsafe {
            CVBufferSetAttachment(
                buf.0,
                kCVImageBufferColorPrimariesKey,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
                PROPAGATE,
            );
            CVBufferSetAttachment(
                buf.0,
                kCVImageBufferTransferFunctionKey,
                kCVImageBufferTransferFunction_ITU_R_709_2,
                PROPAGATE,
            );
            CVBufferSetAttachment(
                buf.0,
                kCVImageBufferYCbCrMatrixKey,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                PROPAGATE,
            );
        }
        Ok(PixelFrame { buf, width: self.width, height: self.height })
    }
}
