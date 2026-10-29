//! libobs's C API, loaded at runtime (`libloading`), so Ferrocast runs and builds where OBS's
//! engine isn't there. The only file in the workspace allowed `unsafe` (LESSONS R-73): every
//! block says why it is sound. Layouts match libobs 32.2 (`obs.h`, `graphics/graphics.h`).
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;

use libloading::Library;

/// libobs's `calldata_t`: arguments for a procedure (here always on our own stack, `fixed`).
#[repr(C)]
pub struct CallData {
    pub stack: *mut u8,
    pub size: usize,
    pub capacity: usize,
    pub fixed: bool,
}

impl CallData {
    /// As `calldata_init_fixed`: on `stack`, empty (its first word, zeroed, ends the list).
    pub fn fixed(stack: &mut [u8]) -> CallData {
        stack.iter_mut().take(std::mem::size_of::<usize>()).for_each(|b| *b = 0);
        CallData { stack: stack.as_mut_ptr(), size: std::mem::size_of::<usize>(), capacity: stack.len(), fixed: true }
    }
}

/// `media_remux_progress_callback`: percent done → keep going.
pub type RemuxCb = unsafe extern "C" fn(*mut c_void, f32) -> bool;

/// Opaque libobs objects.
macro_rules! opaque {
    ($($n:ident),*) => { $( #[repr(C)] pub struct $n { _p: [u8; 0] } )* };
}
opaque!(
    ObsView,
    ObsCanvas,
    ProcHandler,
    RemuxJob,
    ObsSource,
    ObsScene,
    ObsSceneItem,
    ObsData,
    ObsProperties,
    ObsProperty,
    ObsEncoder,
    ObsOutput,
    ObsService,
    ObsDisplay,
    ObsDataArray,
    ObsVolmeter,
    CpuUsage,
    GsTexture,
    GsTexrender,
    GsStagesurf,
    GsEffect,
    GsEparam,
    VideoT,
    AudioT
);

#[repr(C)]
#[derive(Debug)]
pub struct VideoInfo {
    pub graphics_module: *const c_char,
    pub fps_num: u32,
    pub fps_den: u32,
    pub base_width: u32,
    pub base_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub output_format: c_int,
    pub adapter: u32,
    pub gpu_conversion: bool,
    pub colorspace: c_int,
    pub range: c_int,
    pub scale_type: c_int,
}

/// `obs_audio_info2`: the sound setup with how far sources may fall behind (OBS 30+).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioInfo2 {
    pub samples_per_sec: u32,
    pub speakers: c_int,
    /// 0 for libobs's own (about a second).
    pub max_buffering_ms: u32,
    pub fixed_buffering: bool,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

/// `struct vec4` (SSE-aligned).
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Default)]
pub struct Vec4 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

/// `struct matrix4`: a point maps to `x * px + y * py + t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Matrix4 {
    pub x: Vec4,
    pub y: Vec4,
    pub z: Vec4,
    pub t: Vec4,
}

/// `struct gs_init_data`: on macOS the window is an `NSView *`, on Windows an `HWND`.
#[repr(C)]
#[derive(Debug)]
pub struct GsInitData {
    #[cfg(not(target_os = "linux"))]
    pub window: *mut c_void,
    #[cfg(target_os = "linux")]
    pub window_id: u32,
    #[cfg(target_os = "linux")]
    pub display: *mut c_void,
    pub cx: u32,
    pub cy: u32,
    pub num_backbuffers: u32,
    pub format: c_int,
    pub zsformat: c_int,
    pub adapter: u32,
}

// Enum values (libobs 32.2).
pub const VIDEO_FORMAT_NV12: c_int = 2;
pub const VIDEO_CS_709: c_int = 2;
pub const VIDEO_RANGE_PARTIAL: c_int = 1;
pub const OBS_SCALE_BICUBIC: c_int = 2;
pub const SPEAKERS_MONO: c_int = 1;
pub const SPEAKERS_STEREO: c_int = 2;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const GS_BGRA: c_int = 5;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const GS_ZS_NONE: c_int = 0;
pub const OBS_VIDEO_SUCCESS: c_int = 0;
/// `obs-source.h`: a kind replaced by a newer version, and one only for OBS's own use.
pub const OBS_SOURCE_DEPRECATED: u32 = 1 << 8;
pub const OBS_SOURCE_CAP_DISABLED: u32 = 1 << 10;
pub const OBS_BOUNDS_NONE: c_int = 0;
pub const OBS_BOUNDS_STRETCH: c_int = 1;
pub const OBS_BOUNDS_SCALE_INNER: c_int = 2;
pub const OBS_ALIGN_CENTER: u32 = 0;
pub const OBS_ALIGN_LEFT: u32 = 1;
pub const OBS_ALIGN_TOP: u32 = 1 << 2;
pub const OBS_ALIGN_RIGHT: u32 = 1 << 1;
pub const OBS_ALIGN_BOTTOM: u32 = 1 << 3;

/// `struct obs_sceneitem_crop`, pixels off each side.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Crop {
    pub left: c_int,
    pub top: c_int,
    pub right: c_int,
    pub bottom: c_int,
}
/// `obs-source.h`: the kind makes sound.
pub const OBS_SOURCE_AUDIO: u32 = 1 << 1;
/// `OBS_SOURCE_ASYNC`: frames come from a device or file (what deinterlacing applies to).
pub const OBS_SOURCE_ASYNC: u32 = 1 << 2;
/// `OBS_SOURCE_CONTROLLABLE_MEDIA`: a video, playlist or slideshow with play controls.
pub const OBS_SOURCE_CONTROLLABLE_MEDIA: u32 = 1 << 13;
/// `enum obs_order_movement`.
pub const OBS_ORDER_MOVE_UP: c_int = 0;
pub const OBS_ORDER_MOVE_DOWN: c_int = 1;
pub const OBS_ORDER_MOVE_TOP: c_int = 2;
pub const OBS_ORDER_MOVE_BOTTOM: c_int = 3;
/// `enum obs_scene_duplicate_type`: the copy shows the same sources.
pub const OBS_SCENE_DUP_REFS: c_int = 0;
/// A private copy showing the same sources (Studio Mode's "Duplicate scene").
pub const OBS_SCENE_DUP_PRIVATE_REFS: c_int = 2;
/// A private copy with private copies of its sources ("Duplicate sources").
pub const OBS_SCENE_DUP_PRIVATE_COPY: c_int = 3;
/// `OBS_TRANSITION_SOURCE_A`: what a transition goes from.
pub const OBS_TRANSITION_SOURCE_A: c_int = 0;
/// `enum obs_fader_type`: dB on a log scale, as OBS's mixer.
pub const OBS_FADER_LOG: c_int = 2;
/// `enum obs_transition_mode`.
pub const OBS_TRANSITION_MODE_AUTO: c_int = 0;
pub const OBS_TRANSITION_MODE_MANUAL: c_int = 1;
/// `OBS_SOURCE_FLAG_FORCE_MONO`.
pub const OBS_SOURCE_FLAG_FORCE_MONO: u32 = 1 << 1;
/// `media-io/audio-io.h`: the meter callback's arrays hold this many channels.
pub const MAX_AUDIO_CHANNELS: usize = 8;
/// `obs_output_set_delay`: a reconnect keeps the delay's cut-off point.
pub const OBS_OUTPUT_DELAY_PRESERVE: u32 = 1;
/// `obs_reset_video`: outputs are running.
pub const OBS_VIDEO_CURRENTLY_ACTIVE: c_int = -4;
pub const LOG_ERROR: c_int = 100;
pub const LOG_WARNING: c_int = 200;
pub const LOG_INFO: c_int = 300;

pub type LogHandler = unsafe extern "C" fn(c_int, *const c_char, *mut c_void, *mut c_void);
pub type DrawCallback = unsafe extern "C" fn(*mut c_void, u32, u32);
pub type SceneItemCb = unsafe extern "C" fn(*mut ObsScene, *mut ObsSceneItem, *mut c_void) -> bool;
pub type SourceEnumCb = unsafe extern "C" fn(*mut ObsSource, *mut ObsSource, *mut c_void);
pub type LoadSourceCb = unsafe extern "C" fn(*mut c_void, *mut ObsSource);
pub type SaveFilterCb = unsafe extern "C" fn(*mut c_void, *mut ObsSource) -> bool;
pub type VolmeterCb = unsafe extern "C" fn(*mut c_void, *const f32, *const f32, *const f32);
pub type AudioDeviceCb = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> bool;
/// libobs's `obs_source_audio_capture_t`: a source's audio, after its filters.
pub type AudioCaptureCb = unsafe extern "C" fn(*mut c_void, *mut ObsSource, *const AudioData, bool);

/// libobs's `struct audio_data`: up to 8 planes of `frames` samples each.
#[repr(C)]
#[derive(Debug)]
pub struct AudioData {
    pub data: [*mut u8; 8],
    pub frames: u32,
    pub timestamp: u64,
}

macro_rules! api {
    ($( $name:ident : fn($($arg:ident : $a:ty),*) $(-> $r:ty)?; )*) => {
        // One table for every system: the preview's functions are used on macOS only so far.
        #[allow(non_snake_case, dead_code)]
        struct Fns {
            $( $name: unsafe extern "C" fn($($a),*) $(-> $r)?, )*
        }

        /// libobs, loaded. Each method calls the libobs function of the same name.
        pub(crate) struct Api {
            _lib: Library,
            _deps: Vec<Library>,
            fns: Fns,
            /// Functions only newer libobs has (looked up, not required).
            pub(crate) newer: Newer,
        }

        impl std::fmt::Debug for Api {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("libobs")
            }
        }

        impl Api {
            /// Loads libobs from `path` and looks up every function used. `deps` is a folder of
            /// libraries libobs finds by `@rpath` (an OBS.app's Frameworks): loaded first, so
            /// they're found from any program. Ferrocast.app carries its own where libobs looks.
            pub(crate) fn load(path: &Path, deps: Option<&Path>) -> Result<Api, String> {
                // libobs needs ffmpeg loaded first; some deps (obs-frontend-api) need libobs:
                // what couldn't load before it is tried again after.
                let (mut _deps, left) = deps.map(load_deps).unwrap_or_default();
                // SAFETY: loading libobs runs its initialisers, which only set up its own state.
                let lib = unsafe { open(path) }.map_err(|e| e.to_string())?;
                _deps.extend(load_all(left).0);
                let fns = Fns {
                    $(
                        // SAFETY: the symbol has exactly this C signature in libobs 32.2's
                        // headers; the pointer stays valid while `_lib` lives, as long as `Api`.
                        $name: unsafe {
                            *lib.get::<unsafe extern "C" fn($($a),*) $(-> $r)?>(concat!(stringify!($name), "\0").as_bytes())
                                .map_err(|e| format!("{}: {e}", stringify!($name)))?
                        },
                    )*
                };
                let newer = Newer::load(&lib);
                Ok(Api { _lib: lib, _deps, fns, newer })
            }

            $(
                #[allow(clippy::too_many_arguments, clippy::missing_safety_doc, dead_code)]
                pub(crate) fn $name(&self, $($arg: $a),*) $(-> $r)? {
                    // SAFETY: callers pass handles libobs gave out (or NULL where libobs allows
                    // it, it checks) and C strings that outlive the call; libobs copies what it
                    // keeps. Handles are released once, by the owning wrapper in lib.rs.
                    unsafe { (self.fns.$name)($($arg),*) }
                }
            )*
        }
    };
}

/// `struct obs_runtime_module_info` (libobs 33): a folder of modules and what kind they are.
#[repr(C)]
pub(crate) struct RuntimeModuleInfo {
    pub binary: *const c_char,
    pub data: *const c_char,
    pub kind: c_int,
    pub name: *const c_char,
}

/// `struct obs_module_failure_info`.
#[repr(C)]
pub(crate) struct ModuleFailureInfo {
    pub failed_modules: *mut *mut c_char,
    pub count: usize,
}

/// `enum obs_runtime_module_type` OBS_MODULE_TYPE_PLUGIN.
pub(crate) const OBS_MODULE_TYPE_PLUGIN: c_int = 2;

/// libobs functions that only some versions have.
pub(crate) struct Newer {
    /// libobs 33+: loads the modules found at a path. Its `obs_load_all_modules` does nothing,
    /// and its core-module loader looks inside the running app's own bundle (Ferrocast's, not
    /// OBS's), so OBS's plugins are loaded through this instead.
    pub load_plugins: Option<unsafe extern "C" fn(*mut RuntimeModuleInfo, *mut ModuleFailureInfo)>,
    pub failure_free: Option<unsafe extern "C" fn(*mut ModuleFailureInfo)>,
    /// libobs 31+: more than one canvas (the vertical picture).
    pub canvas: Option<CanvasFns>,
}

/// `enum obs_canvas_flags` ACTIVATE: sources in the canvas's scenes play when they show.
pub(crate) const CANVAS_ACTIVATE: u32 = 1 << 1;

/// libobs 31+'s canvas functions (all there or none).
#[derive(Clone, Copy)]
pub(crate) struct CanvasFns {
    pub create: unsafe extern "C" fn(*const c_char, *mut VideoInfo, u32) -> *mut ObsCanvas,
    pub remove: unsafe extern "C" fn(*mut ObsCanvas),
    pub release: unsafe extern "C" fn(*mut ObsCanvas),
    pub scene_create: unsafe extern "C" fn(*mut ObsCanvas, *const c_char) -> *mut ObsScene,
    pub set_channel: unsafe extern "C" fn(*mut ObsCanvas, u32, *mut ObsSource),
    pub get_video: unsafe extern "C" fn(*const ObsCanvas) -> *mut VideoT,
    pub reset_video: unsafe extern "C" fn(*mut ObsCanvas, *mut VideoInfo) -> bool,
    pub save: unsafe extern "C" fn(*mut ObsCanvas) -> *mut ObsData,
    pub load: unsafe extern "C" fn(*mut ObsData) -> *mut ObsCanvas,
    pub source_canvas: unsafe extern "C" fn(*const ObsSource) -> *mut ObsCanvas,
    pub render_texture: unsafe extern "C" fn(*mut ObsCanvas),
    pub source_by_name: unsafe extern "C" fn(*mut ObsCanvas, *const c_char) -> *mut ObsSource,
}

impl Newer {
    fn load(lib: &Library) -> Newer {
        // SAFETY: when present, these symbols have exactly these C signatures in libobs 33's
        // obs.h; the pointers stay valid while `lib` lives, as long as `Api`.
        unsafe {
            Newer {
                load_plugins: lib
                    .get::<unsafe extern "C" fn(*mut RuntimeModuleInfo, *mut ModuleFailureInfo)>(b"obs_load_plugins\0")
                    .ok()
                    .map(|f| *f),
                failure_free: lib
                    .get::<unsafe extern "C" fn(*mut ModuleFailureInfo)>(b"obs_module_failure_info_free\0")
                    .ok()
                    .map(|f| *f),
                canvas: (|| {
                    Some(CanvasFns {
                        create: *lib.get(b"obs_canvas_create\0").ok()?,
                        remove: *lib.get(b"obs_canvas_remove\0").ok()?,
                        release: *lib.get(b"obs_canvas_release\0").ok()?,
                        scene_create: *lib.get(b"obs_canvas_scene_create\0").ok()?,
                        set_channel: *lib.get(b"obs_canvas_set_channel\0").ok()?,
                        get_video: *lib.get(b"obs_canvas_get_video\0").ok()?,
                        reset_video: *lib.get(b"obs_canvas_reset_video\0").ok()?,
                        save: *lib.get(b"obs_save_canvas\0").ok()?,
                        load: *lib.get(b"obs_load_canvas\0").ok()?,
                        source_canvas: *lib.get(b"obs_source_get_canvas\0").ok()?,
                        render_texture: *lib.get(b"obs_render_canvas_texture\0").ok()?,
                        source_by_name: *lib.get(b"obs_canvas_get_source_by_name\0").ok()?,
                    })
                })(),
            }
        }
    }
}

/// The canvas functions, safe to call: each does nothing (or gives null) without libobs 31+.
/// Callers pass canvases these gave out, live sources and data, as for the `Api` methods.
impl Api {
    pub(crate) fn has_canvases(&self) -> bool {
        self.newer.canvas.is_some()
    }

    pub(crate) fn canvas_create(&self, name: &CStr, video: &mut VideoInfo, flags: u32) -> *mut ObsCanvas {
        // SAFETY: obs_canvas_create copies the name and the video setup, which outlive the call.
        self.newer.canvas.map_or(std::ptr::null_mut(), |f| unsafe { (f.create)(name.as_ptr(), video, flags) })
    }

    /// Removes a canvas and lets go of our reference.
    pub(crate) fn canvas_remove_release(&self, canvas: *mut ObsCanvas) {
        if let Some(f) = self.newer.canvas
            && !canvas.is_null()
        {
            // SAFETY: a canvas libobs gave us, with our one reference, let go once.
            unsafe {
                (f.remove)(canvas);
                (f.release)(canvas);
            }
        }
    }

    pub(crate) fn canvas_scene_create(&self, canvas: *mut ObsCanvas, name: &CStr) -> *mut ObsScene {
        // SAFETY: a live canvas; libobs copies the name.
        self.newer.canvas.map_or(std::ptr::null_mut(), |f| unsafe { (f.scene_create)(canvas, name.as_ptr()) })
    }

    pub(crate) fn canvas_set_channel(&self, canvas: *mut ObsCanvas, channel: u32, source: *mut ObsSource) {
        if let Some(f) = self.newer.canvas {
            // SAFETY: a live canvas and a live source or null (libobs takes its own reference).
            unsafe { (f.set_channel)(canvas, channel, source) }
        }
    }

    pub(crate) fn canvas_get_video(&self, canvas: *mut ObsCanvas) -> *mut VideoT {
        // SAFETY: a live canvas.
        self.newer.canvas.map_or(std::ptr::null_mut(), |f| unsafe { (f.get_video)(canvas) })
    }

    pub(crate) fn canvas_reset_video(&self, canvas: *mut ObsCanvas, video: &mut VideoInfo) -> bool {
        // SAFETY: a live canvas; libobs copies the video setup.
        self.newer.canvas.is_some_and(|f| unsafe { (f.reset_video)(canvas, video) })
    }

