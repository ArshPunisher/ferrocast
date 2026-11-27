//! The Studio's preview drawn by our engine, as libobs's `draw_main` draws it: the canvas
//! placed in the view (where the page says, or fitted inside a margin left for handles), the
//! sources' outlines over it (the picked ones with handles, the rest in their colours, off the
//! canvas too), and a dimming layer while a dialog is open. Into a window's surface
//! (`present`), or read back (`render_preview`) for tests and screenshots.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::compose::Target;
use crate::{AvError, Compositor, Scene, Transition};

/// One source's box on the canvas, drawn over the preview.
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    /// Its corners on the canvas (canvas pixels), in order round the box.
    pub corners: [(f32, f32); 4],
    /// Straight alpha, in the view's gamma (as CSS colours are).
    pub color: [f32; 4],
    /// Line width in the view's pixels.
    pub width: f32,
    /// Picked: handles at its corners and edges' middles.
    pub handles: bool,
}

/// Diagonal stripes over the part of a source's box that lies outside the canvas, the way
/// Streamlabs shows what won't be seen: a fill in `a`, bands of `b` over it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overflow {
    /// The fill (straight alpha, the view's gamma): the page's colour round the canvas.
    pub a: [f32; 4],
    /// The bands' colour, same encoding.
    pub b: [f32; 4],
    /// Distance between two bands' starts across the bands, in view pixels.
    pub period: f32,
}

/// How the preview looks round and over the picture.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewLook {
    /// The view's colour round the canvas (straight alpha, the view's gamma).
    pub background: [f32; 4],
    /// Where the canvas sits in the view (view pixels: x, y, w, h) when the page says; its
    /// shape is kept inside that box. None fits it inside `margin`.
    pub area: Option<(f32, f32, f32, f32)>,
    /// The view's size (view pixels) `area` was given for. While the view is another size (the
    /// window being resized: the view follows it at once, the page says where things are a
    /// beat later) the canvas is fitted to the view as it is now, as OBS's preview does,
    /// instead of drawn where it was.
    pub area_for: Option<(u32, u32)>,
    /// Room left round a fitted canvas (view pixels), for handles at its edge.
    pub margin: f32,
    pub outlines: Vec<Outline>,
    /// What to outline, worked out by the engine from its own items each frame (it then
    /// replaces `outlines`): the picture and its boxes from the same moment.
    pub marks: Option<Marks>,
    /// A dark layer over everything, 0 (none) to 1, as a dialog's backdrop dims the page.
    pub shade: f32,
    /// Distance guides for the first picked source (while it's dragged), as OBS draws them:
    /// a line from each side to the canvas edge with its length in canvas pixels. The view's
    /// pixels per point, so the labels read the same size on any screen; None draws none.
    pub guides: Option<f32>,
    /// Stripes over what runs past the canvas, for every outlined source; None draws none.
    pub overflow: Option<Overflow>,
}

impl Default for PreviewLook {
    fn default() -> Self {
        Self {
            background: [0.0, 0.0, 0.0, 1.0],
            area: None,
            area_for: None,
            margin: 0.0,
            outlines: Vec::new(),
            marks: None,
            shade: 0.0,
            guides: None,
            overflow: None,
        }
    }
}

/// Which sources a preview outlines, by their ids in the scene collection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Marks {
    /// A scene whose shown sources all get an outline in their colour.
    pub scene: Option<String>,
    /// The scene and items picked: outlined with handles.
    pub picked: Option<(String, Vec<i64>)>,
    /// The outline colours, an item's by its id (straight alpha, the view's gamma).
    pub colors: Vec<[f32; 4]>,
}

/// What the preview shows: a scene, or two mid-transition.
#[derive(Debug, Clone, Copy)]
pub enum Shown<'a> {
    Scene(&'a Scene),
    Moving(&'a Transition),
}

impl Shown<'_> {
    fn size(&self) -> (u32, u32) {
        match self {
            Self::Scene(s) => (s.width, s.height),
            Self::Moving(t) => (t.from.width, t.from.height),
        }
    }
}

/// Coalesces rapid preview surface resize requests without blocking the caller thread.
#[derive(Debug, Default)]
pub struct PendingResize {
    /// Packed `(width << 32) | height` (both >= 1); `0` means no pending resize.
    packed: AtomicU64,
    /// True while a live window resize is active so `preview_loop` skips swapchain reconfigures.
    live_resizing: AtomicBool,
}

impl PendingResize {
    pub fn new() -> Self {
        Self::default()
    }

    fn pack(width: u32, height: u32) -> u64 {
        (u64::from(width.max(1)) << 32) | u64::from(height.max(1))
    }

    fn unpack(raw: u64) -> Option<(u32, u32)> {
        if raw == 0 {
            return None;
        }
        Some((((raw >> 32) as u32).max(1), (raw as u32).max(1)))
    }

    /// Queues the latest target dimensions in device pixels, overwriting any unconsumed request.
    pub fn request(&self, width: u32, height: u32) {
        self.packed.store(Self::pack(width, height), Ordering::Release);
    }

    /// Marks whether an OS live-resize drag is active.
    pub fn set_live_resizing(&self, active: bool) {
        self.live_resizing.store(active, Ordering::Release);
    }

    /// Whether an OS live-resize drag is active.
    pub fn is_live_resizing(&self) -> bool {
        self.live_resizing.load(Ordering::Acquire)
    }

    /// Returns and clears the latest queued dimensions if no live resize is active.
    pub fn take_if_ready(&self) -> Option<(u32, u32)> {
        self.take_after(|| {})
    }

    /// `take_if_ready`, with `between` run after the live check and before the swap (where a
    /// drag on another thread can begin; a test makes it do so).
    fn take_after(&self, between: impl FnOnce()) -> Option<(u32, u32)> {
        if self.is_live_resizing() {
            return None;
        }
        between();
        let raw = self.packed.swap(0, Ordering::AcqRel);
        // A drag may have begun between the check and the swap, and its first size with it:
        // that size stays queued until the drag settles (a newer one, if any, wins).
        if raw != 0 && self.is_live_resizing() {
            let _ = self.packed.compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire);
            return None;
        }
        Self::unpack(raw)
    }
}

