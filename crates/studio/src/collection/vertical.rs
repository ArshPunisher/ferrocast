//! The vertical picture (libobs 31+): a second canvas, tall, with scenes of its own. Sources
//! are shared with the main picture (one capture, placed differently in each); scenes aren't.
//! It goes out on its own, so phones get a picture made for them instead of one cut from the
//! wide stream.

use std::ptr;

use serde::Serialize;
use serde_json::{Value, json};

use super::{Collection, SceneInfo, no_scene};
use crate::StudioError;
use crate::ffi::{self, cstr, text};

/// The vertical picture's size, as Shorts, TikTok and Reels want it.
pub const VERTICAL_SIZE: (u32, u32) = (1080, 1920);

/// libobs's name for the canvas (also how it's found again when a collection loads).
const CANVAS_NAME: &str = "Ferrocast Vertical";

/// The vertical canvas and its scenes, our reference each.
pub(super) struct Vertical {
    pub(super) canvas: *mut ffi::ObsCanvas,
    pub(super) scenes: Vec<*mut ffi::ObsSource>,
    /// The scene on air in the vertical picture.
    pub(super) current: *mut ffi::ObsSource,
    /// A fade on the canvas's channel 0, so switching vertical scenes looks as smooth as the
    /// main picture's (private).
    pub(super) transition: *mut ffi::ObsSource,
}

/// How the vertical picture stands.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VerticalInfo {
    pub width: u32,
    pub height: u32,
    pub scenes: Vec<SceneInfo>,
    pub current: String,
}

impl Collection {
    fn need_canvases(&self) -> Result<(), StudioError> {
        if self.api().has_canvases() {
            Ok(())
        } else {
            Err(StudioError::Failed(
                "A vertical picture needs OBS 31 or newer. Update OBS, then start the Studio again.".into(),
            ))
        }
    }

    /// The video setup for the vertical canvas: the main one's frame rate and colours, tall.
    fn vertical_video(&self) -> ffi::VideoInfo {
        let c = self.studio.canvas();
        ffi::VideoInfo {
            // Only the main canvas starts the graphics; a canvas's mix doesn't read this.
            graphics_module: ptr::null(),
            fps_num: c.fps,
            fps_den: 1,
            base_width: VERTICAL_SIZE.0,
            base_height: VERTICAL_SIZE.1,
            output_width: VERTICAL_SIZE.0,
            output_height: VERTICAL_SIZE.1,
            output_format: ffi::VIDEO_FORMAT_NV12,
            adapter: 0,
            gpu_conversion: true,
            colorspace: ffi::VIDEO_CS_709,
            range: ffi::VIDEO_RANGE_PARTIAL,
            scale_type: ffi::OBS_SCALE_BICUBIC,
        }
    }

    /// Makes the vertical picture's fade and puts it on the canvas.
    fn vertical_transition(&self, canvas: *mut ffi::ObsCanvas) -> *mut ffi::ObsSource {
        let a = self.api();
        let (id, n) = (cstr("fade_transition"), cstr("Vertical transition"));
        let t = a.obs_source_create_private(id.as_ptr(), n.as_ptr(), ptr::null_mut());
        if !t.is_null() {
            a.canvas_set_channel(canvas, 0, t);
        }
        t
    }

    /// Turns the vertical picture on, with one empty scene called `first_scene` (nothing
    /// happens when it's on already).
    pub fn start_vertical(&self, first_scene: &str) -> Result<(), StudioError> {
        if self.vertical.borrow().is_some() {
            return Ok(());
        }
        self.need_canvases()?;
        self.name_free(first_scene)?;
        let mut video = self.vertical_video();
        let canvas = self.api().canvas_create(&cstr(CANVAS_NAME), &mut video, ffi::CANVAS_ACTIVATE);
        if canvas.is_null() {
            return Err(StudioError::Failed("The vertical picture couldn't be made.".into()));
        }
        let transition = self.vertical_transition(canvas);
        self.vertical.replace(Some(Vertical { canvas, scenes: Vec::new(), current: ptr::null_mut(), transition }));
        let made = self.add_vertical_scene(first_scene);
        if made.is_err() {
            self.drop_vertical();
            return made;
        }
        let first = self.vertical.borrow().as_ref().map(|v| v.scenes[0]).unwrap_or(ptr::null_mut());
        self.vertical_go(first, 0);
        Ok(())
    }

    /// Turns the vertical picture off: its canvas and scenes go (the sources stay where else
    /// they're used).
    pub fn stop_vertical(&self) {
        self.drop_vertical();
    }

