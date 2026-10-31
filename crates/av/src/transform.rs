//! Where an item's picture lands on the canvas: libobs's own sums (obs-scene.c,
//! `update_item_transform` and `calculate_bounds_data`), so both engines place it alike to the
//! pixel. The result maps the cropped picture's pixels onto the canvas.

/// `enum obs_bounds_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Bounds {
    #[default]
    None,
    Stretch,
    /// Fit inside (OBS's scale inner).
    Fit,
    /// Fill (scale outer).
    Fill,
    Width,
    Height,
    /// Shrink only (max only).
    Max,
}

impl Bounds {
    /// From the names Ferrocast's API uses (`ferrocast_studio::BOUNDS`).
    pub fn from_name(name: &str) -> Self {
        match name {
            "stretch" => Self::Stretch,
            "fit" => Self::Fit,
            "fill" => Self::Fill,
            "width" => Self::Width,
            "height" => Self::Height,
            "max" => Self::Max,
            _ => Self::None,
        }
    }
}

/// OBS's alignment bits: 0 the middle; 1 left, 2 right, 4 top, 8 bottom.
pub const LEFT: u32 = 1;
pub const RIGHT: u32 = 2;
pub const TOP: u32 = 4;
pub const BOTTOM: u32 = 8;

/// An item's placement, as OBS's Edit Transform dialog has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f32,
    pub y: f32,
    /// Degrees, clockwise.
    pub rotation: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub alignment: u32,
    pub bounds: Bounds,
    pub bounds_alignment: u32,
    pub bounds_width: f32,
    pub bounds_height: f32,
    /// Left, top, right, bottom, in the source's pixels.
    pub crop: [u32; 4],
}

impl Default for Placement {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            rotation: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            alignment: LEFT | TOP,
            bounds: Bounds::None,
            bounds_alignment: 0,
            bounds_width: 0.0,
            bounds_height: 0.0,
            crop: [0; 4],
        }
    }
}

/// A 2D affine map: `(x, y)` → `(a x + c y + e, b x + d y + f)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Affine {
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f)
    }
}

/// The cropped picture's size: what the map is from.
pub fn cropped(width: u32, height: u32, crop: [u32; 4]) -> (u32, u32) {
    (width.saturating_sub(crop[0]).saturating_sub(crop[2]), height.saturating_sub(crop[1]).saturating_sub(crop[3]))
}

/// libobs's `add_alignment`: whole pixels, halves rounded down, as its integer sums do.
fn add_alignment(origin: &mut (f32, f32), align: u32, cx: i32, cy: i32) {
    if align & RIGHT != 0 {
        origin.0 += cx as f32;
    } else if align & LEFT == 0 {
        origin.0 += (cx / 2) as f32;
    }
    if align & BOTTOM != 0 {
        origin.1 += cy as f32;
    } else if align & TOP == 0 {
        origin.1 += (cy / 2) as f32;
    }
}

/// The map from a `width`×`height` source's cropped pixels to the canvas (OBS's draw transform).
pub fn draw_transform(p: &Placement, width: u32, height: u32) -> Affine {
    placed(p, width, height).0
}

/// How much the cropped picture is scaled on each side (libobs's `output_scale`), which picks
/// the scale filter's shader.
pub fn output_scale(p: &Placement, width: u32, height: u32) -> (f32, f32) {
    placed(p, width, height).1
}

impl Affine {
    pub const IDENTITY: Self = Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    /// A move by (x, y).
    pub fn translate(x: f32, y: f32) -> Self {
        Self { e: x, f: y, ..Self::IDENTITY }
    }

    /// One map after another: a point goes through `self`, then `outer`.
    pub fn then(&self, outer: &Self) -> Self {
        Self {
            a: outer.a * self.a + outer.c * self.b,
            b: outer.b * self.a + outer.d * self.b,
            c: outer.a * self.c + outer.c * self.d,
            d: outer.b * self.c + outer.d * self.d,
            e: outer.a * self.e + outer.c * self.f + outer.e,
            f: outer.b * self.e + outer.d * self.f + outer.f,
        }
    }
}