/// A window surface the preview is presented to.
pub struct PreviewSurface {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pending: Arc<PendingResize>,
}

impl std::fmt::Debug for PreviewSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewSurface").field("size", &(self.config.width, self.config.height)).finish()
    }
}

impl PreviewSurface {
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Handle for queuing async surface resizes off the render thread.
    pub fn pending_resize(&self) -> Arc<PendingResize> {
        self.pending.clone()
    }

    /// Queues a resize without blocking on GPU submission or surface locks.
    pub fn request_resize(&self, width: u32, height: u32) {
        self.pending.request(width, height);
    }
}

/// The dimming layer's colour: the page's backdrop (as libobs's preview draws it).
const SHADE: [f32; 3] = [4.0 / 255.0, 5.0 / 255.0, 8.0 / 255.0];
/// A handle's side, in view pixels at least.
const HANDLE: f32 = 8.0;

/// One vertex: clip-space place, canvas place, colour, and whether it shows the canvas.
type Vertex = [f32; 9];

impl Compositor {
    /// A surface on a Mac view (`NSView *`), `width`×`height` device pixels.
    ///
    /// # Safety
    /// `ns_view` must be a live `NSView` that outlives the returned surface.
    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    pub unsafe fn preview_surface(
        &self,
        ns_view: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> Result<PreviewSurface, AvError> {
        use wgpu::rwh::{AppKitDisplayHandle, AppKitWindowHandle, RawDisplayHandle, RawWindowHandle};
        let view = std::ptr::NonNull::new(ns_view).ok_or_else(|| AvError::NoGpu("no view to draw in".into()))?;
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::AppKit(AppKitDisplayHandle::new())),
            raw_window_handle: RawWindowHandle::AppKit(AppKitWindowHandle::new(view)),
        };
        // SAFETY: the caller keeps the view alive for the surface's life.
        let surface = unsafe { self.instance.create_surface_unsafe(target) }
            .map_err(|e| AvError::NoGpu(format!("the preview's surface couldn't be made: {e}")))?;
        self.surface_from(surface, width, height)
    }

    /// A surface on a Windows window (`HWND`), `width`×`height` device pixels.
    ///
    /// # Safety
    /// `hwnd` must be a live window that outlives the returned surface.
    #[cfg(target_os = "windows")]
    #[allow(unsafe_code)]
    pub unsafe fn preview_surface(
        &self,
        hwnd: *mut std::ffi::c_void,
        width: u32,
        height: u32,
    ) -> Result<PreviewSurface, AvError> {
        use wgpu::rwh::{RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowsDisplayHandle};
        let h =
            std::num::NonZeroIsize::new(hwnd as isize).ok_or_else(|| AvError::NoGpu("no window to draw in".into()))?;
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
            raw_window_handle: RawWindowHandle::Win32(Win32WindowHandle::new(h)),
        };
        // SAFETY: the caller keeps the window alive for the surface's life.
        let surface = unsafe { self.instance.create_surface_unsafe(target) }
            .map_err(|e| AvError::NoGpu(format!("the preview's surface couldn't be made: {e}")))?;
        self.surface_from(surface, width, height)
    }

    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) fn surface_from(
        &self,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<PreviewSurface, AvError> {
        let caps = surface.get_capabilities(&self.adapter);
        // A plain (not sRGB) format: the canvas's bytes and the page's colours go through as they are.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| AvError::NoGpu("the preview's surface takes no format this card draws".into()))?;
        let (w, h) = self.clamped_surface_size(width, height);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: w,
            height: h,
            // Never waits for the screen: the engine's frame loop draws it and mustn't stall.
            present_mode: wgpu::PresentMode::AutoNoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: Vec::new(),
            color_space: Default::default(),
        };
        self.configure(&surface, &config);
        Ok(PreviewSurface { surface, config, pending: Arc::new(PendingResize::new()) })
    }

    fn clamped_surface_size(&self, width: u32, height: u32) -> (u32, u32) {
        let max_dim = self.device.limits().max_texture_dimension_2d.max(1);
        (width.clamp(1, max_dim), height.clamp(1, max_dim))
    }

    /// The view changed size (device pixels).
    pub fn resize_preview(&self, s: &mut PreviewSurface, width: u32, height: u32) {
        let _ = s.pending.packed.swap(0, Ordering::AcqRel);
        let (w, h) = self.clamped_surface_size(width, height);
        if (s.config.width, s.config.height) != (w, h) {
            (s.config.width, s.config.height) = (w, h);
            self.configure(&s.surface, &s.config);
        }
    }

    /// Applies any queued surface resize when no live resize is active.
    /// Returns `true` if the surface dimensions changed and `configure` ran.
    pub fn apply_pending_resize(&self, s: &mut PreviewSurface) -> bool {
        let Some((width, height)) = s.pending.take_if_ready() else {
            return false;
        };
        let (w, h) = self.clamped_surface_size(width, height);
        if (s.config.width, s.config.height) != (w, h) {
            (s.config.width, s.config.height) = (w, h);
            self.configure(&s.surface, &s.config);
            return true;
        }
        false
    }

    /// Draws `shown` and presents it in the surface. A frame the window can't take now (hidden,
    /// being resized) is skipped.
    pub fn present(&self, s: &mut PreviewSurface, shown: Shown<'_>, look: &PreviewLook) -> Result<(), AvError> {
        let got = s.surface.get_current_texture();
        let Some(frame) = self.finish_surface_acquire(s, got) else {
            return Ok(());
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        let canvas = self.canvas_of(&mut enc, shown)?;
        self.paint(&mut enc, &view, s.config.format, (s.config.width, s.config.height), &canvas, look);
        self.submit([enc.finish()]);
        self.present_surface(frame);
        // What finished drawing is let go now (Direct3D and Vulkan hold it until asked).
        let _ = self.device.poll(wgpu::PollType::Poll);
        Ok(())
    }

    fn finish_surface_acquire(
        &self,
        s: &mut PreviewSurface,
        got: wgpu::CurrentSurfaceTexture,
    ) -> Option<wgpu::SurfaceTexture> {
        match got {
            wgpu::CurrentSurfaceTexture::Success(f) => Some(f),
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                // Drop the acquired swapchain texture before reconfiguring; configuring while it is
                // still alive raises `PreviousOutputExists` and leaves the surface unconfigured.
                drop(f);
                self.configure(&s.surface, &s.config);
                None
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.configure(&s.surface, &s.config);
                None
            }
            _ => None,
        }
    }

    /// The same, read back: RGBA rows of a `width`×`height` view.
    pub fn render_preview(
        &self,
        width: u32,
        height: u32,
        shown: Shown<'_>,
        look: &PreviewLook,
    ) -> Result<Vec<u8>, AvError> {
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        let canvas = self.canvas_of(&mut enc, shown)?;
        self.paint(&mut enc, &view, format, (width, height), &canvas, look);
        self.read_back(enc, &texture, width, height)
    }

    fn canvas_of(&self, enc: &mut wgpu::CommandEncoder, shown: Shown<'_>) -> Result<Target, AvError> {
        let (w, h) = shown.size();
        crate::compose::check_size(w, h)?;
        let canvas = self.target(w, h, wgpu::TextureUsages::empty());
        match shown {
            Shown::Scene(scene) => {
                self.validate(&scene.items)?;
                self.clear(enc, &canvas);
                self.draw_items(enc, &canvas, &scene.items, &crate::Affine::IDENTITY)?;
            }
            Shown::Moving(tr) => self.transition_into(enc, &canvas, tr)?,
        }
        Ok(canvas)
    }

    fn painter(&self, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        let mut cache = self.preview_pipelines.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(format)
            .or_insert_with(|| {
                let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("preview"),
                    source: wgpu::ShaderSource::Wgsl(include_str!("preview.wgsl").into()),
                });
                let attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32];
                self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("preview"),
                    layout: None,
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("vs"),
                        compilation_options: Default::default(),
                        buffers: &[Some(wgpu::VertexBufferLayout {
                            array_stride: std::mem::size_of::<Vertex>() as u64,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &attrs,
                        })],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some("fs"),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                })
            })
            .clone()
    }

    fn paint(
        &self,
        enc: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        size: (u32, u32),
        canvas: &Target,
        look: &PreviewLook,
    ) {
        let verts = layout(size, (canvas.width, canvas.height), look);
        let bytes: Vec<u8> = verts.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview"),
            size: bytes.len().max(4) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buf, 0, &bytes);
        let pipeline = self.painter(format);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("preview"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&canvas.gamma) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.bilinear) },
            ],
        });
        let [r, g, b, a] = look.background.map(f64::from);
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("preview"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: r * a, g: g * a, b: b * a, a }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.set_vertex_buffer(0, buf.slice(..));
        pass.draw(0..verts.len() as u32, 0..1);
    }
}

