//! Ferrocast's own engine (docs/plan/ENGINE.md, Part 2). A GPU compositor on `wgpu` (Metal,
//! D3D12, Vulkan) that draws scenes as libobs does, checked against libobs pixel by pixel
//! (`ferrocast-studio`'s `engine_match` example); hardware encoders, FLV for streaming and
//! crash-safe MP4 recording (`video`, `audio`, `flv`, `mp4`, `pipeline`). libobs keeps doing the
//! rest until each part here takes its job over.

#[cfg(target_os = "macos")]
mod apple;
/// Cameras and microphones (macOS: AVFoundation).
#[cfg(target_os = "macos")]
pub use apple::camera;
/// Video and sound files (macOS: AVFoundation).
#[cfg(target_os = "macos")]
pub use apple::media;
/// Screen and window capture (macOS: ScreenCaptureKit).
#[cfg(target_os = "macos")]
pub use apple::screen;
pub mod afx;
pub mod audio;
mod compose;
pub mod engine;
pub mod ffenc;
pub mod flv;
pub mod lut;
pub mod mix;
pub mod mkv;
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub mod monitor;
pub mod mp4;
pub mod obs;
pub mod pipeline;
pub mod preview;
pub mod source;
mod transform;
pub mod video;

pub use compose::Compositor;
pub use compose::bt709_limited;
pub use transform::{Affine, BOTTOM, Bounds, LEFT, Placement, RIGHT, TOP, cropped, draw_transform, output_scale};

/// Why drawing or encoding failed.
#[derive(Debug, thiserror::Error)]
pub enum AvError {
    #[error("no graphics card could be used ({0})")]
    NoGpu(String),
    #[error("a picture is {0}x{1}: it must be 1 to 16384 pixels each way")]
    BadSize(u32, u32),
    #[error("a picture's pixels don't fill {0}x{1} (RGBA, 4 bytes each)")]
    BadPixels(u32, u32),
    #[error("the frame couldn't be read back from the graphics card ({0})")]
    Readback(String),
    #[error("{0}")]
    Encoder(String),
    #[error("{0}")]
    Source(String),
    #[error("picture {0} isn't on the graphics card (upload it first), or its size changed")]
    NotUploaded(u64),
}

/// How an item's picture is scaled: OBS's scale filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// OBS's "Disable": drawn directly, bilinear.
    #[default]
    Disable,
    Point,
    Bilinear,
    Bicubic,
    Lanczos,
    Area,
}

impl Filter {
    /// From the names Ferrocast's API uses (`ferrocast_studio::SCALE_FILTERS`).
    pub fn from_name(name: &str) -> Self {
        match name {
            "point" => Self::Point,
            "bilinear" => Self::Bilinear,
            "bicubic" => Self::Bicubic,
            "lanczos" => Self::Lanczos,
            "area" => Self::Area,
            _ => Self::Disable,
        }
    }
}

/// How an item mixes with what's under it: OBS's blend modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Blend {
    #[default]
    Normal,
    Additive,
    Subtract,
    Screen,
    Multiply,
    Lighten,
    Darken,
}

impl Blend {
    /// From the names Ferrocast's API uses (`ferrocast_studio::BLENDS`).
    pub fn from_name(name: &str) -> Self {
        match name {
            "additive" => Self::Additive,
            "subtract" => Self::Subtract,
            "screen" => Self::Screen,
            "multiply" => Self::Multiply,
            "lighten" => Self::Lighten,
            "darken" => Self::Darken,
            _ => Self::Normal,
        }
    }
}

/// A picture: RGBA rows in sRGB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Colours already multiplied by alpha: libobs's image files (multiplied as stored, in
    /// sRGB) and whatever libobs hands over drawn. Else straight, as a Color source's colour.
    pub premultiplied: bool,
}

impl Picture {
    /// One colour, `width`×`height` (OBS's Color source).
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        Self { width, height, rgba: rgba.repeat(width as usize * height as usize), premultiplied: false }
    }

    /// An image file's pixels (straight alpha), multiplied as libobs's image loader does: on
    /// the stored sRGB values, rounded.
    pub fn image(width: u32, height: u32, mut rgba: Vec<u8>) -> Self {
        for px in rgba.as_chunks_mut::<4>().0 {
            let a = u32::from(px[3]);
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * a + 127) / 255) as u8;
            }
        }
        Self { width, height, rgba, premultiplied: true }
    }
}

