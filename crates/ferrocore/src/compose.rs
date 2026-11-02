//! The compositor: libobs's scene drawing (obs-scene.c `render_item`), step for step, on wgpu.
//!
//! An item is drawn straight onto its scene, unless libobs would first draw it into a texture of
//! its cropped size (a "texrender"): when it's cropped, has a scale filter, isn't blended
//! normally, has sRGB blending off, or is a nested scene. That texture is then drawn with the
//! scale filter's shader and the blend mode. Following the same path keeps every rounding the
//! same, so both engines' pictures match pixel for pixel.

use std::collections::HashMap;
use std::sync::{Mutex, mpsc};

use crate::transform::{Affine, cropped, draw_transform, output_scale};
use crate::{
    AvError, Blend, Content, Effect, Filter, Item, MaskKind, Nv12, Picture, Scene, Transition, TransitionKind,
    cubic_ease_in_out, smoothstep,
};

/// Pictures and canvases are 8-bit RGBA holding sRGB values. Seen through `LINEAR` views the GPU
/// decodes and encodes them, so items mix in linear light (libobs's way since OBS 28); seen
/// through `GAMMA` views, as stored (libobs's way for sources that aren't sRGB-aware).
const GAMMA: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const LINEAR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const MAX: u32 = 16_384;
/// libobs's `EPSILON` for "scaled at all".
const EPSILON: f32 = 0.0001;

/// The fragment shaders: libobs's effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Shader {
    Default,
    Bicubic,
    Lanczos,
    Area,
    AreaUpscale,
    Lowres,
}

impl Shader {
    fn entry(self) -> &'static str {
        match self {
            Self::Default => "fs_default",
            Self::Bicubic => "fs_bicubic",
            Self::Lanczos => "fs_lanczos",
            Self::Area => "fs_area",
            Self::AreaUpscale => "fs_area_upscale",
            Self::Lowres => "fs_lowres",
        }
    }
}

/// libobs's `obs_blend_mode_params`. libobs's Metal renderer applies the operation to colour
/// only (alpha always adds); its D3D11 and OpenGL renderers to both. `metal` picks which to match.
fn blend_state(b: Blend, metal: bool) -> wgpu::BlendState {
    use wgpu::{BlendComponent as C, BlendFactor as F, BlendOperation as O};
    let c = |src, dst, op| C { src_factor: src, dst_factor: dst, operation: op };
    let same = |x: C| wgpu::BlendState { color: x, alpha: if metal { C { operation: O::Add, ..x } } else { x } };
    match b {
        Blend::Normal => same(c(F::One, F::OneMinusSrcAlpha, O::Add)),
        Blend::Additive => same(c(F::One, F::One, O::Add)),
        Blend::Subtract => same(c(F::One, F::One, O::ReverseSubtract)),
        Blend::Screen => {
            wgpu::BlendState { color: c(F::One, F::OneMinusSrc, O::Add), alpha: c(F::One, F::OneMinusSrcAlpha, O::Add) }
        }
        Blend::Multiply => wgpu::BlendState {
            color: c(F::Dst, F::OneMinusSrcAlpha, O::Add),
            alpha: c(F::DstAlpha, F::OneMinusSrcAlpha, O::Add),
        },
        Blend::Lighten => same(c(F::One, F::One, O::Max)),
        Blend::Darken => same(c(F::One, F::One, O::Min)),
    }
}