/// Where the canvas goes in a view of `size` (view pixels: x, y, w, h), as libobs places it.
pub fn canvas_area(size: (u32, u32), canvas: (u32, u32), look: &PreviewLook) -> (f32, f32, f32, f32) {
    let (bw, bh) = (canvas.0.max(1) as f32, canvas.1.max(1) as f32);
    let area = match (look.area, look.area_for) {
        // A pixel or two is the page's rounding, not a resize.
        (Some(a), Some(was))
            if was.0 > 0 && was.1 > 0 && (was.0.abs_diff(size.0) > 2 || was.1.abs_diff(size.1) > 2) =>
        {
            Some(refit(a, was, size))
        }
        (a, _) => a,
    };
    match area {
        // Where the page put it, its shape kept.
        Some((x, y, w, h)) if w >= 1.0 && h >= 1.0 => {
            let scale = (w / bw).min(h / bh);
            (x.round(), y.round(), (bw * scale).round(), (bh * scale).round())
        }
        _ => {
            let (cx, cy) = (size.0 as f32, size.1 as f32);
            let edge = look.margin.max(0.0).min(cx / 4.0).min(cy / 4.0);
            let (aw, ah) = ((cx - 2.0 * edge).max(1.0), (cy - 2.0 * edge).max(1.0));
            let scale = (aw / bw).min(ah / bh);
            let (w, h) = ((bw * scale).round(), (bh * scale).round());
            (((cx - w) / 2.0).trunc(), ((cy - h) / 2.0).trunc(), w, h)
        }
    }
}

/// The page's box for the canvas (`area`, in a view of size `was`) moved into a view of `now`
/// as the page will lay it out once it catches up: a fitted canvas (it nearly fills the view on
/// one side) is fitted again with the same room round it; a zoomed one (75%, 50%, bigger than
/// the view) keeps its size. Either way it keeps where it sat from the view's middle.
fn refit(area: (f32, f32, f32, f32), was: (u32, u32), now: (u32, u32)) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = area;
    let (w0, h0) = (was.0 as f32, was.1 as f32);
    let (w1, h1) = (now.0 as f32, now.1 as f32);
    let room = (w0 - w).min(h0 - h);
    let fitted = w <= w0 && h <= h0 && room <= 0.12 * w0.min(h0) + 2.0;
    let (nw, nh) = if fitted {
        let s = ((w1 - room).max(1.0) / w).min((h1 - room).max(1.0) / h);
        (w * s, h * s)
    } else {
        (w, h)
    };
    let (ox, oy) = (x + w / 2.0 - w0 / 2.0, y + h / 2.0 - h0 / 2.0);
    (w1 / 2.0 + ox - nw / 2.0, h1 / 2.0 + oy - nh / 2.0, nw, nh)
}

