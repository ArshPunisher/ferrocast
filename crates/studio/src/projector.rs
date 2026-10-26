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

/// Where a multiview puts what's on air (and Studio Mode's preview beside it), as OBS's
/// Multiview Layout: above, below, left or right of the scenes, or scenes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiviewLayout {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
    ScenesOnly,
}

/// How a multiview looks: its layout, Studio Mode's preview beside what's on air, names under
/// each box, and the EBU R95 safe areas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MultiviewLook {
    pub layout: MultiviewLayout,
    pub labels: bool,
    pub safe_areas: bool,
}

impl Default for MultiviewLook {
    fn default() -> Self {
        MultiviewLook { layout: MultiviewLayout::Top, labels: true, safe_areas: false }
    }
}

/// `n` boxes of shape `aspect` in a grid inside `region`, as big as they can be.
fn grid(n: usize, region: Area, aspect: f32, gap: f32) -> Vec<Area> {
    if n == 0 || region.w <= 0.0 || region.h <= 0.0 {
        return Vec::new();
    }
    let size = |cols: usize| {
        let rows = n.div_ceil(cols);
        let cw = (region.w - gap * (cols - 1) as f32) / cols as f32;
        let ch = (region.h - gap * (rows - 1) as f32) / rows as f32;
        cw.min(ch * aspect)
    };
    let cols = (1..=n).max_by(|a, b| size(*a).total_cmp(&size(*b))).unwrap_or(1);
    let rows = n.div_ceil(cols);
    let cw = size(cols).max(0.0);
    let ch = cw / aspect;
    let grid_w = cw * cols as f32 + gap * (cols - 1) as f32;
    let grid_h = ch * rows as f32 + gap * (rows - 1) as f32;
    let (x0, y0) = (region.x + (region.w - grid_w) / 2.0, region.y + (region.h - grid_h) / 2.0);
    (0..n)
        .map(|i| {
            let (r, c) = (i / cols, i % cols);
            Area { x: x0 + c as f32 * (cw + gap), y: y0 + r as f32 * (ch + gap), w: cw, h: ch }
        })
        .collect()
}

/// A multiview in a `w`×`h` projector → its big boxes (Studio Mode's preview first when
/// `preview`, then what's on air; none for scenes only) and `n` scene boxes.
pub fn multiview_in(
    layout: MultiviewLayout,
    preview: bool,
    n: usize,
    w: f32,
    h: f32,
    aspect: f32,
) -> (Vec<Area>, Vec<Area>) {
    let gap = h * GAP;
    let inner = Area { x: gap, y: gap, w: w - 2.0 * gap, h: h - 2.0 * gap };
    let (big, rest) = match layout {
        MultiviewLayout::ScenesOnly => return (Vec::new(), grid(n, inner, aspect, gap)),
        MultiviewLayout::Top | MultiviewLayout::Bottom => {
            let half = (inner.h - gap) / 2.0;
            let a = Area { h: half, ..inner };
            let b = Area { y: inner.y + half + gap, h: half, ..inner };
            if layout == MultiviewLayout::Top { (a, b) } else { (b, a) }
        }
        MultiviewLayout::Left | MultiviewLayout::Right => {
            let half = (inner.w - gap) / 2.0;
            let a = Area { w: half, ..inner };
            let b = Area { x: inner.x + half + gap, w: half, ..inner };
            if layout == MultiviewLayout::Left { (a, b) } else { (b, a) }
        }
    };
    // Preview and what's on air share their half: side by side, or one above the other.
    let bigs = if !preview {
        vec![big.fit(aspect)]
    } else if matches!(layout, MultiviewLayout::Top | MultiviewLayout::Bottom) {
        let half = (big.w - gap) / 2.0;
        vec![Area { w: half, ..big }.fit(aspect), Area { x: big.x + half + gap, w: half, ..big }.fit(aspect)]
    } else {
        let half = (big.h - gap) / 2.0;
        vec![Area { h: half, ..big }.fit(aspect), Area { y: big.y + half + gap, h: half, ..big }.fit(aspect)]
    };
    (bigs, grid(n, rest, aspect, gap))
}

/// The scene a click lands on in a multiview laid out as [`multiview_in`], if any.
pub fn multiview_hit_in(
    layout: MultiviewLayout,
    preview: bool,
    n: usize,
    (w, h): (f32, f32),
    aspect: f32,
    (x, y): (f32, f32),
) -> Option<usize> {
    multiview_in(layout, preview, n, w, h, aspect).1.iter().position(|a| a.contains(x, y))
}

