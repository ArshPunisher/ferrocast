//! libobs's C API, loaded at runtime (`libloading`), so Ferrocast runs and builds where OBS's
//! engine isn't there. The only file in the workspace allowed `unsafe` (LESSONS R-73): every
//! block says why it is sound. Layouts match libobs 32.2 (`obs.h`, `graphics/graphics.h`).
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;

use libloading::Library;

/// Opaque libobs objects.
macro_rules! opaque {
    ($($n:ident),*) => { $( #[repr(C)] pub struct $n { _p: [u8; 0] } )* };
}
opaque!(
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

#[repr(C)]
#[derive(Debug)]
pub struct AudioInfo {
    pub samples_per_sec: u32,
    pub speakers: c_int,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
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
pub const SPEAKERS_STEREO: c_int = 2;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const GS_BGRA: c_int = 5;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const GS_ZS_NONE: c_int = 0;
pub const OBS_VIDEO_SUCCESS: c_int = 0;
/// `obs-source.h`: a kind replaced by a newer version, and one only for OBS's own use.
pub const OBS_SOURCE_DEPRECATED: u32 = 1 << 8;
pub const OBS_SOURCE_CAP_DISABLED: u32 = 1 << 10;
pub const OBS_BOUNDS_SCALE_INNER: c_int = 2;
pub const OBS_ALIGN_CENTER: u32 = 0;
pub const OBS_ALIGN_LEFT: u32 = 1;
pub const OBS_ALIGN_TOP: u32 = 1 << 2;
pub const LOG_ERROR: c_int = 100;
pub const LOG_WARNING: c_int = 200;
pub const LOG_INFO: c_int = 300;

pub type LogHandler = unsafe extern "C" fn(c_int, *const c_char, *mut c_void, *mut c_void);
pub type DrawCallback = unsafe extern "C" fn(*mut c_void, u32, u32);

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
                let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;
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
                Ok(Api { _lib: lib, _deps, fns })
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

api! {
    obs_startup: fn(a: *const c_char, b: *const c_char, c: *mut c_void) -> bool;
    obs_initialized: fn() -> bool;
    obs_shutdown: fn();
    obs_wait_for_destroy_queue: fn() -> bool;
    obs_get_version_string: fn() -> *const c_char;
    obs_add_data_path: fn(a: *const c_char);
    obs_reset_audio: fn(a: *const AudioInfo) -> bool;
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
    obs_sceneitem_set_pos: fn(a: *mut ObsSceneItem, b: *const Vec2);
    obs_sceneitem_set_alignment: fn(a: *mut ObsSceneItem, b: u32);
    obs_sceneitem_set_bounds_type: fn(a: *mut ObsSceneItem, b: c_int);
    obs_sceneitem_set_bounds_alignment: fn(a: *mut ObsSceneItem, b: u32);
    obs_sceneitem_set_bounds: fn(a: *mut ObsSceneItem, b: *const Vec2);
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
}

/// Loads every `.dylib` in `dir`, retrying until no more load: a library whose own `@rpath`
/// dependency isn't loaded yet fails the first round and loads in a later one.
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
                    Some("dylib") => Some(p),
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
            match unsafe { Library::new(p) } {
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

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// What the preview's draw callback needs: libobs and the canvas size.
#[derive(Debug)]
pub(crate) struct DrawState {
    pub api: *const Api,
    pub base: (u32, u32),
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
    let scale = (cx as f32 / bw).min(cy as f32 / bh);
    let (w, h) = ((bw * scale).round() as c_int, (bh * scale).round() as c_int);
    let (x, y) = ((cx as c_int - w) / 2, (cy as c_int - h) / 2);
    api.gs_viewport_push();
    api.gs_projection_push();
    api.gs_ortho(0.0, bw, 0.0, bh, -100.0, 100.0);
    api.gs_set_viewport(x, y, w, h);
    api.obs_render_main_texture();
    api.gs_projection_pop();
    api.gs_viewport_pop();
}

/// The native view a preview draws into, inside the window the dashboard lives in (macOS).
#[cfg(target_os = "macos")]
pub(crate) mod mac {
    use std::ffi::c_void;

    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::NSView;
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    /// A child view of the window's content, in top-left coordinates like the web page.
    #[derive(Debug)]
    pub struct ChildView {
        view: Retained<NSView>,
    }

    fn frame(parent: &NSView, x: f64, y: f64, w: f64, h: f64) -> NSRect {
        // AppKit counts from the bottom unless the view is flipped; the page counts from the top.
        let top = if parent.isFlipped() { y } else { parent.frame().size.height - y - h };
        NSRect::new(NSPoint::new(x, top), NSSize::new(w.max(1.0), h.max(1.0)))
    }

    /// Adds a view over `parent` (an `NSView *` the window toolkit handed out) at the rectangle.
    /// None off the main thread (AppKit allows views only there).
    pub fn child_view(parent: *mut c_void, x: f64, y: f64, w: f64, h: f64) -> Option<ChildView> {
        let mtm = MainThreadMarker::new()?;
        if parent.is_null() {
            return None;
        }
        // SAFETY: `parent` is the live NSView from the window toolkit's raw handle (Tauri/tao),
        // used on the main thread while that window exists.
        let parent: &NSView = unsafe { &*(parent as *const NSView) };
        let view = NSView::initWithFrame(NSView::alloc(mtm), frame(parent, x, y, w, h));
        view.setWantsLayer(true);
        parent.addSubview(&view);
        Some(ChildView { view })
    }

    impl ChildView {
        pub fn ptr(&self) -> *mut c_void {
            Retained::as_ptr(&self.view) as *mut c_void
        }

        /// Moves and resizes it (the page's box moved). Main thread only; ignored elsewhere.
        pub fn set_frame(&self, x: f64, y: f64, w: f64, h: f64) {
            if MainThreadMarker::new().is_none() {
                return;
            }
            // SAFETY: on the main thread (checked above); `superview` only reads the view tree.
            if let Some(parent) = unsafe { self.view.superview() } {
                self.view.setFrame(frame(&parent, x, y, w, h));
            }
        }

        pub fn remove(&self) {
            if MainThreadMarker::new().is_some() {
                self.view.removeFromSuperview();
            }
        }
    }
}
