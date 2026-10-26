//! Projectors, as in OBS: what's on air, one scene or source, or a multiview of every scene, in
//! a window of their own or full screen on a display. The window and the drawing are in
//! `ffi.rs`; this is the layout, shared by the drawing and by clicks in a multiview.

/// A box in a projector, from its top-left corner (pixels or points; the layout is the same).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Area {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    /// The largest box of shape `aspect` (width / height) inside this one, centred.
    pub fn fit(&self, aspect: f32) -> Area {
        if aspect.is_nan() || aspect <= 0.0 || self.w <= 0.0 || self.h <= 0.0 {
            return Area { w: 0.0, h: 0.0, ..*self };
        }
        let (w, h) = if self.w / self.h > aspect { (self.h * aspect, self.h) } else { (self.w, self.w / aspect) };
        Area { x: self.x + (self.w - w) / 2.0, y: self.y + (self.h - h) / 2.0, w, h }
    }
}

/// Room between boxes in a multiview, as a share of its height.
const GAP: f32 = 0.012;

/// A multiview of `n` scenes in a `w`×`h` projector: what's on air across the top half, the
/// scenes in a grid below (as many columns as make them biggest), each `aspect` wide.
pub fn multiview(n: usize, w: f32, h: f32, aspect: f32) -> (Area, Vec<Area>) {
    let gap = h * GAP;
    let top = Area { x: gap, y: gap, w: w - 2.0 * gap, h: h / 2.0 - 1.5 * gap }.fit(aspect);
    if n == 0 {
        return (top, Vec::new());
    }
    let bottom = Area { x: gap, y: h / 2.0 + gap / 2.0, w: w - 2.0 * gap, h: h / 2.0 - 1.5 * gap };
    // The column count that makes each scene biggest.
    let size = |cols: usize| {
        let rows = n.div_ceil(cols);
        let cw = (bottom.w - gap * (cols - 1) as f32) / cols as f32;
        let ch = (bottom.h - gap * (rows - 1) as f32) / rows as f32;
        cw.min(ch * aspect)
    };
    let cols = (1..=n).max_by(|a, b| size(*a).total_cmp(&size(*b))).unwrap_or(1);
    let rows = n.div_ceil(cols);
    let cw = size(cols);
    let ch = cw / aspect;
    let grid_w = cw * cols as f32 + gap * (cols - 1) as f32;
    let grid_h = ch * rows as f32 + gap * (rows - 1) as f32;
    let (x0, y0) = (bottom.x + (bottom.w - grid_w) / 2.0, bottom.y + (bottom.h - grid_h) / 2.0);
    let cells = (0..n)
        .map(|i| {
            let (r, c) = (i / cols, i % cols);
            Area { x: x0 + c as f32 * (cw + gap), y: y0 + r as f32 * (ch + gap), w: cw, h: ch }
        })
        .collect();
    (top, cells)
}

/// The scene a click at (`x`, `y`) lands on in a multiview of `n` scenes, if any.
pub fn multiview_hit(n: usize, w: f32, h: f32, aspect: f32, x: f32, y: f32) -> Option<usize> {
    multiview(n, w, h, aspect).1.iter().position(|a| a.contains(x, y))
}

/// What a projector shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectorOf {
    /// What's on air.
    Program,
    /// One scene or source, by name.
    Source(String),
    /// Every scene, with what's on air above; a click puts a scene on air.
    Multiview,
}

/// A display to put a full-screen projector on.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Screen {
    /// Its place in the list (what to ask for).
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// The one with the menu bar.
    pub main: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE: f32 = 16.0 / 9.0;

    #[test]
    fn a_box_fits_inside_another_centred() {
        let a = Area { x: 0.0, y: 0.0, w: 400.0, h: 100.0 }.fit(2.0);
        assert_eq!(a, Area { x: 100.0, y: 0.0, w: 200.0, h: 100.0 });
        let tall = Area { x: 10.0, y: 10.0, w: 100.0, h: 400.0 }.fit(2.0);
        assert_eq!(tall, Area { x: 10.0, y: 185.0, w: 100.0, h: 50.0 });
        assert_eq!(Area { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }.fit(f32::NAN).w, 0.0);
    }

    #[test]
    fn a_multiview_puts_whats_on_air_above_and_scenes_below() {
        let (w, h) = (1920.0, 1080.0);
        let (top, cells) = multiview(8, w, h, WIDE);
        assert!(top.y + top.h <= h / 2.0 && top.h > h * 0.45, "{top:?}");
        assert_eq!(cells.len(), 8);
        for c in &cells {
            assert!(c.y >= h / 2.0 && c.y + c.h <= h && c.x >= 0.0 && c.x + c.w <= w, "{c:?}");
            assert!((c.w / c.h - WIDE).abs() < 0.01);
        }
        // No two scenes overlap.
        for (i, a) in cells.iter().enumerate() {
            for b in &cells[i + 1..] {
                let apart = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{a:?} {b:?}");
            }
        }
        // Eight in a wide window: two rows of four.
        assert_eq!(cells.iter().filter(|c| (c.y - cells[0].y).abs() < 0.5).count(), 4);
        // Many scenes still fit, smaller.
        let (_, many) = multiview(15, w, h, WIDE);
        assert!(many.iter().all(|c| c.y + c.h <= h + 0.01 && c.x + c.w <= w + 0.01));
        assert!(many[0].w < cells[0].w);
        assert!(multiview(0, w, h, WIDE).1.is_empty());
    }

    #[test]
    fn a_click_finds_the_scene_under_it() {
        let (w, h) = (1280.0, 720.0);
        let (top, cells) = multiview(5, w, h, WIDE);
        let mid = |a: &Area| (a.x + a.w / 2.0, a.y + a.h / 2.0);
        for (i, c) in cells.iter().enumerate() {
            let (x, y) = mid(c);
            assert_eq!(multiview_hit(5, w, h, WIDE, x, y), Some(i));
        }
        let (x, y) = mid(&top);
        assert_eq!(multiview_hit(5, w, h, WIDE, x, y), None, "what's on air isn't a scene");
        assert_eq!(multiview_hit(5, w, h, WIDE, 1.0, 1.0), None);
    }
}