    /// A canvas's saved data (our reference: release it).
    pub(crate) fn canvas_save(&self, canvas: *mut ObsCanvas) -> *mut ObsData {
        // SAFETY: a live canvas.
        self.newer.canvas.map_or(std::ptr::null_mut(), |f| unsafe { (f.save)(canvas) })
    }

    /// A canvas made from saved data, with no picture yet (our reference).
    pub(crate) fn canvas_load(&self, data: *mut ObsData) -> *mut ObsCanvas {
        // SAFETY: live obs_data.
        self.newer.canvas.map_or(std::ptr::null_mut(), |f| unsafe { (f.load)(data) })
    }

    /// The canvas a scene or group belongs to (compared only, so no reference is kept).
    pub(crate) fn canvas_of(&self, source: *mut ObsSource) -> *mut ObsCanvas {
        let Some(f) = self.newer.canvas else { return std::ptr::null_mut() };
        if source.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: a live source; the reference it gives is let go at once (the canvas stays
        // alive while the source is on it).
        unsafe {
            let c = (f.source_canvas)(source);
            if !c.is_null() {
                (f.release)(c);
            }
            c
        }
    }

    /// A scene by name: the main picture's (null `canvas`) or another canvas's. Our reference.
    pub(crate) fn scene_named(&self, canvas: *mut ObsCanvas, name: &CStr) -> *mut ObsSource {
        if canvas.is_null() {
            return self.obs_get_source_by_name(name.as_ptr());
        }
        // SAFETY: a live canvas; libobs reads the name during the call.
        self.newer.canvas.map_or(std::ptr::null_mut(), |f| unsafe { (f.source_by_name)(canvas, name.as_ptr()) })
    }

    pub(crate) fn render_canvas(&self, canvas: *mut ObsCanvas) {
        if let Some(f) = self.newer.canvas {
            // SAFETY: called while drawing (inside a display's draw callback), a live canvas.
            unsafe { (f.render_texture)(canvas) }
        }
    }
}

/// Loads OBS's plugins from where OBS keeps them. libobs 33 split loading into core modules,
/// which it looks for inside the running app (Ferrocast, where there are none), and plugins by
/// path; its old `obs_load_all_modules` does nothing. So with 33+ every module is loaded as a
/// plugin from OBS's folder; with older libobs as before.
pub(crate) fn load_modules(api: &Api, places: &[(&std::ffi::CStr, &std::ffi::CStr)]) {
    let Some(load) = api.newer.load_plugins else {
        api.obs_load_all_modules();
        return;
    };
    for (bin, data) in places {
        let mut info = RuntimeModuleInfo {
            binary: bin.as_ptr(),
            data: data.as_ptr(),
            kind: OBS_MODULE_TYPE_PLUGIN,
            name: std::ptr::null(),
        };
        let mut failed = ModuleFailureInfo { failed_modules: std::ptr::null_mut(), count: 0 };
        // SAFETY: both structs are as libobs 33 declares them and outlive the call; the paths
        // are C strings that live past it (libobs copies what it keeps).
        unsafe { load(&mut info, &mut failed) };
        if failed.count > 0 {
            tracing::warn!(count = failed.count, "some OBS plugins didn't load");
        }
        if let Some(free) = api.newer.failure_free {
            // SAFETY: frees what obs_load_plugins filled in, once.
            unsafe { free(&mut failed) };
        }
    }
}

api! {
    obs_startup: fn(a: *const c_char, b: *const c_char, c: *mut c_void) -> bool;
    obs_initialized: fn() -> bool;
    obs_shutdown: fn();
    obs_wait_for_destroy_queue: fn() -> bool;
    obs_get_version_string: fn() -> *const c_char;
    obs_add_data_path: fn(a: *const c_char);
    obs_reset_audio2: fn(a: *const AudioInfo2) -> bool;
    obs_get_audio_info2: fn(a: *mut AudioInfo2) -> bool;
    obs_reset_video: fn(a: *mut VideoInfo) -> c_int;
    obs_add_module_path: fn(a: *const c_char, b: *const c_char);
    obs_add_disabled_module: fn(a: *const c_char);
    obs_load_all_modules: fn();
    obs_post_load_modules: fn();
    base_set_log_handler: fn(a: Option<LogHandler>, b: *mut c_void);
    obs_enum_input_types: fn(a: usize, b: *mut *const c_char) -> bool;
    obs_enum_filter_types: fn(a: usize, b: *mut *const c_char) -> bool;
    obs_enum_transition_types: fn(a: usize, b: *mut *const c_char) -> bool;
    obs_enum_encoder_types: fn(a: usize, b: *mut *const c_char) -> bool;
    obs_enum_output_types: fn(a: usize, b: *mut *const c_char) -> bool;
    obs_source_get_display_name: fn(a: *const c_char) -> *const c_char;
    obs_get_source_output_flags: fn(a: *const c_char) -> u32;
    obs_get_source_defaults: fn(a: *const c_char) -> *mut ObsData;
    obs_encoder_get_display_name: fn(a: *const c_char) -> *const c_char;
    obs_source_create: fn(a: *const c_char, b: *const c_char, c: *mut ObsData, d: *mut ObsData) -> *mut ObsSource;
    obs_source_create_private: fn(a: *const c_char, b: *const c_char, c: *mut ObsData) -> *mut ObsSource;
    obs_source_release: fn(a: *mut ObsSource);
    obs_source_remove: fn(a: *mut ObsSource);
    obs_source_properties: fn(a: *const ObsSource) -> *mut ObsProperties;
    obs_properties_first: fn(a: *mut ObsProperties) -> *mut ObsProperty;
    obs_property_next: fn(a: *mut *mut ObsProperty) -> bool;
    obs_properties_destroy: fn(a: *mut ObsProperties);
    obs_properties_get: fn(a: *mut ObsProperties, b: *const c_char) -> *mut ObsProperty;
    obs_property_button_clicked: fn(a: *mut ObsProperty, b: *mut c_void) -> bool;
    obs_property_name: fn(a: *mut ObsProperty) -> *const c_char;
    obs_property_description: fn(a: *mut ObsProperty) -> *const c_char;
    obs_property_long_description: fn(a: *mut ObsProperty) -> *const c_char;
    obs_property_get_type: fn(a: *mut ObsProperty) -> c_int;
    obs_property_visible: fn(a: *mut ObsProperty) -> bool;
    obs_property_enabled: fn(a: *mut ObsProperty) -> bool;
    obs_property_list_item_count: fn(a: *mut ObsProperty) -> usize;
    obs_property_list_item_name: fn(a: *mut ObsProperty, b: usize) -> *const c_char;
    obs_property_list_format: fn(a: *mut ObsProperty) -> c_int;
    obs_property_list_item_int: fn(a: *mut ObsProperty, b: usize) -> i64;
    obs_property_list_item_float: fn(a: *mut ObsProperty, b: usize) -> f64;
    obs_property_list_item_string: fn(a: *mut ObsProperty, b: usize) -> *const c_char;
    obs_property_list_item_bool: fn(a: *mut ObsProperty, b: usize) -> bool;
    obs_properties_apply_settings: fn(a: *mut ObsProperties, b: *mut ObsData);
    obs_property_group_content: fn(a: *mut ObsProperty) -> *mut ObsProperties;
    obs_property_int_min: fn(a: *mut ObsProperty) -> c_int;
    obs_property_int_max: fn(a: *mut ObsProperty) -> c_int;
    obs_property_int_step: fn(a: *mut ObsProperty) -> c_int;
    obs_property_float_min: fn(a: *mut ObsProperty) -> f64;
    obs_property_float_max: fn(a: *mut ObsProperty) -> f64;
    obs_property_float_step: fn(a: *mut ObsProperty) -> f64;
    obs_property_int_type: fn(a: *mut ObsProperty) -> c_int;
    obs_property_float_type: fn(a: *mut ObsProperty) -> c_int;
    obs_property_int_suffix: fn(a: *mut ObsProperty) -> *const c_char;
    obs_property_float_suffix: fn(a: *mut ObsProperty) -> *const c_char;
    obs_data_create_from_json: fn(a: *const c_char) -> *mut ObsData;
    obs_data_create: fn() -> *mut ObsData;
    obs_data_release: fn(a: *mut ObsData);
    obs_data_set_string: fn(a: *mut ObsData, b: *const c_char, c: *const c_char);
    obs_data_set_int: fn(a: *mut ObsData, b: *const c_char, c: i64);
    obs_data_set_bool: fn(a: *mut ObsData, b: *const c_char, c: bool);
    obs_scene_create: fn(a: *const c_char) -> *mut ObsScene;
    obs_scene_release: fn(a: *mut ObsScene);
    obs_scene_add: fn(a: *mut ObsScene, b: *mut ObsSource) -> *mut ObsSceneItem;
    obs_scene_get_source: fn(a: *const ObsScene) -> *mut ObsSource;
    obs_sceneitem_get_scene: fn(a: *const ObsSceneItem) -> *mut ObsScene;
    obs_sceneitem_set_pos: fn(a: *mut ObsSceneItem, b: *const Vec2);
    obs_sceneitem_set_alignment: fn(a: *mut ObsSceneItem, b: u32);
    obs_sceneitem_set_bounds_type: fn(a: *mut ObsSceneItem, b: c_int);
    obs_sceneitem_set_bounds_alignment: fn(a: *mut ObsSceneItem, b: u32);
    obs_sceneitem_set_bounds: fn(a: *mut ObsSceneItem, b: *const Vec2);
    obs_sceneitem_get_pos: fn(a: *const ObsSceneItem, b: *mut Vec2);
    obs_sceneitem_get_scale: fn(a: *const ObsSceneItem, b: *mut Vec2);
    obs_sceneitem_set_scale: fn(a: *mut ObsSceneItem, b: *const Vec2);
    obs_sceneitem_get_rot: fn(a: *const ObsSceneItem) -> f32;
    obs_sceneitem_get_crop: fn(a: *const ObsSceneItem, b: *mut Crop);
    obs_sceneitem_set_crop: fn(a: *mut ObsSceneItem, b: *const Crop);
    obs_sceneitem_set_order_position: fn(a: *mut ObsSceneItem, b: c_int);
    obs_sceneitem_set_rot: fn(a: *mut ObsSceneItem, b: f32);
    obs_sceneitem_get_alignment: fn(a: *const ObsSceneItem) -> u32;
    obs_sceneitem_get_bounds_type: fn(a: *const ObsSceneItem) -> c_int;
    obs_sceneitem_get_bounds_alignment: fn(a: *const ObsSceneItem) -> u32;
    obs_sceneitem_get_bounds: fn(a: *const ObsSceneItem, b: *mut Vec2);
    obs_sceneitem_get_box_transform: fn(a: *const ObsSceneItem, b: *mut Matrix4);
    obs_sceneitem_force_update_transform: fn(a: *mut ObsSceneItem);
    obs_sceneitem_defer_group_resize_begin: fn(a: *mut ObsSceneItem);
    obs_sceneitem_defer_group_resize_end: fn(a: *mut ObsSceneItem);
    obs_sceneitem_set_blending_mode: fn(a: *mut ObsSceneItem, b: c_int);
    obs_sceneitem_get_blending_mode: fn(a: *const ObsSceneItem) -> c_int;
    obs_source_copy_filters: fn(a: *mut ObsSource, b: *mut ObsSource);
    obs_source_get_width: fn(a: *mut ObsSource) -> u32;
    obs_source_get_height: fn(a: *mut ObsSource) -> u32;
    obs_source_video_render: fn(a: *mut ObsSource);
    obs_source_inc_showing: fn(a: *mut ObsSource);
    obs_source_dec_showing: fn(a: *mut ObsSource);
    gs_texrender_create: fn(a: c_int, b: c_int) -> *mut GsTexrender;
    gs_texrender_destroy: fn(a: *mut GsTexrender);
    gs_texrender_begin: fn(a: *mut GsTexrender, b: u32, c: u32) -> bool;
    gs_texrender_end: fn(a: *mut GsTexrender);
    gs_texrender_get_texture: fn(a: *const GsTexrender) -> *mut GsTexture;
    gs_clear: fn(a: u32, b: *const Vec4, c: f32, d: u8);
    gs_blend_state_push: fn();
    gs_blend_state_pop: fn();
    gs_blend_function: fn(a: c_int, b: c_int);
    gs_stagesurface_create: fn(a: u32, b: u32, c: c_int) -> *mut GsStagesurf;
    gs_stagesurface_destroy: fn(a: *mut GsStagesurf);
    gs_stage_texture: fn(a: *mut GsStagesurf, b: *const GsTexture);
    gs_stagesurface_map: fn(a: *mut GsStagesurf, b: *mut *mut u8, c: *mut u32) -> bool;
    gs_stagesurface_unmap: fn(a: *mut GsStagesurf);
    obs_source_get_private_settings: fn(a: *mut ObsSource) -> *mut ObsData;
    obs_sceneitem_set_scale_filter: fn(a: *mut ObsSceneItem, b: c_int);
    obs_sceneitem_get_scale_filter: fn(a: *const ObsSceneItem) -> c_int;
    obs_source_output_audio: fn(a: *mut ObsSource, b: *const c_void);
    obs_obj_get_data: fn(a: *mut ObsSource) -> *mut c_void;
    os_gettime_ns: fn() -> u64;
    obs_source_duplicate: fn(a: *mut ObsSource, b: *const c_char, c: bool) -> *mut ObsSource;
    obs_sceneitem_set_visible: fn(a: *mut ObsSceneItem, b: bool) -> bool;
    obs_sceneitem_visible: fn(a: *const ObsSceneItem) -> bool;
    obs_sceneitem_remove: fn(a: *mut ObsSceneItem);
    obs_scene_find_source: fn(a: *mut ObsScene, b: *const c_char) -> *mut ObsSceneItem;
    obs_source_update: fn(a: *mut ObsSource, b: *mut ObsData);
    obs_source_get_settings: fn(a: *const ObsSource) -> *mut ObsData;
    obs_source_get_name: fn(a: *const ObsSource) -> *const c_char;
    obs_source_get_id: fn(a: *const ObsSource) -> *const c_char;
    obs_data_get_json: fn(a: *mut ObsData) -> *const c_char;
    obs_data_get_json_with_defaults: fn(a: *mut ObsData) -> *const c_char;
    obs_set_output_source: fn(a: u32, b: *mut ObsSource);
    obs_get_video: fn() -> *mut VideoT;
    obs_get_audio: fn() -> *mut AudioT;
    obs_video_encoder_create: fn(a: *const c_char, b: *const c_char, c: *mut ObsData, d: *mut ObsData) -> *mut ObsEncoder;
    obs_audio_encoder_create: fn(a: *const c_char, b: *const c_char, c: *mut ObsData, d: usize, e: *mut ObsData) -> *mut ObsEncoder;
    obs_encoder_set_video: fn(a: *mut ObsEncoder, b: *mut VideoT);
    obs_encoder_set_audio: fn(a: *mut ObsEncoder, b: *mut AudioT);
    obs_encoder_release: fn(a: *mut ObsEncoder);
    obs_encoder_update: fn(a: *mut ObsEncoder, b: *mut ObsData);
    obs_source_add_audio_capture_callback: fn(a: *mut ObsSource, b: AudioCaptureCb, c: *mut c_void);
    obs_source_remove_audio_capture_callback: fn(a: *mut ObsSource, b: AudioCaptureCb, c: *mut c_void);
    obs_output_create: fn(a: *const c_char, b: *const c_char, c: *mut ObsData, d: *mut ObsData) -> *mut ObsOutput;
    obs_output_set_video_encoder: fn(a: *mut ObsOutput, b: *mut ObsEncoder);
    obs_output_set_audio_encoder: fn(a: *mut ObsOutput, b: *mut ObsEncoder, c: usize);
    obs_output_start: fn(a: *mut ObsOutput) -> bool;
    obs_output_stop: fn(a: *mut ObsOutput);
    obs_output_active: fn(a: *const ObsOutput) -> bool;
    obs_output_get_last_error: fn(a: *mut ObsOutput) -> *const c_char;
    obs_output_release: fn(a: *mut ObsOutput);
    obs_output_set_service: fn(a: *mut ObsOutput, b: *mut ObsService);
    obs_output_set_reconnect_settings: fn(a: *mut ObsOutput, b: c_int, c: c_int);
    obs_output_reconnecting: fn(a: *const ObsOutput) -> bool;
    obs_output_get_total_bytes: fn(a: *const ObsOutput) -> u64;
    obs_output_get_total_frames: fn(a: *const ObsOutput) -> c_int;
    obs_output_get_frames_dropped: fn(a: *const ObsOutput) -> c_int;
    obs_service_create: fn(a: *const c_char, b: *const c_char, c: *mut ObsData, d: *mut ObsData) -> *mut ObsService;
    obs_service_release: fn(a: *mut ObsService);
    obs_get_total_frames: fn() -> u32;
    obs_get_lagged_frames: fn() -> u32;
    obs_display_create: fn(a: *const GsInitData, b: u32) -> *mut ObsDisplay;
    obs_display_set_background_color: fn(a: *mut ObsDisplay, b: u32);
    obs_display_destroy: fn(a: *mut ObsDisplay);
    obs_display_resize: fn(a: *mut ObsDisplay, b: u32, c: u32);
    obs_display_add_draw_callback: fn(a: *mut ObsDisplay, b: DrawCallback, c: *mut c_void);
    obs_display_remove_draw_callback: fn(a: *mut ObsDisplay, b: DrawCallback, c: *mut c_void);
    obs_render_main_texture: fn();
    gs_viewport_push: fn();
    gs_viewport_pop: fn();
    gs_projection_push: fn();
    gs_projection_pop: fn();
    gs_ortho: fn(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32);
    gs_set_viewport: fn(a: c_int, b: c_int, c: c_int, d: c_int);
    obs_save_sources_filtered: fn(a: SaveFilterCb, b: *mut c_void) -> *mut ObsDataArray;
    obs_load_sources: fn(a: *mut ObsDataArray, b: Option<LoadSourceCb>, c: *mut c_void);
    obs_save_source: fn(a: *mut ObsSource) -> *mut ObsData;
    obs_load_source: fn(a: *mut ObsData) -> *mut ObsSource;
    obs_get_source_by_name: fn(a: *const c_char) -> *mut ObsSource;
    obs_source_get_ref: fn(a: *mut ObsSource) -> *mut ObsSource;
    obs_get_output_source: fn(a: u32) -> *mut ObsSource;
    obs_scene_from_source: fn(a: *const ObsSource) -> *mut ObsScene;
    obs_group_from_source: fn(a: *const ObsSource) -> *mut ObsScene;
    obs_source_is_scene: fn(a: *const ObsSource) -> bool;
    obs_scene_enum_items: fn(a: *mut ObsScene, b: SceneItemCb, c: *mut c_void);
    obs_sceneitem_group_enum_items: fn(a: *mut ObsSceneItem, b: SceneItemCb, c: *mut c_void);
    obs_sceneitem_get_source: fn(a: *const ObsSceneItem) -> *mut ObsSource;
    obs_sceneitem_get_id: fn(a: *const ObsSceneItem) -> i64;
    obs_scene_find_sceneitem_by_id: fn(a: *mut ObsScene, b: i64) -> *mut ObsSceneItem;
    obs_sceneitem_set_locked: fn(a: *mut ObsSceneItem, b: bool) -> bool;
    obs_sceneitem_locked: fn(a: *const ObsSceneItem) -> bool;
    obs_sceneitem_set_order: fn(a: *mut ObsSceneItem, b: c_int);
    obs_sceneitem_is_group: fn(a: *mut ObsSceneItem) -> bool;
    obs_sceneitem_get_draw_transform: fn(a: *mut ObsSceneItem, b: *mut Matrix4);
    obs_scene_duplicate: fn(a: *mut ObsScene, b: *const c_char, c: c_int) -> *mut ObsScene;
    obs_source_set_name: fn(a: *mut ObsSource, b: *const c_char);
    obs_source_get_output_flags: fn(a: *const ObsSource) -> u32;
    obs_source_set_volume: fn(a: *mut ObsSource, b: f32);
    obs_source_get_volume: fn(a: *const ObsSource) -> f32;
    obs_source_set_muted: fn(a: *mut ObsSource, b: bool);
    obs_source_muted: fn(a: *const ObsSource) -> bool;
    obs_source_set_monitoring_type: fn(a: *mut ObsSource, b: c_int);
    obs_source_get_monitoring_type: fn(a: *const ObsSource) -> c_int;
    obs_source_enum_filters: fn(a: *mut ObsSource, b: SourceEnumCb, c: *mut c_void);
    obs_source_filter_add: fn(a: *mut ObsSource, b: *mut ObsSource);
    obs_source_filter_remove: fn(a: *mut ObsSource, b: *mut ObsSource);
    obs_source_filter_set_order: fn(a: *mut ObsSource, b: *mut ObsSource, c: c_int);
    obs_source_get_filter_by_name: fn(a: *mut ObsSource, b: *const c_char) -> *mut ObsSource;
    obs_source_set_enabled: fn(a: *mut ObsSource, b: bool);
    obs_source_enabled: fn(a: *const ObsSource) -> bool;
    obs_source_reset_settings: fn(a: *mut ObsSource, b: *mut ObsData);
    obs_source_set_balance_value: fn(a: *mut ObsSource, b: f32);
    obs_source_get_balance_value: fn(a: *const ObsSource) -> f32;
    obs_source_set_sync_offset: fn(a: *mut ObsSource, b: i64);
    obs_source_get_sync_offset: fn(a: *const ObsSource) -> i64;
    obs_source_set_flags: fn(a: *mut ObsSource, b: u32);
    obs_source_get_flags: fn(a: *const ObsSource) -> u32;
    obs_source_set_audio_mixers: fn(a: *mut ObsSource, b: u32);
    obs_source_get_audio_mixers: fn(a: *const ObsSource) -> u32;
    obs_source_set_deinterlace_mode: fn(a: *mut ObsSource, b: c_int);
    obs_source_get_deinterlace_mode: fn(a: *const ObsSource) -> c_int;
    obs_source_set_deinterlace_field_order: fn(a: *mut ObsSource, b: c_int);
    obs_source_get_deinterlace_field_order: fn(a: *const ObsSource) -> c_int;
    obs_sceneitem_set_transition: fn(a: *mut ObsSceneItem, b: bool, c: *mut ObsSource);
    obs_sceneitem_get_transition: fn(a: *mut ObsSceneItem, b: bool) -> *mut ObsSource;
    obs_sceneitem_set_transition_duration: fn(a: *mut ObsSceneItem, b: bool, c: u32);
    obs_sceneitem_get_transition_duration: fn(a: *mut ObsSceneItem, b: bool) -> u32;
    obs_transition_set_manual_time: fn(a: *mut ObsSource, b: f32);
    obs_transition_set_manual_torque: fn(a: *mut ObsSource, b: f32, c: f32);
    obs_transition_get_time: fn(a: *mut ObsSource) -> f32;
    os_generate_formatted_filename: fn(a: *const c_char, b: bool, c: *const c_char) -> *mut c_char;
    bfree: fn(a: *mut c_void);
    obs_output_get_proc_handler: fn(a: *const ObsOutput) -> *mut ProcHandler;
    proc_handler_call: fn(a: *mut ProcHandler, b: *const c_char, c: *mut CallData) -> bool;
    calldata_set_data: fn(a: *mut CallData, b: *const c_char, c: *const c_void, d: usize);
    calldata_get_data: fn(a: *const CallData, b: *const c_char, c: *mut c_void, d: usize) -> bool;
    media_remux_job_create: fn(a: *mut *mut RemuxJob, b: *const c_char, c: *const c_char) -> bool;
    media_remux_job_process: fn(a: *mut RemuxJob, b: Option<RemuxCb>, c: *mut c_void) -> bool;
    media_remux_job_destroy: fn(a: *mut RemuxJob);
    obs_data_apply: fn(a: *mut ObsData, b: *mut ObsData);
    obs_get_encoder_properties: fn(a: *const c_char) -> *mut ObsProperties;
    obs_view_create: fn() -> *mut ObsView;
    obs_scene_add_group2: fn(a: *mut ObsScene, b: *const c_char, c: bool) -> *mut ObsSceneItem;
    obs_scene_insert_group2:
        fn(a: *mut ObsScene, b: *const c_char, c: *mut *mut ObsSceneItem, d: usize, e: bool) -> *mut ObsSceneItem;
    obs_sceneitem_group_ungroup2: fn(a: *mut ObsSceneItem, b: bool);
    obs_sceneitem_group_add_item: fn(a: *mut ObsSceneItem, b: *mut ObsSceneItem);
    obs_sceneitem_group_remove_item: fn(a: *mut ObsSceneItem, b: *mut ObsSceneItem);
    obs_sceneitem_get_group: fn(a: *mut ObsScene, b: *mut ObsSceneItem) -> *mut ObsSceneItem;
    obs_sceneitem_set_blending_method: fn(a: *mut ObsSceneItem, b: c_int);
    obs_sceneitem_get_blending_method: fn(a: *mut ObsSceneItem) -> c_int;
    obs_source_media_play_pause: fn(a: *mut ObsSource, b: bool);
    obs_source_media_restart: fn(a: *mut ObsSource);
    obs_source_media_stop: fn(a: *mut ObsSource);
    obs_source_media_next: fn(a: *mut ObsSource);
    obs_source_media_previous: fn(a: *mut ObsSource);
    obs_source_media_get_duration: fn(a: *mut ObsSource) -> i64;
    obs_source_media_get_time: fn(a: *mut ObsSource) -> i64;
    obs_source_media_set_time: fn(a: *mut ObsSource, b: i64);
    obs_source_media_get_state: fn(a: *mut ObsSource) -> c_int;
    os_get_free_disk_space: fn(a: *const c_char) -> u64;
    video_output_get_skipped_frames: fn(a: *const VideoT) -> u32;
    video_output_get_total_frames: fn(a: *const VideoT) -> u32;
    obs_view_set_source: fn(a: *mut ObsView, b: u32, c: *mut ObsSource);
    obs_view_add: fn(a: *mut ObsView) -> *mut VideoT;
    obs_view_remove: fn(a: *mut ObsView);
    obs_view_destroy: fn(a: *mut ObsView);
    obs_output_set_media: fn(a: *mut ObsOutput, b: *mut VideoT, c: *mut AudioT);
    obs_encoder_defaults: fn(a: *const c_char) -> *mut ObsData;
    obs_volmeter_create: fn(a: c_int) -> *mut ObsVolmeter;
    obs_volmeter_set_peak_meter_type: fn(a: *mut ObsVolmeter, b: c_int);
    obs_source_audio_active: fn(a: *const ObsSource) -> bool;
    obs_volmeter_destroy: fn(a: *mut ObsVolmeter);
    obs_volmeter_attach_source: fn(a: *mut ObsVolmeter, b: *mut ObsSource) -> bool;
    obs_volmeter_add_callback: fn(a: *mut ObsVolmeter, b: VolmeterCb, c: *mut c_void);
    obs_volmeter_remove_callback: fn(a: *mut ObsVolmeter, b: VolmeterCb, c: *mut c_void);
    obs_db_to_mul: fn(a: f32) -> f32;
    obs_mul_to_db: fn(a: f32) -> f32;
    obs_transition_start: fn(a: *mut ObsSource, b: c_int, c: u32, d: *mut ObsSource) -> bool;
    obs_transition_set: fn(a: *mut ObsSource, b: *mut ObsSource);
    obs_transition_get_active_source: fn(a: *mut ObsSource) -> *mut ObsSource;
    obs_transition_get_source: fn(a: *mut ObsSource, b: c_int) -> *mut ObsSource;
    obs_data_set_array: fn(a: *mut ObsData, b: *const c_char, c: *mut ObsDataArray);
    obs_data_get_array: fn(a: *mut ObsData, b: *const c_char) -> *mut ObsDataArray;
    obs_data_array_release: fn(a: *mut ObsDataArray);
    obs_output_pause: fn(a: *mut ObsOutput, b: bool) -> bool;
    obs_output_paused: fn(a: *const ObsOutput) -> bool;
    obs_output_set_delay: fn(a: *mut ObsOutput, b: u32, c: u32);
    obs_encoder_set_scaled_size: fn(a: *mut ObsEncoder, b: u32, c: u32);
    os_cpu_usage_info_start: fn() -> *mut CpuUsage;
    os_cpu_usage_info_query: fn(a: *mut CpuUsage) -> f64;
    os_cpu_usage_info_destroy: fn(a: *mut CpuUsage);
    os_get_proc_resident_size: fn() -> u64;
    obs_get_active_fps: fn() -> f64;
    obs_get_average_frame_time_ns: fn() -> u64;
    obs_set_audio_monitoring_device: fn(a: *const c_char, b: *const c_char) -> bool;
    obs_enum_audio_monitoring_devices: fn(a: AudioDeviceCb, b: *mut c_void);
    obs_get_audio_monitoring_device: fn(a: *mut *const c_char, b: *mut *const c_char);
    obs_register_source_s: fn(a: *const c_void, b: usize);
    obs_properties_create: fn() -> *mut ObsProperties;
    obs_properties_add_text: fn(a: *mut ObsProperties, b: *const c_char, c: *const c_char, d: c_int) -> *mut ObsProperty;
    obs_properties_add_int: fn(a: *mut ObsProperties, b: *const c_char, c: *const c_char, d: c_int, e: c_int, f: c_int) -> *mut ObsProperty;
    obs_data_set_default_string: fn(a: *mut ObsData, b: *const c_char, c: *const c_char);
    obs_data_set_default_int: fn(a: *mut ObsData, b: *const c_char, c: i64);
    obs_data_get_string: fn(a: *mut ObsData, b: *const c_char) -> *const c_char;
    obs_data_get_int: fn(a: *mut ObsData, b: *const c_char) -> i64;
    gs_texture_create: fn(a: u32, b: u32, c: c_int, d: u32, e: *const *const u8, f: u32) -> *mut GsTexture;
    gs_texture_set_image: fn(a: *mut GsTexture, b: *const u8, c: u32, d: bool);
    gs_texture_destroy: fn(a: *mut GsTexture);
    gs_effect_get_param_by_name: fn(a: *const GsEffect, b: *const c_char) -> *mut GsEparam;
    gs_effect_set_texture: fn(a: *mut GsEparam, b: *mut GsTexture);
    gs_draw_sprite: fn(a: *mut GsTexture, b: u32, c: u32, d: u32);
    gs_matrix_push: fn();
    gs_matrix_pop: fn();
    gs_matrix_translate3f: fn(a: f32, b: f32, c: f32);
    gs_matrix_rotaa4f: fn(a: f32, b: f32, c: f32, d: f32);
    gs_matrix_scale3f: fn(a: f32, b: f32, c: f32);
    gs_effect_loop: fn(a: *mut GsEffect, b: *const c_char) -> bool;
    gs_effect_set_vec4: fn(a: *mut GsEparam, b: *const Vec4);
    obs_get_base_effect: fn(a: c_int) -> *mut GsEffect;
    obs_property_path_type: fn(a: *mut ObsProperty) -> c_int;
    obs_property_text_type: fn(a: *mut ObsProperty) -> c_int;
    obs_property_text_monospace: fn(a: *mut ObsProperty) -> bool;
    obs_property_text_set_monospace: fn(a: *mut ObsProperty, b: bool);
    obs_property_path_filter: fn(a: *mut ObsProperty) -> *const c_char;
    obs_property_editable_list_type: fn(a: *mut ObsProperty) -> c_int;
    obs_property_group_type: fn(a: *mut ObsProperty) -> c_int;
    obs_property_editable_list_filter: fn(a: *mut ObsProperty) -> *const c_char;
    obs_enter_graphics: fn();
    obs_leave_graphics: fn();
}

/// Loads every `.dylib` in `dir`, retrying until no more load: a library whose own `@rpath`
/// dependency isn't loaded yet fails the first round and loads in a later one.
/// Opens a library. On Windows its own folder is searched for what it depends on (obs.dll's
/// ffmpeg DLLs sit next to it), as `LoadLibrary` doesn't by default.
///
/// # Safety
/// Loading runs the library's initialisers; callers only open OBS's own libraries.
unsafe fn open(path: &Path) -> Result<Library, libloading::Error> {
    #[cfg(windows)]
    {
        use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library as WinLibrary};
        // SAFETY: as for this function.
        unsafe { WinLibrary::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH) }.map(Library::from)
    }
    #[cfg(not(windows))]
    {
        // SAFETY: as for this function.
        unsafe { Library::new(path) }
    }
}