/// The compositor: a graphics card and what drawing needs on it.
#[derive(Debug)]
pub struct Compositor {
    /// The last error the graphics card reported on its own (out of memory, a device lost), in
    /// words: kept so the engine says so instead of only logging it.
    pub(crate) gpu_error: std::sync::Arc<Mutex<Option<String>>>,
    /// Kept for window surfaces (the preview): they must come from the device's own instance.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) instance: wgpu::Instance,
    pub(crate) adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    module: wgpu::ShaderModule,
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    pipelines: Mutex<HashMap<(Shader, Blend, bool), wgpu::RenderPipeline>>,
    /// Alpha blends as libobs's Metal renderer does (see `blend_state`).
    metal_alpha: bool,
    pub(crate) bilinear: wgpu::Sampler,
    point: wgpu::Sampler,
    /// Transitions: their shaders, layout and pipelines (by entry point and target view).
    transition_module: wgpu::ShaderModule,
    transition_layout: wgpu::BindGroupLayout,
    transition_pipeline_layout: wgpu::PipelineLayout,
    transition_pipelines: Mutex<HashMap<(&'static str, bool), wgpu::RenderPipeline>>,
    /// Ferrocast's filters: their shaders and pipelines (by entry point).
    effect_module: wgpu::ShaderModule,
    /// Filters read the picture before them and, for Image Mask/Blend, a second one.
    effect_layout: wgpu::BindGroupLayout,
    effect_pipeline_layout: wgpu::PipelineLayout,
    /// What the second picture and the cube are when a filter has none.
    blank: wgpu::Texture,
    blank_cube: wgpu::Texture,
    /// LUTs on the card, by the table they came from (dropped with it).
    luts: Mutex<HashMap<usize, (std::sync::Weak<crate::lut::Lut>, wgpu::Texture)>>,
    effect_pipelines: Mutex<HashMap<(&'static str, bool), wgpu::RenderPipeline>>,
    /// Shader effects of one's own, by their code's key: None when the card refused it.
    custom_pipelines: Mutex<HashMap<u64, Option<wgpu::RenderPipeline>>>,
    /// Pictures kept on the card between frames, by the caller's id: texture, width, height.
    uploads: Mutex<HashMap<u64, (wgpu::Texture, u32, u32)>>,
    /// NV12 out: the Y and UV pipelines.
    nv12: (wgpu::RenderPipeline, wgpu::RenderPipeline),
    /// The preview's painter, by the window surface's format.
    pub(crate) preview_pipelines: Mutex<HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>>,
}

/// What an item draws: a picture, a texture made on the GPU (filtered), or other items.
enum Src<'a> {
    Pic(&'a Picture),
    /// An uploaded picture: its id, and whether it's premultiplied.
    Up(u64, bool),
    Tex(Box<Target>),
    Items(&'a [Item]),
}

/// A texture drawn into: its two views and size.
pub(crate) struct Target {
    pub(crate) texture: wgpu::Texture,
    pub(crate) linear: wgpu::TextureView,
    pub(crate) gamma: wgpu::TextureView,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// One quad to draw: where it goes and how.
struct Quad<'a> {
    corners: [(f32, f32); 4],
    view: &'a wgpu::TextureView,
    size: (u32, u32),
    shader: Shader,
    blend: Blend,
    linear: bool,
    straight: bool,
    point: bool,
    /// The item's opacity (1 for every draw but an item's last).
    opacity: f32,
}

impl Compositor {
    /// Opens the system's graphics card, headless (no window needed).
    pub fn new() -> Result<Self, AvError> {
        pollster::block_on(Self::open())
    }

    async fn open() -> Result<Self, AvError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .map_err(|e| AvError::NoGpu(e.to_string()))?;
        // Every texture is seen two ways (linear and as stored), which basic OpenGL can't do.
        if !adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
            return Err(AvError::NoGpu(format!(
                "{} can't show a picture both linear and as stored; a Metal, D3D12 or Vulkan graphics card is needed",
                adapter.get_info().name
            )));
        }
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor { label: Some("ferrocore"), ..Default::default() })
            .await
            .map_err(|e| AvError::NoGpu(e.to_string()))?;
        // A graphics card error is logged, never the end of the app: the frame it hit is lost,
        // the engine draws the next (wgpu would otherwise panic on the thread it happened on).
        let gpu_error = std::sync::Arc::new(Mutex::new(None::<String>));
        {
            let gpu_error = gpu_error.clone();
            device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
                static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::error!(times = n, "the graphics card refused something: {e}");
                }
                // Out of memory (or worse) is said where the Studio shows the engine's errors.
                if let wgpu::Error::OutOfMemory { .. } | wgpu::Error::Internal { .. } = e {
                    *gpu_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                        "The graphics card had a problem ({e}). Stop and start the Studio; if it keeps happening, \
                         use fewer or smaller sources."
                    ));
                }
            }));
        }
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("draw"),
            source: wgpu::ShaderSource::Wgsl(include_str!("draw.wgsl").into()),
        });
        let entry = |binding, visibility, ty| wgpu::BindGroupLayoutEntry { binding, visibility, ty, count: None };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("item"),
            entries: &[
                entry(
                    0,
                    wgpu::ShaderStages::VERTEX_FRAGMENT,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(
                    1,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(2, wgpu::ShaderStages::FRAGMENT, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("item"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let sampler = |filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("pick"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            })
        };
        let bilinear = sampler(wgpu::FilterMode::Linear);
        let point = sampler(wgpu::FilterMode::Nearest);
        let transition_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("transition"),
            source: wgpu::ShaderSource::Wgsl(include_str!("transition.wgsl").into()),
        });
        let tex = |binding| {
            entry(
                binding,
                wgpu::ShaderStages::FRAGMENT,
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
            )
        };
        let transition_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("transition"),
            entries: &[
                entry(
                    0,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                tex(1),
                tex(2),
                tex(3),
                entry(4, wgpu::ShaderStages::FRAGMENT, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            ],
        });
        let transition_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("transition"),
            bind_group_layouts: &[Some(&transition_layout)],
            ..Default::default()
        });
        let effect_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("effect"),
            entries: &[
                entry(
                    0,
                    wgpu::ShaderStages::VERTEX_FRAGMENT,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                tex(1),
                entry(2, wgpu::ShaderStages::FRAGMENT, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                tex(3),
                entry(
                    4,
                    wgpu::ShaderStages::FRAGMENT,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                ),
            ],
        });
        let effect_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("effect"),
            bind_group_layouts: &[Some(&effect_layout)],
            ..Default::default()
        });
        let blank = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blank"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: GAMMA,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[LINEAR],
        });
        let blank_cube = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blank cube"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: GAMMA,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let effect_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("effects"),
            source: wgpu::ShaderSource::Wgsl(include_str!("effects.wgsl").into()),
        });
        let conv = |entry: &str, format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_full"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let nv12 = (conv("fs_y", wgpu::TextureFormat::R8Unorm), conv("fs_uv", wgpu::TextureFormat::Rg8Unorm));
        Ok(Self {
            gpu_error,
            instance,
            adapter,
            preview_pipelines: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            nv12,
            effect_module,
            effect_layout,
            effect_pipeline_layout,
            blank,
            blank_cube,
            luts: Mutex::new(HashMap::new()),
            effect_pipelines: Mutex::new(HashMap::new()),
            custom_pipelines: Mutex::new(HashMap::new()),
            transition_module,
            transition_layout,
            transition_pipeline_layout,
            transition_pipelines: Mutex::new(HashMap::new()),
            device,
            queue,
            module,
            layout,
            pipeline_layout,
            pipelines: Mutex::new(HashMap::new()),
            metal_alpha: cfg!(target_os = "macos"),
            bilinear,
            point,
        })
    }

    /// Which libobs renderer's alpha blending to match: Metal's (the Mac's default here) or
    /// D3D11's and OpenGL's. They differ only in a picture's alpha under Subtract, Lighten and
    /// Darken, which shows when the scene is drawn inside another.
    pub fn match_libobs_metal(&mut self, metal: bool) {
        if self.metal_alpha != metal {
            self.metal_alpha = metal;
            self.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    fn pipeline(&self, shader: Shader, blend: Blend, linear: bool) -> wgpu::RenderPipeline {
        let key = (shader, blend, linear);
        let mut cache = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(key)
            .or_insert_with(|| {
                self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(shader.entry()),
                    layout: Some(&self.pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &self.module,
                        entry_point: Some("vs"),
                        buffers: &[Some(wgpu::VertexBufferLayout {
                            array_stride: 16,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                        })],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &self.module,
                        entry_point: Some(shader.entry()),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: if linear { LINEAR } else { GAMMA },
                            blend: Some(blend_state(blend, self.metal_alpha)),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleStrip,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                })
            })
            .clone()
    }

    /// Draws a scene on a clear canvas → its RGBA rows (premultiplied, sRGB), as libobs's
    /// screenshot of a scene gives them.
    pub fn render(&self, scene: &Scene) -> Result<Vec<u8>, AvError> {
        check_size(scene.width, scene.height)?;
        self.validate(&scene.items)?;
        let mut enc = self.device.create_command_encoder(&Default::default());
        let target = self.target(scene.width, scene.height, wgpu::TextureUsages::COPY_SRC);
        self.clear(&mut enc, &target);
        self.draw_items(&mut enc, &target, &scene.items, &Affine::IDENTITY)?;
        self.read_back(enc, &target.texture, scene.width, scene.height)
    }

    /// Draws two scenes mid-transition, as libobs's transition plugins do → RGBA rows.
    pub fn render_transition(&self, tr: &Transition) -> Result<Vec<u8>, AvError> {
        let (w, h) = (tr.from.width, tr.from.height);
        check_size(w, h)?;
        let mut enc = self.device.create_command_encoder(&Default::default());
        let canvas = self.target(w, h, wgpu::TextureUsages::COPY_SRC);
        self.transition_into(&mut enc, &canvas, tr)?;
        self.read_back(enc, &canvas.texture, w, h)
    }

    /// The same → NV12, for the encoders.
    pub fn render_transition_nv12(&self, tr: &Transition) -> Result<Nv12, AvError> {
        let (w, h) = (tr.from.width, tr.from.height);
        check_size(w, h)?;
        if !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            return Err(AvError::BadSize(w, h));
        }
        let mut enc = self.device.create_command_encoder(&Default::default());
        let canvas = self.target(w, h, wgpu::TextureUsages::empty());
        self.transition_into(&mut enc, &canvas, tr)?;
        self.nv12_out(enc, &canvas)
    }

    pub(crate) fn transition_into(
        &self,
        enc: &mut wgpu::CommandEncoder,
        canvas: &Target,
        tr: &Transition,
    ) -> Result<(), AvError> {
        let (w, h) = (tr.from.width, tr.from.height);
        check_size(w, h)?;
        self.validate(&tr.from.items)?;
        self.validate(&tr.to.items)?;
        if let TransitionKind::LumaWipe { luma, .. } = &tr.kind {
            self.validate(&[Item::picture(luma.clone(), Default::default())])?;
        }
        self.clear(enc, canvas);
        let t = tr.t.clamp(0.0, 1.0);
        // Each scene drawn on its own at the canvas's size (libobs's render_child; a scene of
        // another size is placed top left, as its default "shrink only" scaling leaves it).
        let draw = |enc: &mut wgpu::CommandEncoder, s: &Scene| -> Result<Target, AvError> {
            let target = self.target(w, h, wgpu::TextureUsages::empty());
            self.clear(enc, &target);
            self.draw_items(enc, &target, &s.items, &Affine::IDENTITY)?;
            Ok(target)
        };
        if tr.kind == TransitionKind::Cut || t <= 0.0 || t >= 1.0 {
            let s = if t >= 1.0 || (tr.kind == TransitionKind::Cut && t > 0.0) { &tr.to } else { &tr.from };
            self.draw_items(enc, canvas, &s.items, &Affine::IDENTITY)?;
            return Ok(());
        }
        let a = draw(enc, &tr.from)?;
        let b = draw(enc, &tr.to)?;
        let luma_tex;
        // (entry point, textures and target seen linearly, t, softness, invert, a_dir, b_dir,
        // colour, which textures as a and b, the luma picture)
        let mut u = [0f32; 12];
        let (entry, linear, first, second, luma): (&'static str, bool, &Target, &Target, Option<&wgpu::Texture>) =
            match &tr.kind {
                TransitionKind::Fade | TransitionKind::Move => {
                    u[0] = t;
                    ("fs_fade", false, &a, &b, None)
                }
                TransitionKind::FadeToColor { rgb, switch_point } => {
                    let sp = switch_point.clamp(0.0, 1.0);
                    let swp = if t < sp { smoothstep(0.0, sp, t) } else { 1.0 - smoothstep(sp, 1.0, t) };
                    u[0] = swp;
                    u[8..12].copy_from_slice(&[
                        f32::from(rgb[0]) / 255.0,
                        f32::from(rgb[1]) / 255.0,
                        f32::from(rgb[2]) / 255.0,
                        1.0,
                    ]);
                    ("fs_fade_to_color", false, if t < sp { &a } else { &b }, &b, None)
                }
                TransitionKind::Slide(dir) => {
                    let e = cubic_ease_in_out(t);
                    let (dx, dy) = dir.vector();
                    u[4..8].copy_from_slice(&[dx * e, dy * e, dx * (1.0 - e), dy * (1.0 - e)]);
                    ("fs_slide", true, &a, &b, None)
                }
                TransitionKind::Swipe { direction, swipe_in } => {
                    let e = cubic_ease_in_out(t);
                    let (mut dx, mut dy) = direction.vector();
                    if *swipe_in {
                        (dx, dy) = (-dx, -dy);
                    }
                    let k = if *swipe_in { 1.0 - e } else { e };
                    u[4..6].copy_from_slice(&[dx * k, dy * k]);
                    if *swipe_in { ("fs_swipe", true, &b, &a, None) } else { ("fs_swipe", true, &a, &b, None) }
                }
                TransitionKind::LumaWipe { luma, invert, softness } => {
                    u[0] = t;
                    u[1] = *softness;
                    u[2] = if *invert { 1.0 } else { 0.0 };
                    luma_tex = self.new_picture(luma);
                    ("fs_luma", true, &a, &b, Some(&luma_tex))
                }
                // Drawn above as one scene; here only for completeness.
                TransitionKind::Cut => {
                    u[0] = 1.0;
                    ("fs_fade", false, &a, &b, None)
                }
            };
        let ubuf = self.buffer(&floats(&u), wgpu::BufferUsages::UNIFORM);
        fn pick(t: &Target, linear: bool) -> &wgpu::TextureView {
            if linear { &t.linear } else { &t.gamma }
        }
        let luma_view = luma.map(|l| view(l, GAMMA));
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("transition"),
            layout: &self.transition_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(pick(first, linear)) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(pick(second, linear)) },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(luma_view.as_ref().unwrap_or(pick(first, linear))),
                },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(&self.bilinear) },
            ],
        });
        let pipeline = self.transition_pipeline(entry, linear);
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("transition"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: pick(canvas, linear),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..4, 0..1);
        }
        Ok(())
    }

    fn transition_pipeline(&self, entry: &'static str, linear: bool) -> wgpu::RenderPipeline {
        let mut cache = self.transition_pipelines.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry((entry, linear))
            .or_insert_with(|| {
                self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&self.transition_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &self.transition_module,
                        entry_point: Some("vs"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &self.transition_module,
                        entry_point: Some(entry),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: if linear { LINEAR } else { GAMMA },
                            // libobs draws transitions with blending one, inverse source alpha.
                            blend: Some(blend_state(Blend::Normal, self.metal_alpha)),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleStrip,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                })
            })
            .clone()
    }

    pub(crate) fn target(&self, width: u32, height: u32, extra: wgpu::TextureUsages) -> Target {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("canvas"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: GAMMA,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | extra,
            view_formats: &[LINEAR],
        });
        Target { linear: view(&texture, LINEAR), gamma: view(&texture, GAMMA), texture, width, height }
    }

    pub(crate) fn clear(&self, enc: &mut wgpu::CommandEncoder, t: &Target) {
        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &t.gamma,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
    }

    pub(crate) fn draw_items(
        &self,
        enc: &mut wgpu::CommandEncoder,
        target: &Target,
        items: &[Item],
        parent: &Affine,
    ) -> Result<(), AvError> {
        for item in items {
            self.draw_item(enc, target, item, parent)?;
        }
        Ok(())
    }

    /// libobs's `render_item`.
    fn draw_item(
        &self,
        enc: &mut wgpu::CommandEncoder,
        target: &Target,
        item: &Item,
        parent: &Affine,
    ) -> Result<(), AvError> {
        let (src, w, h, linear) = if item.effects.is_empty() {
            match &item.content {
                Content::Picture(p) => (Src::Pic(p), p.width, p.height, item.linear),
                Content::Uploaded { id, width, height, premultiplied } => {
                    (Src::Up(*id, *premultiplied), *width, *height, item.linear)
                }
                Content::Scene(s) => (Src::Items(&s.items), s.width, s.height, true),
                Content::Group { width, height, items } => (Src::Items(items), *width, *height, true),
            }
        } else {
            // Ferrocast's filters run in gamma space (they aren't sRGB-aware), OBS's own in linear
            // light: the result mixes as the last one ran.
            let t = self.apply_effects(enc, item)?;
            let (w, h) = (t.width, t.height);
            let linear = item.effects.last().is_some_and(Effect::linear);
            (Src::Tex(Box::new(t)), w, h, linear)
        };
        if w == 0 || h == 0 {
            return Ok(());
        }
        let p = &item.placement;
        let (cw, ch) = cropped(w, h, p.crop);
        if cw == 0 || ch == 0 {
            return Ok(());
        }
        let m = draw_transform(p, w, h).then(parent);
        let texrender = p.crop.iter().any(|c| *c > 0)
            || item.filter != Filter::Disable
            || item.srgb_off
            || item.blend != Blend::Normal
            || item.opacity < 1.0
            || matches!(item.content, Content::Scene(_));

        if !texrender {
            return self.draw_src(enc, target, &src, &m, w, h, linear);
        }

        // Drawn into a texture of its cropped size first, moved up and left by the crop.
        let tr = self.target(cw, ch, wgpu::TextureUsages::empty());
        self.clear(enc, &tr);
        let shift = Affine::translate(-(p.crop[0] as f32), -(p.crop[1] as f32));
        self.draw_src(enc, &tr, &src, &shift, w, h, linear)?;

        // Then drawn with the scale filter and the blend mode (render_item_texture).
        let (sx, sy) = output_scale(p, w, h);
        let scaled = (sx - 1.0).abs() > EPSILON || (sy - 1.0).abs() > EPSILON;
        let shader = match item.filter {
            Filter::Bicubic | Filter::Lanczos | Filter::Area if scaled && (sx < 0.5 || sy < 0.5) => Shader::Lowres,
            Filter::Bicubic if scaled => Shader::Bicubic,
            Filter::Lanczos if scaled => Shader::Lanczos,
            Filter::Area if scaled && sx >= 1.0 && sy >= 1.0 => Shader::AreaUpscale,
            Filter::Area if scaled => Shader::Area,
            _ => Shader::Default,
        };
        let linear = !item.srgb_off;
        self.quad(
            enc,
            target,
            &Quad {
                corners: corners(&m, cw as f32, ch as f32),
                view: if linear { &tr.linear } else { &tr.gamma },
                size: (cw, ch),
                shader,
                blend: item.blend,
                linear,
                straight: false,
                point: item.filter == Filter::Point,
                opacity: item.opacity.clamp(0.0, 1.0),
            },
        );
        Ok(())
    }

    /// Draws what an item shows through `m`: a w×h picture or texture, or its items.
    #[allow(clippy::too_many_arguments)]
    fn draw_src(
        &self,
        enc: &mut wgpu::CommandEncoder,
        target: &Target,
        src: &Src<'_>,
        m: &Affine,
        w: u32,
        h: u32,
        linear: bool,
    ) -> Result<(), AvError> {
        let quad = |enc: &mut wgpu::CommandEncoder, v: &wgpu::TextureView, straight: bool| {
            self.quad(
                enc,
                target,
                &Quad {
                    corners: corners(m, w as f32, h as f32),
                    view: v,
                    size: (w, h),
                    shader: Shader::Default,
                    blend: Blend::Normal,
                    linear,
                    straight,
                    point: false,
                    opacity: 1.0,
                },
            );
        };
        match src {
            Src::Pic(pic) => {
                let tex = self.new_picture(pic);
                quad(enc, &view(&tex, if linear { LINEAR } else { GAMMA }), !pic.premultiplied);
            }
            Src::Up(id, premultiplied) => {
                let tex = self.uploaded(*id)?;
                quad(enc, &view(&tex, if linear { LINEAR } else { GAMMA }), !premultiplied);
            }
            Src::Tex(t) => quad(enc, if linear { &t.linear } else { &t.gamma }, false),
            Src::Items(items) => self.draw_items(enc, target, items, m)?,
        }
        Ok(())
    }

    /// Runs an item's filters (libobs's filter chain): its picture drawn raw into a texture, then
    /// each filter into the next, as big as it draws.
    fn apply_effects(&self, enc: &mut wgpu::CommandEncoder, item: &Item) -> Result<Target, AvError> {
        let (w, h) = item.content.size();
        let mut cur = self.target(w.max(1), h.max(1), wgpu::TextureUsages::empty());
        self.clear(enc, &cur);
        // The picture goes in as the first filter reads it: linear for OBS's, raw for ours.
        let first_linear = item.effects.first().is_some_and(Effect::linear);
        let base = if first_linear { LINEAR } else { GAMMA };
        match &item.content {
            Content::Picture(p) => {
                let tex = self.new_picture(p);
                self.quad(
                    enc,
                    &cur,
                    &Quad {
                        corners: corners(&Affine::IDENTITY, w as f32, h as f32),
                        view: &view(&tex, base),
                        size: (w, h),
                        shader: Shader::Default,
                        blend: Blend::Normal,
                        linear: first_linear,
                        straight: !p.premultiplied,
                        point: false,
                        opacity: 1.0,
                    },
                );
            }
            Content::Uploaded { id, premultiplied, .. } => {
                let tex = self.uploaded(*id)?;
                self.quad(
                    enc,
                    &cur,
                    &Quad {
                        corners: corners(&Affine::IDENTITY, w as f32, h as f32),
                        view: &view(&tex, base),
                        size: (w, h),
                        shader: Shader::Default,
                        blend: Blend::Normal,
                        linear: first_linear,
                        straight: !premultiplied,
                        point: false,
                        opacity: 1.0,
                    },
                );
            }
            Content::Scene(s) => self.draw_items(enc, &cur, &s.items, &Affine::IDENTITY)?,
            Content::Group { items, .. } => self.draw_items(enc, &cur, items, &Affine::IDENTITY)?,
        }
        for e in &item.effects {
            // A shader effect of one's own that can't run is left out (the picture as it was).
            let custom = match e {
                Effect::Custom { code, .. } => match self.custom_pipeline(code) {
                    Some(p) => Some(p),
                    None => continue,
                },
                _ => None,
            };
            let pad = e.pad();
            let (ow, oh) = e.out_size(cur.width, cur.height);
            check_size(ow, oh)?;
            let next = self.target(ow, oh, wgpu::TextureUsages::empty());
            self.clear(enc, &next);
            let lin = e.linear();
            let (mut q, mut r, mut m) = ([0f32; 4], [0f32; 4], [0f32; 16]);
            let mut second: Option<wgpu::Texture> = None;
            let mut cube: Option<wgpu::Texture> = None;
            let (entry, p, color) = match e {
                Effect::Custom { params, elapsed_ms, .. } => {
                    q = *params;
                    ("fx_fs", [*elapsed_ms as f32 / 1000.0, 0.0, 0.0, 0.0, 0.0, 0.0], [0.0; 4])
                }
                Effect::RoundedCorners { radius } => ("fs_rounded", [*radius, 0.0, 0.0, 0.0, 0.0, 0.0], [0.0; 4]),
                Effect::Border { width, rgba, radius } => ("fs_border", [*width, *radius, 0.0, 0.0, 0.0, 0.0], *rgba),
                Effect::Shadow { rgba, blur, x, y } => ("fs_shadow", [*blur, *x, *y, 0.0, 0.0, 0.0], *rgba),
                Effect::Shape { shape, feather, stretch } => {
                    ("fs_shape", [*shape as f32, *feather, if *stretch { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0], [0.0; 4])
                }
                Effect::BlurArea { pixelate, strength, area } => (
                    "fs_blur_area",
                    [if *pixelate { 1.0 } else { 0.0 }, *strength, area[0], area[1], area[2], area[3]],
                    [0.0; 4],
                ),
                Effect::Look { look, strength } => ("fs_look", [*look as f32, *strength, 0.0, 0.0, 0.0, 0.0], [0.0; 4]),
                Effect::ChromaKey { key, similarity, smoothness, spill, opacity, contrast, brightness, gamma } => {
                    let k = [f32::from(key[0]) / 255.0, f32::from(key[1]) / 255.0, f32::from(key[2]) / 255.0, 1.0];
                    let dot = |v: [f32; 4]| k[0] * v[0] + k[1] * v[1] + k[2] * v[2] + k[3] * v[3];
                    q = [
                        dot([-0.100644, -0.338572, 0.439216, 0.501961]),
                        dot([0.439216, -0.398942, -0.040274, 0.501961]),
                        1.0 / cur.width as f32,
                        1.0 / cur.height as f32,
                    ];
                    r = [similarity / 1000.0, smoothness / 1000.0, spill / 1000.0, *opacity];
                    (
                        "fs_chroma_key",
                        [obs_contrast(*contrast), *brightness, obs_gamma(*gamma), 0.0, 0.0, 0.0],
                        [0.0; 4],
                    )
                }
                Effect::ColorKey { key, similarity, smoothness, opacity, contrast, brightness, gamma } => {
                    q = [f32::from(key[0]) / 255.0, f32::from(key[1]) / 255.0, f32::from(key[2]) / 255.0, 1.0];
                    r = [similarity / 1000.0, smoothness / 1000.0, *opacity, 0.0];
                    ("fs_color_key", [obs_contrast(*contrast), *brightness, obs_gamma(*gamma), 0.0, 0.0, 0.0], [0.0; 4])
                }
                Effect::LumaKey { max, min, max_smooth, min_smooth } => {
                    r = [*max, *min, *max_smooth, *min_smooth];
                    ("fs_luma_key", [0.0; 6], [0.0; 4])
                }
                Effect::Sharpen { sharpness } => ("fs_sharpen", [*sharpness, 0.0, 0.0, 0.0, 0.0, 0.0], [0.0; 4]),
                Effect::Crop { left, top, .. } => {
                    ("fs_crop", [*left as f32, *top as f32, 0.0, 0.0, 0.0, 0.0], [0.0; 4])
                }
                Effect::Lut { lut, amount, passthrough_alpha } => {
                    let t = self.lut_texture(lut);
                    let tech = if lut.one_d {
                        second = Some(t);
                        0.0
                    } else {
                        cube = Some(t);
                        if !lut.full_domain() {
                            4.0
                        } else if *amount < 1.0 {
                            3.0
                        } else if *passthrough_alpha {
                            1.0
                        } else {
                            2.0
                        }
                    };
                    let (scale, offset) = lut.scale_offset();
                    q = [scale[0], scale[1], scale[2], 0.0];
                    r = [offset[0], offset[1], offset[2], 0.0];
                    let (lo, hi) = (lut.domain_min, lut.domain_max);
                    m[..3].copy_from_slice(&lo);
                    m[4..7].copy_from_slice(&hi);
                    ("fs_lut", [tech, *amount, 0.0, 0.0, 0.0, 0.0], [0.0; 4])
                }
                Effect::Scroll { speed_x, speed_y, looping, elapsed_ms, .. } => {
                    let (bw, bh) = (cur.width as f32, cur.height as f32);
                    let (ox, oy) = scroll_offset(*speed_x / bw, *speed_y / bh, *elapsed_ms, *looping);
                    q = [ow as f32 / bw, oh as f32 / bh, ox, oy];
                    ("fs_scroll", [if *looping { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0, 0.0, 0.0], [0.0; 4])
                }
                Effect::Cutout { mask, blur, blur_px, soft } => {
                    second = Some(self.uploaded(*mask)?);
                    ("fs_cutout", [if *blur { 1.0 } else { 0.0 }, *soft, *blur_px, 0.0, 0.0, 0.0], [0.0; 4])
                }
                Effect::Mask { kind, image, rgba, stretch } => {
                    let t = self.uploaded(*image)?;
                    q = mask_mapping((cur.width, cur.height), (t.width(), t.height()), *stretch);
                    second = Some(t);
                    let k = match kind {
                        MaskKind::Alpha => 0.0,
                        MaskKind::Color => 1.0,
                        MaskKind::Multiply => 2.0,
                        MaskKind::Add => 3.0,
                        MaskKind::Subtract => 4.0,
                    };
                    ("fs_mask", [k, 0.0, 0.0, 0.0, 0.0, 0.0], *rgba)
                }
                Effect::ColorCorrection {
                    gamma,
                    contrast,
                    brightness,
                    saturation,
                    hue_shift,
                    opacity,
                    multiply,
                    add,
                } => {
                    m = color_correction_matrix(
                        *contrast,
                        *brightness,
                        *saturation,
                        *hue_shift,
                        *opacity,
                        *multiply,
                        *add,
                    );
                    ("fs_color_correction", [obs_gamma(*gamma), 0.0, 0.0, 0.0, 0.0, 0.0], [0.0; 4])
                }
            };
            let u = [
                cur.width as f32,
                cur.height as f32,
                ow as f32,
                oh as f32,
                pad as f32,
                p[0],
                p[1],
                p[2],
                p[3],
                p[4],
                p[5],
                0.0,
                color[0],
                color[1],
                color[2],
                color[3],
            ]
            .into_iter()
            .chain(q)
            .chain(r)
            .chain(m)
            .collect::<Vec<f32>>();
            let ubuf = self.buffer(&floats(&u), wgpu::BufferUsages::UNIFORM);
            let second = match &second {
                Some(t) => t.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(lut_view_format(t.format(), lin)),
                    ..Default::default()
                }),
                None => view(&self.blank, if lin { LINEAR } else { GAMMA }),
            };
            let cube = match &cube {
                Some(t) => t.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(lut_view_format(t.format(), true)),
                    ..Default::default()
                }),
                None => self.blank_cube.create_view(&Default::default()),
            };
            let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("effect"),
                layout: &self.effect_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(if lin { &cur.linear } else { &cur.gamma }),
                    },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.bilinear) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&second) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&cube) },
                ],
            });
            let pipeline = match custom {
                Some(p) => p,
                None => self.effect_pipeline(entry, lin),
            };
            {
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("effect"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: if lin { &next.linear } else { &next.gamma },
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.draw(0..4, 0..1);
            }
            cur = next;
        }
        Ok(cur)
    }

    /// A shader effect's pipeline, made once per code; None when it doesn't pass `shaderfx::check`
    /// or the card refuses it (caught, never a crash), and not tried again.
    fn custom_pipeline(&self, code: &str) -> Option<wgpu::RenderPipeline> {
        let mut cache = self.custom_pipelines.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(crate::shaderfx::key(code))
            .or_insert_with(|| {
                if let Err(e) = crate::shaderfx::check(code) {
                    tracing::warn!(error = %e, "a shader effect was left out");
                    return None;
                }
                let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
                let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("shader effect"),
                    source: wgpu::ShaderSource::Wgsl(crate::shaderfx::source(code).into()),
                });
                let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("shader effect"),
                    layout: Some(&self.effect_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("fx_vs"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some("fx_fs"),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: GAMMA,
                            blend: Some(blend_state(Blend::Normal, self.metal_alpha)),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleStrip,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                });
                match pollster::block_on(scope.pop()) {
                    None => Some(pipeline),
                    Some(e) => {
                        tracing::warn!(error = %e, "the graphics card refused a shader effect: left out");
                        None
                    }
                }
            })
            .clone()
    }

    fn effect_pipeline(&self, entry: &'static str, linear: bool) -> wgpu::RenderPipeline {
        let mut cache = self.effect_pipelines.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry((entry, linear))
            .or_insert_with(|| {
                self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&self.effect_pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &self.effect_module,
                        entry_point: Some("vs"),
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &self.effect_module,
                        entry_point: Some(entry),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: if linear { LINEAR } else { GAMMA },
                            blend: Some(blend_state(Blend::Normal, self.metal_alpha)),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleStrip,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                })
            })
            .clone()
    }

    /// Keeps a picture on the graphics card under `id`, to draw as `Content::Uploaded` in later
    /// frames without sending it again. The same id again replaces it (in place when the size is
    /// the same: a camera's next frame).
    pub fn upload(&self, id: u64, p: &Picture) -> Result<(), AvError> {
        check_size(p.width, p.height)?;
        if p.rgba.len() != p.width as usize * p.height as usize * 4 {
            return Err(AvError::BadPixels(p.width, p.height));
        }
        let mut ups = self.uploads.lock().unwrap_or_else(|e| e.into_inner());
        match ups.get(&id) {
            Some((tex, w, h)) if *w == p.width && *h == p.height => self.write(tex, p),
            _ => {
                let tex = self.new_picture(p);
                ups.insert(id, (tex, p.width, p.height));
            }
        }
        Ok(())
    }

    /// Lets an uploaded picture go.
    pub fn forget(&self, id: u64) {
        self.uploads.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    }

    /// A LUT on the card: a 3D texture (a 1D one for a 1D table), made once per table.
    fn lut_texture(&self, lut: &std::sync::Arc<crate::lut::Lut>) -> wgpu::Texture {
        use crate::lut::LutData;
        let key = std::sync::Arc::as_ptr(lut) as usize;
        let mut luts = self.luts.lock().unwrap_or_else(|e| e.into_inner());
        luts.retain(|_, (w, _)| w.strong_count() > 0);
        if let Some((_, t)) = luts.get(&key) {
            return t.clone();
        }
        let n = lut.size;
        let (format, bytes, px): (_, Vec<u8>, u32) = match &lut.data {
            LutData::Srgb8(d) => (GAMMA, d.clone(), 4),
            LutData::Half(d) => (wgpu::TextureFormat::Rgba16Float, d.iter().flat_map(|h| h.to_le_bytes()).collect(), 8),
        };
        let (size, dimension) = if lut.one_d {
            (wgpu::Extent3d { width: n, height: 1, depth_or_array_layers: 1 }, wgpu::TextureDimension::D2)
        } else {
            (wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n }, wgpu::TextureDimension::D3)
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lut"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: if format == GAMMA { &[LINEAR] } else { &[] },
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(n * px),
                rows_per_image: Some(if lut.one_d { 1 } else { n }),
            },
            size,
        );
        luts.insert(key, (std::sync::Arc::downgrade(lut), texture.clone()));
        texture
    }

    fn uploaded(&self, id: u64) -> Result<wgpu::Texture, AvError> {
        let ups = self.uploads.lock().unwrap_or_else(|e| e.into_inner());
        ups.get(&id).map(|(t, _, _)| t.clone()).ok_or(AvError::NotUploaded(id))
    }

    /// Draws a scene → NV12 (BT.709, limited range, as libobs gives its encoders).
    pub fn render_nv12(&self, scene: &Scene) -> Result<Nv12, AvError> {
        let (w, h) = (scene.width, scene.height);
        check_size(w, h)?;
        if !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            return Err(AvError::BadSize(w, h));
        }
        self.validate(&scene.items)?;
        let mut enc = self.device.create_command_encoder(&Default::default());
        let canvas = self.target(w, h, wgpu::TextureUsages::empty());
        self.clear(&mut enc, &canvas);
        self.draw_items(&mut enc, &canvas, &scene.items, &Affine::IDENTITY)?;
        self.nv12_out(enc, &canvas)
    }

    /// Converts a drawn canvas to NV12 and reads it back.
    fn nv12_out(&self, mut enc: wgpu::CommandEncoder, canvas: &Target) -> Result<Nv12, AvError> {
        let (w, h) = (canvas.width, canvas.height);
        let plane = |format, pw, ph| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("nv12"),
                size: wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let y = plane(wgpu::TextureFormat::R8Unorm, w, h);
        let uv = plane(wgpu::TextureFormat::Rg8Unorm, w / 2, h / 2);
        let mut u = Vec::with_capacity(16);
        for row in bt709_limited() {
            u.extend_from_slice(&row);
        }
        u.extend_from_slice(&[1.0 / w as f32, 0.0, (w / 2) as f32, (h / 2) as f32]);
        let ubuf = self.buffer(&floats(&u), wgpu::BufferUsages::UNIFORM);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&canvas.gamma) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.bilinear) },
            ],
        });
        for (tex, pipeline) in [(&y, &self.nv12.0), (&uv, &self.nv12.1)] {
            let v = tex.create_view(&Default::default());
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nv12"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &v,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        let mut planes = self.read_planes(enc, &[(&y, w, h, 1), (&uv, w / 2, h / 2, 2)])?;
        let uv = planes.pop().unwrap_or_default();
        let y = planes.pop().unwrap_or_default();
        Ok(Nv12 { width: w, height: h, y, uv })
    }

    fn write(&self, texture: &wgpu::Texture, p: &Picture) {
        let size = wgpu::Extent3d { width: p.width, height: p.height, depth_or_array_layers: 1 };
        self.queue.write_texture(
            texture.as_image_copy(),
            &p.rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(p.width * 4), rows_per_image: Some(p.height) },
            size,
        );
    }

    fn new_picture(&self, p: &Picture) -> wgpu::Texture {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("picture"),
            size: wgpu::Extent3d { width: p.width, height: p.height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: GAMMA,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[LINEAR],
        });
        self.write(&texture, p);
        texture
    }

    /// Draws one quad in its own pass (the textures it reads are finished by then).
    fn quad(&self, enc: &mut wgpu::CommandEncoder, target: &Target, q: &Quad<'_>) {
        let (bw, bh) = (q.size.0 as f32, q.size.1 as f32);
        let uniforms = floats(&[
            target.width as f32,
            target.height as f32,
            if q.straight { 1.0 } else { 0.0 },
            q.opacity,
            bw,
            bh,
            1.0 / bw,
            1.0 / bh,
        ]);
        let ubuf = self.buffer(&uniforms, wgpu::BufferUsages::UNIFORM);
        let mut verts = Vec::with_capacity(16);
        for ((x, y), (u, v)) in q.corners.iter().zip([(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)]) {
            verts.extend_from_slice(&[*x, *y, u, v]);
        }
        let vbuf = self.buffer(&floats(&verts), wgpu::BufferUsages::VERTEX);
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("item"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(q.view) },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(if q.point { &self.point } else { &self.bilinear }),
                },
            ],
        });
        let pipeline = self.pipeline(q.shader, q.blend, q.linear);
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("item"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: if q.linear { &target.linear } else { &target.gamma },
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.set_vertex_buffer(0, vbuf.slice(..));
        pass.draw(0..4, 0..1);
    }

    fn buffer(&self, bytes: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes.len() as u64,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buf, 0, bytes);
        buf
    }

    /// Copies the canvas into memory, row by row.
    pub(crate) fn read_back(
        &self,
        enc: wgpu::CommandEncoder,
        target: &wgpu::Texture,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>, AvError> {
        Ok(self.read_planes(enc, &[(target, w, h, 4)])?.pop().unwrap_or_default())
    }

    /// Copies textures (each with its size and bytes per pixel) into memory, rows packed (the
    /// GPU pads them to 256 bytes on the way).
    fn read_planes(
        &self,
        mut enc: wgpu::CommandEncoder,
        planes: &[(&wgpu::Texture, u32, u32, u32)],
    ) -> Result<Vec<Vec<u8>>, AvError> {
        let mut bufs = Vec::new();
        for (target, w, h, bpp) in planes {
            let (w, h) = (*w, *h);
            let row = w * bpp;
            let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let out = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("readback"),
                size: u64::from(padded) * u64::from(h),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            enc.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &out,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(h),
                    },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            bufs.push((out, row, padded, h));
        }
        self.queue.submit([enc.finish()]);
        let (tx, rx) = mpsc::channel();
        for (out, ..) in &bufs {
            let tx = tx.clone();
            out.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        }
        drop(tx);
        self.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| AvError::Readback(e.to_string()))?;
        for r in rx {
            r.map_err(|e| AvError::Readback(e.to_string()))?;
        }
        let mut result = Vec::new();
        for (out, row, padded, h) in bufs {
            let data = out.slice(..).get_mapped_range().map_err(|e| AvError::Readback(e.to_string()))?;
            let mut px = Vec::with_capacity((row * h) as usize);
            for y in 0..h as usize {
                let start = y * padded as usize;
                px.extend_from_slice(&data[start..start + row as usize]);
            }
            drop(data);
            out.unmap();
            result.push(px);
        }
        Ok(result)
    }

    /// Every picture's size and pixel count, and that uploaded ones are there, before anything
    /// goes to the GPU.
    pub(crate) fn validate(&self, items: &[Item]) -> Result<(), AvError> {
        for item in items {
            match &item.content {
                Content::Picture(p) => {
                    check_size(p.width, p.height)?;
                    if p.rgba.len() != p.width as usize * p.height as usize * 4 {
                        return Err(AvError::BadPixels(p.width, p.height));
                    }
                }
                Content::Uploaded { id, width, height, .. } => {
                    let ups = self.uploads.lock().unwrap_or_else(|e| e.into_inner());
                    match ups.get(id) {
                        Some((_, w, h)) if w == width && h == height => {}
                        _ => return Err(AvError::NotUploaded(*id)),
                    }
                }
                Content::Scene(s) => {
                    check_size(s.width, s.height)?;
                    self.validate(&s.items)?;
                }
                Content::Group { width, height, items } => {
                    check_size(*width, *height)?;
                    self.validate(items)?;
                }
            }
        }
        Ok(())
    }
}

