//! Smart vertical (plan item 8, 29 Oct): an item in a vertical scene (a camera, or the whole
//! wide picture as a nested scene) slides sideways so the face in it stays in the middle. The
//! item is made to fill the picture's height, and is never moved so far that an edge shows.
//! Finding the face is Apple's Vision on a Mac (`ffi::vision`); the rest works anywhere.
//! The stage moves the item (`stage::Stage::start_follow`).

use serde::Serialize;

use super::Transform;
#[cfg(target_os = "macos")]
use crate::ffi;

/// How wide the snapshot faces are looked for in is (small: a face fills plenty of it).
pub(crate) const LOOK_WIDTH: u32 = 320;

/// A move smaller than this share of the picture's width isn't made (no jitter).
const DEAD_ZONE: f32 = 0.03;

/// How much of the way to the face each step goes (eased, not a jump).
const EASE: f32 = 0.25;

/// A face's box, 0 to 1 of the picture it was found in, from its top left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Face {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Faces in an RGBA picture (rows top first), biggest first; none where Vision isn't.
pub fn find_faces(rgba: &[u8], w: u32, h: u32) -> Vec<Face> {
    #[cfg(target_os = "macos")]
    {
        ffi::vision::faces(rgba, w, h).into_iter().map(|f| Face { x: f.x, y: f.y, w: f.w, h: f.h }).collect()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (rgba, w, h);
        Vec::new()
    }
}

/// Where the eye goes in an RGBA picture (what stands out: the action in a game), biggest
/// first; none where Vision isn't.
pub fn find_busy(rgba: &[u8], w: u32, h: u32) -> Vec<Face> {
    #[cfg(target_os = "macos")]
    {
        ffi::vision::busy(rgba, w, h).into_iter().map(|f| Face { x: f.x, y: f.y, w: f.w, h: f.h }).collect()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (rgba, w, h);
        Vec::new()
    }
}

/// A face at least this share of the picture's width is the subject (a camera filling it);
/// a smaller one is a facecam in a corner, and the action is followed instead.
const BIG_FACE: f64 = 0.12;

/// Where to keep the eye in an RGBA picture, across (0 to 1): a face big enough to be the
/// subject, else what stands out; None when nothing does.
pub fn focus_x(rgba: &[u8], w: u32, h: u32) -> Option<f32> {
    let pick = |f: &Face| (f.x + f.w / 2.0) as f32;
    find_faces(rgba, w, h)
        .iter()
        .find(|f| f.w >= BIG_FACE)
        .map(pick)
        .or_else(|| find_busy(rgba, w, h).first().map(pick))
}

/// A small picture of what an item shows, to look for faces in.
#[derive(Debug, Clone)]
pub struct FollowLook {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// One item following a face: how it was before (put back when following stops).
#[derive(Debug, Clone)]
pub struct FaceFollow {
    pub scene: String,
    pub id: i64,
    /// Follows what stands out (the action) instead of a face.
    pub action: bool,
    pub(crate) before: Transform,
    /// Looks in a row without a face.
    pub misses: u32,
}

/// Where the item's left edge goes so the face's middle (`face_mid`, 0 to 1 of the item as
/// shown) sits in the middle of a `canvas_w` picture, when the item shows `shown_w` wide.
/// Never past an edge; an item narrower than the picture stays centred.
pub(crate) fn aim(face_mid: f32, shown_w: f32, canvas_w: f32) -> f32 {
    if shown_w <= canvas_w {
        return (canvas_w - shown_w) / 2.0;
    }
    (canvas_w / 2.0 - face_mid.clamp(0.0, 1.0) * shown_w).clamp(canvas_w - shown_w, 0.0)
}

/// One eased step from `now` toward `to`; none inside the dead zone.
pub(crate) fn ease(now: f32, to: f32, canvas_w: f32) -> f32 {
    if (to - now).abs() < canvas_w * DEAD_ZONE { now } else { now + (to - now) * EASE }
}

/// The face's middle as a share of the item as shown (after its crop), from where it was found
/// in the whole source `src_w` wide.
pub(crate) fn shown_mid(face: &Face, src_w: f32, crop_left: f32, crop_right: f32) -> f32 {
    let shown = (src_w - crop_left - crop_right).max(1.0);
    let px = (face.x + face.w / 2.0) as f32 * src_w;
    ((px - crop_left) / shown).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vision finds what stands out: a bright square on the right of a dark picture.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_eye_goes_to_what_stands_out() {
        let (w, h) = (320u32, 180u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 60..120 {
            for x in 230..290 {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[255, 220, 40, 255]);
            }
        }
        let x = focus_x(&rgba, w, h).expect("something stands out");
        // Vision boxes attention loosely: right of the middle is enough.
        assert!(x > 0.55, "{x}");
    }

    #[test]
    fn the_face_goes_to_the_middle_without_showing_an_edge() {
        // A 16:9 picture 3413 wide in a 1080 wide vertical one.
        let w = 3413.0;
        assert_eq!(aim(0.5, w, 1080.0), 540.0 - w / 2.0);
        // A face at the far left or right stops at the edge.
        assert_eq!(aim(0.0, w, 1080.0), 0.0);
        assert_eq!(aim(1.0, w, 1080.0), 1080.0 - w);
        assert_eq!(aim(-3.0, w, 1080.0), 0.0);
        // An item narrower than the picture stays centred.
        assert_eq!(aim(0.9, 800.0, 1080.0), 140.0);
    }

    #[test]
    fn steps_are_eased_and_small_moves_are_skipped() {
        assert_eq!(ease(0.0, -100.0, 1080.0), -25.0);
        assert_eq!(ease(-500.0, -510.0, 1080.0), -500.0);
        // Many steps get there (within the dead zone).
        let mut x = 0.0;
        for _ in 0..40 {
            x = ease(x, -1200.0, 1080.0);
        }
        assert!((x + 1200.0).abs() < 1080.0 * DEAD_ZONE, "{x}");
    }

    #[test]
    fn a_crop_moves_where_the_face_is_on_the_item() {
        let face = Face { x: 0.45, y: 0.2, w: 0.1, h: 0.2 };
        assert!((shown_mid(&face, 1920.0, 0.0, 0.0) - 0.5).abs() < 1e-6);
        // 480 cropped off the left: the face is further right of what shows.
        let m = shown_mid(&face, 1920.0, 480.0, 0.0);
        assert!((m - (960.0 - 480.0) / 1440.0).abs() < 1e-6, "{m}");
        // A face in the cropped-off part pins to the edge, and nothing divides by zero.
        assert_eq!(shown_mid(&Face { x: 0.0, y: 0.0, w: 0.01, h: 0.01 }, 1920.0, 960.0, 0.0), 0.0);
        assert!(shown_mid(&face, 100.0, 60.0, 60.0).is_finite());
    }
}