/// Loads what it can of OBS's bundled libraries → (loaded, the ones that couldn't load yet).
fn load_deps(dir: &Path) -> (Vec<Library>, Vec<std::path::PathBuf>) {
    // Loose libraries (ffmpeg, x264…) and the frameworks plugins link by @rpath: Qt, which
    // obs-frontend-api, obs-browser and obs-vst need at load time even with no window, and
    // Syphon. Not libobs (loaded on its own), CEF (obs-browser loads it itself) or Sparkle
    // (OBS's updater).
    const SKIP: [&str; 3] = ["libobs", "Chromium Embedded Framework", "Sparkle"];
    let left: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter_map(|p| match p.extension().and_then(|x| x.to_str()) {
                    Some("dylib" | "dll") => Some(p),
                    Some("framework") => {
                        let name = p.file_stem()?.to_str()?.to_string();
                        let bin = p.join(&name);
                        (!SKIP.contains(&name.as_str()) && bin.exists()).then_some(bin)
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    load_all(left)
}

/// Loads each library, retrying the failed ones while that loads more (they depend on each
/// other in any order) → (loaded, still not loadable).
fn load_all(mut left: Vec<std::path::PathBuf>) -> (Vec<Library>, Vec<std::path::PathBuf>) {
    let mut loaded = Vec::new();
    loop {
        let before = left.len();
        left.retain(|p| {
            // SAFETY: these are OBS's own bundled libraries (ffmpeg, x264, mbedtls, srt, Qt…);
            // loading them only runs their initialisers.
            match unsafe { open(p) } {
                Ok(l) => {
                    loaded.push(l);
                    false
                }
                Err(_) => true,
            }
        });
        if left.is_empty() || left.len() == before {
            break;
        }
    }
    (loaded, left)
}

/// A Rust string as a C string (an interior NUL is cut there, never a crash).
pub fn cstr(s: &str) -> CString {
    let end = s.find('\0').unwrap_or(s.len());
    CString::new(&s[..end]).unwrap_or_default()
}

/// A C string from libobs as Rust text (NULL is empty).
pub fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: libobs returns NUL-terminated strings it owns, valid for this call; copied at once.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

// The UCRT only has the printf family inline in its headers; this library has linkable copies
// (the C99 `vsnprintf`, which ends a truncated line with NUL like everywhere else).
#[cfg_attr(all(windows, target_env = "msvc"), link(name = "legacy_stdio_definitions"))]
unsafe extern "C" {
    fn vsnprintf(buf: *mut c_char, n: usize, fmt: *const c_char, args: *mut c_void) -> c_int;
}

/// libobs's log line as text. `args` is the `va_list` libobs passed to the log handler: a
/// pointer on macOS arm64, Windows and x86-64 Linux, so it can go straight to `vsnprintf`.
pub fn format_log(fmt: *const c_char, args: *mut c_void) -> String {
    let mut buf = vec![0 as c_char; 1024];
    // SAFETY: `buf` has room for 1024 bytes and vsnprintf writes at most that (NUL included);
    // `fmt` and `args` come straight from libobs's log call and are valid during it.
    let n = unsafe { vsnprintf(buf.as_mut_ptr(), buf.len(), fmt, args) };
    if n < 0 {
        return String::new();
    }
    text(buf.as_ptr())
}

/// libobs's log, into Ferrocast's (`tracing`): errors and warnings as such, the rest as debug.
pub(crate) extern "C" fn log_to_tracing(level: c_int, fmt: *const c_char, args: *mut c_void, _p: *mut c_void) {
    let line = format_log(fmt, args);
    match level {
        LOG_ERROR => tracing::error!(target: "libobs", "{line}"),
        LOG_WARNING => tracing::warn!(target: "libobs", "{line}"),
        LOG_INFO => tracing::info!(target: "libobs", "{line}"),
        _ => tracing::debug!(target: "libobs", "{line}"),
    }
}

/// Collects the items `obs_scene_enum_items` or `obs_sceneitem_group_enum_items` walks
/// (bottom first) into the `Vec<*mut ObsSceneItem>` behind `param`.
pub(crate) unsafe extern "C" fn collect_items(_s: *mut ObsScene, item: *mut ObsSceneItem, param: *mut c_void) -> bool {
    // SAFETY: `param` is the `&mut Vec` collection.rs passes for the length of this enum call,
    // which libobs makes on the calling thread before returning.
    let out = unsafe { &mut *(param as *mut Vec<*mut ObsSceneItem>) };
    out.push(item);
    true
}

/// Collects a source's filters (in order) into the `Vec<*mut ObsSource>` behind `param`.
pub(crate) unsafe extern "C" fn collect_filters(_parent: *mut ObsSource, filter: *mut ObsSource, param: *mut c_void) {
    // SAFETY: as for `collect_items`: a `&mut Vec` alive for the synchronous enum call.
    let out = unsafe { &mut *(param as *mut Vec<*mut ObsSource>) };
    out.push(filter);
}

/// What `keep_scenes` needs while a collection loads.
pub(crate) struct LoadCtx {
    pub api: *const Api,
    /// Scenes loaded, each with a reference of our own.
    pub scenes: Vec<*mut ObsSource>,
}

/// `obs_load_sources` callback: keeps a reference to every scene loaded (libobs releases its
/// own when loading ends; a scene with no holder would go).
pub(crate) unsafe extern "C" fn keep_scenes(param: *mut c_void, source: *mut ObsSource) {
    // SAFETY: `param` is the `&mut LoadCtx` collection.rs passes for the synchronous
    // `obs_load_sources` call; its Api outlives the call.
    let ctx = unsafe { &mut *(param as *mut LoadCtx) };
    // SAFETY: as above.
    let api = unsafe { &*ctx.api };
    if api.obs_source_is_scene(source) {
        let r = api.obs_source_get_ref(source);
        if !r.is_null() {
            ctx.scenes.push(r);
        }
    }
}

/// libobs's remux functions, copied out so a remux can run on another thread (they touch no
/// libobs state; libobs stays loaded while Ferrocast runs).
#[derive(Clone, Copy)]
pub(crate) struct RemuxFns {
    create: unsafe extern "C" fn(*mut *mut RemuxJob, *const c_char, *const c_char) -> bool,
    process: unsafe extern "C" fn(*mut RemuxJob, Option<RemuxCb>, *mut c_void) -> bool,
    destroy: unsafe extern "C" fn(*mut RemuxJob),
}

impl std::fmt::Debug for RemuxFns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemuxFns")
    }
}

impl Api {
    pub(crate) fn remux_fns(&self) -> RemuxFns {
        RemuxFns {
            create: self.fns.media_remux_job_create,
            process: self.fns.media_remux_job_process,
            destroy: self.fns.media_remux_job_destroy,
        }
    }
}

impl RemuxFns {
    /// Remuxes `from` into `to` → whether it got to the end.
    pub(crate) fn run(&self, from: &CStr, to: &CStr, progress: &mut dyn FnMut(f32) -> bool) -> Result<bool, ()> {
        let mut job: *mut RemuxJob = std::ptr::null_mut();
        // SAFETY: the pointers are libobs's own exports, valid while libobs is loaded (for
        // the process's life); the strings outlive the calls; `job` is destroyed once.
        unsafe {
            if !(self.create)(&mut job, from.as_ptr(), to.as_ptr()) || job.is_null() {
                return Err(());
            }
            let mut f: &mut dyn FnMut(f32) -> bool = progress;
            let ok = (self.process)(job, Some(remux_progress), &mut f as *mut _ as *mut c_void);
            (self.destroy)(job);
            Ok(ok)
        }
    }
}

/// `media_remux_job_process` callback: hands the percent done to the caller's closure.
pub(crate) unsafe extern "C" fn remux_progress(data: *mut c_void, pct: f32) -> bool {
    // SAFETY: `data` is the `&mut &mut dyn FnMut(f32) -> bool` lib.rs passes for the
    // synchronous process call; it outlives the call.
    let f = unsafe { &mut *(data as *mut &mut dyn FnMut(f32) -> bool) };
    f(pct)
}

/// `obs_save_sources_filtered` callback: saves only the sources listed (those in use; a
/// source held only by an undo step isn't saved).
pub(crate) unsafe extern "C" fn save_only_listed(param: *mut c_void, source: *mut ObsSource) -> bool {
    // SAFETY: `param` is the `&Vec` collection.rs passes for the synchronous save call.
    let keep = unsafe { &*(param as *const Vec<*mut ObsSource>) };
    keep.contains(&source)
}

/// `obs_enum_audio_monitoring_devices` callback: collects (name, id) into the `Vec` behind
/// `param`.
pub(crate) unsafe extern "C" fn collect_devices(param: *mut c_void, name: *const c_char, id: *const c_char) -> bool {
    // SAFETY: `param` is the `&mut Vec` lib.rs passes for the synchronous enum call; libobs's
    // strings are valid during it and copied at once.
    let out = unsafe { &mut *(param as *mut Vec<(String, String)>) };
    out.push((text(name), text(id)));
    true
}

/// One sound's level, dBFS, for its first two channels (left, right).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub peak: [f32; 2],
    pub magnitude: [f32; 2],
}