/// OBS's filters' contrast setting → its multiplier.
fn obs_contrast(c: f32) -> f32 {
    if c < 0.0 { 1.0 / (-c + 1.0) } else { c + 1.0 }
}

/// OBS's filters' gamma setting → its exponent.
fn obs_gamma(g: f32) -> f32 {
    if g < 0.0 { -g + 1.0 } else { 1.0 / (g + 1.0) }
}

fn srgb_to_linear(u: f32) -> f32 {
    if u <= 0.04045 { u / 12.92 } else { ((u + 0.055) / 1.055).powf(2.4) }
}

/// libobs's 4×4 matrices: four rows (x, y, z, t), points as rows; `mul(a, b)` is a then b.
type M4 = [[f32; 4]; 4];

fn m4_mul(a: &M4, b: &M4) -> M4 {
    let mut out = [[0f32; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            out[i][j] = (0..4).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

/// color-correction-filter.c's `final_matrix`: contrast, brightness, saturation, hue and
/// opacity, then the colour wash, as libobs builds it (rows, for the shader's `m * colour`).
fn color_correction_matrix(
    contrast: f32,
    brightness: f32,
    saturation: f32,
    hue: f32,
    opacity: f32,
    multiply: u32,
    add: u32,
) -> [f32; 16] {
    let c = obs_contrast(contrast);
    let con: M4 = [[c, 0.0, 0.0, 0.0], [0.0, c, 0.0, 0.0], [0.0, 0.0, c, 0.0], [0.0, 0.0, 0.0, 1.0]];
    let bright: M4 =
        [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [brightness, brightness, brightness, 1.0]];
    let s = saturation + 1.0;
    let (wr, wg, wb) = (0.299f32, 0.587f32, 0.114f32);
    let (omr, omg, omb) = ((1.0 - s) * wr, (1.0 - s) * wg, (1.0 - s) * wb);
    let sat: M4 = [[omr + s, omr, omr, 0.0], [omg, omg + s, omg, 0.0], [omb, omb, omb + s, 0.0], [0.0, 0.0, 0.0, 1.0]];
    let half = 0.5 * (hue / (180.0 / std::f32::consts::PI));
    let rq = 0.57735f32 * half.sin();
    let w = half.cos();
    let cross = rq * rq;
    let square = 2.0 * rq * rq;
    let wimag = rq * w;
    let diag = 0.5 - square;
    let a = cross + wimag;
    let b = cross - wimag;
    let hue_op: M4 = [
        [diag * 2.0, b * 2.0, a * 2.0, 0.0],
        [a * 2.0, diag * 2.0, b * 2.0, 0.0],
        [b * 2.0, a * 2.0, diag * 2.0, 0.0],
        [0.0, 0.0, 0.0, opacity],
    ];
    let lin = |v: u32, shift: u32| srgb_to_linear(((v >> shift) & 0xff) as f32 / 255.0);
    let wash: M4 = [
        [lin(multiply, 0), 0.0, 0.0, 0.0],
        [0.0, lin(multiply, 8), 0.0, 0.0],
        [0.0, 0.0, lin(multiply, 16), 0.0],
        [lin(add, 0), lin(add, 8), lin(add, 16), 1.0],
    ];
    let f = m4_mul(&m4_mul(&m4_mul(&m4_mul(&con, &bright), &sat), &hue_op), &wash);
    let mut out = [0f32; 16];
    for (i, row) in f.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(row);
    }
    out
}

/// BT.709 limited range, RGB → Y, Cb, Cr: each row's weights and offset, as libobs converts.
pub fn bt709_limited() -> [[f32; 4]; 3] {
    let (kr, kb) = (0.2126f32, 0.0722f32);
    let kg = 1.0 - kr - kb;
    let ys = 219.0 / 255.0;
    let cs = 224.0 / 255.0;
    let (cb, cr) = (cs / (2.0 * (1.0 - kb)), cs / (2.0 * (1.0 - kr)));
    [
        [ys * kr, ys * kg, ys * kb, 16.0 / 255.0],
        [-cb * kr, -cb * kg, cb * (1.0 - kb), 128.0 / 255.0],
        [cr * (1.0 - kr), -cr * kg, -cr * kb, 128.0 / 255.0],
    ]
}

pub(crate) fn check_size(w: u32, h: u32) -> Result<(), AvError> {
    if w == 0 || h == 0 || w > MAX || h > MAX { Err(AvError::BadSize(w, h)) } else { Ok(()) }
}

/// A w×h rectangle's corners through `m`: top left, top right, bottom left, bottom right.
fn corners(m: &Affine, w: f32, h: f32) -> [(f32, f32); 4] {
    [m.apply(0.0, 0.0), m.apply(w, 0.0), m.apply(0.0, h), m.apply(w, h)]
}

/// How a filter's second picture or LUT is read: an 8-bit one through its sRGB view when the
/// filter works in linear light (libobs's gs_effect_set_texture_srgb), a float one as it is.
fn lut_view_format(format: wgpu::TextureFormat, linear: bool) -> wgpu::TextureFormat {
    if format == GAMMA && linear { LINEAR } else { format }
}

/// How far Scroll has moved, as a share of the picture each way, `ms` after it was shown:
/// scroll-filter.c adds speed × seconds each frame and takes 1 off past 1 when looping (else stops
/// at 1); this is the same without a frame's rounding.
fn scroll_offset(per_sec_x: f32, per_sec_y: f32, ms: u64, looping: bool) -> (f32, f32) {
    let secs = ms as f64 / 1000.0;
    let one = |v: f32| {
        let o = f64::from(v) * secs;
        if looping {
            // Past 1 wraps; below 0 it keeps going (the sampler wraps it), as OBS's.
            if o > 1.0 { o.fract() as f32 } else { o as f32 }
        } else {
            o.min(1.0) as f32
        }
    };
    (one(per_sec_x), one(per_sec_y))
}

/// Where Image Mask/Blend reads its picture (mask-filter.c): a multiplier and an offset on the
/// filtered picture's coordinates; kept in shape and centred unless stretched.
fn mask_mapping(source: (u32, u32), mask: (u32, u32), stretch: bool) -> [f32; 4] {
    let (sx, sy) = (source.0 as f32, source.1 as f32);
    let (mx, my) = (mask.0.max(1) as f32, mask.1.max(1) as f32);
    if stretch || sx <= 0.0 || sy <= 0.0 {
        return [1.0, 1.0, 0.0, 0.0];
    }
    let fix = if sx / sy < mx / my { sx / mx } else { sy / my };
    let (mx, my) = (mx * fix, my * fix);
    [sx / mx, sy / my, -(sx / 2.0 - mx / 2.0) / mx, -(sy / 2.0 - my / 2.0) / my]
}

fn view(texture: &wgpu::Texture, format: wgpu::TextureFormat) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor { format: Some(format), ..Default::default() })
}

fn floats(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

#[cfg(test)]
mod mask_tests {
    use super::{mask_mapping, scroll_offset};

    #[test]
    fn scroll_wraps_or_stops() {
        // A quarter of the picture a second, 6 s on: once round and half again.
        let (x, y) = scroll_offset(0.25, 0.0, 6_000, true);
        assert!((x - 0.5).abs() < 1e-6 && y == 0.0, "{x} {y}");
        assert_eq!(scroll_offset(0.25, 0.0, 6_000, false).0, 1.0, "not looping: it's gone");
        assert!((scroll_offset(-0.25, 0.0, 2_000, true).0 + 0.5).abs() < 1e-6, "backwards keeps going");
    }

    #[test]
    fn a_mask_keeps_its_shape_centred() {
        assert_eq!(mask_mapping((200, 100), (50, 50), true), [1.0, 1.0, 0.0, 0.0]);
        // A square mask on a wide picture: as tall as it, centred across.
        let [mx, my, ax, ay] = mask_mapping((200, 100), (50, 50), false);
        assert_eq!((mx, my, ay), (2.0, 1.0, 0.0));
        assert!((ax + 0.5).abs() < 1e-6, "{ax}");
    }
}