fn placed(p: &Placement, width: u32, height: u32) -> (Affine, (f32, f32)) {
    let (mut cx, mut cy) = cropped(width, height, p.crop);
    let mut scale = (p.scale_x, p.scale_y);
    let mut origin = (0.0f32, 0.0f32);
    if p.bounds != Bounds::None && cx > 0 && cy > 0 {
        let w = cx as f32 * scale.0.abs();
        let h = cy as f32 * scale.1.abs();
        let item_aspect = w / h;
        let bounds_aspect = p.bounds_width / p.bounds_height;
        let mut kind = p.bounds;
        if kind == Bounds::Max && (w > p.bounds_width || h > p.bounds_height) {
            kind = Bounds::Fit;
        }
        match kind {
            Bounds::Fit | Bounds::Fill => {
                let mut use_width = bounds_aspect < item_aspect;
                if kind == Bounds::Fill {
                    use_width = !use_width;
                }
                let mul = if use_width { p.bounds_width / w } else { p.bounds_height / h };
                scale = (scale.0 * mul, scale.1 * mul);
            }
            Bounds::Width => {
                let mul = p.bounds_width / w;
                scale = (scale.0 * mul, scale.1 * mul);
            }
            Bounds::Height => {
                let mul = p.bounds_height / h;
                scale = (scale.0 * mul, scale.1 * mul);
            }
            Bounds::Stretch => {
                scale =
                    ((p.bounds_width / cx as f32).copysign(scale.0), (p.bounds_height / cy as f32).copysign(scale.1));
            }
            Bounds::None | Bounds::Max => {}
        }
        let w = cx as f32 * scale.0;
        let h = cy as f32 * scale.1;
        let width_diff = p.bounds_width - w;
        let height_diff = p.bounds_height - h;
        cx = p.bounds_width as u32;
        cy = p.bounds_height as u32;
        add_alignment(&mut origin, p.bounds_alignment, -width_diff as i32, -height_diff as i32);
    } else {
        cx = (cx as f32 * scale.0) as u32;
        cy = (cy as f32 * scale.1) as u32;
    }
    add_alignment(&mut origin, p.alignment, cx as i32, cy as i32);

    // Scale, move by -origin, turn, move to the position: libobs's matrices, row vectors.
    let (sin, cos) = p.rotation.to_radians().sin_cos();
    let (sx, sy) = scale;
    let (ox, oy) = origin;
    let m = Affine {
        a: sx * cos,
        b: sx * sin,
        c: -sy * sin,
        d: sy * cos,
        e: -ox * cos + oy * sin + p.x,
        f: -ox * sin - oy * cos + p.y,
    };
    (m, scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    #[test]
    fn top_left_is_the_position() {
        let p = Placement { x: 100.0, y: 50.0, ..Default::default() };
        let m = draw_transform(&p, 400, 300);
        assert!(near(m.apply(0.0, 0.0), (100.0, 50.0)));
        assert!(near(m.apply(400.0, 300.0), (500.0, 350.0)));
    }

    #[test]
    fn centred_alignment_rounds_halves_down() {
        // 401 wide: libobs's half is 200, not 200.5.
        let p = Placement { x: 640.0, y: 360.0, alignment: 0, ..Default::default() };
        let m = draw_transform(&p, 401, 301);
        assert!(near(m.apply(0.0, 0.0), (440.0, 210.0)));
    }

    #[test]
    fn turning_is_clockwise_round_the_alignment_point() {
        let p = Placement { x: 100.0, y: 100.0, rotation: 90.0, ..Default::default() };
        let m = draw_transform(&p, 200, 100);
        // The top edge now points down the canvas.
        assert!(near(m.apply(200.0, 0.0), (100.0, 300.0)));
        assert!(near(m.apply(0.0, 100.0), (0.0, 100.0)));
    }

    #[test]
    fn crop_shrinks_what_is_placed() {
        let p = Placement { crop: [10, 20, 30, 40], ..Default::default() };
        assert_eq!(cropped(400, 300, p.crop), (360, 240));
        let m = draw_transform(&p, 400, 300);
        assert!(near(m.apply(360.0, 240.0), (360.0, 240.0)));
    }

    #[test]
    fn maps_chain_inner_first() {
        let inner = Affine::translate(10.0, 0.0);
        let outer = draw_transform(&Placement { rotation: 90.0, ..Default::default() }, 10, 10);
        let both = inner.then(&outer);
        let (x, y) = inner.apply(1.0, 2.0);
        assert!(near(both.apply(1.0, 2.0), outer.apply(x, y)));
    }

    #[test]
    fn output_scale_includes_the_box() {
        let p = Placement { bounds: Bounds::Fit, bounds_width: 200.0, bounds_height: 200.0, ..Default::default() };
        assert_eq!(output_scale(&p, 100, 50), (2.0, 2.0));
    }

    #[test]
    fn huge_crops_leave_nothing() {
        assert_eq!(cropped(400, 300, [u32::MAX, 0, u32::MAX, u32::MAX]), (0, 0));
    }

    #[test]
    fn fit_letterboxes_inside_the_box() {
        let p = Placement {
            bounds: Bounds::Fit,
            bounds_alignment: 0,
            bounds_width: 1280.0,
            bounds_height: 720.0,
            ..Default::default()
        };
        // A square into 16:9: 720 tall, centred.
        let m = draw_transform(&p, 100, 100);
        assert!(near(m.apply(0.0, 0.0), (280.0, 0.0)));
        assert!(near(m.apply(100.0, 100.0), (1000.0, 720.0)));
    }

    #[test]
    fn stretch_fills_the_box() {
        let p = Placement { bounds: Bounds::Stretch, bounds_width: 300.0, bounds_height: 50.0, ..Default::default() };
        let m = draw_transform(&p, 100, 100);
        assert!(near(m.apply(100.0, 100.0), (300.0, 50.0)));
    }
}