/// Volume meter callback (libobs's audio thread): stores the latest level.
pub(crate) unsafe extern "C" fn store_level(param: *mut c_void, mag: *const f32, peak: *const f32, _input: *const f32) {
    // SAFETY: `param` is the boxed Mutex<Level> a Meter in collection.rs registered and keeps
    // until it has removed this callback (libobs then makes no more calls); the arrays hold
    // MAX_AUDIO_CHANNELS floats each during the call.
    let (cell, mag, peak) = unsafe {
        (
            &*(param as *const std::sync::Mutex<Level>),
            std::slice::from_raw_parts(mag, MAX_AUDIO_CHANNELS),
            std::slice::from_raw_parts(peak, MAX_AUDIO_CHANNELS),
        )
    };
    if let Ok(mut l) = cell.lock() {
        *l = Level { peak: [peak[0], peak[1]], magnitude: [mag[0], mag[1]] };
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// What the preview's draw callback needs: libobs and the canvas size.
#[derive(Debug)]
pub(crate) struct DrawState {
    pub api: *const Api,
    pub base: (u32, u32),
    /// The item picked in the preview (its scene's name and its id), outlined with handles.
    /// The scene and the ids picked in it (each outlined with handles, as OBS does).
    pub selected: std::sync::Mutex<Option<(std::ffi::CString, Vec<i64>)>>,
    /// A scene to show instead of what's on air (Studio Mode's preview): our reference, and
    /// showing. Null for what's on air.
    pub scene: std::sync::Mutex<*mut ObsSource>,
    /// The margin round the picture, in device pixels: the view is bigger than the page's box
    /// by this much on every side, for handles at the canvas's edge.
    pub edge: std::sync::atomic::AtomicU32,
    /// Where the canvas is inside the view (device pixels: x, y, w, h), when the page says;
    /// else it's fitted inside the margin. Around it, outlines of what's off the canvas show.
    pub canvas: std::sync::Mutex<Option<(f32, f32, f32, f32)>>,
    /// A scene whose sources all get an outline, each in its own colour.
    pub frames: std::sync::Mutex<Option<std::ffi::CString>>,
    /// A dark layer over everything, as a dialog's backdrop dims the page round the view
    /// (thousandths of full strength; 0 for none).
    pub shade: std::sync::atomic::AtomicU32,
    /// The canvas it shows (the vertical picture), or null for the main one.
    pub on: *mut ObsCanvas,
}

/// The colours of sources' outlines, picked by item id (`id % 8`); the page's list shows the
/// same: orange, sky, lime, pink, amber, violet, teal, blue.
pub const FRAME_COLORS: [(f32, f32, f32); 8] = [
    (1.0, 0.353, 0.122),
    (0.2, 0.831, 1.0),
    (0.639, 0.902, 0.208),
    (0.957, 0.447, 0.714),
    (0.98, 0.8, 0.082),
    (0.655, 0.545, 0.98),
    (0.176, 0.831, 0.749),
    (0.376, 0.647, 0.98),
];

fn frame_color(id: i64) -> Vec4 {
    let (x, y, z) = FRAME_COLORS[id.rem_euclid(8) as usize];
    Vec4 { x, y, z, w: 1.0 }
}

/// Every showing source's box in a scene, bottom first (groups' own sources too, placed by
/// their group's transform as OBS's preview places them).
struct AllBoxes<'a> {
    api: &'a Api,
    found: Vec<(i64, Matrix4)>,
    /// While walking a group: the group's transform (its space to the canvas).
    parent: Option<Matrix4>,
}

/// `g` after `m`: a box in a group's space put on the canvas (2D parts only).
fn then(m: &Matrix4, g: &Matrix4) -> Matrix4 {
    let map = |v: &Vec4, w: f32| Vec4 {
        x: g.x.x * v.x + g.y.x * v.y + w * g.t.x,
        y: g.x.y * v.x + g.y.y * v.y + w * g.t.y,
        z: v.z,
        w: v.w,
    };
    Matrix4 { x: map(&m.x, 0.0), y: map(&m.y, 0.0), z: m.z, t: map(&m.t, 1.0) }
}

unsafe extern "C" fn all_boxes_cb(_s: *mut ObsScene, item: *mut ObsSceneItem, param: *mut c_void) -> bool {
    // SAFETY: `param` is the `&mut AllBoxes` all_boxes passes for this synchronous enum call.
    let f = unsafe { &mut *(param as *mut AllBoxes) };
    if f.api.obs_sceneitem_visible(item) {
        let mut m = Matrix4::default();
        f.api.obs_sceneitem_get_box_transform(item, &mut m);
        let m = match &f.parent {
            Some(g) => then(&m, g),
            None => m,
        };
        f.found.push((f.api.obs_sceneitem_get_id(item), m));
        if f.parent.is_none() && f.api.obs_sceneitem_is_group(item) {
            let mut g = Matrix4::default();
            f.api.obs_sceneitem_get_draw_transform(item, &mut g);
            f.parent = Some(g);
            f.api.obs_sceneitem_group_enum_items(item, all_boxes_cb, param);
            f.parent = None;
        }
    }
    true
}

fn all_boxes(api: &Api, on: *mut ObsCanvas, scene: &std::ffi::CStr) -> Vec<(i64, Matrix4)> {
    let src = api.scene_named(on, scene);
    if src.is_null() {
        return Vec::new();
    }
    let sc = api.obs_scene_from_source(src);
    let mut f = AllBoxes { api, found: Vec::new(), parent: None };
    if !sc.is_null() {
        api.obs_scene_enum_items(sc, all_boxes_cb, &mut f as *mut AllBoxes as *mut c_void);
    }
    api.obs_source_release(src);
    f.found
}

/// What `find_box` looks for and found.
struct FindBox<'a> {
    api: &'a Api,
    id: i64,
    found: Option<Matrix4>,
}

/// Finds a visible item's box while libobs holds the scene's lock (the main thread may be
/// changing the scene; the items are only read here, inside the enum call).
unsafe extern "C" fn find_box(_s: *mut ObsScene, item: *mut ObsSceneItem, param: *mut c_void) -> bool {
    // SAFETY: `param` is the `&mut FindBox` item_box passes for this synchronous enum call.
    let f = unsafe { &mut *(param as *mut FindBox) };
    if f.api.obs_sceneitem_get_id(item) != f.id {
        return true;
    }
    if f.api.obs_sceneitem_visible(item) {
        let mut m = Matrix4::default();
        f.api.obs_sceneitem_get_box_transform(item, &mut m);
        f.found = Some(m);
    }
    false
}

/// The box of item `id` in scene `scene` (canvas pixels), if it's there and showing.
fn item_box(api: &Api, on: *mut ObsCanvas, scene: &std::ffi::CStr, id: i64) -> Option<Matrix4> {
    let src = api.scene_named(on, scene);
    if src.is_null() {
        return None;
    }
    let sc = api.obs_scene_from_source(src);
    let mut f = FindBox { api, id, found: None };
    if !sc.is_null() {
        api.obs_scene_enum_items(sc, find_box, &mut f as *mut FindBox as *mut c_void);
    }
    api.obs_source_release(src);
    f.found
}

/// `enum obs_base_effect` OBS_EFFECT_SOLID.
const OBS_EFFECT_SOLID: c_int = 3;

/// Draws `m`'s box (a unit square mapped onto the canvas) as OBS's preview does: an outline
/// and, when `handles`, eight handles; in `rim`, the source's colour. `px` is one screen pixel
/// in canvas units.
fn draw_selection(api: &Api, m: &Matrix4, px: f32, rim: &Vec4, handles: bool, thick: f32) {
    let effect = api.obs_get_base_effect(OBS_EFFECT_SOLID);
    if effect.is_null() {
        return;
    }
    let color = api.gs_effect_get_param_by_name(effect, c"color".as_ptr());
    let at = |u: f32, v: f32| (m.x.x * u + m.y.x * v + m.t.x, m.x.y * u + m.y.y * v + m.t.y);
    // A filled quad from (x, y) along angle `a` (radians), `len` long and `thick` wide,
    // centred on that line. The colour is set for each: ending the effect's technique
    // puts its parameters back to their defaults (white).
    let quad = |c: &Vec4, x: f32, y: f32, a: f32, len: f32, thick: f32| {
        api.gs_effect_set_vec4(color, c);
        api.gs_matrix_push();
        api.gs_matrix_translate3f(x, y, 0.0);
        api.gs_matrix_rotaa4f(0.0, 0.0, 1.0, a);
        api.gs_matrix_translate3f(0.0, -thick / 2.0, 0.0);
        api.gs_matrix_scale3f(len, thick, 1.0);
        while api.gs_effect_loop(effect, c"Solid".as_ptr()) {
            api.gs_draw_sprite(std::ptr::null_mut(), 0, 1, 1);
        }
        api.gs_matrix_pop();
    };
    let corners = [at(0.0, 0.0), at(1.0, 0.0), at(1.0, 1.0), at(0.0, 1.0)];
    let white = Vec4 { x: 1.0, y: 1.0, z: 1.0, w: 1.0 };
    for i in 0..4 {
        let (a, b) = (corners[i], corners[(i + 1) % 4]);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len > 0.0 {
            quad(rim, a.0, a.1, dy.atan2(dx), len, thick * px);
        }
    }
    if !handles {
        return;
    }
    // Handles: white squares with a rim in the source's colour, at the corners and the middle
    // of each side.
    let spots = [(0.0, 0.0), (0.5, 0.0), (1.0, 0.0), (1.0, 0.5), (1.0, 1.0), (0.5, 1.0), (0.0, 1.0), (0.0, 0.5)];
    for (size, c) in [(10.0, rim), (6.0, &white)] {
        for (u, v) in spots {
            let (x, y) = at(u, v);
            let s = size * px;
            quad(c, x - s / 2.0, y, 0.0, s, s);
        }
    }
}

/// A black canvas under the picture, as OBS's DrawBackdrop: the canvas's own transparent parts
/// show black, and its edge stands out from the stage round it.
fn draw_backdrop(api: &Api, w: f32, h: f32) {
    draw_solid(api, w, h, &Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 });
}

/// A `w`×`h` rectangle of one colour at the origin (alpha blends over what's drawn).
fn draw_solid(api: &Api, w: f32, h: f32, rgba: &Vec4) {
    let effect = api.obs_get_base_effect(OBS_EFFECT_SOLID);
    if effect.is_null() {
        return;
    }
    let color = api.gs_effect_get_param_by_name(effect, c"color".as_ptr());
    api.gs_effect_set_vec4(color, rgba);
    api.gs_matrix_push();
    api.gs_matrix_scale3f(w, h, 1.0);
    while api.gs_effect_loop(effect, c"Solid".as_ptr()) {
        api.gs_draw_sprite(std::ptr::null_mut(), 0, 1, 1);
    }
    api.gs_matrix_pop();
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// Draws what's on air into a preview, fitted inside it (letterboxed), as OBS's own preview.
pub(crate) unsafe extern "C" fn draw_main(param: *mut c_void, cx: u32, cy: u32) {
    // SAFETY: `param` is the boxed DrawState registered with this callback; lib.rs keeps the box
    // and the Studio (which owns the Api) alive until the callback is removed.
    let st = unsafe { &*(param as *const DrawState) };
    // SAFETY: as above, the Api outlives every registered draw callback.
    let api = unsafe { &*st.api };
    let (bw, bh) = (st.base.0.max(1) as f32, st.base.1.max(1) as f32);
    let placed = st.canvas.lock().ok().and_then(|g| *g);
    let (x, y, w, h, scale) = match placed {
        // Where the page put it (zoomed out, it leaves room to see what's off the canvas).
        Some((px_, py, pw, ph)) if pw >= 1.0 && ph >= 1.0 => {
            // The canvas's shape is kept: it fits the box the page gave.
            let scale = (pw / bw).min(ph / bh);
            (px_.round() as c_int, py.round() as c_int, pw.round() as c_int, (bh * scale).round() as c_int, scale)
        }
        _ => {
            // The picture fits inside the margin; the margin is left for the selection's handles.
            let edge = st.edge.load(std::sync::atomic::Ordering::Relaxed).min(cx / 4).min(cy / 4);
            let (aw, ah) = (cx.saturating_sub(2 * edge).max(1), cy.saturating_sub(2 * edge).max(1));
            let scale = (aw as f32 / bw).min(ah as f32 / bh);
            let (w, h) = ((bw * scale).round() as c_int, (bh * scale).round() as c_int);
            (((cx as c_int - w) / 2), ((cy as c_int - h) / 2), w, h, scale)
        }
    };
    api.gs_viewport_push();
    api.gs_projection_push();
    api.gs_ortho(0.0, bw, 0.0, bh, -100.0, 100.0);
    api.gs_set_viewport(x, y, w, h);
    draw_backdrop(api, bw, bh);
    {
        // Held while drawing: Preview::show_scene can't release the source mid-render.
        let shown = st.scene.lock();
        match shown.as_ref().map(|g| **g) {
            _ if !st.on.is_null() => api.render_canvas(st.on),
            Ok(scene) if !scene.is_null() => api.obs_source_video_render(scene),
            _ => api.obs_render_main_texture(),
        }
    }
    // Outlines are drawn over the whole view, margin included, with the canvas where the
    // picture is: what's off the canvas still shows its box.
    let s = scale.max(1e-6);
    api.gs_set_viewport(0, 0, cx as c_int, cy as c_int);
    api.gs_ortho(-x as f32 / s, (cx as f32 - x as f32) / s, -y as f32 / s, (cy as f32 - y as f32) / s, -100.0, 100.0);
    let picked = st.selected.lock().ok().and_then(|g| g.clone());
    if let Ok(frames) = st.frames.lock()
        && let Some(scene) = frames.as_ref()
    {
        for (id, m) in all_boxes(api, st.on, scene) {
            if picked.as_ref().is_some_and(|(_, p)| p.contains(&id)) {
                continue;
            }
            draw_selection(api, &m, 1.0 / s, &frame_color(id), false, 1.5);
        }
    }
    if let Some((scene, ids)) = picked.as_ref() {
        // Sources inside groups have their boxes only in the group's own pixels: all_boxes
        // puts them on the canvas.
        let mut nested: Option<Vec<(i64, Matrix4)>> = None;
        for id in ids {
            let m = item_box(api, st.on, scene, *id).or_else(|| {
                nested
                    .get_or_insert_with(|| all_boxes(api, st.on, scene))
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, m)| *m)
            });
            if let Some(m) = m {
                draw_selection(api, &m, 1.0 / s, &frame_color(*id), true, 2.0);
            }
        }
    }
    // Under a dialog: the page's backdrop colour over the whole view, so the picture dims with
    // the page instead of shining through it.
    let shade = st.shade.load(std::sync::atomic::Ordering::Relaxed);
    if shade > 0 {
        api.gs_ortho(0.0, cx as f32, 0.0, cy as f32, -100.0, 100.0);
        draw_solid(
            api,
            cx as f32,
            cy as f32,
            &Vec4 { x: 4.0 / 255.0, y: 5.0 / 255.0, z: 8.0 / 255.0, w: shade as f32 / 1000.0 },
        );
    }
    api.gs_projection_pop();
    api.gs_viewport_pop();
}