/// Ferrocast's own picture filters, applied to an item's picture in order before it's placed
/// (as libobs runs a source's filters). Colours are red, green, blue, alpha, 0 to 1.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    RoundedCorners {
        radius: f32,
    },
    Border {
        width: f32,
        rgba: [f32; 4],
        radius: f32,
    },
    /// Drawn past the picture's edges: the picture grows by `pad()` on every side.
    Shadow {
        rgba: [f32; 4],
        blur: f32,
        x: f32,
        y: f32,
    },
    /// 0 circle, 1 hexagon, 2 diamond, 3 star, 4 heart.
    Shape {
        shape: u32,
        feather: f32,
        stretch: bool,
    },
    /// The area in percent of the picture: left, top, width, height.
    BlurArea {
        pixelate: bool,
        strength: f32,
        area: [f32; 4],
    },
    /// 0 warm, 1 cool, 2 teal and orange, 3 black and white, 4 vintage, 5 punchy, 6 soft fade;
    /// strength in percent.
    Look {
        look: u32,
        strength: f32,
    },
    /// OBS's Chroma Key (its settings as OBS keeps them: similarity, smoothness and spill 1 to
    /// 1000, contrast, brightness and gamma -1 to 1, opacity 0 to 1; the key colour as RGB).
    ChromaKey {
        key: [u8; 3],
        similarity: f32,
        smoothness: f32,
        spill: f32,
        opacity: f32,
        contrast: f32,
        brightness: f32,
        gamma: f32,
    },
    /// OBS's Color Correction (its settings: gamma -3 to 3, contrast -4 to 4, brightness -1 to 1,
    /// saturation -1 to 5, hue shift in degrees, opacity 0 to 1; colours as libobs keeps them,
    /// 0xAABBGGRR).
    ColorCorrection {
        gamma: f32,
        contrast: f32,
        brightness: f32,
        saturation: f32,
        hue_shift: f32,
        opacity: f32,
        multiply: u32,
        add: u32,
    },
    /// OBS's Color Key (similarity and smoothness 1 to 1000, opacity, contrast, brightness,
    /// gamma as OBS keeps them; the key as RGB).
    ColorKey {
        key: [u8; 3],
        similarity: f32,
        smoothness: f32,
        opacity: f32,
        contrast: f32,
        brightness: f32,
        gamma: f32,
    },
    /// OBS's Luma Key (luminance 0 to 1: kept between `min` and `max`, softened by the two
    /// smoothness widths).
    LumaKey {
        max: f32,
        min: f32,
        max_smooth: f32,
        min_smooth: f32,
    },
    /// OBS's Crop/Pad: pixels off each side (negative pads with transparency), or with `size`
    /// that size from the left and top offsets (OBS's absolute mode).
    Crop {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        size: Option<(u32, u32)>,
    },
    /// OBS's Image Mask/Blend: a picture uploaded under `image` masks or blends with this
    /// one, tinted by `rgba` (straight, 0 to 1); fitted to keep its shape unless `stretch`.
    Mask {
        kind: MaskKind,
        image: u64,
        rgba: [f32; 4],
        stretch: bool,
    },
    /// OBS's Apply LUT: `amount` 0 to 1; `passthrough_alpha` reads colours as they are,
    /// without undoing their alpha first.
    Lut {
        lut: std::sync::Arc<lut::Lut>,
        amount: f32,
        passthrough_alpha: bool,
    },
    /// OBS's Scroll: moving `speed` pixels a second each way, optionally shown at a size of its
    /// own, wrapping round when `looping` (else it scrolls away); `elapsed_ms` since it was shown
    /// (the engine sets it each frame).
    Scroll {
        speed_x: f32,
        speed_y: f32,
        width: Option<u32>,
        height: Option<u32>,
        looping: bool,
        elapsed_ms: u64,
    },
    /// OBS's Sharpen (0 to 1).
    Sharpen {
        sharpness: f32,
    },
    /// Ferrocast's Remove background: the person in the picture kept by the mask uploaded under
    /// `mask` (its red: 1 a person, 0 not; stretched over the picture), the rest taken out, or
    /// blurred `blur_px` when `blur`; `soft` (0 to 0.4) softens the edge.
    Cutout {
        mask: u64,
        blur: bool,
        blur_px: f32,
        soft: f32,
    },
}

/// What OBS's Image Mask/Blend does with its picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskKind {
    /// Its alpha.
    Alpha,
    /// Its brightness (the average of red, green and blue).
    Color,
    Multiply,
    Add,
    Subtract,
}

impl Effect {
    /// OBS's own filters run in linear light (they're sRGB-aware); Ferrocast's in gamma space.
    pub fn linear(&self) -> bool {
        matches!(
            self,
            Self::ChromaKey { .. }
                | Self::ColorCorrection { .. }
                | Self::ColorKey { .. }
                | Self::LumaKey { .. }
                | Self::Sharpen { .. }
                | Self::Crop { .. }
                | Self::Mask { .. }
                | Self::Lut { .. }
                | Self::Scroll { .. }
        )
    }

    /// How far it draws past the picture on every side, in pixels.
    pub fn pad(&self) -> u32 {
        match self {
            Self::Shadow { blur, x, y, .. } => (blur + x.abs().max(y.abs())).ceil().clamp(0.0, 1000.0) as u32,
            _ => 0,
        }
    }