/// A point in the view or on the canvas, and a filled four-cornered shape.
type Pt = (f32, f32);
type Quad = [Pt; 4];

/// The guides' colour, and their labels' text.
const GUIDE: [f32; 4] = [1.0, 0.27, 0.27, 1.0];
const GUIDE_TEXT: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

/// The guides round a box (canvas pixels): from each side's middle to the canvas edge it
/// faces, for the sides inside the canvas, with the length rounded (as OBS's spacing helpers).
fn guide_lines(corners: Quad, canvas: (u32, u32)) -> Vec<(Pt, Pt, i64)> {
    let (cw, ch) = (canvas.0 as f32, canvas.1 as f32);
    let xs = corners.map(|c| c.0);
    let ys = corners.map(|c| c.1);
    let (x0, x1) = (xs.iter().copied().fold(f32::MAX, f32::min), xs.iter().copied().fold(f32::MIN, f32::max));
    let (y0, y1) = (ys.iter().copied().fold(f32::MAX, f32::min), ys.iter().copied().fold(f32::MIN, f32::max));
    let (mx, my) = (((x0 + x1) / 2.0).clamp(0.0, cw), ((y0 + y1) / 2.0).clamp(0.0, ch));
    let mut out = Vec::new();
    let mut add = |from: Pt, to: Pt, d: f32| {
        if d >= 0.5 && d.is_finite() {
            out.push((from, to, d.round() as i64));
        }
    };
    if x0 < cw {
        add((0.0, my), (x0, my), x0);
    }
    if x1 > 0.0 {
        add((x1, my), (cw, my), cw - x1);
    }
    if y0 < ch {
        add((mx, 0.0), (mx, y0), y0);
    }
    if y1 > 0.0 {
        add((mx, y1), (mx, ch), ch - y1);
    }
    out
}

/// A 5x7 pixel font for the guides' labels (digits, a minus, `p` and `x`), rows top down,
/// two more below the line for `p`'s tail.
#[rustfmt::skip]
fn glyph(c: char) -> [u8; 9] {
    match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110, 0b00000, 0b00000],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110, 0b00000, 0b00000],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111, 0b00000, 0b00000],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110, 0b00000, 0b00000],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010, 0b00000, 0b00000],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110, 0b00000, 0b00000],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110, 0b00000, 0b00000],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00000, 0b00000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110, 0b00000, 0b00000],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100, 0b00000, 0b00000],
        '-' => [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000],
        'p' => [0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000],
        'x' => [0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b00000, 0b00000],
        _ => [0; 9],
    }
}

/// A label's quads centred on `at` (view pixels): a backing in the guides' colour, then its
/// text, `ppp` the view's pixels per point.
fn label(text: &str, at: Pt, ppp: f32) -> Vec<(Quad, [f32; 4])> {
    let dot = (1.5 * ppp).round().max(1.0);
    let pad = 3.0 * dot;
    let chars: Vec<char> = text.chars().collect();
    let tw = (chars.len() as f32 * 6.0 - 1.0).max(0.0) * dot;
    let th = 7.0 * dot;
    let (left, top) = ((at.0 - tw / 2.0).round(), (at.1 - th / 2.0).round());
    let r = |x: f32, y: f32, w: f32, h: f32| [(x, y), (x + w, y), (x + w, y + h), (x, y + h)];
    let mut out = vec![(r(left - pad, top - pad, tw + 2.0 * pad, th + 2.0 * pad), GUIDE)];
    for (i, c) in chars.iter().enumerate() {
        for (row, bits) in glyph(*c).iter().enumerate() {
            for col in 0..5 {
                if bits & (16 >> col) != 0 {
                    let x = left + (i as f32 * 6.0 + col as f32) * dot;
                    out.push((r(x, top + row as f32 * dot, dot, dot), GUIDE_TEXT));
                }
            }
        }
    }
    out
}