/// The native view a preview draws into, inside the window the dashboard lives in (macOS).
/// What a projector shows, read by its draw callback (graphics thread) under the lock. Every
/// source here holds a reference; scenes are also "showing" so their sources draw.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug)]
pub(crate) enum Show {
    Program,
    One(*mut ObsSource),
    /// Each scene with its name label (a private text source), and the "On air" label;
    /// Studio Mode's preview scene with its label; how it's laid out.
    Multi {
        cells: Vec<(*mut ObsSource, *mut ObsSource)>,
        air: *mut ObsSource,
        preview: Option<(*mut ObsSource, *mut ObsSource)>,
        look: crate::projector::MultiviewLook,
    },
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct ProjectorState {
    pub api: *const Api,
    pub base: (u32, u32),
    pub show: std::sync::Mutex<Show>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl Show {
    /// Lets go of every source (main thread, with the draw callback unable to read it).
    pub(crate) fn release(&mut self, api: &Api) {
        match std::mem::replace(self, Show::Program) {
            Show::Program => {}
            Show::One(src) => {
                api.obs_source_dec_showing(src);
                api.obs_source_release(src);
            }
            Show::Multi { cells, air, preview, .. } => {
                for (scene, label) in cells.into_iter().chain(preview) {
                    api.obs_source_dec_showing(scene);
                    api.obs_source_release(scene);
                    api.obs_source_release(label);
                }
                api.obs_source_release(air);
            }
        }
    }
}

/// A source by name, with our reference and showing (so it draws while projected).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn shown_source(api: &Api, name: &str) -> Option<*mut ObsSource> {
    let n = cstr(name);
    let src = api.obs_get_source_by_name(n.as_ptr());
    if src.is_null() {
        return None;
    }
    api.obs_source_inc_showing(src);
    Some(src)
}

/// A label for a multiview: white bold text, drawn by OBS's own text source.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn label(api: &Api, text: &str) -> *mut ObsSource {
    let settings = serde_json::json!({
        "text": text,
        "font": { "face": "Helvetica Neue", "size": 64, "style": "Bold", "flags": 1 },
        "color1": 0xFFFF_FFFFu32,
        "color2": 0xFFFF_FFFFu32,
    });
    let json = cstr(&settings.to_string());
    let data = api.obs_data_create_from_json(json.as_ptr());
    let (id, name) = (cstr("text_ft2_source_v2"), cstr(&format!("Multiview label {text}")));
    let src = api.obs_source_create_private(id.as_ptr(), name.as_ptr(), data);
    if !data.is_null() {
        api.obs_data_release(data);
    }
    src
}

/// Draws into `a` (pixels from the top left) a picture `w`×`h` units big, as `f` draws it.
fn region(api: &Api, a: crate::projector::Area, w: f32, h: f32, f: impl Fn()) {
    if a.w < 1.0 || a.h < 1.0 || w <= 0.0 || h <= 0.0 {
        return;
    }
    api.gs_viewport_push();
    api.gs_projection_push();
    api.gs_set_viewport(a.x.round() as c_int, a.y.round() as c_int, a.w.round() as c_int, a.h.round() as c_int);
    api.gs_ortho(0.0, w, 0.0, h, -100.0, 100.0);
    f();
    api.gs_projection_pop();
    api.gs_viewport_pop();
}

/// A filled box (pixels), in the current pixel-sized projection.
fn fill(api: &Api, c: &Vec4, a: crate::projector::Area) {
    let effect = api.obs_get_base_effect(OBS_EFFECT_SOLID);
    if effect.is_null() || a.w <= 0.0 || a.h <= 0.0 {
        return;
    }
    // Set for every draw: ending the technique puts it back to white (R-79).
    api.gs_effect_set_vec4(api.gs_effect_get_param_by_name(effect, c"color".as_ptr()), c);
    api.gs_matrix_push();
    api.gs_matrix_translate3f(a.x, a.y, 0.0);
    api.gs_matrix_scale3f(a.w, a.h, 1.0);
    while api.gs_effect_loop(effect, c"Solid".as_ptr()) {
        api.gs_draw_sprite(std::ptr::null_mut(), 0, 1, 1);
    }
    api.gs_matrix_pop();
}

/// A frame `t` pixels thick just inside `a`.
fn frame(api: &Api, c: &Vec4, a: crate::projector::Area, t: f32) {
    use crate::projector::Area as A;
    fill(api, c, A { h: t, ..a });
    fill(api, c, A { y: a.y + a.h - t, h: t, ..a });
    fill(api, c, A { w: t, ..a });
    fill(api, c, A { x: a.x + a.w - t, w: t, ..a });
}

/// A name bar along the bottom of `a`: the label centred on a dark (or red, on air) band.
fn name_bar(api: &Api, a: crate::projector::Area, label: *mut ObsSource, band: &Vec4) {
    use crate::projector::Area as A;
    let bar = (a.h * 0.12).clamp(14.0, 34.0);
    let strip = A { y: a.y + a.h - bar, h: bar, ..a };
    fill(api, band, strip);
    if label.is_null() {
        return;
    }
    let (tw, th) = (api.obs_source_get_width(label) as f32, api.obs_source_get_height(label) as f32);
    if tw <= 0.0 || th <= 0.0 {
        return;
    }
    let k = (bar * 0.7 / th).min(a.w * 0.92 / tw);
    let (w, h) = (tw * k, th * k);
    let at = A { x: a.x + (a.w - w) / 2.0, y: strip.y + (bar - h) / 2.0, w, h };
    region(api, at, tw, th, || api.obs_source_video_render(label));
}

/// OBS's on-air red.
const RED: Vec4 = Vec4 { x: 0.86, y: 0.15, z: 0.15, w: 1.0 };

/// Draws a projector: what's on air, one source, or a multiview (each fitted, letterboxed).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) unsafe extern "C" fn draw_projector(param: *mut c_void, cx: u32, cy: u32) {
    use crate::projector::{Area, multiview_in, safe_areas};
    // SAFETY: `param` is the boxed ProjectorState registered with this callback; lib.rs keeps
    // it, and the Studio owning the Api, alive until the callback is removed.
    let st = unsafe { &*(param as *const ProjectorState) };
    // SAFETY: as above.
    let api = unsafe { &*st.api };
    let Ok(show) = st.show.lock() else { return };
    let (bw, bh) = (st.base.0.max(1) as f32, st.base.1.max(1) as f32);
    let whole = Area { x: 0.0, y: 0.0, w: cx as f32, h: cy as f32 };
    match &*show {
        Show::Program => region(api, whole.fit(bw / bh), bw, bh, || api.obs_render_main_texture()),
        Show::One(src) => {
            let (sw, sh) = (api.obs_source_get_width(*src) as f32, api.obs_source_get_height(*src) as f32);
            if sw > 0.0 && sh > 0.0 {
                region(api, whole.fit(sw / sh), sw, sh, || api.obs_source_video_render(*src));
            }
        }
        Show::Multi { cells, air, preview, look } => {
            let (bigs, areas) = multiview_in(look.layout, preview.is_some(), cells.len(), whole.w, whole.h, bw / bh);
            // The scene on air: the transition's active source (our reference, let go below).
            let t = api.obs_get_output_source(0);
            let on = if t.is_null() { std::ptr::null_mut() } else { api.obs_transition_get_active_source(t) };
            if !t.is_null() {
                api.obs_source_release(t);
            }
            // Each box gets a black ground first, so empty scenes still show where they are.
            let pixels = |f: &dyn Fn()| {
                api.gs_viewport_push();
                api.gs_projection_push();
                api.gs_set_viewport(0, 0, cx as c_int, cy as c_int);
                api.gs_ortho(0.0, whole.w, 0.0, whole.h, -100.0, 100.0);
                f();
                api.gs_projection_pop();
                api.gs_viewport_pop();
            };
            let black = Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };
            pixels(&|| {
                for a in bigs.iter().chain(&areas) {
                    fill(api, &black, *a);
                }
            });
            // With a preview, it's the first big box; what's on air is the last.
            let program = bigs.last().copied();
            let pv = preview.as_ref().and_then(|p| bigs.first().filter(|_| bigs.len() == 2).map(|a| (*a, *p)));
            if let Some((a, (scene, _))) = pv {
                region(api, a, bw, bh, || api.obs_source_video_render(scene));
            }
            if let Some(a) = program {
                region(api, a, bw, bh, || api.obs_render_main_texture());
            }
            for (a, (scene, _)) in areas.iter().zip(cells) {
                region(api, *a, bw, bh, || api.obs_source_video_render(*scene));
            }
            // Frames, names and safe areas on top, in pixels.
            pixels(&|| {
                let dark = Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 0.62 };
                let green = Vec4 { x: 0.2, y: 0.75, z: 0.35, w: 1.0 };
                let guide = Vec4 { x: 1.0, y: 1.0, z: 1.0, w: 0.45 };
                let thin = (whole.h / 360.0).max(1.0);
                if let Some((a, (_, label))) = pv {
                    frame(api, &green, a, 2.0 * thin);
                    if look.labels {
                        name_bar(api, a, label, &green);
                    }
                }
                if let Some(a) = program {
                    frame(api, &RED, a, 2.0 * thin);
                    if look.labels {
                        name_bar(api, a, *air, &RED);
                    }
                    if look.safe_areas {
                        for s in safe_areas(a) {
                            frame(api, &guide, s, thin);
                        }
                    }
                }
                for (a, (scene, label)) in areas.iter().zip(cells) {
                    let live = !on.is_null() && *scene == on;
                    let next = pv.is_some_and(|(_, (p, _))| p == *scene);
                    if live {
                        frame(api, &RED, *a, 2.0 * thin);
                    } else if next {
                        frame(api, &green, *a, 2.0 * thin);
                    }
                    if look.labels {
                        name_bar(
                            api,
                            *a,
                            *label,
                            if live {
                                &RED
                            } else if next {
                                &green
                            } else {
                                &dark
                            },
                        );
                    }
                }
            });
            if !on.is_null() {
                api.obs_source_release(on);
            }
        }
    }
}

/// Live captions with Apple's Speech recognizer (plan item 7).
#[cfg(target_os = "macos")]
pub(crate) mod speech;

/// Faces in a picture with Apple's Vision (plan item 8, smart vertical).
#[cfg(target_os = "macos")]
pub(crate) mod vision;

#[cfg(target_os = "macos")]
pub(crate) mod mac {
    use std::ffi::c_void;