/// EBU R95's safe areas inside a picture box: action safe (93%), graphics safe (90%) and the
/// 4:3 centre (for old screens), as OBS draws them.
pub fn safe_areas(a: Area) -> [Area; 3] {
    let inset =
        |k: f32| Area { x: a.x + a.w * (1.0 - k) / 2.0, y: a.y + a.h * (1.0 - k) / 2.0, w: a.w * k, h: a.h * k };
    let four_three = Area { x: a.x + (a.w - a.h * 4.0 / 3.0).max(0.0) / 2.0, w: (a.h * 4.0 / 3.0).min(a.w), ..a };
    [inset(0.93), inset(0.90), four_three]
}

/// A multiview of `n` scenes in a `w`×`h` projector: what's on air across the top half, the
/// scenes in a grid below (as many columns as make them biggest), each `aspect` wide.
pub fn multiview(n: usize, w: f32, h: f32, aspect: f32) -> (Area, Vec<Area>) {
    let (big, cells) = multiview_in(MultiviewLayout::Top, false, n, w, h, aspect);
    (big[0], cells)
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

    fn apart(cells: &[Area]) -> bool {
        cells.iter().enumerate().all(|(i, a)| {
            cells[i + 1..].iter().all(|b| a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y)
        })
    }

    #[test]
    fn every_layout_places_the_big_boxes_and_the_scenes_apart() {
        let (w, h) = (1920.0, 1080.0);
        for layout in [
            MultiviewLayout::Top,
            MultiviewLayout::Bottom,
            MultiviewLayout::Left,
            MultiviewLayout::Right,
            MultiviewLayout::ScenesOnly,
        ] {
            for preview in [false, true] {
                let (big, cells) = multiview_in(layout, preview, 8, w, h, WIDE);
                let want = if layout == MultiviewLayout::ScenesOnly {
                    0
                } else if preview {
                    2
                } else {
                    1
                };
                assert_eq!(big.len(), want, "{layout:?} {preview}");
                assert_eq!(cells.len(), 8);
                let all: Vec<Area> = big.iter().chain(&cells).copied().collect();
                assert!(apart(&all), "{layout:?} {preview}: {all:?}");
                assert!(all.iter().all(|a| a.x >= 0.0 && a.y >= 0.0 && a.x + a.w <= w + 0.01 && a.y + a.h <= h + 0.01));
                assert!(all.iter().all(|a| (a.w / a.h - WIDE).abs() < 0.01));
            }
        }
        // Where the big box goes.
        let (top, _) = multiview_in(MultiviewLayout::Top, false, 4, w, h, WIDE);
        let (bottom, _) = multiview_in(MultiviewLayout::Bottom, false, 4, w, h, WIDE);
        let (left, _) = multiview_in(MultiviewLayout::Left, false, 4, w, h, WIDE);
        let (right, _) = multiview_in(MultiviewLayout::Right, false, 4, w, h, WIDE);
        assert!(top[0].y < h / 2.0 && bottom[0].y > h / 2.0 - 1.0);
        assert!(left[0].x < w / 2.0 && right[0].x >= w / 2.0 - 1.0);
        // Studio Mode on top: preview left, what's on air right.
        let (pv, _) = multiview_in(MultiviewLayout::Top, true, 4, w, h, WIDE);
        assert!(pv[0].x < pv[1].x);
        // Scenes only use the whole projector: bigger boxes than with what's on air.
        let (_, only) = multiview_in(MultiviewLayout::ScenesOnly, false, 4, w, h, WIDE);
        let (_, with) = multiview_in(MultiviewLayout::Top, false, 4, w, h, WIDE);
        assert!(only[0].w > with[0].w);
        // A click finds the scene in any layout.
        let (_, cells) = multiview_in(MultiviewLayout::Right, true, 6, w, h, WIDE);
        let c = cells[3];
        assert_eq!(multiview_hit_in(MultiviewLayout::Right, true, 6, (w, h), WIDE, (c.x + 2.0, c.y + 2.0)), Some(3));
    }

    #[test]
    fn safe_areas_sit_inside_the_picture() {
        let a = Area { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 };
        let [action, graphics, centre] = safe_areas(a);
        assert!((action.w - 1785.6).abs() < 0.1 && (graphics.w - 1728.0).abs() < 0.1);
        assert!((centre.w - 1440.0).abs() < 0.1 && (centre.x - 240.0).abs() < 0.1);
        assert!(graphics.x > action.x);
    }

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