/// Every triangle the preview draws: the canvas, the outlines and handles, the shade.
fn layout(size: (u32, u32), canvas: (u32, u32), look: &PreviewLook) -> Vec<Vertex> {
    let (vw, vh) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let clip = |x: f32, y: f32| [x / vw * 2.0 - 1.0, 1.0 - y / vh * 2.0];
    let mut v: Vec<Vertex> = Vec::new();
    // `mode`: 0 a colour, 1 the canvas picture, 2 a colour only outside the canvas, and
    // 3 + the period: stripes of the colour only outside the canvas (see preview.wgsl).
    let mut quad = |p: [(f32, f32); 4], uv: [(f32, f32); 4], color: [f32; 4], mode: f32| {
        for i in [0, 1, 2, 0, 2, 3] {
            let [x, y] = clip(p[i].0, p[i].1);
            let [r, g, b, a] = color;
            v.push([x, y, uv[i].0, uv[i].1, r, g, b, a, mode]);
        }
    };
    let none = [(0.0, 0.0); 4];
    let (ax, ay, aw, ah) = canvas_area(size, canvas, look);
    quad(
        [(ax, ay), (ax + aw, ay), (ax + aw, ay + ah), (ax, ay + ah)],
        [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
        [0.0; 4],
        1.0,
    );
    let scale = aw / canvas.0.max(1) as f32;
    let place = |(x, y): (f32, f32)| (ax + x * scale, ay + y * scale);
    // What runs past the canvas, under the outlines: only boxes that leave it get a layer, and
    // the shader keeps it to the part outside (each corner says where it is on the canvas).
    if let Some(o) = look.overflow {
        let (cw, ch) = (canvas.0 as f32, canvas.1 as f32);
        for out in &look.outlines {
            let leaves = out.corners.iter().any(|&(x, y)| x < 0.0 || y < 0.0 || x > cw || y > ch);
            if !leaves {
                continue;
            }
            let c = out.corners.map(place);
            let uv = out.corners.map(|(x, y)| (x / cw, y / ch));
            quad(c, uv, o.a, 2.0);
            quad(c, uv, o.b, 3.0 + o.period.max(2.0));
        }
    }
    // Outlines (picked ones last, so they're on top), then their handles.
    let mut order: Vec<&Outline> = look.outlines.iter().filter(|o| !o.handles).collect();
    order.extend(look.outlines.iter().filter(|o| o.handles));
    for o in order {
        let c = o.corners.map(place);
        let half = o.width.max(0.5) / 2.0;
        for i in 0..4 {
            let (a, b) = (c[i], c[(i + 1) % 4]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len = (dx * dx + dy * dy).sqrt().max(1e-3);
            // Along the edge, a half width past each end so corners meet; across it, a width.
            let (ux, uy) = (dx / len * half, dy / len * half);
            let (nx, ny) = (-uy, ux);
            quad(
                [
                    (a.0 - ux + nx, a.1 - uy + ny),
                    (b.0 + ux + nx, b.1 + uy + ny),
                    (b.0 + ux - nx, b.1 + uy - ny),
                    (a.0 - ux - nx, a.1 - uy - ny),
                ],
                none,
                o.color,
                0.0,
            );
        }
        if o.handles {
            let side = HANDLE.max(o.width * 4.0) / 2.0;
            for i in 0..4 {
                let (a, b) = (c[i], c[(i + 1) % 4]);
                for (x, y) in [a, ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)] {
                    quad(
                        [(x - side, y - side), (x + side, y - side), (x + side, y + side), (x - side, y + side)],
                        none,
                        o.color,
                        0.0,
                    );
                }
            }
        }
    }
    if let Some(ppp) = look.guides
        && let Some(o) = look.outlines.iter().find(|o| o.handles)
    {
        for (from, to, n) in guide_lines(o.corners, canvas) {
            let (a, b) = (place(from), place(to));
            let half = ppp.max(0.5) / 2.0;
            let (x0, x1, y0, y1) = (a.0.min(b.0) - half, a.0.max(b.0) + half, a.1.min(b.1) - half, a.1.max(b.1) + half);
            quad([(x0, y0), (x1, y0), (x1, y1), (x0, y1)], none, GUIDE, 0.0);
            let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            for (r, color) in label(&format!("{n}px"), mid, ppp) {
                quad(r, none, color, 0.0);
            }
        }
    }
    if look.shade > 0.0 {
        let [r, g, b] = SHADE;
        quad([(0.0, 0.0), (vw, 0.0), (vw, vh), (0.0, vh)], none, [r, g, b, look.shade.min(1.0)], 0.0);
    }
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{Item, Picture, Placement};

    #[test]
    fn the_canvas_fits_inside_the_margin_or_where_the_page_says() {
        // The window grew before the page said where things went: a fitted canvas is fitted to
        // the view as it is now, from the middle; a zoomed one keeps its size.
        let page =
            PreviewLook { area: Some((16.0, 16.0, 768.0, 432.0)), area_for: Some((800, 464)), ..Default::default() };
        assert_eq!(canvas_area((800, 464), (1920, 1080), &page), (16.0, 16.0, 768.0, 432.0));
        let (x, y, w, h) = canvas_area((1000, 600), (1920, 1080), &page);
        // The same 32 pixels of room round it as the page left (1000 - 32 wide).
        assert_eq!((w, h), (968.0, 545.0), "fitted: {w}x{h}");
        assert!(w <= 1000.0 && h <= 600.0 && (x + w / 2.0 - 500.0).abs() <= 1.0 && (y + h / 2.0 - 300.0).abs() <= 1.0);
        let zoomed = PreviewLook {
            area: Some((-100.0, -50.0, 1000.0, 563.0)),
            area_for: Some((800, 464)),
            ..Default::default()
        };
        let (_, _, w, h) = canvas_area((1000, 600), (1920, 1080), &zoomed);
        assert_eq!((w, h), (1000.0, 563.0), "a zoomed canvas keeps its size");
        let fitted = PreviewLook { margin: 10.0, ..Default::default() };
        // 400x300 view, 16:9 canvas: 380 wide inside the margin, centred top to bottom.
        assert_eq!(canvas_area((400, 300), (1920, 1080), &fitted), (10.0, 43.0, 380.0, 214.0));
        let placed = PreviewLook { area: Some((5.0, 6.0, 192.0, 500.0)), ..Default::default() };
        assert_eq!(canvas_area((400, 300), (1920, 1080), &placed), (5.0, 6.0, 192.0, 108.0));
    }

    #[test]
    fn the_preview_shows_the_canvas_outlines_and_shade() {
        let Ok(gpu) = Compositor::new() else { return };
        // A red canvas, 160x90, in a 200x100 view on grey with a 10 px margin.
        let scene = Scene {
            width: 160,
            height: 90,
            items: vec![Item::picture(Picture::solid(160, 90, [255, 0, 0, 255]), Placement::default())],
        };
        let mut look = PreviewLook { background: [0.5, 0.5, 0.5, 1.0], margin: 10.0, ..Default::default() };
        look.outlines.push(Outline {
            corners: [(40.0, 20.0), (120.0, 20.0), (120.0, 70.0), (40.0, 70.0)],
            color: [0.0, 1.0, 0.0, 1.0],
            width: 2.0,
            handles: true,
        });
        let px = gpu.render_preview(200, 100, Shown::Scene(&scene), &look).unwrap_or_else(|e| panic!("{e}"));
        let at = |x: usize, y: usize| &px[(y * 200 + x) * 4..(y * 200 + x) * 4 + 3];
        // The canvas fills 142x80 at (29, 10): grey round it, red inside.
        let (ax, ay, aw, _) = canvas_area((200, 100), (160, 90), &look);
        assert_eq!((ax, ay, aw), (29.0, 10.0, 142.0));
        // Half grey: 127 or 128 by the card's rounding (NVIDIA on Windows gives 127).
        assert!(at(5, 50).iter().all(|c| (127..=128).contains(c)), "background {:?}", at(5, 50));
        assert_eq!(at(100, 50), [255, 0, 0], "canvas");
        // The outline's top edge: canvas y 20 → view y 10 + 20 * 142/160 ≈ 27.75.
        assert_eq!(at(100, 28), [0, 255, 0], "outline");
        // A handle at the top-left corner (canvas 40,20 → view ~64.5, 27.75).
        assert_eq!(at(61, 25), [0, 255, 0], "handle");
        // Guides: the top one runs down from the canvas edge (view y 10) to the box at its
        // middle (canvas x 80 → view 100), drawn over the canvas; its label sits half way.
        look.guides = Some(1.0);
        let guided = gpu.render_preview(200, 100, Shown::Scene(&scene), &look).unwrap_or_else(|e| panic!("{e}"));
        let g = &guided[(11 * 200 + 100) * 4..(11 * 200 + 100) * 4 + 3];
        assert!(g[0] == 255 && (60..=80).contains(&g[1]), "guide {g:?}");
        assert_eq!(at(100, 11), [255, 0, 0], "no guide unless asked");
        look.guides = None;
        look.shade = 1.0;
        let dark = gpu.render_preview(200, 100, Shown::Scene(&scene), &look).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(&dark[(50 * 200 + 100) * 4..(50 * 200 + 100) * 4 + 3], [4, 5, 8], "dimmed under a dialog");
    }

    #[test]
    fn a_canvas_shown_smaller_is_averaged_not_picked_from() {
        // Owner, 12 Nov: a 2560 canvas in a ~1100 view looked less clear than OBS's. Shown
        // 2.3 times smaller, one-pixel stripes must turn an even grey (every canvas pixel
        // counted), not a moiré of black and white picked by one sample each.
        let Ok(gpu) = Compositor::new() else { return };
        let (cw, ch) = (460u32, 260u32);
        let px: Vec<u8> = (0..cw * ch).flat_map(|i| if i % cw % 2 == 0 { [0, 0, 0, 255] } else { [255; 4] }).collect();
        let pic = Picture { width: cw, height: ch, rgba: px, premultiplied: true };
        let scene = Scene { width: cw, height: ch, items: vec![Item::picture(pic, Placement::default())] };
        let look = PreviewLook { background: [0.0, 0.0, 0.0, 1.0], margin: 0.0, ..Default::default() };
        let (vw, vh) = (200usize, 113usize);
        let out =
            gpu.render_preview(vw as u32, vh as u32, Shown::Scene(&scene), &look).unwrap_or_else(|e| panic!("{e}"));
        let row: Vec<u8> = (20..vw - 20).map(|x| out[((vh / 2) * vw + x) * 4]).collect();
        let (lo, hi) = (row.iter().min().copied().unwrap_or(0), row.iter().max().copied().unwrap_or(255));
        assert!(hi - lo < 40 && (100..=160).contains(&lo), "an even grey, {lo} to {hi}");
    }

    #[test]
    fn guides_run_from_each_side_to_the_canvas_edge() {
        // A 80x50 box at (40, 20) on a 160x90 canvas: 40 left, 40 right, 20 above, 20 below.
        let corners = [(40.0, 20.0), (120.0, 20.0), (120.0, 70.0), (40.0, 70.0)];
        let lines = guide_lines(corners, (160, 90));
        let lengths: Vec<i64> = lines.iter().map(|l| l.2).collect();
        assert_eq!(lengths, [40, 40, 20, 20]);
        assert_eq!(lines[0].0, (0.0, 45.0), "the left one from the canvas edge, at the box's middle");
        assert_eq!(lines[2].1, (80.0, 20.0), "the top one to the box's top");
        // Touching the left edge and hanging off the bottom: no guide on either side.
        let off = [(0.0, 50.0), (100.0, 50.0), (100.0, 120.0), (0.0, 120.0)];
        assert_eq!(guide_lines(off, (160, 90)).iter().map(|l| l.2).collect::<Vec<_>>(), [60, 50]);
        // Wholly off the canvas to the right: nothing to measure there.
        let gone = [(200.0, 10.0), (240.0, 10.0), (240.0, 30.0), (200.0, 30.0)];
        assert!(guide_lines(gone, (160, 90)).iter().all(|l| l.0.0 < 200.0 || l.1.0 <= 160.0));
    }

    #[test]
    fn a_guides_label_spells_its_length() {
        // "12px" at 1 pixel per point: dots of 2, four glyphs 5 wide with gaps of 1.
        let quads = label("12px", (100.0, 50.0), 1.0);
        let (back, color) = quads[0];
        assert_eq!(color, GUIDE);
        // 23 dots wide (4 * 6 - 1) of 2, plus 6 padding each side; 14 high plus 12.
        assert_eq!((back[1].0 - back[0].0, back[3].1 - back[0].1), (58.0, 26.0));
        let dots = quads[1..].iter().filter(|q| q.1 == GUIDE_TEXT).count();
        let lit = |c: char| glyph(c).iter().map(|b| b.count_ones() as usize).sum::<usize>();
        assert_eq!(dots, lit('1') + lit('2') + lit('p') + lit('x'));
        assert!(glyph('?').iter().all(|b| *b == 0), "a character it can't draw is a space");
    }

    fn box_at(x0: f32, x1: f32) -> Outline {
        Outline {
            corners: [(x0, 20.0), (x1, 20.0), (x1, 70.0), (x0, 70.0)],
            color: [0.0, 1.0, 0.0, 1.0],
            width: 2.0,
            handles: false,
        }
    }

    #[test]
    fn stripes_are_laid_only_over_boxes_that_leave_the_canvas() {
        let stripes = Some(Overflow { a: [0.0, 0.0, 1.0, 1.0], b: [1.0, 1.0, 0.0, 1.0], period: 18.0 });
        let mut look = PreviewLook { margin: 10.0, overflow: stripes, ..Default::default() };
        let picture = |l: &PreviewLook| layout((200, 100), (160, 90), l);
        let overflow_quads = |l: &PreviewLook| picture(l).iter().filter(|v| v[8] >= 2.0).count();
        // Inside the canvas: nothing to stripe.
        look.outlines = vec![box_at(40.0, 120.0)];
        assert_eq!(overflow_quads(&look), 0);
        // Hanging off the right edge: a fill and a stripe layer, a quad (6 vertices) each.
        look.outlines = vec![box_at(100.0, 220.0)];
        let v = picture(&look);
        assert_eq!(v.iter().filter(|v| v[8] == 2.0).count(), 6, "the fill under the stripes");
        assert_eq!(v.iter().filter(|v| v[8] == 21.0).count(), 6, "the stripes, 3 + the period");
        // The corners carry canvas positions, so the shader can tell which side of the edge.
        assert!(v.iter().filter(|v| v[8] >= 2.0).any(|v| v[2] > 1.0), "uv past the canvas's right edge");
        // Not asked for: as before.
        look.overflow = None;
        assert_eq!(overflow_quads(&look), 0);
    }

    #[test]
    fn the_part_of_a_box_outside_the_canvas_is_striped_in_the_two_colours() {
        let Ok(gpu) = Compositor::new() else { return };
        let scene = Scene {
            width: 160,
            height: 90,
            items: vec![Item::picture(Picture::solid(160, 90, [255, 0, 0, 255]), Placement::default())],
        };
        // A box from canvas x 100 to 220: 60 of it on the canvas, the rest past its right edge.
        let blue = [0.0, 0.0, 1.0, 1.0];
        let yellow = [1.0, 1.0, 0.0, 1.0];
        let look = PreviewLook {
            background: [0.5, 0.5, 0.5, 1.0],
            margin: 10.0,
            outlines: vec![box_at(100.0, 220.0)],
            overflow: Some(Overflow { a: blue, b: yellow, period: 18.0 }),
            ..Default::default()
        };
        let px = gpu.render_preview(200, 100, Shown::Scene(&scene), &look).unwrap_or_else(|e| panic!("{e}"));
        let at = |x: usize, y: usize| [px[(y * 200 + x) * 4], px[(y * 200 + x) * 4 + 1], px[(y * 200 + x) * 4 + 2]];
        // The canvas fills 142x80 at (29, 10): its right edge is view x 171. Inside it, under the box:
        // still the picture.
        assert_eq!(at(150, 50), [255, 0, 0], "the canvas shows through");
        // Past the edge, inside the box (to view x 200, the view's edge): both stripe colours.
        let row: Vec<[u8; 3]> = (174..200).map(|x| at(x, 50)).collect();
        assert!(row.contains(&[0, 0, 255]), "the first colour: {row:?}");
        assert!(row.contains(&[255, 255, 0]), "the second colour: {row:?}");
        assert!(row.iter().all(|p| *p == [0, 0, 255] || *p == [255, 255, 0]), "nothing else: {row:?}");
        // Past the edge but outside the box (below its bottom): the plain surround.
        let below = at(185, 95);
        assert!(below.iter().all(|c| (127..=128).contains(c)), "background {below:?}");
        // Without the stripes the same place is the surround.
        let plain = gpu
            .render_preview(200, 100, Shown::Scene(&scene), &PreviewLook { overflow: None, ..look.clone() })
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(plain[(50 * 200 + 185) * 4..(50 * 200 + 185) * 4 + 3].iter().all(|c| (127..=128).contains(c)));
    }

    #[test]
    fn the_preview_draws_guides_only_when_asked() {
        let o = Outline {
            corners: [(40.0, 20.0), (120.0, 20.0), (120.0, 70.0), (40.0, 70.0)],
            color: [0.0, 1.0, 0.0, 1.0],
            width: 2.0,
            handles: true,
        };
        let mut look = PreviewLook { margin: 10.0, outlines: vec![o], ..Default::default() };
        let without = layout((200, 100), (160, 90), &look).len();
        look.guides = Some(1.0);
        let with = layout((200, 100), (160, 90), &look);
        assert!(with.len() > without, "guides add quads");
        let red = with.iter().filter(|v| v[4..8] == GUIDE).count();
        assert!(red >= 6 * 8, "four lines and four label backings");
        // Without a picked source there's nothing to guide.
        look.outlines[0].handles = false;
        let plain = layout((200, 100), (160, 90), &PreviewLook { guides: None, ..look.clone() }).len();
        assert_eq!(layout((200, 100), (160, 90), &look).len(), plain);
    }

    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    pub(crate) fn test_metal_surface(
        gpu: &Compositor,
        width: u32,
        height: u32,
    ) -> (objc2::rc::Retained<objc2::runtime::AnyObject>, PreviewSurface) {
        use objc2::msg_send;
        use objc2::rc::Retained;
        use objc2::runtime::{AnyClass, AnyObject};
        use std::ffi::c_void;
        let Some(cls) = AnyClass::get(c"CAMetalLayer") else {
            panic!("CAMetalLayer class missing");
        };
        // SAFETY: `CAMetalLayer` is an Objective-C `CALayer` subclass; `+new` returns a retained instance.
        let layer: Retained<AnyObject> = unsafe { msg_send![cls, new] };
        let ptr: *mut c_void = Retained::as_ptr(&layer).cast_mut().cast();
        // SAFETY: `layer` is kept alive by the returned `Retained<AnyObject>`, which callers drop after the surface.
        let raw = unsafe { gpu.instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(ptr)) }
            .unwrap_or_else(|e| panic!("metal surface: {e}"));
        let surf = gpu.surface_from(raw, width, height).unwrap_or_else(|e| panic!("surface_from: {e}"));
        (layer, surf)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn oversized_preview_surface_clamps_without_validation_error() {
        let Ok(gpu) = Compositor::new() else { return };
        let (layer, mut surf) = test_metal_surface(&gpu, 100_000, 100_000);
        assert_eq!(
            gpu.uncaptured_errors.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "initial oversized configure must clamp instead of triggering TooLarge validation error"
        );
        gpu.resize_preview(&mut surf, 100_000, 100_000);
        assert_eq!(
            gpu.uncaptured_errors.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "resize_preview with oversized bounds must clamp instead of triggering validation error"
        );
        let scene = Scene { width: 320, height: 180, items: Vec::new() };
        gpu.resize_preview(&mut surf, 320, 180);
        let _ = gpu.present(&mut surf, Shown::Scene(&scene), &PreviewLook::default());
        assert_eq!(gpu.uncaptured_errors.load(std::sync::atomic::Ordering::Relaxed), 0);
        drop(surf);
        drop(layer);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn suboptimal_surface_reconfigures_cleanly_without_previous_output_exists() {
        let Ok(gpu) = Compositor::new() else { return };
        let (layer, mut surf) = test_metal_surface(&gpu, 320, 180);
        let wgpu::CurrentSurfaceTexture::Success(frame) = surf.surface.get_current_texture() else {
            return;
        };
        if let Some(left) = gpu.finish_surface_acquire(&mut surf, wgpu::CurrentSurfaceTexture::Suboptimal(frame)) {
            gpu.present_surface(left);
        }
        let scene = Scene { width: 320, height: 180, items: Vec::new() };
        let _ = gpu.present(&mut surf, Shown::Scene(&scene), &PreviewLook::default());
        assert_eq!(
            gpu.uncaptured_errors.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "Suboptimal surface must drop the acquired texture before reconfiguring so NotConfigured never follows"
        );
        drop(surf);
        drop(layer);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn concurrent_present_and_resize_surfaces_emit_zero_validation_errors() {
        let Ok(gpu) = Compositor::new() else { return };
        let (layer_a, mut surf_a) = test_metal_surface(&gpu, 320, 180);
        let (layer_b, mut surf_b) = test_metal_surface(&gpu, 320, 180);
        std::thread::scope(|s| {
            let gpu_b = &gpu;
            s.spawn(move || {
                for i in 0..30u32 {
                    let w = 300 + (i % 10) * 8;
                    gpu_b.resize_preview(&mut surf_b, w, 180);
                }
                drop(surf_b);
            });
            let scene = Scene { width: 320, height: 180, items: Vec::new() };
            for _ in 0..30 {
                let _ = gpu.present(&mut surf_a, Shown::Scene(&scene), &PreviewLook::default());
            }
            drop(surf_a);
        });
        assert_eq!(
            gpu.uncaptured_errors.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "concurrent present_surface and configure must not trigger GpuWaitTimeout"
        );
        drop(layer_b);
        drop(layer_a);
    }

    #[test]
    fn pending_resize_coalesces_100_writes_and_gates_on_live_resize() {
        let pending = PendingResize::new();
        assert_eq!(pending.take_if_ready(), None);

        pending.set_live_resizing(true);
        for i in 1..=100u32 {
            pending.request(640 + i, 360 + i);
        }
        assert_eq!(pending.take_if_ready(), None, "must not consume while live resizing");

        pending.set_live_resizing(false);
        assert_eq!(pending.take_if_ready(), Some((740, 460)));
        assert_eq!(pending.take_if_ready(), None, "consumed once");
    }

    #[test]
    fn a_drag_that_begins_while_a_size_is_taken_keeps_its_size_queued() {
        let pending = PendingResize::new();
        pending.request(500, 300);
        // The drag starts (and queues its first size) after the check, before the swap.
        let taken = pending.take_after(|| {
            pending.set_live_resizing(true);
            pending.request(510, 310);
        });
        assert_eq!(taken, None, "no reconfigure mid-drag");
        assert_eq!(pending.take_if_ready(), None, "still held while the drag lasts");
        pending.set_live_resizing(false);
        assert_eq!(pending.take_if_ready(), Some((510, 310)), "the drag's size, once it settles");
    }

    #[test]
    fn pending_resize_clamps_zero_dimensions_without_dropping_sentinel() {
        let pending = PendingResize::new();
        pending.request(0, 0);
        assert_eq!(pending.take_if_ready(), Some((1, 1)));
        assert_eq!(pending.take_if_ready(), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pending_resize_coalesces_100_writes_into_one_metal_reconfigure() {
        let Ok(gpu) = Compositor::new() else { return };
        let (layer, mut surf) = test_metal_surface(&gpu, 320, 180);
        let pending = surf.pending_resize();
        pending.set_live_resizing(true);
        for i in 0..100u32 {
            pending.request(400 + i, 220 + i);
        }
        assert!(!gpu.apply_pending_resize(&mut surf), "must defer reconfigure while live resizing");
        assert_eq!(surf.size(), (320, 180));

        pending.set_live_resizing(false);
        assert!(gpu.apply_pending_resize(&mut surf), "single reconfigure on end");
        assert_eq!(surf.size(), (499, 319));
        assert!(!gpu.apply_pending_resize(&mut surf), "no redundant second reconfigure");
        assert_eq!(gpu.uncaptured_errors.load(Ordering::Relaxed), 0);
        drop(surf);
        drop(layer);
    }

    #[test]
    fn wgpu_validation_error_collapses_to_single_diagnostic_line() {
        let err = wgpu::Error::Validation {
            source: Box::new(std::io::Error::other("surface not configured")),
            description: "Validation Error\n\nCaused by:\n  Surface is not configured for presentation\n".into(),
        };
        let line = crate::compose::format_wgpu_error(&err);
        assert!(!line.contains('\n'), "{line:?}");
        assert!(line.contains("Caused by: Surface is not configured"), "{line:?}");
    }
}