    use objc2::encode::{Encoding, RefEncode};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{NSAutoresizingMaskOptions, NSResponder, NSView};
    use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};

    /// CoreGraphics' path (`CGPathRef`), only passed along.
    #[repr(C)]
    struct CGPath {
        _opaque: [u8; 0],
    }

    // SAFETY: CGPathRef is `struct CGPath *`, which the Objective-C runtime encodes as below.
    unsafe impl RefEncode for CGPath {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGPath", &[]));
    }

    /// CoreGraphics' colour (`CGColorRef`), only passed along.
    #[repr(C)]
    struct CGColor {
        _opaque: [u8; 0],
    }

    // SAFETY: CGColorRef is `struct CGColor *`, encoded by the runtime as below.
    unsafe impl RefEncode for CGColor {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
    }

    // Linked for CALayer (the mask layer is made by name).
    #[link(name = "QuartzCore", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPathCreateWithRoundedRect(rect: NSRect, rx: f64, ry: f64, m: *const c_void) -> *mut CGPath;
        fn CGPathRelease(path: *mut CGPath);
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            info: u32,
        ) -> *mut c_void;
        fn CGBitmapContextCreateImage(ctx: *mut c_void) -> *mut c_void;
        fn CGContextRelease(ctx: *mut c_void);
        fn CGContextScaleCTM(ctx: *mut c_void, sx: f64, sy: f64);
        fn CGContextSetRGBFillColor(ctx: *mut c_void, r: f64, g: f64, b: f64, a: f64);
        fn CGContextFillRect(ctx: *mut c_void, rect: NSRect);
        fn CGContextSetBlendMode(ctx: *mut c_void, mode: i32);
        fn CGContextAddPath(ctx: *mut c_void, path: *const CGPath);
        fn CGContextFillPath(ctx: *mut c_void);
        fn CGImageRelease(image: *mut c_void);
        fn CGColorCreateSRGB(r: f64, g: f64, b: f64, a: f64) -> *mut CGColor;
        fn CGColorRelease(color: *mut CGColor);
    }

    /// The display drawn into the view, resized with it.
    #[derive(Debug, Default)]
    pub struct PassIvars {
        resize: std::cell::Cell<Option<(*const super::Api, *mut super::ObsDisplay)>>,
    }

    define_class!(
        // SAFETY: NSView allows subclassing; PassView's ivars are plain Cells (no Drop needed).
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastPreviewView"]
        #[ivars = PassIvars]
        struct PassView;

        impl PassView {
            /// Resized (by the page, or by AppKit as the window is resized): the picture is
            /// redrawn at the new size at once, as OBS's preview does, instead of a frame or
            /// two later stretched.
            /// The window jumped in size (full screen, zoom): hidden at once, as the page can't
            /// say where it goes until the change settles (the main thread is busy with the
            /// animation), and a view left at the old size spills over the panels. The page
            /// shows it again where things ended up. A drag's small steps leave it alone: the
            /// page shows a still of it then.
            #[unsafe(method(resizeWithOldSuperviewSize:))]
            fn resize_with_old(&self, old: NSSize) {
                // SAFETY: NSView's own resizeWithOldSuperviewSize: with AppKit's argument.
                let _: () = unsafe { msg_send![super(self), resizeWithOldSuperviewSize: old] };
                // SAFETY: on the main thread (AppKit calls this there); only reads the tree.
                if let Some(p) = unsafe { self.superview() } {
                    let now = p.bounds().size;
                    if (now.width - old.width).abs() > 60.0 || (now.height - old.height).abs() > 60.0 {
                        self.setAlphaValue(0.0);
                    }
                }
            }

            #[unsafe(method(setFrameSize:))]
            fn set_frame_size(&self, size: NSSize) {
                // SAFETY: NSView's own setFrameSize: with the size AppKit passed.
                let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
                if let Some((api, display)) = self.ivars().resize.get() {
                    let scale = self.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
                    let px = |v: f64| (v * scale).round().clamp(1.0, 16_384.0) as u32;
                    // SAFETY: set by `ChildView::follow` from a live Preview, which clears it
                    // (`remove`) before the display and the Api go.
                    unsafe { &*api }.obs_display_resize(display, px(size.width), px(size.height));
                }
            }

            /// Clicks go through to the page under it, which handles picking, moving and
            /// resizing sources (the picture is drawn here, the page only listens).
            #[unsafe(method(hitTest:))]
            fn hit_test(&self, _point: NSPoint) -> *mut NSView {
                std::ptr::null_mut()
            }
        }
    );

    /// A child view of the window's content, in top-left coordinates like the web page.
    pub struct ChildView {
        view: Retained<NSView>,
        pass: Retained<PassView>,
    }

    impl std::fmt::Debug for ChildView {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ChildView").field("view", &self.view).finish_non_exhaustive()
        }
    }

    /// Where the web page sits inside `parent`: the largest view in it (the webview; a preview
    /// is always smaller). The page can start below the window's title bar, so its box, not
    /// the parent's, is what page coordinates count from (2.1: the preview sat a title bar
    /// too high and covered the buttons above it).
    fn page(parent: &NSView, except: Option<&NSView>) -> NSRect {
        let mut best = parent.bounds();
        let mut area = 0.0;
        for v in parent.subviews().iter() {
            if except.is_some_and(|e| std::ptr::eq(e, &*v)) {
                continue;
            }
            let f = v.frame();
            let a = f.size.width * f.size.height;
            if a > area {
                area = a;
                best = f;
            }
        }
        best
    }

    fn frame(parent: &NSView, except: Option<&NSView>, x: f64, y: f64, w: f64, h: f64, vh: f64) -> NSRect {
        let p = page(parent, except);
        // The page's visible top: `vh` up from the webview's bottom (it can be lower than the
        // webview's top when the page draws under the title bar).
        let vh = if vh > 0.0 && vh <= p.size.height { vh } else { p.size.height };
        // AppKit counts from the bottom unless the view is flipped; the page counts from the top.
        let top = if parent.isFlipped() { p.origin.y + (p.size.height - vh) + y } else { p.origin.y + vh - y - h };
        // On whole device pixels, covering the box: a frame on a half pixel (the page's boxes
        // often are) left a row of the GL surface undrawn, a black line along the stage.
        let s = parent.window().map(|w| w.backingScaleFactor()).filter(|s| *s > 0.0).unwrap_or(1.0);
        let (x0, y0) = (((p.origin.x + x) * s).floor() / s, (top * s).floor() / s);
        let (x1, y1) = (((p.origin.x + x + w.max(1.0)) * s).ceil() / s, ((top + h.max(1.0)) * s).ceil() / s);
        NSRect::new(NSPoint::new(x0, y0), NSSize::new(x1 - x0, y1 - y0))
    }

    /// Adds a view over `parent` (an `NSView *` the window toolkit handed out) at the rectangle.
    /// None off the main thread (AppKit allows views only there).
    pub fn child_view(parent: *mut c_void, x: f64, y: f64, w: f64, h: f64, vh: f64) -> Option<ChildView> {
        let mtm = MainThreadMarker::new()?;
        if parent.is_null() {
            return None;
        }
        // SAFETY: `parent` is the live NSView from the window toolkit's raw handle (Tauri/tao),
        // used on the main thread while that window exists.
        let parent: &NSView = unsafe { &*(parent as *const NSView) };
        let pass = PassView::alloc(mtm).set_ivars(PassIvars::default());
        // SAFETY: NSView's designated initialiser on a freshly allocated PassView.
        let view: Retained<PassView> =
            unsafe { msg_send![super(pass), initWithFrame: frame(parent, None, x, y, w, h, vh)] };
        let pass = view.clone();
        let view: Retained<NSView> = Retained::into_super(view);
        view.setWantsLayer(true);
        // Kept the same distance from the top when the window's height changes: unflipped,
        // AppKit would keep it from the bottom and slide it over the page above (2.1 test).
        if !parent.isFlipped() {
            view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        }
        parent.addSubview(&view);
        Some(ChildView { view, pass })
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    /// macOS lets this app record the screen (as it was when the app started: macOS applies a
    /// change only after a restart).
    pub fn screen_access() -> bool {
        // SAFETY: a plain CoreGraphics query with no arguments, callable from any thread.
        unsafe { CGPreflightScreenCaptureAccess() }
    }

    /// Asks for screen recording: macOS shows its prompt (once) and lists the app in System
    /// Settings. True if it's already allowed.
    pub fn request_screen_access() -> bool {
        // SAFETY: as above; it only shows the system's prompt.
        unsafe { CGRequestScreenCaptureAccess() }
    }

    impl ChildView {
        pub fn ptr(&self) -> *mut c_void {
            Retained::as_ptr(&self.view) as *mut c_void
        }

        /// Its size in device pixels (what libobs draws into). None off the main thread.
        pub fn pixels(&self) -> Option<(u32, u32)> {
            MainThreadMarker::new()?;
            let f = self.view.frame();
            let s = self.view.window().map(|w| w.backingScaleFactor()).unwrap_or(1.0);
            let px = |v: f64| (v * s).round().clamp(1.0, 16_384.0) as u32;
            Some((px(f.size.width), px(f.size.height)))
        }

        /// Moves and resizes it (the page's box moved). Main thread only; ignored elsewhere.
        pub fn set_frame(&self, x: f64, y: f64, w: f64, h: f64, vh: f64) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: on the main thread (checked above); `superview` only reads the view tree.
            if let Some(parent) = unsafe { self.view.superview() } {
                self.view.setFrame(frame(&parent, Some(&self.view), x, y, w, h, vh));
            }
        }

        pub fn remove(&self) {
            if MainThreadMarker::new().is_some() {
                self.pass.ivars().resize.set(None);
                self.view.removeFromSuperview();
            }
        }

        /// Resizes `display` whenever the view changes size. `api` and `display` must outlive
        /// the view's use of them: `remove` lets go. Main thread only.
        pub fn resize_with(&self, api: *const super::Api, display: *mut super::ObsDisplay) {
            if MainThreadMarker::new().is_some() {
                self.pass.ivars().resize.set(Some((api, display)));
            }
        }

        /// The view's own colour under the picture (0xAABBGGRR, as libobs takes it), shown
        /// where the picture isn't drawn yet. Main thread only; ignored elsewhere.
        pub fn set_backdrop(&self, abgr: u32) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: `layer` on a layer-backed NSView (wantsLayer is set when it's made).
            let layer: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.view, layer] };
            let Some(layer) = layer else { return };
            let c = |shift: u32| f64::from((abgr >> shift) & 0xff) / 255.0;
            // SAFETY: a new colour handed to the layer (which keeps its own reference), then
            // released here.
            unsafe {
                let color = CGColorCreateSRGB(c(0), c(8), c(16), 1.0);
                if color.is_null() {
                    return;
                }
                let _: () = msg_send![&*layer, setBackgroundColor: color];
                CGColorRelease(color);
            }
        }

        /// How much of the view shows, 0 to 1. Main thread only; ignored elsewhere.
        pub fn set_opacity(&self, opacity: f64) {
            if MainThreadMarker::new().is_some() {
                self.view.setAlphaValue(opacity.clamp(0.0, 1.0));
            }
        }

        /// Leaves the picture out of `holes` (x, y, w, h and corner radius in points from the
        /// view's top left) so the page's menus and panels there show through it; none: all
        /// of it shows. Main thread only; ignored elsewhere.
        ///
        /// The mask is drawn as a picture rather than an even-odd path: holes that overlap (a
        /// sub-menu over its menu, a list over a dialog) stay holes, and rounded corners show
        /// the page's rounded panel, not a square cut of what's under it.
        pub fn set_holes(&self, holes: &[(f64, f64, f64, f64, f64)]) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: `layer` on a layer-backed NSView (wantsLayer is set when it's made).
            let layer: Option<Retained<AnyObject>> = unsafe { msg_send![&*self.view, layer] };
            let Some(layer) = layer else { return };
            let b = self.view.bounds();
            let inside: Vec<(NSRect, f64)> = holes
                .iter()
                .filter_map(|&(x, y, w, h, rad)| {
                    // The whole box, drawn past the view's edge where it reaches over it: only
                    // its own corners are round (rounding the part inside cut round notches
                    // where a dialog crosses the stage's edge).
                    let inside = x < b.size.width && y < b.size.height && x + w > 0.0 && y + h > 0.0;
                    (inside && w > 0.0 && h > 0.0).then(|| {
                        // The layer counts up from the bottom like its view (not flipped).
                        let y0 = if self.view.isFlipped() { y } else { b.size.height - y - h };
                        let rad = rad.max(0.0).min(w / 2.0).min(h / 2.0);
                        (NSRect::new(NSPoint::new(x, y0), NSSize::new(w, h)), rad)
                    })
                })
                .collect();
            let none: Option<&AnyObject> = None;
            if inside.is_empty() || b.size.width < 1.0 || b.size.height < 1.0 {
                // SAFETY: CALayer's `mask` property takes nil.
                let _: () = unsafe { msg_send![&*layer, setMask: none] };
                return;
            }
            let scale: f64 = self.view.window().map(|w| w.backingScaleFactor()).unwrap_or(2.0);
            let (pw, ph) = ((b.size.width * scale).ceil() as usize, (b.size.height * scale).ceil() as usize);
            let Some(class) = AnyClass::get(c"CALayer") else { return };
            // SAFETY: `+[CALayer layer]` makes a new layer (QuartzCore is linked above).
            let mask: Option<Retained<AnyObject>> = unsafe { msg_send![class, layer] };
            let Some(mask) = mask else { return };
            // SAFETY: a fresh RGBA bitmap context of the view's size in pixels, filled opaque,
            // then each hole cleared; its image is handed to the layer (which keeps its own
            // reference) and everything made here is released here.
            unsafe {
                let space = CGColorSpaceCreateDeviceRGB();
                // kCGImageAlphaPremultipliedLast
                let ctx = CGBitmapContextCreate(std::ptr::null_mut(), pw, ph, 8, 0, space, 1);
                CGColorSpaceRelease(space);
                if ctx.is_null() {
                    return;
                }
                CGContextScaleCTM(ctx, scale, scale);
                CGContextSetRGBFillColor(ctx, 0.0, 0.0, 0.0, 1.0);
                CGContextFillRect(ctx, NSRect::new(NSPoint::new(0.0, 0.0), b.size));
                // kCGBlendModeClear: what's drawn now becomes see-through.
                CGContextSetBlendMode(ctx, 16);
                for (r, rad) in &inside {
                    let path = CGPathCreateWithRoundedRect(*r, *rad, *rad, std::ptr::null());
                    if path.is_null() {
                        continue;
                    }
                    CGContextAddPath(ctx, path);
                    CGContextFillPath(ctx);
                    CGPathRelease(path);
                }
                let image = CGBitmapContextCreateImage(ctx);
                CGContextRelease(ctx);
                if image.is_null() {
                    return;
                }
                let frame = NSRect::new(NSPoint::new(0.0, 0.0), b.size);
                let _: () = msg_send![&*mask, setFrame: frame];
                let _: () = msg_send![&*mask, setContentsScale: scale];
                let _: () = msg_send![&*mask, setContents: image as *mut AnyObject];
                let _: () = msg_send![&*layer, setMask: &*mask];
                CGImageRelease(image);
            }
        }
    }
}

/// `enum gs_color_format` GS_RGBA, `GS_CLEAR_COLOR`, blend one/zero.
const GS_RGBA: c_int = 3;
const GS_CLEAR_COLOR: u32 = 1;
const GS_BLEND_ZERO: c_int = 0;
const GS_BLEND_ONE: c_int = 1;

/// Draws a source (a scene or anything else) once at `w`×`h` and reads it back as RGBA rows,
/// as OBS's own screenshot does. Waits for the graphics thread (main thread is fine).
pub(crate) fn snapshot(api: &Api, src: *mut ObsSource, w: u32, h: u32) -> Option<Vec<u8>> {
    snapshot_scaled(api, src, (w, h), w, h)
}

/// The same, drawing `src` as if it were `base` big (a scene: the canvas) into `w`×`h`, so a
/// whole scene comes out small (a thumbnail).
pub(crate) fn snapshot_scaled(api: &Api, src: *mut ObsSource, base: (u32, u32), w: u32, h: u32) -> Option<Vec<u8>> {
    if src.is_null() || w == 0 || h == 0 || w > 16_384 || h > 16_384 || base.0 == 0 || base.1 == 0 {
        return None;
    }
    api.obs_enter_graphics();
    let tr = api.gs_texrender_create(GS_RGBA, GS_ZS_NONE);
    let mut out = None;
    if !tr.is_null() && api.gs_texrender_begin(tr, w, h) {
        let zero = Vec4::default();
        api.gs_clear(GS_CLEAR_COLOR, &zero, 0.0, 0);
        api.gs_ortho(0.0, base.0 as f32, 0.0, base.1 as f32, -100.0, 100.0);
        api.gs_blend_state_push();
        api.gs_blend_function(GS_BLEND_ONE, GS_BLEND_ZERO);
        api.obs_source_inc_showing(src);
        api.obs_source_video_render(src);
        api.obs_source_dec_showing(src);
        api.gs_blend_state_pop();
        api.gs_texrender_end(tr);
        let stage = api.gs_stagesurface_create(w, h, GS_RGBA);
        if !stage.is_null() {
            api.gs_stage_texture(stage, api.gs_texrender_get_texture(tr));
            let (mut data, mut line) = (std::ptr::null_mut::<u8>(), 0u32);
            if api.gs_stagesurface_map(stage, &mut data, &mut line) {
                let row = w as usize * 4;
                if !data.is_null() && line as usize >= row {
                    let mut px = Vec::with_capacity(row * h as usize);
                    for y in 0..h as usize {
                        // SAFETY: libobs maps h rows of `line` bytes each (line >= w*4) until unmap.
                        let r = unsafe { std::slice::from_raw_parts(data.add(y * line as usize), row) };
                        px.extend_from_slice(r);
                    }
                    out = Some(px);
                }
                api.gs_stagesurface_unmap(stage);
            }
            api.gs_stagesurface_destroy(stage);
        }
    }
    if !tr.is_null() {
        api.gs_texrender_destroy(tr);
    }
    api.obs_leave_graphics();
    out
}