    /// The picture's size after this filter, from its size before (at least 1×1 for a crop, as
    /// OBS's).
    pub fn out_size(&self, w: u32, h: u32) -> (u32, u32) {
        match self {
            Self::Scroll { width, height, .. } => {
                (width.unwrap_or(w).clamp(1, 16_384), height.unwrap_or(h).clamp(1, 16_384))
            }
            Self::Crop { size: Some((cw, ch)), .. } => ((*cw).clamp(1, 16_384), (*ch).clamp(1, 16_384)),
            Self::Crop { left, top, right, bottom, size: None } => (
                (i64::from(w) - i64::from(*left) - i64::from(*right)).clamp(1, 16_384) as u32,
                (i64::from(h) - i64::from(*top) - i64::from(*bottom)).clamp(1, 16_384) as u32,
            ),
            _ => (w + 2 * self.pad(), h + 2 * self.pad()),
        }
    }

    /// A libobs colour (0xAABBGGRR) as red, green, blue, alpha.
    pub fn rgba(v: u32) -> [f32; 4] {
        let c = |shift: u32| ((v >> shift) & 0xff) as f32 / 255.0;
        [c(0), c(8), c(16), c(24)]
    }
}

/// What an item shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Picture(Picture),
    /// A picture already on the graphics card (`Compositor::upload`), drawn without sending it
    /// again: a background, a logo, a camera updated in place.
    Uploaded {
        id: u64,
        width: u32,
        height: u32,
        premultiplied: bool,
    },
    /// Another scene shown inside this one (drawn at its own size first, as libobs does).
    Scene(Scene),
    /// A group: its items, placed inside its own box of this size.
    Group {
        width: u32,
        height: u32,
        items: Vec<Item>,
    },
}

impl Content {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Picture(p) => (p.width, p.height),
            Self::Uploaded { width, height, .. } => (*width, *height),
            Self::Scene(s) => (s.width, s.height),
            Self::Group { width, height, .. } => (*width, *height),
        }
    }
}

/// An item in a scene: what it shows and how.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub content: Content,
    /// Filters on its picture, first to last.
    pub effects: Vec<Effect>,
    pub placement: Placement,
    pub filter: Filter,
    pub blend: Blend,
    /// OBS's "SRGB Off" blending method: mixed in gamma space even when sRGB-aware.
    pub srgb_off: bool,
    /// Mixed in linear light (OBS's sRGB-aware sources: colours, images, cameras); else in
    /// gamma space, as libobs mixes older sources (Text on macOS and Linux, for one).
    pub linear: bool,
    /// 0 (gone) to 1 (as is). Below 1 the item is drawn on its own first, so a group fades as one.
    pub opacity: f32,
}

impl Item {
    /// A picture placed plainly: normal blending, no scale filter, sRGB-aware.
    pub fn picture(picture: Picture, placement: Placement) -> Self {
        Self {
            content: Content::Picture(picture),
            effects: Vec::new(),
            placement,
            filter: Filter::Disable,
            blend: Blend::Normal,
            srgb_off: false,
            linear: true,
            opacity: 1.0,
        }
    }
}

/// Which way a slide or swipe goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    /// OBS's names ("left", "right", "up", "down"); anything else is left, as OBS reads it.
    pub fn from_name(name: &str) -> Self {
        match name {
            "right" => Self::Right,
            "up" => Self::Up,
            "down" => Self::Down,
            _ => Self::Left,
        }
    }

    /// OBS's direction vector for it.
    fn vector(self) -> (f32, f32) {
        match self {
            Self::Left => (1.0, 0.0),
            Self::Right => (-1.0, 0.0),
            Self::Up => (0.0, 1.0),
            Self::Down => (0.0, -1.0),
        }
    }
}

/// OBS's transitions, with their settings.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionKind {
    Cut,
    Fade,
    /// Through a colour (RGB), switching scenes at `switch_point` (0 to 1).
    FadeToColor {
        rgb: [u8; 3],
        switch_point: f32,
    },
    Slide(Direction),
    Swipe {
        direction: Direction,
        swipe_in: bool,
    },
    /// Sources in both scenes glide to their new places; the rest fade (OBS's Move plugin,
    /// built in). Needs both scenes' items, so the engine draws it; the compositor alone fades.
    Move,
    /// The new scene comes in where `luma` (a greyscale picture, stretched over the canvas) is
    /// darkest first.
    LumaWipe {
        luma: Picture,
        invert: bool,
        softness: f32,
    },
}

/// Two scenes mid-transition: `t` from 0 (all `from`) to 1 (all `to`).
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub kind: TransitionKind,
    pub t: f32,
    pub from: Scene,
    pub to: Scene,
}

/// OBS's `cubic_ease_in_out`, which slides and swipes move by.
pub fn cubic_ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let k = 2.0 * t - 2.0;
        (t - 1.0) * k * k + 1.0
    }
}

fn smoothstep(min: f32, max: f32, x: f32) -> f32 {
    let x = ((x - min) / (max - min)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// A frame in NV12 (what hardware encoders take): the Y plane, then the interleaved CbCr plane
/// at half size, rows packed (no padding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
}

/// A scene: its canvas and its items, bottom first.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub width: u32,
    pub height: u32,
    pub items: Vec<Item>,
}
