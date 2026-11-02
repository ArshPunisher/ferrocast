//! The Studio's preview drawn by our engine, as libobs's `draw_main` draws it: the canvas
//! placed in the view (where the page says, or fitted inside a margin left for handles), the
//! sources' outlines over it (the picked ones with handles, the rest in their colours, off the
//! canvas too), and a dimming layer while a dialog is open. Into a window's surface
//! (`present`), or read back (`render_preview`) for tests and screenshots.

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

/// How the preview looks round and over the picture.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewLook {
    /// The view's colour round the canvas (straight alpha, the view's gamma).
    pub background: [f32; 4],
    /// Where the canvas sits in the view (view pixels: x, y, w, h) when the page says; its
    /// shape is kept inside that box. None fits it inside `margin`.
    pub area: Option<(f32, f32, f32, f32)>,
    /// Room left round a fitted canvas (view pixels), for handles at its edge.
    pub margin: f32,
    pub outlines: Vec<Outline>,
    /// A dark layer over everything, 0 (none) to 1, as a dialog's backdrop dims the page.
    pub shade: f32,
}

impl Default for PreviewLook {
    fn default() -> Self {
        Self { background: [0.0, 0.0, 0.0, 1.0], area: None, margin: 0.0, outlines: Vec::new(), shade: 0.0 }
    }
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

/// A window surface the preview is presented to.
pub struct PreviewSurface {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
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

    fn surface_from(
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
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            // Never waits for the screen: the engine's frame loop draws it and mustn't stall.
            present_mode: wgpu::PresentMode::AutoNoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: Vec::new(),
            color_space: Default::default(),
        };
        surface.configure(&self.device, &config);
        Ok(PreviewSurface { surface, config })
    }

    /// The view changed size (device pixels).
    pub fn resize_preview(&self, s: &mut PreviewSurface, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if (s.config.width, s.config.height) != (w, h) {
            (s.config.width, s.config.height) = (w, h);
            s.surface.configure(&self.device, &s.config);
        }
    }

    /// Draws `shown` and presents it in the surface. A frame the window can't take now (hidden,
    /// being resized) is skipped.
    pub fn present(&self, s: &mut PreviewSurface, shown: Shown<'_>, look: &PreviewLook) -> Result<(), AvError> {
        let frame = match s.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                s.surface.configure(&self.device, &s.config);
                f
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                s.surface.configure(&self.device, &s.config);
                return Ok(());
            }
            _ => return Ok(()),
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        let canvas = self.canvas_of(&mut enc, shown)?;
        self.paint(&mut enc, &view, s.config.format, (s.config.width, s.config.height), &canvas, look);
        self.queue.submit([enc.finish()]);
        self.queue.present(frame);
        Ok(())
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
    match look.area {
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

/// Every triangle the preview draws: the canvas, the outlines and handles, the shade.
fn layout(size: (u32, u32), canvas: (u32, u32), look: &PreviewLook) -> Vec<Vertex> {
    let (vw, vh) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let clip = |x: f32, y: f32| [x / vw * 2.0 - 1.0, 1.0 - y / vh * 2.0];
    let mut v: Vec<Vertex> = Vec::new();
    let mut quad = |p: [(f32, f32); 4], uv: [(f32, f32); 4], color: [f32; 4], picture: bool| {
        for i in [0, 1, 2, 0, 2, 3] {
            let [x, y] = clip(p[i].0, p[i].1);
            let [r, g, b, a] = color;
            v.push([x, y, uv[i].0, uv[i].1, r, g, b, a, if picture { 1.0 } else { 0.0 }]);
        }
    };
    let none = [(0.0, 0.0); 4];
    let (ax, ay, aw, ah) = canvas_area(size, canvas, look);
    quad(
        [(ax, ay), (ax + aw, ay), (ax + aw, ay + ah), (ax, ay + ah)],
        [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)],
        [0.0; 4],
        true,
    );
    let scale = aw / canvas.0.max(1) as f32;
    let place = |(x, y): (f32, f32)| (ax + x * scale, ay + y * scale);
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
                false,
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
                        false,
                    );
                }
            }
        }
    }
    if look.shade > 0.0 {
        let [r, g, b] = SHADE;
        quad([(0.0, 0.0), (vw, 0.0), (vw, vh), (0.0, vh)], none, [r, g, b, look.shade.min(1.0)], false);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Item, Picture, Placement};

    #[test]
    fn the_canvas_fits_inside_the_margin_or_where_the_page_says() {
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
        assert_eq!(at(5, 50), [128, 128, 128], "background");
        assert_eq!(at(100, 50), [255, 0, 0], "canvas");
        // The outline's top edge: canvas y 20 → view y 10 + 20 * 142/160 ≈ 27.75.
        assert_eq!(at(100, 28), [0, 255, 0], "outline");
        // A handle at the top-left corner (canvas 40,20 → view ~64.5, 27.75).
        assert_eq!(at(61, 25), [0, 255, 0], "handle");
        look.shade = 1.0;
        let dark = gpu.render_preview(200, 100, Shown::Scene(&scene), &look).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(&dark[(50 * 200 + 100) * 4..(50 * 200 + 100) * 4 + 3], [4, 5, 8], "dimmed under a dialog");
    }
}