/// Ferrocast's Web page source (2.1, the Browser source on macOS's own WebKit; LESSONS R-78):
/// a source kind registered in libobs whose picture is a web page rendered by a WKWebView in
/// an off-screen window, snapshotted while the source shows and drawn as a texture. Settings
/// use OBS's Browser source keys (url, width, height, css, fps), so a collection moves over.
#[cfg(target_os = "macos")]
pub(crate) mod web {
    use std::ffi::{CStr, c_char, c_int, c_void};
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchTime};
    use std::sync::Weak;

    use base64::Engine as _;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{NSBackingStoreType, NSImage, NSScreen, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask};
    use objc2_foundation::{NSError, NSPoint, NSRect, NSSize, NSString, NSURL, NSURLRequest};
    use objc2_web_kit::{
        WKAudiovisualMediaTypes, WKInactiveSchedulingPolicy, WKScriptMessage, WKScriptMessageHandler,
        WKSnapshotConfiguration, WKUserContentController, WKUserScript, WKUserScriptInjectionTime, WKWebView,
        WKWebViewConfiguration,
    };

    use super::{Api, GsEffect, GsTexture, ObsData, ObsProperties, ObsSource, cstr, text};

    /// The kind's id and name.
    pub const KIND: &CStr = c"ferrocast_web";
    const NAME: &CStr = c"Web page";
    /// The page starts clear (OBS's Browser source default), so overlays show what's under.
    const DEFAULT_CSS: &str = "body { background-color: rgba(0, 0, 0, 0); margin: 0px auto; overflow: hidden; }";

    /// libobs, for the callbacks (set once at registration; the Studio keeps it for the
    /// process's life: libobs never restarts in one process).
    static API: AtomicPtr<Api> = AtomicPtr::new(std::ptr::null_mut());

    fn api() -> Option<&'static Api> {
        let p = API.load(Ordering::Acquire);
        // SAFETY: set from the Studio's Api, which lives until libobs shuts down, after every
        // source (and so every callback) is gone (R-76).
        (!p.is_null()).then(|| unsafe { &*p })
    }

    /// The latest frame, straight-alpha BGRA, shared between the main thread (which copies
    /// snapshots in) and libobs's graphics thread (which uploads them).
    #[derive(Default)]
    struct Frame {
        w: u32,
        h: u32,
        bgra: Vec<u8>,
        seq: u64,
    }

    /// What every thread may touch.
    struct Shared {
        frame: Mutex<Frame>,
        /// The source is visible somewhere (preview or program): snapshots only then.
        shown: AtomicBool,
        /// The source still exists; the snapshot loop stops when it doesn't.
        alive: AtomicBool,
        w: AtomicU32,
        h: AtomicU32,
        fps: AtomicU32,
        /// The libobs source the page's sound goes out through; null once it's going.
        source: Mutex<SrcPtr>,
    }

    struct SrcPtr(*mut ObsSource);
    // SAFETY: only used under the Mutex, and only while libobs keeps the source (cleared in
    // `destroy` before libobs frees it).
    unsafe impl Send for SrcPtr {}

    /// The off-screen window and its web view. Main thread only.
    struct Page {
        window: Retained<NSWindow>,
        /// Holds the web view; scaled to fit the screen while the page is being clicked.
        holder: Retained<NSView>,
        view: Retained<WKWebView>,
        css: String,
        url: String,
        size: (u32, u32),
        /// On screen, to click and type in ("Interact"), with this title.
        interact: Option<String>,
        /// Kept alive with the page: WebKit and AppKit hold these weakly or until removed.
        tap: Retained<AudioTap>,
        closer: Option<Retained<WindowCloser>>,
    }

    /// The name pages post their sound to.
    const AUDIO_HANDLER: &str = "ferrocastAudio";

    /// Runs first in every page and frame: sends what the page plays (Web Audio and audio or
    /// video elements) to Ferrocast instead of the speakers, as 16-bit stereo chunks, so it
    /// goes into the stream through the mixer like OBS's "Control audio via OBS".
    const AUDIO_SCRIPT: &str = r#"(function () {
  if (window.__ferrocastAudio) return
  window.__ferrocastAudio = true
  var h = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.ferrocastAudio
  var AC = window.AudioContext || window.webkitAudioContext
  if (!h || !AC) return
  var connect = AudioNode.prototype.connect
  var disconnect = AudioNode.prototype.disconnect
  var taps = new WeakMap()
  function send(rate, l, r) {
    var n = l.length, out = new Int16Array(n * 2), loud = false
    for (var i = 0; i < n; i++) {
      var a = l[i], b = r[i]
      if (a !== 0 || b !== 0) loud = true
      a = a < -1 ? -1 : a > 1 ? 1 : a
      b = b < -1 ? -1 : b > 1 ? 1 : b
      out[2 * i] = a * 32767
      out[2 * i + 1] = b * 32767
    }
    if (!loud) return
    var bytes = new Uint8Array(out.buffer), s = ''
    for (var j = 0; j < bytes.length; j += 0x8000)
      s += String.fromCharCode.apply(null, bytes.subarray(j, j + 0x8000))
    h.postMessage(rate + ':' + btoa(s))
  }
  function tap(ctx) {
    var t = taps.get(ctx)
    if (t) return t
    t = ctx.createScriptProcessor(2048, 2, 2)
    var mute = ctx.createGain()
    mute.gain.value = 0
    connect.call(t, mute)
    connect.call(mute, ctx.destination)
    t.onaudioprocess = function (e) {
      var b = e.inputBuffer, l = b.getChannelData(0)
      send(ctx.sampleRate, l, b.numberOfChannels > 1 ? b.getChannelData(1) : l)
    }
    taps.set(ctx, t)
    return t
  }
  AudioNode.prototype.connect = function (dest) {
    if (dest instanceof AudioDestinationNode) {
      var args = Array.prototype.slice.call(arguments)
      args[0] = tap(this.context)
      connect.apply(this, args)
      return dest
    }
    return connect.apply(this, arguments)
  }
  AudioNode.prototype.disconnect = function (dest) {
    if (dest instanceof AudioDestinationNode) {
      var args = Array.prototype.slice.call(arguments)
      args[0] = tap(this.context)
      return disconnect.apply(this, args)
    }
    return disconnect.apply(this, arguments)
  }
  var shared = null, routed = new WeakSet()
  function route(el) {
    if (!(el instanceof HTMLMediaElement) || routed.has(el)) return
    routed.add(el)
    try {
      shared = shared || new AC()
      shared.createMediaElementSource(el).connect(shared.destination)
      if (shared.state === 'suspended') shared.resume()
    } catch (e) {}
  }
  var play = HTMLMediaElement.prototype.play
  HTMLMediaElement.prototype.play = function () {
    route(this)
    return play.apply(this, arguments)
  }
  document.addEventListener('play', function (e) { route(e.target) }, true)
})()"#;

    /// `struct obs_source_audio`.
    #[repr(C)]
    struct SourceAudio {
        data: [*const u8; 8],
        frames: u32,
        speakers: c_int,
        format: c_int,
        samples_per_sec: u32,
        timestamp: u64,
    }
    const SPEAKERS_STEREO: c_int = 2;
    const AUDIO_FORMAT_16BIT: c_int = 2;

    /// One chunk from the page ("48000:<base64 of 16-bit stereo>") into the source's sound.
    fn feed(shared: &Shared, msg: &str) {
        let Some(a) = api() else { return };
        let Some((rate, b64)) = msg.split_once(':') else { return };
        let Ok(rate) = rate.parse::<u32>() else { return };
        if !(8_000..=192_000).contains(&rate) {
            return;
        }
        let Ok(pcm) = base64::engine::general_purpose::STANDARD.decode(b64) else { return };
        let frames = (pcm.len() / 4) as u32;
        if frames == 0 {
            return;
        }
        let Ok(src) = shared.source.lock() else { return };
        if src.0.is_null() {
            return;
        }
        let span = u64::from(frames) * 1_000_000_000 / u64::from(rate);
        let mut data = [std::ptr::null::<u8>(); 8];
        data[0] = pcm.as_ptr();
        let audio = SourceAudio {
            data,
            frames,
            speakers: SPEAKERS_STEREO,
            format: AUDIO_FORMAT_16BIT,
            samples_per_sec: rate,
            timestamp: a.os_gettime_ns().saturating_sub(span),
        };
        a.obs_source_output_audio(src.0, (&audio as *const SourceAudio).cast());
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; AudioTap doesn't implement Drop.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastWebAudioTap"]
        #[ivars = Arc<Shared>]
        struct AudioTap;

        unsafe impl NSObjectProtocol for AudioTap {}

        unsafe impl WKScriptMessageHandler for AudioTap {
            #[unsafe(method(userContentController:didReceiveScriptMessage:))]
            fn did_receive(&self, _ucc: &WKUserContentController, message: &WKScriptMessage) {
                // SAFETY: the message is live for this call; its body is a JS string here.
                let body = unsafe { message.body() };
                if let Ok(s) = body.downcast::<NSString>() {
                    feed(self.ivars(), &s.to_string());
                }
            }
        }
    );

    impl AudioTap {
        fn new(mtm: MainThreadMarker, shared: Arc<Shared>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(shared);
            // SAFETY: NSObject's init on a freshly allocated object.
            unsafe { msg_send![super(this), init] }
        }
    }

    type PageSlot = Arc<Mutex<Option<OnMain<Page>>>>;

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; WindowCloser doesn't implement Drop.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastWebWindowCloser"]
        #[ivars = Weak<Mutex<Option<OnMain<Page>>>>]
        struct WindowCloser;

        unsafe impl NSObjectProtocol for WindowCloser {}

        unsafe impl NSWindowDelegate for WindowCloser {
            /// The close button puts the page back off-screen instead of closing it.
            #[unsafe(method(windowShouldClose:))]
            fn window_should_close(&self, _w: &NSWindow) -> bool {
                if let Some(page) = self.ivars().upgrade() {
                    let job = OnMain(page);
                    DispatchQueue::main().exec_async(move || {
                        let OnMain(page) = job;
                        if let Ok(mut p) = page.lock()
                            && let Some(OnMain(p)) = p.as_mut()
                        {
                            place(p, None);
                        }
                    });
                }
                false
            }
        }
    );

    impl WindowCloser {
        fn new(mtm: MainThreadMarker, page: &PageSlot) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Arc::downgrade(page));
            // SAFETY: NSObject's init on a freshly allocated object.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Off-screen at full size, or (with a title) on screen to click, scaled to fit.
    fn place(p: &mut Page, interact: Option<String>) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let (w, h) = (f64::from(p.size.0), f64::from(p.size.1));
        let full = NSSize::new(w, h);
        p.view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), full));
        match &interact {
            Some(title) => {
                let fit = NSScreen::mainScreen(mtm).map(|s| s.visibleFrame().size).unwrap_or(full);
                let k = (fit.width * 0.9 / w).min(fit.height * 0.85 / h).min(1.0);
                let shown = NSSize::new((w * k).round(), (h * k).round());
                p.window.setStyleMask(
                    NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable,
                );
                p.window.setTitle(&NSString::from_str(title));
                p.window.setContentSize(shown);
                p.holder.setFrameSize(shown);
                p.holder.setBoundsSize(full);
                p.window.center();
                p.window.makeKeyAndOrderFront(None);
            }
            None => {
                p.window.setStyleMask(NSWindowStyleMask::Borderless);
                p.window.setContentSize(full);
                p.holder.setFrameSize(full);
                p.holder.setBoundsSize(full);
                p.window.setFrameOrigin(NSPoint::new(-20_000.0, -20_000.0));
                p.window.orderBack(None);
            }
        }
        p.interact = interact;
    }

    /// Carries main-thread objects to the main queue; they're only used there.
    struct OnMain<T>(T);
    // SAFETY: the wrapped value is created on the main thread and only used in closures run on
    // the main queue (DispatchQueue::main), never touched elsewhere.
    unsafe impl<T> Send for OnMain<T> {}

    /// The source's own data (`void *data` in libobs's callbacks).
    struct Source {
        shared: Arc<Shared>,
        page: Arc<Mutex<Option<OnMain<Page>>>>,
        /// Graphics thread only.
        tex: *mut GsTexture,
        tex_size: (u32, u32),
        seq: u64,
    }

    fn settings_of(s: *mut ObsData) -> (String, u32, u32, String, u32) {
        let Some(a) = api() else { return (String::new(), 1920, 1080, DEFAULT_CSS.into(), 30) };
        let get_s = |k: &CStr| text(a.obs_data_get_string(s, k.as_ptr()));
        let get_i = |k: &CStr| a.obs_data_get_int(s, k.as_ptr());
        let clamp = |v: i64, lo: i64, hi: i64| v.clamp(lo, hi) as u32;
        (
            get_s(c"url"),
            clamp(get_i(c"width"), 1, 8192),
            clamp(get_i(c"height"), 1, 8192),
            get_s(c"css"),
            clamp(get_i(c"fps"), 1, 60),
        )
    }

    /// The page's CSS goes in as a style element once the document loads.
    fn css_script(css: &str) -> String {
        let quoted = serde_json::to_string(css).unwrap_or_else(|_| "\"\"".into());
        format!(
            "(function(){{var s=document.getElementById('ferrocast-css')||document.createElement('style');s.id='ferrocast-css';s.textContent={quoted};(document.head||document.documentElement).appendChild(s);}})();"
        )
    }

    fn open(
        mtm: MainThreadMarker,
        slot: &PageSlot,
        shared: &Arc<Shared>,
        url: &str,
        w: u32,
        h: u32,
        css: &str,
    ) -> Option<Page> {
        let rect = NSRect::new(NSPoint::new(-20_000.0, -20_000.0), NSSize::new(f64::from(w), f64::from(h)));
        // SAFETY: AppKit window creation on the main thread (mtm) with valid arguments.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: as above; a window we own is released by its Retained, not on close.
        unsafe { window.setReleasedWhenClosed(false) };
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(f64::from(w), f64::from(h)));
        // SAFETY: WebKit objects made and configured on the main thread; each optional call is
        // made only where the object answers to it (respondsToSelector:).
        let view = unsafe {
            let cfg = WKWebViewConfiguration::new(mtm);
            // Sound plays without a click (it goes to the mixer, not the speakers).
            cfg.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::None);
            // The page keeps running though its window is off-screen: macOS 14+ says so in
            // public (inactiveSchedulingPolicy = none) …
            let prefs = cfg.preferences();
            let has: bool = msg_send![&*prefs, respondsToSelector: objc2::sel!(setInactiveSchedulingPolicy:)];
            if has {
                prefs.setInactiveSchedulingPolicy(WKInactiveSchedulingPolicy::None);
            }
            let view = WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &cfg);
            // … and WebKit's own switch covers macOS 13 (animations froze off-screen, 2.1 test).
            let sel = objc2::sel!(_setWindowOcclusionDetectionEnabled:);
            let has: bool = msg_send![&*view, respondsToSelector: sel];
            if has {
                let _: () = msg_send![&*view, _setWindowOcclusionDetectionEnabled: false];
            }
            view
        };
        // A clear page background (WKWebView's private-but-stable key, as OBS-like hosts use).
        // SAFETY: KVC on a live WKWebView on the main thread; "drawsBackground" is a BOOL key.
        unsafe {
            let no: Retained<AnyObject> = msg_send![objc2::class!(NSNumber), numberWithBool: false];
            let key = NSString::from_str("drawsBackground");
            let _: () = msg_send![&*view, setValue: &*no, forKey: &*key];
        }
        let holder = NSView::initWithFrame(NSView::alloc(mtm), frame);
        holder.addSubview(&view);
        window.setContentView(Some(&holder));
        let tap = AudioTap::new(mtm, shared.clone());
        let closer = WindowCloser::new(mtm, slot);
        window.setDelegate(Some(ProtocolObject::from_ref(&*closer)));
        window.orderBack(None);
        let mut page = Page {
            window,
            holder,
            view,
            css: css.to_string(),
            url: String::new(),
            size: (w, h),
            interact: None,
            tap,
            closer: Some(closer),
        };
        // SAFETY: registering a message handler on the live view's controller, main thread.
        unsafe {
            page.view
                .configuration()
                .userContentController()
                .addScriptMessageHandler_name(ProtocolObject::from_ref(&*page.tap), &NSString::from_str(AUDIO_HANDLER));
        }
        load(&mut page, url, css);
        Some(page)
    }

    /// The page's CSS, injected at the end of every document it loads (replaces the last).
    fn set_css(view: &WKWebView, css: &str) {
        let Some(mtm) = MainThreadMarker::new() else { return };
        // SAFETY: WebKit configuration calls on the live view, main thread.
        unsafe {
            let ucc = view.configuration().userContentController();
            ucc.removeAllUserScripts();
            let audio = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                WKUserScript::alloc(mtm),
                &NSString::from_str(AUDIO_SCRIPT),
                WKUserScriptInjectionTime::AtDocumentStart,
                false,
            );
            ucc.addUserScript(&audio);
            if !css.trim().is_empty() {
                let src = NSString::from_str(&css_script(css));
                let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                    WKUserScript::alloc(mtm),
                    &src,
                    WKUserScriptInjectionTime::AtDocumentEnd,
                    true,
                );
                ucc.addUserScript(&script);
            }
        }
    }

    fn load(page: &mut Page, url: &str, css: &str) {
        if page.css != css || page.url.is_empty() {
            set_css(&page.view, css);
        }
        page.css = css.to_string();
        page.url = url.to_string();
        let url = if url.trim().is_empty() { "about:blank" } else { url.trim() };
        let Some(u) = NSURL::URLWithString(&NSString::from_str(url)) else { return };
        let req = NSURLRequest::requestWithURL(&u);
        // SAFETY: a valid request on the live web view, main thread.
        unsafe {
            let _ = page.view.loadRequest(&req);
        }
    }

    fn resize(page: &mut Page, w: u32, h: u32) {
        if page.size == (w, h) {
            return;
        }
        page.size = (w, h);
        let at = page.interact.clone();
        place(page, at);
    }

    /// Something to do with a page: put it on screen to click (with the window's title), take
    /// it back, or load it again.
    pub(crate) enum Do {
        Interact(String),
        Stop,
        Reload,
    }

    /// Does it to the page behind a source's data (from `obs_obj_get_data`).
    pub(crate) fn act(data: *mut c_void, what: Do) {
        if data.is_null() {
            return;
        }
        // SAFETY: `data` is a live Source of this kind (the caller checked the kind and holds
        // the source); only its Arc'd page is cloned here.
        let page = unsafe { &*data.cast::<Source>() }.page.clone();
        let job = OnMain((page, what));
        DispatchQueue::main().exec_async(move || {
            let OnMain((page, what)) = job;
            let Ok(mut p) = page.lock() else { return };
            let Some(OnMain(p)) = p.as_mut() else { return };
            match what {
                Do::Interact(title) => place(p, Some(title)),
                Do::Stop => place(p, None),
                Do::Reload => {
                    // SAFETY: reload of the live view on the main thread.
                    unsafe {
                        let _ = p.view.reload();
                    }
                }
            }
        });
    }

    /// Copies a snapshot into the frame: drawn into a BGRA buffer of the source's size, then
    /// un-premultiplied (OBS's default draw takes straight alpha).
    fn copy(img: &NSImage, shared: &Shared) {
        let (w, h) = (shared.w.load(Ordering::Relaxed), shared.h.load(Ordering::Relaxed));
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        // SAFETY: CoreGraphics C calls with a buffer of exactly w*h*4 bytes and stride w*4; the
        // CGImage comes from a live NSImage during this call and is only borrowed.
        unsafe {
            let cg: *mut c_void = msg_send![img, CGImageForProposedRect: std::ptr::null_mut::<NSRect>(), context: std::ptr::null_mut::<AnyObject>(), hints: std::ptr::null_mut::<AnyObject>()];
            if cg.is_null() {
                return;
            }
            let cs = cg::CGColorSpaceCreateDeviceRGB();
            let ctx = cg::CGBitmapContextCreate(
                buf.as_mut_ptr().cast(),
                w as usize,
                h as usize,
                8,
                (w as usize) * 4,
                cs,
                cg::PREMULTIPLIED_FIRST | cg::BYTE_ORDER_32_LITTLE,
            );
            if !ctx.is_null() {
                let r = cg::CGRect { x: 0.0, y: 0.0, w: f64::from(w), h: f64::from(h) };
                cg::CGContextDrawImage(ctx, r, cg);
                cg::CGContextRelease(ctx);
            }
            cg::CGColorSpaceRelease(cs);
        }
        for px in buf.as_chunks_mut::<4>().0 {
            let a = u32::from(px[3]);
            if a != 0 && a != 255 {
                for c in &mut px[..3] {
                    *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        if let Ok(mut f) = shared.frame.lock() {
            *f = Frame { w, h, bgra: buf, seq: f.seq + 1 };
        }
    }

    /// Takes a snapshot when the source shows, then again after one frame's time; stops when
    /// the source goes. Main thread.
    fn shoot(page: Arc<Mutex<Option<OnMain<Page>>>>, shared: Arc<Shared>) {
        if !shared.alive.load(Ordering::Acquire) {
            return;
        }
        let again = |page: Arc<Mutex<Option<OnMain<Page>>>>, shared: Arc<Shared>, ms: u64| {
            let when = DispatchTime::try_from(Duration::from_millis(ms)).unwrap_or(DispatchTime::NOW);
            let job = OnMain((page, shared));
            let _ = DispatchQueue::main().after(when, move || {
                let OnMain((page, shared)) = job;
                shoot(page, shared);
            });
        };
        let fps = shared.fps.load(Ordering::Relaxed).max(1);
        if !shared.shown.load(Ordering::Acquire) {
            return again(page, shared, 250);
        }
        let view = match page.lock().ok().and_then(|p| p.as_ref().map(|p| p.0.view.clone())) {
            Some(v) => v,
            None => return again(page, shared, 250),
        };
        let started = std::time::Instant::now();
        let (p2, s2) = (page.clone(), shared.clone());
        let done = RcBlock::new(move |img: *mut NSImage, _err: *mut NSError| {
            if !img.is_null() {
                // SAFETY: WebKit hands a live NSImage for the length of this callback.
                copy(unsafe { &*img }, &s2);
            }
            let spent = started.elapsed().as_millis() as u64;
            let frame = 1000 / u64::from(fps);
            again(p2.clone(), s2.clone(), frame.saturating_sub(spent).max(1));
        });
        let mtm = MainThreadMarker::new();
        let Some(mtm) = mtm else { return };
        // SAFETY: snapshot of the live view on the main thread; the block outlives the call
        // (WebKit copies it).
        unsafe {
            let cfg = WKSnapshotConfiguration::new(mtm);
            cfg.setAfterScreenUpdates(false);
            view.takeSnapshotWithConfiguration_completionHandler(Some(&cfg), &done);
        }
    }

    unsafe extern "C" fn get_name(_t: *mut c_void) -> *const c_char {
        NAME.as_ptr()
    }

    unsafe extern "C" fn create(settings: *mut ObsData, obs: *mut ObsSource) -> *mut c_void {
        let (url, w, h, css, fps) = settings_of(settings);
        let shared = Arc::new(Shared {
            frame: Mutex::new(Frame::default()),
            shown: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            w: AtomicU32::new(w),
            h: AtomicU32::new(h),
            fps: AtomicU32::new(fps),
            source: Mutex::new(SrcPtr(obs)),
        });
        let page: PageSlot = Arc::new(Mutex::new(None));
        let job = OnMain((page.clone(), shared.clone()));
        let make = move || {
            let OnMain((page, shared)) = job;
            if let Some(mtm) = MainThreadMarker::new()
                && let Some(p) = open(mtm, &page, &shared, &url, w, h, &css)
                && let Ok(mut slot) = page.lock()
            {
                *slot = Some(OnMain(p));
            }
            shoot(page, shared);
        };
        // Sources are made on the main thread by the Studio; anything else waits for it.
        if MainThreadMarker::new().is_some() {
            make();
        } else {
            DispatchQueue::main().exec_async(make);
        }
        Box::into_raw(Box::new(Source { shared, page, tex: std::ptr::null_mut(), tex_size: (0, 0), seq: 0 })).cast()
    }

    unsafe extern "C" fn destroy(data: *mut c_void) {
        // SAFETY: `data` is the Box made in `create`, handed back once by libobs.
        let src = unsafe { Box::from_raw(data.cast::<Source>()) };
        src.shared.alive.store(false, Ordering::Release);
        if let Ok(mut s) = src.shared.source.lock() {
            s.0 = std::ptr::null_mut();
        }
        // The web view and its window go on the main thread.
        let job = OnMain(src.page.clone());
        DispatchQueue::main().exec_async(move || {
            let OnMain(page) = job;
            if let Ok(mut p) = page.lock()
                && let Some(OnMain(mut p)) = p.take()
            {
                // SAFETY: undoing what `open` registered, on the live objects, main thread.
                unsafe {
                    p.view
                        .configuration()
                        .userContentController()
                        .removeScriptMessageHandlerForName(&NSString::from_str(AUDIO_HANDLER));
                }
                p.window.setDelegate(None);
                p.closer = None;
                p.window.close();
            }
        });
        if !src.tex.is_null()
            && let Some(a) = api()
        {
            a.obs_enter_graphics();
            a.gs_texture_destroy(src.tex);
            a.obs_leave_graphics();
        }
    }

    unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
        // SAFETY: `data` is the live Source from `create`.
        unsafe { &*data.cast::<Source>() }.shared.w.load(Ordering::Relaxed)
    }

    unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
        // SAFETY: as for get_width.
        unsafe { &*data.cast::<Source>() }.shared.h.load(Ordering::Relaxed)
    }

    unsafe extern "C" fn get_defaults(s: *mut ObsData) {
        let Some(a) = api() else { return };
        let css = cstr(DEFAULT_CSS);
        a.obs_data_set_default_string(s, c"url".as_ptr(), c"".as_ptr());
        a.obs_data_set_default_int(s, c"width".as_ptr(), 1920);
        a.obs_data_set_default_int(s, c"height".as_ptr(), 1080);
        a.obs_data_set_default_string(s, c"css".as_ptr(), css.as_ptr());
        a.obs_data_set_default_int(s, c"fps".as_ptr(), 30);
    }

    unsafe extern "C" fn get_properties(_data: *mut c_void) -> *mut ObsProperties {
        let Some(a) = api() else { return std::ptr::null_mut() };
        let p = a.obs_properties_create();
        a.obs_properties_add_text(p, c"url".as_ptr(), c"Address".as_ptr(), 0);
        a.obs_properties_add_int(p, c"width".as_ptr(), c"Width".as_ptr(), 1, 8192, 1);
        a.obs_properties_add_int(p, c"height".as_ptr(), c"Height".as_ptr(), 1, 8192, 1);
        a.obs_properties_add_int(p, c"fps".as_ptr(), c"Frame rate".as_ptr(), 1, 60, 1);
        // A box of several lines (OBS_TEXT_MULTILINE), in a code font.
        let css = a.obs_properties_add_text(p, c"css".as_ptr(), c"Custom CSS".as_ptr(), 2);
        if !css.is_null() {
            a.obs_property_text_set_monospace(css, true);
        }
        p
    }

    unsafe extern "C" fn update(data: *mut c_void, s: *mut ObsData) {
        // SAFETY: `data` is the live Source from `create`.
        let src = unsafe { &*data.cast::<Source>() };
        let (url, w, h, css, fps) = settings_of(s);
        src.shared.w.store(w, Ordering::Relaxed);
        src.shared.h.store(h, Ordering::Relaxed);
        src.shared.fps.store(fps, Ordering::Relaxed);
        let job = OnMain(src.page.clone());
        DispatchQueue::main().exec_async(move || {
            let OnMain(page) = job;
            let Ok(mut p) = page.lock() else { return };
            let Some(OnMain(p)) = p.as_mut() else { return };
            resize(p, w, h);
            if p.url != url || p.css != css {
                load(p, &url, &css);
            }
        });
    }

    unsafe extern "C" fn show(data: *mut c_void) {
        // SAFETY: `data` is the live Source from `create`.
        unsafe { &*data.cast::<Source>() }.shared.shown.store(true, Ordering::Release);
    }

    unsafe extern "C" fn hide(data: *mut c_void) {
        // SAFETY: as for show.
        unsafe { &*data.cast::<Source>() }.shared.shown.store(false, Ordering::Release);
    }

    unsafe extern "C" fn video_render(data: *mut c_void, effect: *mut GsEffect) {
        let Some(a) = api() else { return };
        // SAFETY: `data` is the live Source; libobs calls render on its graphics thread only,
        // the one thread that touches `tex`, `tex_size` and `seq`.
        let src = unsafe { &mut *data.cast::<Source>() };
        if let Ok(f) = src.shared.frame.try_lock()
            && f.seq != src.seq
            && f.w > 0
            && f.h > 0
        {
            if src.tex.is_null() || src.tex_size != (f.w, f.h) {
                if !src.tex.is_null() {
                    a.gs_texture_destroy(src.tex);
                }
                let ptr = f.bgra.as_ptr();
                src.tex = a.gs_texture_create(f.w, f.h, super::GS_BGRA, 1, &ptr, GS_DYNAMIC);
                src.tex_size = (f.w, f.h);
            } else {
                a.gs_texture_set_image(src.tex, f.bgra.as_ptr(), f.w * 4, false);
            }
            src.seq = f.seq;
        }
        if src.tex.is_null() || effect.is_null() {
            return;
        }
        let param = a.gs_effect_get_param_by_name(effect, c"image".as_ptr());
        a.gs_effect_set_texture(param, src.tex);
        a.gs_draw_sprite(src.tex, 0, src.tex_size.0, src.tex_size.1);
    }

    const GS_DYNAMIC: u32 = 1 << 1;

    /// `struct obs_source_info` up to `video_render` (libobs 32.2's order); libobs takes the
    /// size given and zero-fills the rest.
    #[repr(C)]
    struct SourceInfo {
        id: *const c_char,
        kind: c_int,
        output_flags: u32,
        get_name: Option<unsafe extern "C" fn(*mut c_void) -> *const c_char>,
        create: Option<unsafe extern "C" fn(*mut ObsData, *mut ObsSource) -> *mut c_void>,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
        get_width: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
        get_height: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
        get_defaults: Option<unsafe extern "C" fn(*mut ObsData)>,
        get_properties: Option<unsafe extern "C" fn(*mut c_void) -> *mut ObsProperties>,
        update: Option<unsafe extern "C" fn(*mut c_void, *mut ObsData)>,
        activate: Option<unsafe extern "C" fn(*mut c_void)>,
        deactivate: Option<unsafe extern "C" fn(*mut c_void)>,
        show: Option<unsafe extern "C" fn(*mut c_void)>,
        hide: Option<unsafe extern "C" fn(*mut c_void)>,
        video_tick: Option<unsafe extern "C" fn(*mut c_void, f32)>,
        video_render: Option<unsafe extern "C" fn(*mut c_void, *mut GsEffect)>,
    }

    /// Registers the kind (once per process, after the plugins load).
    pub(crate) fn register(api: &Api) {
        if !API.load(Ordering::Acquire).is_null() {
            return;
        }
        API.store(api as *const Api as *mut Api, Ordering::Release);
        let info = SourceInfo {
            id: KIND.as_ptr(),
            kind: 0,
            output_flags: OBS_SOURCE_VIDEO | OBS_SOURCE_AUDIO | OBS_SOURCE_DO_NOT_DUPLICATE,
            get_name: Some(get_name),
            create: Some(create),
            destroy: Some(destroy),
            get_width: Some(get_width),
            get_height: Some(get_height),
            get_defaults: Some(get_defaults),
            get_properties: Some(get_properties),
            update: Some(update),
            activate: None,
            deactivate: None,
            show: Some(show),
            hide: Some(hide),
            video_tick: None,
            video_render: Some(video_render),
        };
        api.obs_register_source_s((&info as *const SourceInfo).cast(), std::mem::size_of::<SourceInfo>());
    }

    /// `obs-source.h`: the kind draws a picture and makes sound; it can't be duplicated.
    const OBS_SOURCE_VIDEO: u32 = 1 << 0;
    const OBS_SOURCE_AUDIO: u32 = 1 << 1;
    const OBS_SOURCE_DO_NOT_DUPLICATE: u32 = 1 << 7;

    /// The few CoreGraphics calls the snapshot copy needs.
    mod cg {
        use std::ffi::c_void;

        #[repr(C)]
        pub struct CGRect {
            pub x: f64,
            pub y: f64,
            pub w: f64,
            pub h: f64,
        }
        /// kCGImageAlphaPremultipliedFirst and kCGBitmapByteOrder32Little: BGRA in memory.
        pub const PREMULTIPLIED_FIRST: u32 = 2;
        pub const BYTE_ORDER_32_LITTLE: u32 = 2 << 12;

        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            pub fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
            pub fn CGColorSpaceRelease(cs: *mut c_void);
            pub fn CGBitmapContextCreate(
                data: *mut c_void,
                width: usize,
                height: usize,
                bits_per_component: usize,
                bytes_per_row: usize,
                space: *mut c_void,
                bitmap_info: u32,
            ) -> *mut c_void;
            pub fn CGContextDrawImage(c: *mut c_void, rect: CGRect, image: *mut c_void);
            pub fn CGContextRelease(c: *mut c_void);
        }
    }
}

