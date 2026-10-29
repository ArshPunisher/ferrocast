//! Smart vertical (plan item 8, 29 Oct): an item in a vertical scene (a camera, or the whole
//! wide picture as a nested scene) slides sideways so the face in it stays in the middle. The
//! item is made to fill the picture's height, and is never moved so far that an edge shows.
//! Finding the face is Apple's Vision on a Mac (`ffi::vision`); the rest works anywhere.

use serde::Serialize;

use super::{Collection, Transform};
use crate::StudioError;
use crate::ffi;

/// How wide the snapshot faces are looked for in is (small: a face fills plenty of it).
const LOOK_WIDTH: u32 = 320;

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
    before: Transform,
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

impl Collection {
    /// Starts an item following faces: it fills the picture's height, centred, from its top
    /// left (how it was comes back with [`Collection::stop_follow`]).
    pub fn start_follow(&self, scene: &str, id: i64, action: bool) -> Result<FaceFollow, StudioError> {
        let before = self.transform(scene, id)?;
        let (cw, ch) = self.size_of(self.scene_ptr(scene)?);
        let shown_w = (before.width as i32 - before.crop_left - before.crop_right).max(1) as f32;
        let shown_h = (before.height as i32 - before.crop_top - before.crop_bottom).max(1) as f32;
        if before.width == 0 || before.height == 0 {
            return Err(StudioError::Failed("That source shows no picture yet.".into()));
        }
        let scale = ch as f32 / shown_h;
        let t = Transform {
            x: aim(0.5, shown_w * scale, cw as f32),
            y: 0.0,
            rotation: 0.0,
            scale_x: scale,
            scale_y: scale,
            alignment: 5,
            bounds: "none".into(),
            bounds_alignment: 0,
            bounds_width: 0.0,
            bounds_height: 0.0,
            ..before.clone()
        };
        self.set_transform(scene, id, &t)?;
        Ok(FaceFollow { scene: scene.into(), id, action, before, misses: 0 })
    }

    /// A small picture of what the item's source shows now.
    pub fn follow_look(&self, f: &FaceFollow) -> Result<Option<FollowLook>, StudioError> {
        let item = self.item(&f.scene, f.id)?;
        let a = self.api();
        let src = a.obs_sceneitem_get_source(item);
        let (sw, sh) = (a.obs_source_get_width(src), a.obs_source_get_height(src));
        if sw == 0 || sh == 0 {
            return Ok(None);
        }
        let h = ((LOOK_WIDTH as f32 * sh as f32 / sw as f32).round() as u32).clamp(1, LOOK_WIDTH * 4);
        Ok(ffi::snapshot_scaled(a, src, (sw, sh), LOOK_WIDTH, h).map(|rgba| FollowLook {
            rgba,
            width: LOOK_WIDTH,
            height: h,
        }))
    }

    /// Moves the item a step toward `face` (the biggest found), or keeps it where it is.
    pub fn follow_step(&self, f: &mut FaceFollow, face: Option<Face>) -> Result<(), StudioError> {
        let Some(face) = face else {
            f.misses += 1;
            return Ok(());
        };
        f.misses = 0;
        let t = self.transform(&f.scene, f.id)?;
        let (cw, _) = self.size_of(self.scene_ptr(&f.scene)?);
        let shown_w = (t.width as i32 - t.crop_left - t.crop_right).max(1) as f32 * t.scale_x.abs();
        let mid = shown_mid(&face, t.width as f32, t.crop_left as f32, t.crop_right as f32);
        // Measured from its left edge: where that is depends on how it's anchored.
        let left = match t.alignment & 3 {
            1 => t.x,
            2 => t.x - shown_w,
            _ => t.x - shown_w / 2.0,
        };
        let to = ease(left, aim(mid, shown_w, cw as f32), cw as f32);
        if to != left {
            self.set_transform(&f.scene, f.id, &Transform { x: t.x + (to - left), ..t })?;
        }
        Ok(())
    }

    /// Puts the item back as it was before following (when it's still there).
    pub fn stop_follow(&self, f: &FaceFollow) {
        if self.item(&f.scene, f.id).is_ok() {
            let _ = self.set_transform(&f.scene, f.id, &f.before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