    /// Lets go of the vertical canvas and its scenes.
    pub(super) fn drop_vertical(&self) {
        let Some(v) = self.vertical.replace(None) else { return };
        let a = self.api();
        a.canvas_set_channel(v.canvas, 0, ptr::null_mut());
        if !v.transition.is_null() {
            a.obs_source_release(v.transition);
        }
        for s in v.scenes {
            a.obs_source_remove(s);
            a.obs_source_release(s);
        }
        a.canvas_remove_release(v.canvas);
    }

    pub fn vertical(&self) -> Option<VerticalInfo> {
        let v = self.vertical.borrow();
        let v = v.as_ref()?;
        let a = self.api();
        Some(VerticalInfo {
            width: VERTICAL_SIZE.0,
            height: VERTICAL_SIZE.1,
            scenes: v
                .scenes
                .iter()
                .map(|s| SceneInfo {
                    name: text(a.obs_source_get_name(*s)),
                    items: self.raw_items(a.obs_scene_from_source(*s)).len(),
                    current: *s == v.current,
                    preview: false,
                    transition: None,
                })
                .collect(),
            current: if v.current.is_null() { String::new() } else { text(a.obs_source_get_name(v.current)) },
        })
    }

    /// The vertical canvas, when it's on.
    pub(crate) fn vertical_canvas(&self) -> Option<*mut ffi::ObsCanvas> {
        self.vertical.borrow().as_ref().map(|v| v.canvas)
    }

    /// The vertical picture's video, for an encoder (None when it's off).
    pub(crate) fn vertical_video_out(&self) -> Option<*mut ffi::VideoT> {
        let v = self.api().canvas_get_video(self.vertical_canvas()?);
        (!v.is_null()).then_some(v)
    }

    /// A preview of the vertical picture, as `Studio::preview` (macOS, main thread).
    pub fn vertical_preview(
        &self,
        window: raw_window_handle::RawWindowHandle,
        rect: crate::Rect,
        scale: f64,
    ) -> Result<crate::Preview, StudioError> {
        let canvas = self.vertical_canvas().ok_or_else(|| StudioError::Failed("The vertical picture is off.".into()))?;
        self.studio.preview_on(window, rect, scale, canvas, VERTICAL_SIZE)
    }

    /// The vertical scene called `name`.
    pub(super) fn vertical_scene(&self, name: &str) -> Option<*mut ffi::ObsSource> {
        let a = self.api();
        self.vertical.borrow().as_ref()?.scenes.iter().copied().find(|s| text(a.obs_source_get_name(*s)) == name)
    }

    /// Whether a scene (a scene source or a group's) belongs to the vertical picture.
    pub(super) fn is_vertical(&self, scene: *mut ffi::ObsScene) -> bool {
        let Some(canvas) = self.vertical_canvas() else { return false };
        self.api().canvas_of(self.api().obs_scene_get_source(scene)) == canvas
    }

    /// The size of the picture a scene is drawn in (the main canvas, or the vertical one).
    pub(super) fn size_of(&self, scene: *mut ffi::ObsScene) -> (u32, u32) {
        if self.is_vertical(scene) {
            VERTICAL_SIZE
        } else {
            let c = self.studio.canvas();
            (c.width, c.height)
        }
    }

    pub fn add_vertical_scene(&self, name: &str) -> Result<(), StudioError> {
        self.need_canvases()?;
        let canvas = self.vertical_canvas().ok_or_else(|| StudioError::Failed("Turn the vertical picture on first.".into()))?;
        self.name_free(name)?;
        let a = self.api();
        let n = cstr(name);
        let scene = a.canvas_scene_create(canvas, &n);
        if scene.is_null() {
            return Err(StudioError::Failed(format!("the scene \"{name}\" couldn't be made")));
        }
        let src = a.obs_source_get_ref(a.obs_scene_get_source(scene));
        a.obs_scene_release(scene);
        if let Some(v) = self.vertical.borrow_mut().as_mut() {
            v.scenes.push(src);
        }
        Ok(())
    }

    /// Takes a vertical scene away (never the last one); if it was on air, the one before it
    /// goes on.
    pub(super) fn remove_vertical_scene(&self, src: *mut ffi::ObsSource) -> Result<(), StudioError> {
        let next = {
            let mut v = self.vertical.borrow_mut();
            let Some(v) = v.as_mut() else { return Ok(()) };
            if v.scenes.len() == 1 {
                return Err(StudioError::Failed(
                    "The vertical picture keeps at least one scene. Add another first.".into(),
                ));
            }
            let at = v.scenes.iter().position(|s| *s == src).unwrap_or(0);
            v.scenes.remove(at);
            (v.current == src).then(|| v.scenes[at.saturating_sub(1)])
        };
        if let Some(next) = next {
            self.vertical_go(next, 0);
        }
        let a = self.api();
        a.obs_source_remove(src);
        a.obs_source_release(src);
        Ok(())
    }

