//! The Move transition (what OBS's Move plugin does): going to a scene, each source it shares
//! with the scene on air starts where it was there and glides to its own place (size, turn,
//! crop and all), eased in and out. Underneath it's a cut, so the new scene draws as it is:
//! crops, filters and groups all work. The app calls `tick_moves` each frame while `moving`.
//! Items that change alignment or bounding box kind, and Studio Mode, just cut.

use std::time::Instant;

use super::{BOUNDS, Collection, Transform};
use crate::{Studio, ffi};

/// The kind's id, as transitions are named.
pub(crate) const MOVE: &str = "ferrocast_move";

/// How long a Move lasts when the transition has no time of its own.
const DEFAULT_MS: u32 = 500;

/// One item gliding: our reference to it, where it starts and where it ends.
struct Glide {
    item: *mut ffi::ObsSceneItem,
    from: Transform,
    to: Transform,
    /// `to`'s bounding box kind, an index of [`BOUNDS`].
    bounds: usize,
}

/// A Move under way.
pub(crate) struct MoveRun {
    studio: Studio,
    glides: Vec<Glide>,
    start: Instant,
    ms: u32,
}

impl Drop for MoveRun {
    fn drop(&mut self) {
        for g in &self.glides {
            self.studio.inner.api.obs_sceneitem_release(g.item);
        }
    }
}

/// Slow, fast, slow (cubic).
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// `a` to `b` at `t` (0 to 1): numbers straight, a turn the short way round, crops whole.
fn between(a: &Transform, b: &Transform, t: f32) -> Transform {
    let mix = |x: f32, y: f32| x + (y - x) * t;
    let crop = |x: i32, y: i32| (x as f32 + (y - x) as f32 * t).round() as i32;
    let turn = ((b.rotation - a.rotation + 540.0).rem_euclid(360.0)) - 180.0;
    Transform {
        x: mix(a.x, b.x),
        y: mix(a.y, b.y),
        rotation: a.rotation + turn * t,
        scale_x: mix(a.scale_x, b.scale_x),
        scale_y: mix(a.scale_y, b.scale_y),
        bounds_width: mix(a.bounds_width, b.bounds_width),
        bounds_height: mix(a.bounds_height, b.bounds_height),
        crop_left: crop(a.crop_left, b.crop_left),
        crop_top: crop(a.crop_top, b.crop_top),
        crop_right: crop(a.crop_right, b.crop_right),
        crop_bottom: crop(a.crop_bottom, b.crop_bottom),
        ..b.clone()
    }
}

/// Whether `a` can glide to `b`: the same anchor and kind of bounding box.
fn can_glide(a: &Transform, b: &Transform) -> bool {
    a.alignment == b.alignment && a.bounds == b.bounds && a.bounds_alignment == b.bounds_alignment && a != b
}

impl Collection {
    /// Before going to `to`: what it shares with the scene on air is put where it is there, and
    /// the glides to its own places are returned (None: nothing to glide).
    pub(super) fn plan_move(&self, to: *mut ffi::ObsSource, ms: u32) -> Option<MoveRun> {
        let from = *self.current.borrow();
        if from.is_null() || from == to {
            return None;
        }
        let a = self.api();
        let (sa, sb) = (a.obs_scene_from_source(from), a.obs_scene_from_source(to));
        if sa.is_null() || sb.is_null() {
            return None;
        }
        let plain = |i: &*mut ffi::ObsSceneItem| !a.obs_sceneitem_is_group(*i) && a.obs_sceneitem_visible(*i);
        let there: Vec<_> = self.raw_items(sa).into_iter().filter(plain).collect();
        let mut taken = Vec::new();
        let mut glides = Vec::new();
        for item in self.raw_items(sb).into_iter().filter(plain) {
            let src = a.obs_sceneitem_get_source(item);
            // The same source on air (the first one not matched yet).
            let Some(old) = there.iter().find(|o| a.obs_sceneitem_get_source(**o) == src && !taken.contains(*o)) else {
                continue;
            };
            taken.push(*old);
            let (start, end) = (self.transform_of(*old), self.transform_of(item));
            let Some(bounds) = BOUNDS.iter().position(|b| *b == end.bounds) else { continue };
            if !can_glide(&start, &end) {
                continue;
            }
            self.apply_transform(item, &start, bounds);
            a.obs_sceneitem_addref(item);
            glides.push(Glide { item, from: start, to: end, bounds });
        }
        (!glides.is_empty()).then(|| MoveRun {
            studio: self.studio.clone(),
            glides,
            start: Instant::now(),
            ms: if ms == 0 { DEFAULT_MS } else { ms },
        })
    }