/// Projector windows (macOS): titled and resizable, or borderless over a whole display.
#[cfg(target_os = "macos")]
pub(crate) mod projector_window {
    use std::rc::Rc;

    use dispatch2::DispatchQueue;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{
        NSBackingStoreType, NSEvent, NSResponder, NSScreen, NSView, NSWindow, NSWindowCollectionBehavior,
        NSWindowDelegate, NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    /// What a projector window tells its owner.
    pub struct Hooks {
        /// A click at (x, y) in a view `w`×`h` points big, from its top left.
        pub click: Box<dyn Fn(f64, f64, f64, f64)>,
        /// Closed (its close button, or Esc full screen): the owner drops it.
        pub close: Box<dyn Fn()>,
        /// The view's new size in pixels.
        pub resize: Box<dyn Fn(u32, u32)>,
    }

    /// Carries main-thread values to the main queue, where alone they're used.
    struct OnMain<T>(T);
    // SAFETY: made on the main thread and only used in closures the main queue runs.
    unsafe impl<T> Send for OnMain<T> {}

    /// Runs `f` on the main queue after the current event (never inside AppKit's callback, so
    /// the window can go away safely).
    fn later(hooks: &Rc<Hooks>, f: fn(&Hooks)) {
        let job = OnMain(hooks.clone());
        DispatchQueue::main().exec_async(move || {
            // The whole wrapper moves in (a closure would otherwise take just its field).
            let job = job;
            f(&job.0);
        });
    }

    define_class!(
        // SAFETY: NSView allows subclassing; ProjectorView doesn't implement Drop.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastProjectorView"]
        #[ivars = Rc<Hooks>]
        struct ProjectorView;

        impl ProjectorView {
            #[unsafe(method(acceptsFirstResponder))]
            fn accepts_first_responder(&self) -> bool {
                true
            }

            #[unsafe(method(acceptsFirstMouse:))]
            fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
                true
            }

            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, e: &NSEvent) {
                let p = self.convertPoint_fromView(e.locationInWindow(), None);
                let b = self.bounds();
                // Unflipped: AppKit counts up from the bottom.
                (self.ivars().click)(p.x, b.size.height - p.y, b.size.width, b.size.height);
            }

            #[unsafe(method(keyDown:))]
            fn key_down(&self, e: &NSEvent) {
                // Esc leaves a full-screen projector (OBS's key for it).
                if e.keyCode() == 53 {
                    later(self.ivars(), |h| (h.close)());
                }
            }

            #[unsafe(method(setFrameSize:))]
            fn set_frame_size(&self, size: NSSize) {
                // SAFETY: NSView's own setFrameSize: on this view, with the size AppKit gave.
                let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
                let px = self.convertSizeToBacking(size);
                (self.ivars().resize)(px.width.round().max(1.0) as u32, px.height.round().max(1.0) as u32);
            }
        }
    );

    define_class!(
        // SAFETY: NSWindow allows subclassing; ProjectorWin adds no ivars, no Drop.
        #[unsafe(super(NSWindow, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastProjectorWindow"]
        struct ProjectorWin;

        impl ProjectorWin {
            /// A borderless window takes keys too (Esc), like OBS's full-screen projector.
            #[unsafe(method(canBecomeKeyWindow))]
            fn can_become_key(&self) -> bool {
                true
            }
        }
    );

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; Closer doesn't implement Drop.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FerrocastProjectorCloser"]
        #[ivars = Rc<Hooks>]
        struct Closer;

        unsafe impl NSObjectProtocol for Closer {}

        unsafe impl NSWindowDelegate for Closer {
            /// The close button asks the owner to drop the projector (which closes it).
            #[unsafe(method(windowShouldClose:))]
            fn window_should_close(&self, _w: &NSWindow) -> bool {
                later(self.ivars(), |h| (h.close)());
                false
            }
        }
    );

    thread_local! {
        /// Where the next projector window's top left goes (main thread).
        static CASCADE: std::cell::Cell<Option<NSPoint>> = const { std::cell::Cell::new(None) };
    }

    /// An open projector window.
    pub struct Win {
        window: Retained<ProjectorWin>,
        view: Retained<ProjectorView>,
        _closer: Retained<Closer>,
    }

    /// The displays, the main one first as macOS lists them: (name, width, height in points).
    pub fn screens() -> Vec<(String, f64, f64)> {
        let Some(mtm) = MainThreadMarker::new() else { return Vec::new() };
        NSScreen::screens(mtm)
            .iter()
            .map(|s| {
                let f = s.frame();
                (s.localizedName().to_string(), f.size.width, f.size.height)
            })
            .collect()
    }

    /// Opens one: full screen on display `screen` (borderless, over the menu bar), or a window
    /// the shape `aspect` (width / height). None off the main thread or for no such display.
    pub fn open(title: &str, aspect: f64, screen: Option<usize>, hooks: Hooks) -> Option<Win> {
        let mtm = MainThreadMarker::new()?;
        let hooks = Rc::new(hooks);
        let full = match screen {
            Some(i) => {
                let all = NSScreen::screens(mtm);
                if i >= all.count() {
                    return None;
                }
                Some(all.objectAtIndex(i).frame())
            }
            None => None,
        };
        let (rect, style) = match full {
            Some(f) => (f, NSWindowStyleMask::Borderless),
            None => {
                let w = 960.0;
                let h = (w / if aspect > 0.0 { aspect } else { 16.0 / 9.0 }).round();
                (
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)),
                    NSWindowStyleMask::Titled
                        | NSWindowStyleMask::Closable
                        | NSWindowStyleMask::Miniaturizable
                        | NSWindowStyleMask::Resizable,
                )
            }
        };
        let alloc = ProjectorWin::alloc(mtm).set_ivars(());
        // SAFETY: NSWindow's designated initialiser on a freshly allocated ProjectorWin, on the
        // main thread, with valid arguments.
        let window: Retained<ProjectorWin> = unsafe {
            msg_send![super(alloc), initWithContentRect: rect, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        // SAFETY: the window is ours (its Retained releases it), not released on close.
        unsafe { window.setReleasedWhenClosed(false) };
        let content = NSRect::new(NSPoint::new(0.0, 0.0), rect.size);
        let alloc = ProjectorView::alloc(mtm).set_ivars(hooks.clone());
        // SAFETY: NSView's designated initialiser on a freshly allocated ProjectorView.
        let view: Retained<ProjectorView> = unsafe { msg_send![super(alloc), initWithFrame: content] };
        view.setWantsLayer(true);
        window.setContentView(Some(&view));
        let closer: Retained<Closer> = {
            let alloc = Closer::alloc(mtm).set_ivars(hooks);
            // SAFETY: NSObject's init on a freshly allocated object.
            unsafe { msg_send![super(alloc), init] }
        };
        window.setDelegate(Some(ProtocolObject::from_ref(&*closer)));
        window.setTitle(&NSString::from_str(title));
        if full.is_some() {
            // Over the menu bar and the Dock, on every Space, as OBS's full-screen projector.
            window.setLevel(25);
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            window.setFrame_display(rect, true);
        } else {
            window.setContentAspectRatio(NSSize::new(aspect.max(0.1), 1.0));
            // Each new window steps down and right from the last, as macOS cascades them.
            CASCADE.with(|at| match at.get() {
                Some(p) => at.set(Some(window.cascadeTopLeftFromPoint(p))),
                None => {
                    window.center();
                    let f = window.frame();
                    at.set(Some(window.cascadeTopLeftFromPoint(NSPoint::new(f.origin.x, f.origin.y + f.size.height))));
                }
            });
        }
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));
        Some(Win { window, view, _closer: closer })
    }

    impl std::fmt::Debug for Win {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Win").finish_non_exhaustive()
        }
    }

    impl Win {
        pub fn view_ptr(&self) -> *mut std::ffi::c_void {
            Retained::as_ptr(&self.view) as *mut std::ffi::c_void
        }

        pub fn number(&self) -> isize {
            self.window.windowNumber()
        }

        /// The view's size in pixels now.
        pub fn pixels(&self) -> (u32, u32) {
            let px = self.view.convertSizeToBacking(self.view.bounds().size);
            (px.width.round().max(1.0) as u32, px.height.round().max(1.0) as u32)
        }

        /// Keeps a windowed projector above other apps' windows (OBS's "Always on top"); a
        /// full-screen one stays above everything anyway.
        pub fn set_on_top(&self, on: bool) {
            if MainThreadMarker::new().is_some() && self.window.level() != 25 {
                // NSFloatingWindowLevel is 3; NSNormalWindowLevel 0.
                self.window.setLevel(if on { 3 } else { 0 });
            }
        }

        pub fn close(&self) {
            if MainThreadMarker::new().is_some() {
                self.window.setDelegate(None);
                self.window.orderOut(None);
                self.window.close();
            }
        }
    }
}