    pub(super) fn move_vertical_scene(&self, src: *mut ffi::ObsSource, to: usize) {
        if let Some(v) = self.vertical.borrow_mut().as_mut()
            && let Some(at) = v.scenes.iter().position(|s| *s == src)
        {
            let s = v.scenes.remove(at);
            let to = to.min(v.scenes.len());
            v.scenes.insert(to, s);
        }
    }

    /// Puts a vertical scene on air (a fade of `ms`; 0 cuts).
    pub(super) fn vertical_go(&self, scene: *mut ffi::ObsSource, ms: u32) {
        let a = self.api();
        let mut v = self.vertical.borrow_mut();
        let Some(v) = v.as_mut() else { return };
        if !v.transition.is_null()
            && (ms == 0 || !a.obs_transition_start(v.transition, ffi::OBS_TRANSITION_MODE_AUTO, ms, scene))
        {
            a.obs_transition_set(v.transition, scene);
        }
        v.current = scene;
    }

    /// Puts the vertical scene called `name` on air with the Studio's transition time.
    pub fn set_vertical_current(&self, name: &str) -> Result<(), StudioError> {
        let src = self.vertical_scene(name).ok_or_else(|| no_scene(name))?;
        let ms = self.transition_info.borrow().ms;
        self.vertical_go(src, ms);
        Ok(())
    }

    /// The vertical picture as saved in the collection.
    pub(super) fn save_vertical(&self) -> Option<Value> {
        let v = self.vertical.borrow();
        let v = v.as_ref()?;
        let a = self.api();
        let data = a.canvas_save(v.canvas);
        let canvas = self.data_json(data);
        if !data.is_null() {
            a.obs_data_release(data);
        }
        Some(json!({
            "canvas": canvas,
            "scene_order": v.scenes.iter().map(|s| json!({ "name": text(a.obs_source_get_name(*s)) })).collect::<Vec<_>>(),
            "current_scene": if v.current.is_null() { String::new() } else { text(a.obs_source_get_name(v.current)) },
        }))
    }

    /// Loads the vertical canvas saved in a collection, before its sources load (so its scenes
    /// find it). Returns it, or None (none saved, or this libobs has no canvases).
    pub(super) fn load_vertical_canvas(&self, saved: &Value) -> Option<*mut ffi::ObsCanvas> {
        let a = self.api();
        if !a.has_canvases() {
            return None;
        }
        let canvas_json = saved.get("canvas").filter(|c| c.is_object())?;
        let s = cstr(&canvas_json.to_string());
        let data = a.obs_data_create_from_json(s.as_ptr());
        if data.is_null() {
            return None;
        }
        let canvas = a.canvas_load(data);
        a.obs_data_release(data);
        if canvas.is_null() {
            return None;
        }
        // Made the size and rate ours whatever was saved (and given a mix if it had none).
        let mut video = self.vertical_video();
        a.canvas_reset_video(canvas, &mut video);
        Some(canvas)
    }

    /// After the sources loaded: the vertical scenes are taken out of `scenes` (they're on
    /// `canvas`) and put on the vertical picture, in the saved order.
    pub(super) fn adopt_vertical(
        &self,
        canvas: *mut ffi::ObsCanvas,
        saved: &Value,
        scenes: &mut Vec<*mut ffi::ObsSource>,
    ) {
        let a = self.api();
        let mut mine = Vec::new();
        scenes.retain(|s| {
            if a.canvas_of(*s) == canvas {
                mine.push(*s);
                false
            } else {
                true
            }
        });
        let order: Vec<String> = saved
            .get("scene_order")
            .and_then(Value::as_array)
            .map(|v| v.iter().filter_map(|s| s.get("name")?.as_str().map(String::from)).collect())
            .unwrap_or_default();
        mine.sort_by_key(|s| {
            let n = text(a.obs_source_get_name(*s));
            order.iter().position(|o| *o == n).unwrap_or(usize::MAX)
        });
        let transition = self.vertical_transition(canvas);
        self.vertical.replace(Some(Vertical { canvas, scenes: mine, current: ptr::null_mut(), transition }));
        let empty = self.vertical.borrow().as_ref().is_some_and(|v| v.scenes.is_empty());
        if empty && self.add_vertical_scene("Vertical").is_err() {
            self.drop_vertical();
            return;
        }
        let want = saved.get("current_scene").and_then(Value::as_str).and_then(|n| self.vertical_scene(n));
        let first = self.vertical.borrow().as_ref().map(|v| v.scenes[0]).unwrap_or(ptr::null_mut());
        self.vertical_go(want.unwrap_or(first), 0);
    }
}