    /// A Move is under way (call `tick_moves` each frame until it's not).
    pub fn moving(&self) -> bool {
        self.moves.borrow().is_some()
    }

    /// Moves each gliding item to where it is now → still moving.
    pub fn tick_moves(&self) -> bool {
        let done = {
            let run = self.moves.borrow();
            let Some(run) = run.as_ref() else { return false };
            let t = run.start.elapsed().as_secs_f32() * 1000.0 / run.ms as f32;
            let k = ease(t);
            for g in &run.glides {
                self.apply_transform(g.item, &between(&g.from, &g.to, k), g.bounds);
            }
            t >= 1.0
        };
        if done {
            self.finish_moves();
        }
        !done
    }

    /// Puts each gliding item in its own place without ending the Move (to save it so; the
    /// next tick puts it back on its way) → a Move is under way.
    pub(super) fn settle_moves(&self) -> bool {
        let run = self.moves.borrow();
        let Some(run) = run.as_ref() else { return false };
        for g in &run.glides {
            self.apply_transform(g.item, &g.to, g.bounds);
        }
        true
    }

    /// Ends a Move at once, every item in its own place.
    pub fn finish_moves(&self) {
        let Some(run) = self.moves.borrow_mut().take() else { return };
        for g in &run.glides {
            self.apply_transform(g.item, &g.to, g.bounds);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32, rotation: f32, crop: i32) -> Transform {
        Transform {
            x,
            y,
            rotation,
            scale_x: 1.0,
            scale_y: 1.0,
            alignment: 5,
            bounds: "none".into(),
            bounds_alignment: 0,
            bounds_width: 0.0,
            bounds_height: 0.0,
            crop_left: crop,
            crop_top: 0,
            crop_right: 0,
            crop_bottom: 0,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn it_eases_and_goes_half_way_in_the_middle() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!((ease(0.5) - 0.5).abs() < 1e-6);
        assert!(ease(0.1) < 0.1 && ease(0.9) > 0.9, "slow at the ends");
        let mid = between(&at(0.0, 0.0, 0.0, 0), &at(100.0, 50.0, 90.0, 10), 0.5);
        assert_eq!((mid.x, mid.y, mid.rotation, mid.crop_left), (50.0, 25.0, 45.0, 5));
    }

    #[test]
    fn a_turn_goes_the_short_way_round() {
        let mid = between(&at(0.0, 0.0, 350.0, 0), &at(0.0, 0.0, 10.0, 0), 0.5);
        assert!((mid.rotation.rem_euclid(360.0)).abs() < 1e-3, "{}", mid.rotation);
    }

    #[test]
    fn only_the_same_anchor_and_box_glide() {
        let a = at(0.0, 0.0, 0.0, 0);
        assert!(can_glide(&a, &at(10.0, 0.0, 0.0, 0)));
        assert!(!can_glide(&a, &a.clone()), "nothing to move");
        assert!(!can_glide(&a, &Transform { alignment: 0, ..at(10.0, 0.0, 0.0, 0) }));
        assert!(!can_glide(&a, &Transform { bounds: "fit".into(), ..at(10.0, 0.0, 0.0, 0) }));
    }
}
