//! The Studio's engine (2.1, LIVE-STUDIO.md Track B): OBS's own libobs and plugins, loaded at
//! runtime, behind a small safe API. Every source, filter, transition and encoder OBS has is
//! here unchanged (STUDIO-PARITY.md); options are read live through `obs_properties`, so a
//! generic settings form shows every one of them.
//!
//! libobs is one global engine per process: [`Studio::start`] refuses a second one, and
//! dropping the [`Studio`] shuts it down after every scene, source and output is released.

mod collection;
mod ffi;
pub mod import;
pub mod projector;

use std::ffi::c_int;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use raw_window_handle::RawWindowHandle;
use serde::{Deserialize, Serialize};

/// The kind id of Ferrocast's Web page source (macOS; OBS's Browser source keys).
pub const WEB_PAGE_KIND: &str = "ferrocast_web";

pub use collection::{
    AudioInfo, AudioSnap, BOUNDS, CHANNEL_KEYS, Collection, DEINTERLACE, FilterInfo, FilterSnap, ItemEdit, ItemInfo,
    ItemSnap, LevelInfo, Monitor, SceneInfo, SceneSnap, SourceUse, StudioOptions, TBarState, Transform, TransitionInfo,
    WebAct,
};
use ffi::{Api, cstr, text};
pub use projector::{ProjectorOf, Screen};

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Plugins that only work inside OBS's own (Qt) window; the Studio has its own interface
/// (spikes/libobs-mac: they crash without a QApplication).
const UI_ONLY: [&str; 6] =
    ["frontend-tools", "decklink-output-ui", "aja-output-ui", "decklink-captions", "obs-websocket", "aja"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StudioError {
    #[error("OBS's engine couldn't be loaded: {0}")]
    Load(String),
    #[error("the Studio is already running")]
    AlreadyRunning,
    #[error("OBS's engine didn't start")]
    Startup,
    #[error("the picture couldn't be set up (code {0})")]
    Video(i32),
    #[error("sound couldn't be set up")]
    Audio,
    #[error("{0}")]
    Failed(String),
}

/// Where libobs, its renderer, its data and its plugins are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The libobs library file itself.
    pub libobs: PathBuf,
    /// The renderer (libobs-metal / libobs-opengl / libobs-d3d11), with its file extension.
    pub renderer: PathBuf,
    /// libobs's own data (shaders): ends with a separator.
    pub data: String,
    /// Plugin binaries, `%module%` standing for each plugin's name.
    pub plugin_bin: String,
    /// Plugin data, `%module%` likewise.
    pub plugin_data: String,
    /// Libraries libobs and its plugins link to, loaded first (an OBS.app's Frameworks, or
    /// OBS's `bin/64bit` on Windows). None when they sit where libobs looks.
    pub deps: Option<PathBuf>,
}

impl Paths {
    /// An installed OBS.app (macOS): `contents` is `…/OBS.app/Contents`.
    pub fn from_obs_app(contents: &Path, metal: bool) -> Paths {
        let c = contents.display();
        let fw = contents.join("Frameworks");
        Paths {
            libobs: fw.join("libobs.framework/Versions/A/libobs"),
            renderer: fw.join(if metal { "libobs-metal.dylib" } else { "libobs-opengl.dylib" }),
            data: format!("{c}/Frameworks/libobs.framework/Resources/"),
            plugin_bin: format!("{c}/PlugIns/%module%.plugin/Contents/MacOS/"),
            plugin_data: format!("{c}/PlugIns/%module%.plugin/Contents/Resources/"),
            deps: Some(fw),
        }
    }

    /// An OBS install or portable folder on Windows: `root` holds `bin`, `data` and
    /// `obs-plugins`. D3D11 is OBS's renderer there (WARP, Windows's software one, where
    /// there's no GPU).
    pub fn from_obs_windows(root: &Path) -> Paths {
        let r = root.display();
        let bin = root.join("bin").join("64bit");
        Paths {
            libobs: bin.join("obs.dll"),
            renderer: bin.join("libobs-d3d11.dll"),
            data: format!("{r}/data/libobs/"),
            plugin_bin: format!("{r}/obs-plugins/64bit/"),
            plugin_data: format!("{r}/data/obs-plugins/%module%/"),
            deps: Some(bin),
        }
    }
}

/// The canvas: what's composed and sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for Canvas {
    fn default() -> Self {
        Canvas { width: 1920, height: 1080, fps: 30 }
    }
}

/// A kind of source, filter, transition, encoder or output, as OBS names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kind {
    pub id: String,
    pub name: String,
}

/// Every kind the loaded plugins offer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Kinds {
    pub inputs: Vec<Kind>,
    pub filters: Vec<Kind>,
    pub transitions: Vec<Kind>,
    pub encoders: Vec<Kind>,
    pub outputs: Vec<Kind>,
}

/// One option of a source or filter, enough to draw it in a settings form.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OptionInfo {
    /// The settings key.
    pub key: String,
    pub label: String,
    /// bool, int, float, text, path, list, color, color_alpha, button, font, editable_list,
    /// frame_rate, group.
    pub kind: &'static str,
    pub help: String,
    pub visible: bool,
    pub enabled: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    /// A number OBS shows as a slider.
    pub slider: bool,
    /// A number's unit as OBS writes it after the value (" dB", " ms", "%"), or "".
    pub suffix: String,
    /// A list's choices, as shown.
    pub choices: Vec<String>,
    /// The value saved for each choice (same order): a number, text or true/false.
    pub values: Vec<serde_json::Value>,
    /// A group's own options.
    pub group: Vec<OptionInfo>,
    /// A group with an on/off switch of its own (its key holds true or false).
    pub checkable: bool,
    /// A path's kind: "file", "save" or "folder"; an editable list's: "files" (files, or
    /// files and addresses) or "" (plain text, as for anything else).
    pub path: &'static str,
    /// The file types of a path or a list of files, as OBS writes them
    /// ("Image Files (*.png *.jpg);;All Files (*.*)").
    pub filter: String,
}

fn kind_name(t: c_int) -> &'static str {
    match t {
        1 => "bool",
        2 => "int",
        3 => "float",
        4 => "text",
        5 => "path",
        6 => "list",
        7 => "color",
        8 => "button",
        9 => "font",
        10 => "editable_list",
        11 => "frame_rate",
        12 => "group",
        13 => "color_alpha",
        _ => "other",
    }
}

/// How sound is set up: 48 kHz stereo unless asked otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSetup {
    pub sample_rate: u32,
    pub stereo: bool,
}

impl Default for AudioSetup {
    fn default() -> Self {
        AudioSetup { sample_rate: 48_000, stereo: true }
    }
}

/// A box inside a window, in points from its top-left corner (the page's own coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// The page's visible height (`window.innerHeight`), or 0 if unknown. The page can draw
    /// under a see-through title bar, so its top sits that far inside the webview; counting
    /// up from the webview's bottom by this height finds it (2.1: the preview sat a title bar
    /// too high).
    pub vh: f64,
}

/// OBS's engine, running. Cheap to clone (one engine, shared); scenes, recordings and
/// previews hold it too, so libobs shuts down only after the last of them is gone. Not `Send`:
/// it lives on one thread (the app's main thread, which the preview needs on macOS).
#[derive(Debug, Clone)]
pub struct Studio {
    inner: std::rc::Rc<Inner>,
}

#[derive(Debug)]
struct Inner {
    api: Api,
    canvas: std::cell::Cell<Canvas>,
    /// libobs's CPU meter for this process (`os_cpu_usage_info`).
    cpu: *mut ffi::CpuUsage,
}

impl Studio {
    /// Loads libobs, starts it with `canvas`, and loads every plugin but the interface-only
    /// ones. `config` holds the plugins' own settings and caches.
    pub fn start(paths: &Paths, canvas: Canvas, config: &Path) -> Result<Studio, StudioError> {
        Self::start_with(paths, canvas, AudioSetup::default(), config)
    }

    /// As [`Studio::start`], with the sound's sample rate and channels (fixed until the
    /// process ends: libobs sets sound up once).
    pub fn start_with(paths: &Paths, canvas: Canvas, audio: AudioSetup, config: &Path) -> Result<Studio, StudioError> {
        if RUNNING.swap(true, Ordering::SeqCst) {
            return Err(StudioError::AlreadyRunning);
        }
        match Self::start_inner(paths, canvas, audio, config) {
            Ok(s) => Ok(s),
            Err(e) => {
                RUNNING.store(false, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    fn start_inner(paths: &Paths, canvas: Canvas, audio: AudioSetup, config: &Path) -> Result<Studio, StudioError> {
        let api = Api::load(&paths.libobs, paths.deps.as_deref()).map_err(StudioError::Load)?;
        api.base_set_log_handler(Some(ffi::log_to_tracing), ptr::null_mut());
        let _ = std::fs::create_dir_all(config);
        let locale = cstr("en-US");
        let cfg = cstr(&format!("{}/", config.display()));
        if !api.obs_startup(locale.as_ptr(), cfg.as_ptr(), ptr::null_mut()) && !api.obs_initialized() {
            return Err(StudioError::Startup);
        }
        let data = cstr(&paths.data);
        api.obs_add_data_path(data.as_ptr());

        let audio = ffi::AudioInfo {
            samples_per_sec: audio.sample_rate,
            speakers: if audio.stereo { ffi::SPEAKERS_STEREO } else { ffi::SPEAKERS_MONO },
        };
        if !api.obs_reset_audio(&audio) {
            api.obs_shutdown();
            return Err(StudioError::Audio);
        }
        let renderer = cstr(&paths.renderer.display().to_string());
        let mut video = ffi::VideoInfo {
            graphics_module: renderer.as_ptr(),
            fps_num: canvas.fps,
            fps_den: 1,
            base_width: canvas.width,
            base_height: canvas.height,
            output_width: canvas.width,
            output_height: canvas.height,
            output_format: ffi::VIDEO_FORMAT_NV12,
            adapter: 0,
            gpu_conversion: true,
            colorspace: ffi::VIDEO_CS_709,
            range: ffi::VIDEO_RANGE_PARTIAL,
            scale_type: ffi::OBS_SCALE_BICUBIC,
        };
        let code = api.obs_reset_video(&mut video);
        if code != ffi::OBS_VIDEO_SUCCESS {
            api.obs_shutdown();
            return Err(StudioError::Video(code));
        }
        let (bin, pdata) = (cstr(&paths.plugin_bin), cstr(&paths.plugin_data));
        api.obs_add_module_path(bin.as_ptr(), pdata.as_ptr());
        for m in UI_ONLY {
            let name = cstr(m);
            api.obs_add_disabled_module(name.as_ptr());
        }
        api.obs_load_all_modules();
        api.obs_post_load_modules();
        let cpu = api.os_cpu_usage_info_start();
        let inner = std::rc::Rc::new(Inner { api, canvas: std::cell::Cell::new(canvas), cpu });
        // Ferrocast's own Web page source (WebKit), in place of OBS's Chromium one (R-78).
        #[cfg(target_os = "macos")]
        ffi::web::register(&inner.api);
        Ok(Studio { inner })
    }

    pub fn canvas(&self) -> Canvas {
        self.inner.canvas.get()
    }

    /// Changes the canvas (size, frame rate). Refused while streaming or recording, as OBS
    /// does; a preview made before shows the old size until it's made again.
    pub fn set_canvas(&self, canvas: Canvas, renderer: &Path) -> Result<(), StudioError> {
        if canvas == self.canvas() {
            return Ok(());
        }
        if !(320..=7680).contains(&canvas.width)
            || !(240..=4320).contains(&canvas.height)
            || !(1..=240).contains(&canvas.fps)
        {
            return Err(StudioError::Failed("canvas: 320x240 to 7680x4320, 1 to 240 fps.".into()));
        }
        let module = cstr(&renderer.display().to_string());
        let mut video = ffi::VideoInfo {
            graphics_module: module.as_ptr(),
            fps_num: canvas.fps,
            fps_den: 1,
            base_width: canvas.width,
            base_height: canvas.height,
            output_width: canvas.width,
            output_height: canvas.height,
            output_format: ffi::VIDEO_FORMAT_NV12,
            adapter: 0,
            gpu_conversion: true,
            colorspace: ffi::VIDEO_CS_709,
            range: ffi::VIDEO_RANGE_PARTIAL,
            scale_type: ffi::OBS_SCALE_BICUBIC,
        };
        match self.inner.api.obs_reset_video(&mut video) {
            ffi::OBS_VIDEO_SUCCESS => {
                self.inner.canvas.set(canvas);
                Ok(())
            }
            ffi::OBS_VIDEO_CURRENTLY_ACTIVE => Err(StudioError::Failed(
                "The picture size can't change while streaming or recording. Stop first.".into(),
            )),
            code => Err(StudioError::Video(code)),
        }
    }

    /// How the engine is doing now.
    pub fn stats(&self) -> EngineStats {
        let a = &self.inner.api;
        EngineStats {
            cpu: if self.inner.cpu.is_null() { 0.0 } else { a.os_cpu_usage_info_query(self.inner.cpu) },
            fps: a.obs_get_active_fps(),
            frame_ms: a.obs_get_average_frame_time_ns() as f64 / 1_000_000.0,
            lagged: a.obs_get_lagged_frames(),
            frames: a.obs_get_total_frames(),
            memory_mb: a.os_get_proc_resident_size() / (1024 * 1024),
        }
    }

    /// Where "Hear it" plays: every output device (name, id); "default" is the system's.
    pub fn monitoring_devices(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        self.inner
            .api
            .obs_enum_audio_monitoring_devices(ffi::collect_devices, &mut out as *mut _ as *mut std::ffi::c_void);
        out
    }

    pub fn monitoring_device(&self) -> (String, String) {
        let (mut name, mut id) = (ptr::null(), ptr::null());
        self.inner.api.obs_get_audio_monitoring_device(&mut name, &mut id);
        (text(name), text(id))
    }

    pub fn set_monitoring_device(&self, name: &str, id: &str) -> Result<(), StudioError> {
        let (n, i) = (cstr(name), cstr(id));
        if self.inner.api.obs_set_audio_monitoring_device(n.as_ptr(), i.as_ptr()) {
            Ok(())
        } else {
            Err(StudioError::Failed(format!("the device \"{name}\" can't play sound")))
        }
    }

    /// libobs's version ("32.2.2").
    pub fn version(&self) -> String {
        text(self.inner.api.obs_get_version_string())
    }

    /// Every kind the loaded plugins offer. Like OBS's Add menu, source kinds replaced by a
    /// newer version or kept for OBS's own use are left out.
    pub fn kinds(&self) -> Kinds {
        // `source`: input, filter and transition kinds, which have source flags.
        let list = |f: &dyn Fn(usize, *mut *const std::ffi::c_char) -> bool, encoder: bool, source: bool| {
            let mut out = Vec::new();
            for i in 0.. {
                let mut id = ptr::null();
                if !f(i, &mut id) {
                    break;
                }
                let flags = if source { self.inner.api.obs_get_source_output_flags(id) } else { 0 };
                if flags & (ffi::OBS_SOURCE_DEPRECATED | ffi::OBS_SOURCE_CAP_DISABLED) != 0 {
                    continue;
                }
                let name = if encoder {
                    text(self.inner.api.obs_encoder_get_display_name(id))
                } else {
                    text(self.inner.api.obs_source_get_display_name(id))
                };
                out.push(Kind { id: text(id), name });
            }
            out
        };
        Kinds {
            inputs: list(&|i, p| self.inner.api.obs_enum_input_types(i, p), false, true),
            filters: list(&|i, p| self.inner.api.obs_enum_filter_types(i, p), false, true),
            transitions: list(&|i, p| self.inner.api.obs_enum_transition_types(i, p), false, true),
            encoders: list(&|i, p| self.inner.api.obs_enum_encoder_types(i, p), true, false),
            outputs: list(&|i, p| self.inner.api.obs_enum_output_types(i, p), false, false)
                .into_iter()
                .map(|k| Kind { name: k.id.clone(), ..k })
                .collect(),
        }
    }

    /// The settings a new source of `kind` starts with (what OBS fills in for keys not given).
    pub fn defaults(&self, kind: &str) -> Result<serde_json::Value, StudioError> {
        let a = &self.inner.api;
        let id = cstr(kind);
        let data = a.obs_get_source_defaults(id.as_ptr());
        if data.is_null() {
            return Err(StudioError::Failed(format!("OBS has no source kind \"{kind}\".")));
        }
        let json = text(a.obs_data_get_json_with_defaults(data));
        a.obs_data_release(data);
        serde_json::from_str(&json).map_err(|e| StudioError::Failed(format!("OBS's settings didn't read: {e}")))
    }

    /// Every option of a kind of source or filter, read from a private (unlisted) source the
    /// way OBS's own dialog does: some plugins can't describe themselves without one. A camera
    /// or screen kind may ask macOS for permission the first time.
    pub fn options(&self, kind: &str) -> Result<Vec<OptionInfo>, StudioError> {
        let id = cstr(kind);
        let name = cstr("ferrocast-probe");
        let src = self.inner.api.obs_source_create_private(id.as_ptr(), name.as_ptr(), ptr::null_mut());
        if src.is_null() {
            return Err(StudioError::Failed(format!("OBS has no source kind \"{kind}\".")));
        }
        let out = self.source_options(src, false);
        self.inner.api.obs_source_release(src);
        Ok(out)
    }

    /// An encoder's own options (OBS's Advanced output settings), as they look for
    /// `settings` (some show or hide by what's chosen), and its defaults.
    pub fn encoder_options(
        &self,
        id: &str,
        settings: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Vec<OptionInfo>, serde_json::Value), StudioError> {
        let a = &self.inner.api;
        let k = cstr(id);
        let defaults = a.obs_encoder_defaults(k.as_ptr());
        if defaults.is_null() {
            return Err(StudioError::Failed(format!("OBS has no encoder \"{id}\".")));
        }
        let json = cstr(&serde_json::Value::Object(settings.clone()).to_string());
        let over = a.obs_data_create_from_json(json.as_ptr());
        if !over.is_null() {
            a.obs_data_apply(defaults, over);
            a.obs_data_release(over);
        }
        let props = a.obs_get_encoder_properties(k.as_ptr());
        let out = if props.is_null() {
            Vec::new()
        } else {
            a.obs_properties_apply_settings(props, defaults);
            let o = self.read_options(props);
            a.obs_properties_destroy(props);
            o
        };
        let values = serde_json::from_str(&text(a.obs_data_get_json_with_defaults(defaults))).unwrap_or_default();
        a.obs_data_release(defaults);
        Ok((out, values))
    }

    /// A source's options; with `current`, as they look for its own settings (OBS shows or
    /// hides some options, and fills some lists, from what's chosen).
    fn source_options(&self, src: *mut ffi::ObsSource, current: bool) -> Vec<OptionInfo> {
        let a = &self.inner.api;
        let props = a.obs_source_properties(src);
        if props.is_null() {
            return Vec::new();
        }
        if current {
            let data = a.obs_source_get_settings(src);
            if !data.is_null() {
                a.obs_properties_apply_settings(props, data);
                a.obs_data_release(data);
            }
        }
        let out = self.read_options(props);
        a.obs_properties_destroy(props);
        out
    }

    fn read_options(&self, props: *mut ffi::ObsProperties) -> Vec<OptionInfo> {
        let a = &self.inner.api;
        let mut out = Vec::new();
        let mut p = a.obs_properties_first(props);
        while !p.is_null() {
            let t = a.obs_property_get_type(p);
            let kind = kind_name(t);
            // A number OBS shows as a slider, and its unit (" dB", " ms", "%").
            let (slider, suffix) = match kind {
                "int" => (a.obs_property_int_type(p) == 1, text(a.obs_property_int_suffix(p))),
                "float" => (a.obs_property_float_type(p) == 1, text(a.obs_property_float_suffix(p))),
                _ => (false, String::new()),
            };
            let (min, max, step) = match kind {
                "int" => (
                    Some(f64::from(a.obs_property_int_min(p))),
                    Some(f64::from(a.obs_property_int_max(p))),
                    Some(f64::from(a.obs_property_int_step(p))),
                ),
                "float" => (
                    Some(a.obs_property_float_min(p)),
                    Some(a.obs_property_float_max(p)),
                    Some(a.obs_property_float_step(p)),
                ),
                _ => (None, None, None),
            };
            let (choices, values) = if kind == "list" {
                let format = a.obs_property_list_format(p);
                (0..a.obs_property_list_item_count(p))
                    .map(|i| {
                        let value = match format {
                            1 => serde_json::json!(a.obs_property_list_item_int(p, i)),
                            2 => serde_json::json!(a.obs_property_list_item_float(p, i)),
                            4 => serde_json::json!(a.obs_property_list_item_bool(p, i)),
                            _ => serde_json::json!(text(a.obs_property_list_item_string(p, i))),
                        };
                        (text(a.obs_property_list_item_name(p, i)), value)
                    })
                    .unzip()
            } else {
                (Vec::new(), Vec::new())
            };
            let group = if kind == "group" {
                let g = a.obs_property_group_content(p);
                if g.is_null() { Vec::new() } else { self.read_options(g) }
            } else {
                Vec::new()
            };
            let (path, filter) = if kind == "path" {
                let t = match a.obs_property_path_type(p) {
                    1 => "save",
                    2 => "folder",
                    _ => "file",
                };
                (t, text(a.obs_property_path_filter(p)))
            } else if kind == "editable_list" {
                // OBS_EDITABLE_LIST_TYPE_STRINGS is 0; FILES and FILES_AND_URLS take files.
                let t = if a.obs_property_editable_list_type(p) == 0 { "" } else { "files" };
                (t, text(a.obs_property_editable_list_filter(p)))
            } else {
                ("", String::new())
            };
            // enum obs_group_type: OBS_GROUP_CHECKABLE is 2.
            let checkable = kind == "group" && a.obs_property_group_type(p) == 2;
            out.push(OptionInfo {
                checkable,
                path,
                filter,
                key: text(a.obs_property_name(p)),
                label: text(a.obs_property_description(p)),
                kind,
                help: text(a.obs_property_long_description(p)),
                visible: a.obs_property_visible(p),
                enabled: a.obs_property_enabled(p),
                min,
                max,
                step,
                slider,
                suffix: suffix.trim().to_string(),
                choices,
                values,
                group,
            });
            if !a.obs_property_next(&mut p) {
                break;
            }
        }
        out
    }

    /// A new, empty scene.
    pub fn scene(&self, name: &str) -> Result<Scene, StudioError> {
        let n = cstr(name);
        let s = self.inner.api.obs_scene_create(n.as_ptr());
        if s.is_null() {
            return Err(StudioError::Failed(format!("the scene \"{name}\" couldn't be made")));
        }
        Ok(Scene { studio: self.clone(), ptr: s, sources: std::cell::RefCell::new(Vec::new()) })
    }

    /// Puts a scene on air (what's recorded and streamed).
    pub fn set_program(&self, scene: &Scene) {
        self.inner.api.obs_set_output_source(0, self.inner.api.obs_scene_get_source(scene.ptr));
    }

    /// Takes whatever is on air off (nothing is recorded or streamed until a scene goes back).
    pub fn clear_program(&self) {
        self.inner.api.obs_set_output_source(0, ptr::null_mut());
    }

    /// Frames drawn since start, and how many were late.
    pub fn frames(&self) -> (u32, u32) {
        (self.inner.api.obs_get_total_frames(), self.inner.api.obs_get_lagged_frames())
    }

    /// Starts recording what's on air to `path` (`.mp4` or `.mov`, OBS's own crash-safe hybrid
    /// writers); video as `enc` says, AAC sound (track 1).
    pub fn record(&self, path: &Path, enc: &EncodeSettings) -> Result<Recording, StudioError> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if ext != "mp4" && ext != "mov" {
            return Err(StudioError::Failed("recordings are MP4 or MOV".into()));
        }
        self.record_to(path, &RecordPlan { format: ext, encode: enc.clone(), ..RecordPlan::default() })
    }

    /// A recording's file name now, from OBS's name pattern (`%CCYY-%MM-%DD %hh-%mm-%ss`…), in
    /// `plan.folder`; with " (2)", " (3)"… when a file of that name is there (unless it may be
    /// overwritten).
    pub fn record_path(&self, plan: &RecordPlan) -> PathBuf {
        let a = &self.inner.api;
        let (ext, fmt) = (cstr(plan.extension()), cstr(&plan.name));
        let p = a.os_generate_formatted_filename(ext.as_ptr(), plan.spaces, fmt.as_ptr());
        let name = if p.is_null() {
            format!("Recording.{}", plan.extension())
        } else {
            let n = text(p);
            a.bfree(p.cast());
            n
        };
        let first = plan.folder.join(&name);
        if plan.overwrite || !first.exists() {
            return first;
        }
        let stem = first.file_stem().and_then(|s| s.to_str()).unwrap_or("Recording").to_string();
        let gap = if plan.spaces { " " } else { "_" };
        (2..)
            .map(|n| plan.folder.join(format!("{stem}{gap}({n}).{}", plan.extension())))
            .find(|p| !p.exists())
            .unwrap_or(first)
    }

    /// Whether OBS's mux helper sits next to this program (MKV, FLV, TS and fragmented
    /// MP4/MOV recordings need it).
    pub fn muxer_ready() -> bool {
        let name = if cfg!(windows) { "obs-ffmpeg-mux.exe" } else { "obs-ffmpeg-mux" };
        std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name))).is_some_and(|p| p.exists())
    }

    /// Starts recording what's on air to `path` as `plan` says: its format, the audio tracks,
    /// splitting by time or size, and the names of the next parts.
    pub fn record_to(&self, path: &Path, plan: &RecordPlan) -> Result<Recording, StudioError> {
        let a = &self.inner.api;
        let (id, muxer) = match plan.format.as_str() {
            "mp4" => ("mp4_output", ""),
            "mov" => ("mov_output", ""),
            "fmp4" | "fmov" => ("ffmpeg_muxer", "movflags=frag_keyframe+empty_moov+delay_moov"),
            "mkv" | "flv" | "ts" => ("ffmpeg_muxer", ""),
            f => return Err(StudioError::Failed(format!("format: mp4, mov, fmp4, fmov, mkv, flv or ts, not \"{f}\""))),
        };
        if id == "ffmpeg_muxer" && !Studio::muxer_ready() {
            return Err(StudioError::Failed(
                "MKV, FLV, TS and fragmented recordings need OBS's mux helper beside Ferrocast; record as MP4 or MOV"
                    .into(),
            ));
        }
        let tracks: Vec<u8> = if plan.tracks.is_empty() { vec![1] } else { plan.tracks.clone() };
        if tracks.iter().any(|t| !(1..=6).contains(t)) {
            return Err(StudioError::Failed("tracks: numbers 1 to 6.".into()));
        }
        let (venc, audio) = self.encoders_for_tracks(&plan.encode, &tracks)?;
        let settings = a.obs_data_create();
        let set_str = |k: &str, v: &str| {
            let (k, v) = (cstr(k), cstr(v));
            a.obs_data_set_string(settings, k.as_ptr(), v.as_ptr());
        };
        let set_int = |k: &str, v: i64| {
            let k = cstr(k);
            a.obs_data_set_int(settings, k.as_ptr(), v);
        };
        let set_bool = |k: &str, v: bool| {
            let k = cstr(k);
            a.obs_data_set_bool(settings, k.as_ptr(), v);
        };
        set_str("path", &path.display().to_string());
        if !muxer.is_empty() {
            set_str("muxer_settings", muxer);
        }
        // Splitting: each part is named like the first, in the same folder.
        set_bool("split_file", true);
        set_int("max_time_sec", i64::from(plan.split_minutes) * 60);
        set_int("max_size_mb", i64::from(plan.split_mb));
        set_bool("allow_overwrite", plan.overwrite);
        set_str("directory", &path.parent().unwrap_or(&plan.folder).display().to_string());
        set_str("format", &plan.name);
        set_str("extension", plan.extension());
        set_bool("allow_spaces", plan.spaces);
        let (oid, oname) = (cstr(id), cstr("ferrocast-record"));
        let out = a.obs_output_create(oid.as_ptr(), oname.as_ptr(), settings, ptr::null_mut());
        a.obs_data_release(settings);
        let rec = Recording { studio: self.clone(), out, venc, audio, path: path.to_path_buf() };
        if out.is_null() {
            return Err(StudioError::Failed("the recorder couldn't be made".into()));
        }
        a.obs_output_set_video_encoder(out, venc);
        for (i, e) in rec.audio.iter().enumerate() {
            a.obs_output_set_audio_encoder(out, *e, i);
        }
        if !a.obs_output_start(out) {
            return Err(StudioError::Failed(format!(
                "recording didn't start: {}",
                text(a.obs_output_get_last_error(out))
            )));
        }
        Ok(rec)
    }

    /// Starts OBS's replay buffer: the last `plan.seconds` kept, saved on request (needs
    /// OBS's mux helper beside this program).
    pub fn replay_buffer(&self, plan: &ReplayPlan) -> Result<ReplayBuffer, StudioError> {
        let a = &self.inner.api;
        if !Studio::muxer_ready() {
            return Err(StudioError::Failed("The replay buffer needs OBS's mux helper beside Ferrocast.".into()));
        }
        if !(5..=21_600).contains(&plan.seconds) {
            return Err(StudioError::Failed("seconds: 5 to 21600.".into()));
        }
        if !["mkv", "mp4", "mov", "flv", "ts"].contains(&plan.format.as_str()) {
            return Err(StudioError::Failed("format: mkv, mp4, mov, flv or ts.".into()));
        }
        let tracks: Vec<u8> = if plan.tracks.is_empty() { vec![1] } else { plan.tracks.clone() };
        if tracks.iter().any(|t| !(1..=6).contains(t)) {
            return Err(StudioError::Failed("tracks: numbers 1 to 6.".into()));
        }
        let (venc, audio) = self.encoders_for_tracks(&plan.encode, &tracks)?;
        let settings = a.obs_data_create();
        let set_str = |k: &str, v: &str| {
            let (k, v) = (cstr(k), cstr(v));
            a.obs_data_set_string(settings, k.as_ptr(), v.as_ptr());
        };
        let set_int = |k: &str, v: i64| {
            let k = cstr(k);
            a.obs_data_set_int(settings, k.as_ptr(), v);
        };
        set_str("directory", &plan.folder.display().to_string());
        set_str("format", &plan.name);
        set_str("extension", &plan.format);
        set_int("max_time_sec", i64::from(plan.seconds));
        set_int("max_size_mb", i64::from(plan.mb));
        let k = cstr("allow_spaces");
        a.obs_data_set_bool(settings, k.as_ptr(), plan.spaces);
        let (oid, oname) = (cstr("replay_buffer"), cstr("ferrocast-replay"));
        let out = a.obs_output_create(oid.as_ptr(), oname.as_ptr(), settings, ptr::null_mut());
        a.obs_data_release(settings);
        let rb = ReplayBuffer { studio: self.clone(), out, venc, audio };
        if out.is_null() {
            return Err(StudioError::Failed("the replay buffer couldn't be made".into()));
        }
        a.obs_output_set_video_encoder(out, venc);
        for (i, e) in rb.audio.iter().enumerate() {
            a.obs_output_set_audio_encoder(out, *e, i);
        }
        if !a.obs_output_start(out) {
            return Err(StudioError::Failed(format!(
                "the replay buffer didn't start: {}",
                text(a.obs_output_get_last_error(out))
            )));
        }
        Ok(rb)
    }

    /// Starts OBS's virtual camera showing what's on air, or one scene or source.
    pub fn virtual_camera(&self, shows: &CameraShows) -> Result<VirtualCamera, StudioError> {
        let a = &self.inner.api;
        let (oid, oname) = (cstr("virtualcam_output"), cstr("ferrocast-virtualcam"));
        let out = a.obs_output_create(oid.as_ptr(), oname.as_ptr(), ptr::null_mut(), ptr::null_mut());
        if out.is_null() {
            return Err(StudioError::Failed("OBS's virtual camera isn't available here".into()));
        }
        let mut cam = VirtualCamera { studio: self.clone(), out, view: ptr::null_mut() };
        if let CameraShows::Source(name) = shows {
            let n = cstr(name);
            let src = a.obs_get_source_by_name(n.as_ptr());
            if src.is_null() {
                return Err(StudioError::Failed(format!("there's no scene or source called \"{name}\"")));
            }
            let view = a.obs_view_create();
            a.obs_view_set_source(view, 0, src);
            a.obs_source_release(src);
            cam.view = view;
            let video = a.obs_view_add(view);
            if video.is_null() {
                return Err(StudioError::Failed("the virtual camera's picture couldn't be made".into()));
            }
            a.obs_output_set_media(out, video, a.obs_get_audio());
        }
        if !a.obs_output_start(out) {
            let why = text(a.obs_output_get_last_error(out));
            return Err(StudioError::Failed(if why.is_empty() {
                "the virtual camera didn't start".into()
            } else {
                format!("the virtual camera didn't start: {why}")
            }));
        }
        Ok(cam)
    }

    /// The video encoder and one AAC encoder per audio track (1 to 6), in that order.
    fn encoders_for_tracks(
        &self,
        e: &EncodeSettings,
        tracks: &[u8],
    ) -> Result<(*mut ffi::ObsEncoder, Vec<*mut ffi::ObsEncoder>), StudioError> {
        let a = &self.inner.api;
        let (venc, aenc) = self.encoders(e)?;
        // The default sound encoder mixes track 1; each track gets its own instead.
        a.obs_encoder_release(aenc);
        let mut audio = Vec::new();
        for t in tracks {
            match self.audio_encoder(e.audio_kbps, usize::from(t - 1)) {
                Ok(enc) => audio.push(enc),
                Err(err) => {
                    a.obs_encoder_release(venc);
                    audio.iter().for_each(|x| a.obs_encoder_release(*x));
                    return Err(err);
                }
            }
        }
        Ok((venc, audio))
    }

    /// An AAC encoder mixing audio track `mixer` (0 to 5).
    fn audio_encoder(&self, kbps: u32, mixer: usize) -> Result<*mut ffi::ObsEncoder, StudioError> {
        let a = &self.inner.api;
        let s = a.obs_data_create();
        let b = cstr("bitrate");
        a.obs_data_set_int(s, b.as_ptr(), i64::from(kbps));
        let name = cstr(&format!("ferrocast-audio-{}", mixer + 1));
        let mut enc = ptr::null_mut();
        for id in ["ffmpeg_aac", "CoreAudio_AAC"] {
            let aid = cstr(id);
            enc = a.obs_audio_encoder_create(aid.as_ptr(), name.as_ptr(), s, mixer, ptr::null_mut());
            if !enc.is_null() {
                break;
            }
        }
        a.obs_data_release(s);
        if enc.is_null() {
            return Err(StudioError::Failed("no AAC encoder is available".into()));
        }
        a.obs_encoder_set_audio(enc, a.obs_get_audio());
        Ok(enc)
    }

    /// Rewrites a recording (MKV, FLV, TS…) as `to`, as OBS's Remux does. See [`Remuxer`].
    pub fn remux(&self, from: &Path, to: &Path, progress: impl FnMut(f32) -> bool) -> Result<(), StudioError> {
        self.remuxer().remux(from, to, progress)
    }

    /// A handle that remuxes on any thread (a long file needn't hold up the main one).
    pub fn remuxer(&self) -> Remuxer {
        Remuxer { fns: self.inner.api.remux_fns() }
    }

    /// Streams what's on air over RTMP to `server` with `key` (OBS's own RTMP output), with
    /// its reconnects and delay as `s` says. Stops when the handle is dropped.
    pub fn stream(&self, server: &str, key: &str, s: &StreamSettings) -> Result<Streaming, StudioError> {
        let a = &self.inner.api;
        let (venc, aenc) = self.encoders(&s.encode)?;
        let settings = a.obs_data_create();
        for (k, v) in [("server", server), ("key", key)] {
            let (k, v) = (cstr(k), cstr(v));
            a.obs_data_set_string(settings, k.as_ptr(), v.as_ptr());
        }
        let (sid, sname) = (cstr("rtmp_custom"), cstr("ferrocast-service"));
        let service = a.obs_service_create(sid.as_ptr(), sname.as_ptr(), settings, ptr::null_mut());
        a.obs_data_release(settings);
        let (oid, oname) = (cstr("rtmp_output"), cstr("ferrocast-stream"));
        let out = a.obs_output_create(oid.as_ptr(), oname.as_ptr(), ptr::null_mut(), ptr::null_mut());
        let stream =
            Streaming { studio: self.clone(), out, service, venc, aenc, settings: s.clone(), directs: Vec::new() };
        if service.is_null() || out.is_null() {
            return Err(StudioError::Failed("the stream output couldn't be made".into()));
        }
        a.obs_output_set_video_encoder(out, venc);
        a.obs_output_set_audio_encoder(out, aenc, 0);
        a.obs_output_set_service(out, service);
        match s.reconnect {
            Some((retries, every_s)) => a.obs_output_set_reconnect_settings(out, retries as c_int, every_s as c_int),
            None => a.obs_output_set_reconnect_settings(out, 0, 0),
        }
        if s.delay_s > 0 {
            a.obs_output_set_delay(out, s.delay_s, ffi::OBS_OUTPUT_DELAY_PRESERVE);
        }
        if !a.obs_output_start(out) {
            let why = text(a.obs_output_get_last_error(out));
            return Err(StudioError::Failed(if why.is_empty() {
                "the stream didn't start: the server refused or couldn't be reached".into()
            } else {
                format!("the stream didn't start: {why}")
            }));
        }
        Ok(stream)
    }

    /// The video encoder `e` describes and an AAC audio encoder, tied to the canvas.
    fn encoders(&self, e: &EncodeSettings) -> Result<(*mut ffi::ObsEncoder, *mut ffi::ObsEncoder), StudioError> {
        self.encoders_for(e, false)
    }

    /// As [`Studio::encoders`]; `opus` makes the sound Opus (WebRTC takes only that).
    fn encoders_for(
        &self,
        e: &EncodeSettings,
        opus: bool,
    ) -> Result<(*mut ffi::ObsEncoder, *mut ffi::ObsEncoder), StudioError> {
        let a = &self.inner.api;
        let vs = a.obs_data_create();
        let set_int = |k: &str, v: i64| {
            let k = cstr(k);
            a.obs_data_set_int(vs, k.as_ptr(), v);
        };
        let set_str = |k: &str, v: &str| {
            let (k, v) = (cstr(k), cstr(v));
            a.obs_data_set_string(vs, k.as_ptr(), v.as_ptr());
        };
        let x264 = e.video_encoder == "obs_x264";
        match e.quality {
            // A bitrate, steady (CBR) or varying around it (VBR).
            None => {
                set_int("bitrate", i64::from(e.video_kbps));
                set_str("rate_control", if e.vbr { "VBR" } else { "CBR" });
            }
            // Constant quality for recordings: 0 lossless … 51 worst (x264 CRF); VideoToolbox
            // takes 0-100, higher better.
            Some(q) => {
                set_str("rate_control", "CRF");
                if x264 {
                    set_int("crf", i64::from(q.min(51)));
                } else {
                    set_int("quality", i64::from(100 - (q.min(51) * 100 / 51)));
                }
            }
        }
        set_int("keyint_sec", i64::from(e.keyframe_s));
        set_str("profile", &e.profile);
        if x264 {
            set_str("preset", if e.quality == Some(0) { "ultrafast" } else { "veryfast" });
            if !e.bframes {
                set_str("x264opts", "bframes=0");
            }
        } else {
            let k = cstr("bframes");
            a.obs_data_set_bool(vs, k.as_ptr(), e.bframes);
        }
        if !e.extra.is_empty() {
            let json = cstr(&serde_json::Value::Object(e.extra.clone()).to_string());
            let over = a.obs_data_create_from_json(json.as_ptr());
            if !over.is_null() {
                a.obs_data_apply(vs, over);
                a.obs_data_release(over);
            }
        }
        let (vid, vname) = (cstr(&e.video_encoder), cstr("ferrocast-video"));
        let venc = a.obs_video_encoder_create(vid.as_ptr(), vname.as_ptr(), vs, ptr::null_mut());
        a.obs_data_release(vs);
        if venc.is_null() {
            return Err(StudioError::Failed(format!("the encoder \"{}\" isn't available", e.video_encoder)));
        }
        if let Some((w, h)) = e.scale {
            a.obs_encoder_set_scaled_size(venc, w, h);
        }
        let audio_kbps = e.audio_kbps;
        let as_ = a.obs_data_create();
        let b = cstr("bitrate");
        a.obs_data_set_int(as_, b.as_ptr(), i64::from(audio_kbps));
        let aname = cstr("ferrocast-audio");
        let mut aenc = ptr::null_mut();
        let kinds: &[&str] = if opus { &["ffmpeg_opus"] } else { &["ffmpeg_aac", "CoreAudio_AAC"] };
        for id in kinds {
            let aid = cstr(id);
            aenc = a.obs_audio_encoder_create(aid.as_ptr(), aname.as_ptr(), as_, 0, ptr::null_mut());
            if !aenc.is_null() {
                break;
            }
        }
        a.obs_data_release(as_);
        if aenc.is_null() {
            a.obs_encoder_release(venc);
            return Err(StudioError::Failed(
                if opus { "no Opus encoder is available" } else { "no AAC encoder is available" }.into(),
            ));
        }
        a.obs_encoder_set_video(venc, a.obs_get_video());
        a.obs_encoder_set_audio(aenc, a.obs_get_audio());
        Ok((venc, aenc))
    }
}

impl Studio {
    /// Shows what's on air at `rect` inside `window` (the raw handle the window toolkit gives,
    /// e.g. Tauri's), drawn by libobs straight into a native view: no copies through the page.
    /// `scale` is the screen's pixels per point (2 on a Retina display). macOS, main thread.
    pub fn preview(&self, window: RawWindowHandle, rect: Rect, scale: f64) -> Result<Preview, StudioError> {
        #[cfg(target_os = "macos")]
        {
            let RawWindowHandle::AppKit(h) = window else {
                return Err(StudioError::Failed("that isn't a macOS window".into()));
            };
            let view = ffi::mac::child_view(h.ns_view.as_ptr(), rect.x, rect.y, rect.w, rect.h, rect.vh)
                .ok_or_else(|| StudioError::Failed("the preview can only be made on the main thread".into()))?;
            let (cx, cy) = px(rect, scale);
            let init = ffi::GsInitData {
                window: view.ptr(),
                cx,
                cy,
                num_backbuffers: 0,
                format: ffi::GS_BGRA,
                zsformat: ffi::GS_ZS_NONE,
                adapter: 0,
            };
            // The clear colour (0xAABBGGRR) is the Studio's stage, #0a0b0d, never red.
            let display = self.inner.api.obs_display_create(&init, 0xFF0D_0B0A);
            if display.is_null() {
                view.remove();
                return Err(StudioError::Failed("the preview couldn't be made".into()));
            }
            let state = Box::new(ffi::DrawState {
                api: &self.inner.api,
                base: (self.canvas().width, self.canvas().height),
                selected: std::sync::Mutex::new(None),
                scene: std::sync::Mutex::new(ptr::null_mut()),
            });
            let param = &*state as *const ffi::DrawState as *mut std::ffi::c_void;
            self.inner.api.obs_display_add_draw_callback(display, ffi::draw_main, param);
            Ok(Preview { studio: self.clone(), display, state, view, scale })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (window, rect, scale);
            Err(StudioError::Failed("the preview isn't on this system yet".into()))
        }
    }
}

/// A rectangle in device pixels (at least 1x1).
fn px(rect: Rect, scale: f64) -> (u32, u32) {
    let s = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    ((rect.w * s).round().clamp(1.0, 16_384.0) as u32, (rect.h * s).round().clamp(1.0, 16_384.0) as u32)
}

/// A live preview in a window. Removed when dropped.
#[derive(Debug)]
pub struct Preview {
    studio: Studio,
    display: *mut ffi::ObsDisplay,
    state: Box<ffi::DrawState>,
    #[cfg(target_os = "macos")]
    view: ffi::mac::ChildView,
    scale: f64,
}

impl Preview {
    /// Moves or resizes it (the page's box changed). Main thread.
    pub fn set_rect(&mut self, rect: Rect, scale: f64) {
        self.scale = scale;
        #[cfg(target_os = "macos")]
        self.view.set_frame(rect.x, rect.y, rect.w, rect.h, rect.vh);
        let (cx, cy) = px(rect, scale);
        self.studio.inner.api.obs_display_resize(self.display, cx, cy);
    }

    /// Shows scene `name` instead of what's on air (Studio Mode's preview), or (None) what's on
    /// air again. Main thread.
    pub fn show_scene(&self, name: Option<&str>) {
        let a = &self.studio.inner.api;
        let next = name.map_or(ptr::null_mut(), |n| {
            let src = a.obs_get_source_by_name(cstr(n).as_ptr());
            if !src.is_null() {
                a.obs_source_inc_showing(src);
            }
            src
        });
        // The swap waits for a draw in progress; the old source goes only after it.
        let old = match self.state.scene.lock() {
            Ok(mut g) => std::mem::replace(&mut *g, next),
            Err(e) => std::mem::replace(&mut *e.into_inner(), next),
        };
        if !old.is_null() {
            a.obs_source_dec_showing(old);
            a.obs_source_release(old);
        }
    }

    /// The scene it shows instead of what's on air, if any.
    pub fn shown_scene(&self) -> Option<String> {
        let p = self.state.scene.lock().map(|g| *g).unwrap_or(ptr::null_mut());
        (!p.is_null()).then(|| text(self.studio.inner.api.obs_source_get_name(p)))
    }

    /// Outlines item `id` of `scene` with handles (as OBS marks the picked source), or nothing.
    pub fn select(&self, pick: Option<(&str, i64)>) {
        let pick = pick.map(|(scene, id)| (cstr(scene), id));
        if let Ok(mut sel) = self.state.selected.lock() {
            *sel = pick;
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        let a = &self.studio.inner.api;
        let param = &*self.state as *const ffi::DrawState as *mut std::ffi::c_void;
        a.obs_display_remove_draw_callback(self.display, ffi::draw_main, param);
        a.obs_display_destroy(self.display);
        let shown = self.state.scene.lock().map(|g| *g).unwrap_or(ptr::null_mut());
        if !shown.is_null() {
            a.obs_source_dec_showing(shown);
            a.obs_source_release(shown);
        }
        #[cfg(target_os = "macos")]
        self.view.remove();
    }
}

/// The displays a full-screen projector can go on (macOS; none elsewhere yet). Main thread.
pub fn screens() -> Vec<Screen> {
    #[cfg(target_os = "macos")]
    {
        ffi::projector_window::screens()
            .into_iter()
            .enumerate()
            .map(|(index, (name, w, h))| Screen {
                index,
                name,
                width: w.round() as u32,
                height: h.round() as u32,
                main: index == 0,
            })
            .collect()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

impl Studio {
    /// What a projector of `of` draws, holding its sources (`scenes` for a multiview).
    #[cfg(target_os = "macos")]
    fn show(&self, of: &ProjectorOf, scenes: &[String]) -> Result<ffi::Show, StudioError> {
        let a = &self.inner.api;
        Ok(match of {
            ProjectorOf::Program => ffi::Show::Program,
            ProjectorOf::Source(name) => ffi::Show::One(
                ffi::shown_source(a, name)
                    .ok_or_else(|| StudioError::Failed(format!("there's no source called \"{name}\"")))?,
            ),
            ProjectorOf::Multiview => ffi::Show::Multi {
                cells: scenes.iter().filter_map(|n| ffi::shown_source(a, n).map(|s| (s, ffi::label(a, n)))).collect(),
                air: ffi::label(a, "On air"),
            },
        })
    }

    /// Opens a projector of `of`: full screen on display `screen` (from [`screens`]), or in a
    /// window. A multiview shows `scenes` in order and calls `on_pick` with the one clicked.
    /// `on_close` runs when its window is closed (or Esc full screen): drop it then. macOS,
    /// main thread.
    pub fn projector(
        &self,
        of: &ProjectorOf,
        scenes: &[String],
        screen: Option<usize>,
        on_close: Box<dyn Fn()>,
        on_pick: Box<dyn Fn(String)>,
    ) -> Result<Projector, StudioError> {
        #[cfg(target_os = "macos")]
        {
            use std::cell::{Cell, RefCell};
            use std::rc::Rc;
            let a = &self.inner.api;
            let base = (self.canvas().width.max(1), self.canvas().height.max(1));
            let canvas_aspect = f64::from(base.0) / f64::from(base.1);
            let show = self.show(of, scenes)?;
            let aspect = match &show {
                ffi::Show::One(src) => {
                    let (w, h) = (a.obs_source_get_width(*src), a.obs_source_get_height(*src));
                    if w > 0 && h > 0 { f64::from(w) / f64::from(h) } else { canvas_aspect }
                }
                _ => canvas_aspect,
            };
            let names = Rc::new(RefCell::new(if *of == ProjectorOf::Multiview { scenes.to_vec() } else { Vec::new() }));
            let display: Rc<Cell<*mut ffi::ObsDisplay>> = Rc::new(Cell::new(ptr::null_mut()));
            let hooks = {
                let (names, display, studio) = (names.clone(), display.clone(), self.clone());
                let multiview = *of == ProjectorOf::Multiview;
                ffi::projector_window::Hooks {
                    click: Box::new(move |x, y, w, h| {
                        if !multiview {
                            return;
                        }
                        let names = names.borrow();
                        let at = projector::multiview_hit(
                            names.len(),
                            w as f32,
                            h as f32,
                            canvas_aspect as f32,
                            x as f32,
                            y as f32,
                        );
                        if let Some(n) = at.and_then(|i| names.get(i)) {
                            on_pick(n.clone());
                        }
                    }),
                    close: on_close,
                    resize: Box::new(move |w, h| {
                        let d = display.get();
                        if !d.is_null() {
                            studio.inner.api.obs_display_resize(d, w, h);
                        }
                    }),
                }
            };
            let title = match of {
                ProjectorOf::Program => "On air".to_string(),
                ProjectorOf::Source(n) => n.clone(),
                ProjectorOf::Multiview => "Multiview".to_string(),
            };
            let mut show = show;
            let Some(win) = ffi::projector_window::open(&title, aspect, screen, hooks) else {
                show.release(a);
                return Err(StudioError::Failed("That display isn't there any more.".into()));
            };
            let (cx, cy) = win.pixels();
            let init = ffi::GsInitData {
                window: win.view_ptr(),
                cx,
                cy,
                num_backbuffers: 0,
                format: ffi::GS_BGRA,
                zsformat: ffi::GS_ZS_NONE,
                adapter: 0,
            };
            let d = a.obs_display_create(&init, 0xFF0D_0B0A);
            if d.is_null() {
                win.close();
                show.release(a);
                return Err(StudioError::Failed("the projector couldn't be made".into()));
            }
            display.set(d);
            let state = Box::new(ffi::ProjectorState { api: a, base, show: std::sync::Mutex::new(show) });
            let param = &*state as *const ffi::ProjectorState as *mut std::ffi::c_void;
            a.obs_display_add_draw_callback(d, ffi::draw_projector, param);
            Ok(Projector { studio: self.clone(), display: d, state, win, names, shared_display: display })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (of, scenes, screen, on_close, on_pick);
            Err(StudioError::Failed("projectors aren't on this system yet".into()))
        }
    }
}

/// A projector window. Closes when dropped.
#[derive(Debug)]
pub struct Projector {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    studio: Studio,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    display: *mut ffi::ObsDisplay,
    #[cfg(target_os = "macos")]
    state: Box<ffi::ProjectorState>,
    #[cfg(target_os = "macos")]
    win: ffi::projector_window::Win,
    #[cfg(target_os = "macos")]
    names: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    /// The display as the window's resize hook sees it; emptied before the display goes.
    #[cfg(target_os = "macos")]
    shared_display: std::rc::Rc<std::cell::Cell<*mut ffi::ObsDisplay>>,
}

impl Projector {
    /// Its window's number (to screenshot it in checks). macOS.
    #[doc(hidden)]
    pub fn window_number(&self) -> isize {
        #[cfg(target_os = "macos")]
        {
            self.win.number()
        }
        #[cfg(not(target_os = "macos"))]
        {
            0
        }
    }

    /// A multiview follows the scenes as they change (added, removed, renamed, moved).
    pub fn set_scenes(&self, scenes: &[String]) {
        #[cfg(target_os = "macos")]
        {
            if self.names.borrow().is_empty()
                && !matches!(*self.state.show.lock().unwrap_or_else(|e| e.into_inner()), ffi::Show::Multi { .. })
            {
                return;
            }
            if *self.names.borrow() == scenes {
                return;
            }
            let Ok(next) = self.studio.show(&ProjectorOf::Multiview, scenes) else { return };
            let a = &self.studio.inner.api;
            if let Ok(mut show) = self.state.show.lock() {
                show.release(a);
                *show = next;
            }
            *self.names.borrow_mut() = scenes.to_vec();
        }
        #[cfg(not(target_os = "macos"))]
        let _ = scenes;
    }
}

impl Drop for Projector {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let a = &self.studio.inner.api;
            // A resize arriving while the window closes must not reach a destroyed display.
            self.shared_display.set(ptr::null_mut());
            let param = &*self.state as *const ffi::ProjectorState as *mut std::ffi::c_void;
            a.obs_display_remove_draw_callback(self.display, ffi::draw_projector, param);
            a.obs_display_destroy(self.display);
            if let Ok(mut show) = self.state.show.lock() {
                show.release(a);
            }
            self.win.close();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Nothing on air; then sources finish being torn down on libobs's own thread before the
        // plugins unload (a text source freed after FreeType went away crashed, 25 Oct spike).
        self.api.obs_set_output_source(0, ptr::null_mut());
        // Every scene and source must be removed before this (Scene's drop does it): libobs 32's
        // canvas holds scenes, and one left alive is freed after its plugin unloaded (a text
        // source crashed in FreeType that way, 25 Oct). Then let the destroy thread finish.
        self.api.obs_wait_for_destroy_queue();
        if !self.cpu.is_null() {
            self.api.os_cpu_usage_info_destroy(self.cpu);
        }
        std::thread::sleep(Duration::from_millis(100));
        self.api.obs_shutdown();
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// A source in a scene, as listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceInfo {
    pub name: String,
    /// Its kind's id (`color_source`, `screen_capture`…).
    pub kind: String,
    pub visible: bool,
}

/// A source name: 1 to 100 letters on one line (OBS's own names stay unique per scene).
/// This app may record the screen (macOS's Screen Recording switch; always true elsewhere).
/// macOS answers for the app as it started: a change shows only after a restart.
pub fn screen_access() -> bool {
    #[cfg(target_os = "macos")]
    {
        ffi::mac::screen_access()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Asks macOS for screen recording (its prompt, and Ferrocast listed in System Settings).
pub fn request_screen_access() -> bool {
    #[cfg(target_os = "macos")]
    {
        ffi::mac::request_screen_access()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

pub fn check_name(name: &str) -> Result<(), StudioError> {
    let n = name.chars().count();
    if n == 0 || n > 100 || name.trim() != name || name.chars().any(char::is_control) {
        return Err(StudioError::Failed("name: 1 to 100 letters on one line.".into()));
    }
    Ok(())
}

/// A scene. Released when dropped (sources in it stay while the scene does).
#[derive(Debug)]
pub struct Scene {
    studio: Studio,
    ptr: *mut ffi::ObsScene,
    /// Our own reference to each source added, so they can be removed and released with it.
    sources: std::cell::RefCell<Vec<*mut ffi::ObsSource>>,
}

impl Scene {
    /// Adds a new source of `kind` named `name`, with OBS settings as JSON (the same keys OBS
    /// saves in its scene collections), at `pos` on the canvas. Names are unique in a scene.
    pub fn add(
        &self,
        kind: &str,
        name: &str,
        settings: &serde_json::Value,
        pos: (f32, f32),
    ) -> Result<(), StudioError> {
        check_name(name)?;
        let a = &self.studio.inner.api;
        if !self.item(name).is_null() {
            return Err(StudioError::Failed(format!("name: there's already a source called \"{name}\" here.")));
        }
        let json = cstr(&settings.to_string());
        let data = a.obs_data_create_from_json(json.as_ptr());
        let (id, n) = (cstr(kind), cstr(name));
        let src = a.obs_source_create(id.as_ptr(), n.as_ptr(), data, ptr::null_mut());
        if !data.is_null() {
            a.obs_data_release(data);
        }
        if src.is_null() {
            return Err(StudioError::Failed(format!("the source \"{name}\" ({kind}) couldn't be made")));
        }
        let item = a.obs_scene_add(self.ptr, src);
        if item.is_null() {
            a.obs_source_remove(src);
            a.obs_source_release(src);
            return Err(StudioError::Failed(format!("\"{name}\" couldn't be put in the scene")));
        }
        // Kept until it's removed or the scene goes, which removes and releases it.
        self.sources.borrow_mut().push(src);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: pos.0, y: pos.1 });
        Ok(())
    }

    fn item(&self, name: &str) -> *mut ffi::ObsSceneItem {
        let n = cstr(name);
        self.studio.inner.api.obs_scene_find_source(self.ptr, n.as_ptr())
    }

    fn source(&self, name: &str) -> Result<*mut ffi::ObsSource, StudioError> {
        let a = &self.studio.inner.api;
        self.sources
            .borrow()
            .iter()
            .copied()
            .find(|s| text(a.obs_source_get_name(*s)) == name)
            .ok_or_else(|| StudioError::Failed(format!("there's no source called \"{name}\" here")))
    }

    /// The sources in the scene, bottom first.
    pub fn sources(&self) -> Vec<SourceInfo> {
        let a = &self.studio.inner.api;
        self.sources
            .borrow()
            .iter()
            .map(|s| {
                let name = text(a.obs_source_get_name(*s));
                let item = self.item(&name);
                SourceInfo {
                    kind: text(a.obs_source_get_id(*s)),
                    visible: !item.is_null() && a.obs_sceneitem_visible(item),
                    name,
                }
            })
            .collect()
    }

    /// A source's options, as they look for its settings now.
    pub fn options(&self, name: &str) -> Result<Vec<OptionInfo>, StudioError> {
        Ok(self.studio.source_options(self.source(name)?, true))
    }

    /// A source's settings (JSON), its kind's defaults filled in for keys never set.
    pub fn settings(&self, name: &str) -> Result<serde_json::Value, StudioError> {
        let a = &self.studio.inner.api;
        let data = a.obs_source_get_settings(self.source(name)?);
        if data.is_null() {
            return Ok(serde_json::json!({}));
        }
        let json = text(a.obs_data_get_json_with_defaults(data));
        a.obs_data_release(data);
        serde_json::from_str(&json).map_err(|e| StudioError::Failed(format!("OBS's settings didn't read: {e}")))
    }

    /// Changes a source's settings: only the keys given, the rest stay (as OBS's dialog does).
    pub fn update(&self, name: &str, settings: &serde_json::Value) -> Result<(), StudioError> {
        if !settings.is_object() {
            return Err(StudioError::Failed("settings: an object of OBS setting keys and values.".into()));
        }
        let a = &self.studio.inner.api;
        let src = self.source(name)?;
        let json = cstr(&settings.to_string());
        let data = a.obs_data_create_from_json(json.as_ptr());
        if data.is_null() {
            return Err(StudioError::Failed("settings: OBS couldn't read them.".into()));
        }
        a.obs_source_update(src, data);
        a.obs_data_release(data);
        Ok(())
    }

    /// Shows or hides a source.
    pub fn set_visible(&self, name: &str, visible: bool) -> Result<(), StudioError> {
        self.source(name)?;
        let item = self.item(name);
        if !item.is_null() {
            self.studio.inner.api.obs_sceneitem_set_visible(item, visible);
        }
        Ok(())
    }

    /// Fits a source inside the canvas, centred, keeping its shape (OBS's "Fit to screen").
    pub fn fit(&self, name: &str) -> Result<(), StudioError> {
        self.source(name)?;
        let a = &self.studio.inner.api;
        let item = self.item(name);
        if item.is_null() {
            return Ok(());
        }
        let c = self.studio.canvas();
        a.obs_sceneitem_set_alignment(item, ffi::OBS_ALIGN_LEFT | ffi::OBS_ALIGN_TOP);
        a.obs_sceneitem_set_pos(item, &ffi::Vec2 { x: 0.0, y: 0.0 });
        a.obs_sceneitem_set_bounds_type(item, ffi::OBS_BOUNDS_SCALE_INNER);
        a.obs_sceneitem_set_bounds_alignment(item, ffi::OBS_ALIGN_CENTER);
        a.obs_sceneitem_set_bounds(item, &ffi::Vec2 { x: c.width as f32, y: c.height as f32 });
        Ok(())
    }

    /// Takes a source out of the scene and away.
    pub fn remove(&self, name: &str) -> Result<(), StudioError> {
        let src = self.source(name)?;
        let a = &self.studio.inner.api;
        let item = self.item(name);
        if !item.is_null() {
            a.obs_sceneitem_remove(item);
        }
        a.obs_source_remove(src);
        a.obs_source_release(src);
        self.sources.borrow_mut().retain(|s| *s != src);
        Ok(())
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        // As OBS's own app clears a scene collection: remove each source and the scene (libobs 32
        // keeps scenes in its canvas until removed), then let go of our references.
        let a = &self.studio.inner.api;
        for src in self.sources.borrow_mut().drain(..) {
            a.obs_source_remove(src);
            a.obs_source_release(src);
        }
        a.obs_source_remove(a.obs_scene_get_source(self.ptr));
        a.obs_scene_release(self.ptr);
    }
}

/// A recording. Stops (and waits for the file to be finished) when dropped.
#[derive(Debug)]
pub struct Recording {
    studio: Studio,
    out: *mut ffi::ObsOutput,
    venc: *mut ffi::ObsEncoder,
    /// One encoder per audio track recorded.
    audio: Vec<*mut ffi::ObsEncoder>,
    path: PathBuf,
}

/// What a replay buffer keeps and where it saves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPlan {
    pub folder: PathBuf,
    /// OBS's name pattern for saved replays.
    pub name: String,
    /// mkv, mp4, mov, flv or ts (through OBS's mux helper).
    pub format: String,
    pub spaces: bool,
    /// The last this many seconds are kept (and at most this many megabytes, 0: no limit).
    pub seconds: u32,
    pub mb: u32,
    pub tracks: Vec<u8>,
    pub encode: EncodeSettings,
}

/// OBS's replay buffer: the last seconds kept in memory, saved to a file on request.
#[derive(Debug)]
pub struct ReplayBuffer {
    studio: Studio,
    out: *mut ffi::ObsOutput,
    venc: *mut ffi::ObsEncoder,
    audio: Vec<*mut ffi::ObsEncoder>,
}

impl ReplayBuffer {
    pub fn active(&self) -> bool {
        self.studio.inner.api.obs_output_active(self.out)
    }

    /// Saves what's kept so far to a file, written in the background ([`Self::last_replay`]
    /// names it once it's done).
    pub fn save(&self) -> Result<(), StudioError> {
        let a = &self.studio.inner.api;
        let ph = a.obs_output_get_proc_handler(self.out);
        let mut stack = [0u8; 64];
        let mut cd = ffi::CallData::fixed(&mut stack);
        let n = cstr("save");
        if ph.is_null() || !a.proc_handler_call(ph, n.as_ptr(), &mut cd) {
            return Err(StudioError::Failed("The replay buffer couldn't save.".into()));
        }
        Ok(())
    }

    /// The last replay saved, if any.
    pub fn last_replay(&self) -> Option<PathBuf> {
        let a = &self.studio.inner.api;
        let ph = a.obs_output_get_proc_handler(self.out);
        if ph.is_null() {
            return None;
        }
        let mut stack = [0u8; 4200];
        let mut cd = ffi::CallData::fixed(&mut stack);
        let n = cstr("get_last_replay");
        if !a.proc_handler_call(ph, n.as_ptr(), &mut cd) {
            return None;
        }
        let used = cd.size.min(stack.len());
        calldata_string(&stack[..used], "path").filter(|p| !p.is_empty()).map(PathBuf::from)
    }

    pub fn last_error(&self) -> Option<String> {
        let e = text(self.studio.inner.api.obs_output_get_last_error(self.out));
        (!e.is_empty()).then_some(e)
    }
}

impl Drop for ReplayBuffer {
    fn drop(&mut self) {
        let a = &self.studio.inner.api;
        if !self.out.is_null() {
            stop_output(a, self.out);
            a.obs_output_release(self.out);
        }
        a.obs_encoder_release(self.venc);
        for e in &self.audio {
            a.obs_encoder_release(*e);
        }
    }
}

/// A string from libobs's serialized call data (`[name size][name][data size][data]`…, ended
/// by a zero size), read as bytes.
fn calldata_string(stack: &[u8], name: &str) -> Option<String> {
    let w = std::mem::size_of::<usize>();
    let size_at = |i: usize| -> Option<usize> {
        stack.get(i..i + w).map(|b| {
            let mut a = [0u8; std::mem::size_of::<usize>()];
            a.copy_from_slice(b);
            usize::from_ne_bytes(a)
        })
    };
    let mut i = 0;
    loop {
        let name_size = size_at(i)?;
        if name_size == 0 {
            return None;
        }
        i += w;
        let key = stack.get(i..i + name_size)?;
        i += name_size;
        let data_size = size_at(i)?;
        i += w;
        let data = stack.get(i..i + data_size)?;
        i += data_size;
        if key.strip_suffix(&[0]).unwrap_or(key) == name.as_bytes() {
            let s = data.strip_suffix(&[0]).unwrap_or(data);
            return Some(String::from_utf8_lossy(s).into_owned());
        }
    }
}

/// What the virtual camera shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "what", content = "name")]
pub enum CameraShows {
    /// What's on air.
    Program,
    /// One scene or source (named), whatever is on air.
    Source(String),
}

/// OBS's virtual camera: what's on air (or one scene or source) as a camera other apps see.
#[derive(Debug)]
pub struct VirtualCamera {
    studio: Studio,
    out: *mut ffi::ObsOutput,
    view: *mut ffi::ObsView,
}

impl VirtualCamera {
    pub fn active(&self) -> bool {
        self.studio.inner.api.obs_output_active(self.out)
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        let a = &self.studio.inner.api;
        if !self.out.is_null() {
            stop_output(a, self.out);
            a.obs_output_release(self.out);
        }
        if !self.view.is_null() {
            a.obs_view_remove(self.view);
            a.obs_view_set_source(self.view, 0, ptr::null_mut());
            a.obs_view_destroy(self.view);
        }
    }
}

/// Remuxes recordings, from any thread: OBS's media-remux (no encoding again).
#[derive(Debug, Clone, Copy)]
pub struct Remuxer {
    fns: ffi::RemuxFns,
}

impl Remuxer {
    /// Rewrites `from` as `to` (its extension picks the format). `progress` gets the percent
    /// done; returning false stops it. Never writes over a file that's there.
    pub fn remux(&self, from: &Path, to: &Path, mut progress: impl FnMut(f32) -> bool) -> Result<(), StudioError> {
        if !from.is_file() {
            return Err(StudioError::Failed(format!("{} isn't there", from.display())));
        }
        if to.exists() {
            return Err(StudioError::Failed(format!("{} is there already", to.display())));
        }
        let (i, o) = (cstr(&from.display().to_string()), cstr(&to.display().to_string()));
        match self.fns.run(&i, &o, &mut progress) {
            Ok(true) => Ok(()),
            Ok(false) => {
                let _ = std::fs::remove_file(to);
                Err(StudioError::Failed("the remux stopped before the end".into()))
            }
            Err(()) => Err(StudioError::Failed(format!("{} can't be remuxed", from.display()))),
        }
    }
}

/// How a recording is made: its format, file names, tracks, splitting and encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordPlan {
    pub folder: PathBuf,
    /// OBS's name pattern: `%CCYY` year, `%MM` month, `%DD` day, `%hh`, `%mm`, `%ss`…
    pub name: String,
    /// mp4, mov (crash-safe), fmp4, fmov (fragmented), mkv, flv or ts.
    pub format: String,
    /// Spaces allowed in the name (else they become underscores).
    pub spaces: bool,
    /// A file of the same name is replaced (else " (2)" is added).
    pub overwrite: bool,
    /// A new file every this many minutes or megabytes (0: never).
    pub split_minutes: u32,
    pub split_mb: u32,
    /// Audio tracks (1 to 6) in the file; empty is track 1.
    pub tracks: Vec<u8>,
    pub encode: EncodeSettings,
}

impl Default for RecordPlan {
    fn default() -> Self {
        RecordPlan {
            folder: PathBuf::new(),
            name: "%CCYY-%MM-%DD %hh-%mm-%ss".into(),
            format: "mp4".into(),
            spaces: true,
            overwrite: false,
            split_minutes: 0,
            split_mb: 0,
            tracks: vec![1],
            encode: EncodeSettings::with_encoder("obs_x264"),
        }
    }
}

impl RecordPlan {
    /// The file extension the format writes.
    pub fn extension(&self) -> &str {
        match self.format.as_str() {
            "fmp4" => "mp4",
            "fmov" => "mov",
            f => f,
        }
    }
}

impl Recording {
    /// The file it started writing.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Starts a new file now (OBS's "Split recording"). Err when this format can't.
    pub fn split(&self) -> Result<(), StudioError> {
        let mut stack = [0u8; 128];
        let mut cd = ffi::CallData::fixed(&mut stack);
        if !self.call("split_file", &mut cd) {
            return Err(StudioError::Failed("This recording can't be split.".into()));
        }
        let mut on = false;
        let k = cstr("split_file_enabled");
        self.studio.inner.api.calldata_get_data(&cd, k.as_ptr(), (&mut on as *mut bool).cast(), 1);
        if on { Ok(()) } else { Err(StudioError::Failed("This recording can't be split.".into())) }
    }

    /// Marks a chapter here (MP4 and MOV): named, or numbered when the name is empty.
    pub fn chapter(&self, name: &str) -> Result<(), StudioError> {
        let mut stack = [0u8; 512];
        let mut cd = ffi::CallData::fixed(&mut stack);
        let name: String = name.chars().filter(|c| !c.is_control()).take(100).collect();
        let (k, v) = (cstr("chapter_name"), cstr(&name));
        self.studio.inner.api.calldata_set_data(&mut cd, k.as_ptr(), v.as_ptr().cast(), name.len() + 1);
        if self.call("add_chapter", &mut cd) {
            Ok(())
        } else {
            Err(StudioError::Failed("Chapters need an MP4 or MOV recording.".into()))
        }
    }

    fn call(&self, proc_name: &str, cd: &mut ffi::CallData) -> bool {
        let a = &self.studio.inner.api;
        let ph = a.obs_output_get_proc_handler(self.out);
        if ph.is_null() {
            return false;
        }
        let n = cstr(proc_name);
        a.proc_handler_call(ph, n.as_ptr(), cd)
    }

    /// Stops and waits up to 10 s for the file to be written out.
    pub fn stop(self) {
        drop(self);
    }

    pub fn pause(&self, on: bool) -> Result<(), StudioError> {
        if self.studio.inner.api.obs_output_pause(self.out, on) {
            Ok(())
        } else {
            Err(StudioError::Failed("This recording can't pause.".into()))
        }
    }

    pub fn stats(&self) -> RecordStats {
        let a = &self.studio.inner.api;
        RecordStats {
            active: a.obs_output_active(self.out),
            paused: a.obs_output_paused(self.out),
            bytes: a.obs_output_get_total_bytes(self.out),
            frames: u32::try_from(a.obs_output_get_total_frames(self.out)).unwrap_or(0),
        }
    }

    pub fn last_error(&self) -> Option<String> {
        let e = text(self.studio.inner.api.obs_output_get_last_error(self.out));
        (!e.is_empty()).then_some(e)
    }
}

/// How a recording is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RecordStats {
    pub active: bool,
    pub paused: bool,
    pub bytes: u64,
    pub frames: u32,
}

impl Drop for Recording {
    fn drop(&mut self) {
        let a = &self.studio.inner.api;
        if !self.out.is_null() {
            a.obs_output_stop(self.out);
            let until = Instant::now() + Duration::from_secs(10);
            while a.obs_output_active(self.out) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            a.obs_output_release(self.out);
        }
        a.obs_encoder_release(self.venc);
        for e in &self.audio {
            a.obs_encoder_release(*e);
        }
    }
}

/// How video and sound are encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeSettings {
    /// An encoder id from `kinds().encoders` (`obs_x264`, VideoToolbox's…).
    pub video_encoder: String,
    pub video_kbps: u32,
    /// Varying bitrate (VBR) instead of steady (CBR).
    pub vbr: bool,
    /// Constant quality instead of a bitrate (recordings): 0 is lossless, 18 very high.
    pub quality: Option<u32>,
    /// Seconds between keyframes (platforms ask for 2; 0 lets the encoder pick).
    pub keyframe_s: u32,
    /// "high", "main" or "baseline".
    pub profile: String,
    pub bframes: bool,
    /// Sent at this size instead of the canvas's.
    pub scale: Option<(u32, u32)>,
    pub audio_kbps: u32,
    /// The encoder's own options (OBS's keys), applied over the ones above.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl EncodeSettings {
    /// This encoder at 6000 kbps CBR, 2 s keyframes, high profile, AAC 160.
    pub fn with_encoder(id: &str) -> EncodeSettings {
        EncodeSettings {
            video_encoder: id.into(),
            video_kbps: 6000,
            vbr: false,
            quality: None,
            keyframe_s: 2,
            profile: "high".into(),
            bframes: true,
            scale: None,
            audio_kbps: 160,
            extra: Default::default(),
        }
    }
}

/// What a stream sends, and what it does when the connection drops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSettings {
    pub encode: EncodeSettings,
    /// Tries again (retries, every n seconds) when the connection drops; None gives up.
    pub reconnect: Option<(u32, u32)>,
    /// Viewers see it this many seconds later (0: no delay).
    pub delay_s: u32,
}

/// The engine's own load.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct EngineStats {
    /// Ferrocast's CPU use, % of one machine.
    pub cpu: f64,
    pub fps: f64,
    /// Average time to render a frame.
    pub frame_ms: f64,
    /// Frames rendered late since start, and all frames.
    pub lagged: u32,
    pub frames: u32,
    pub memory_mb: u64,
}

/// How a stream is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StreamStats {
    pub active: bool,
    pub reconnecting: bool,
    pub bytes: u64,
    pub frames: u32,
    pub dropped: u32,
}

/// A stream in progress. Dropping it stops it (and waits up to 10 s for that).
#[derive(Debug)]
pub struct Streaming {
    studio: Studio,
    out: *mut ffi::ObsOutput,
    service: *mut ffi::ObsService,
    venc: *mut ffi::ObsEncoder,
    aenc: *mut ffi::ObsEncoder,
    settings: StreamSettings,
    /// Destinations sent straight from here (SRT, RIST, WebRTC), by id.
    directs: Vec<Direct>,
}

/// How a destination is sent straight from the Studio (not through Go live's RTMP forwarders).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// `srt://host:port?…`; the key, if any, is the stream id.
    Srt,
    /// `rist://host:port?…`; the key, if any, is the passphrase.
    Rist,
    /// WebRTC by WHIP: an `https://` endpoint; the key, if any, is its bearer token.
    Whip,
}

impl Protocol {
    /// The protocol a server address asks for, or None for RTMP (and anything else).
    pub fn of(url: &str) -> Option<Protocol> {
        let u = url.trim().to_ascii_lowercase();
        if u.starts_with("srt://") {
            Some(Protocol::Srt)
        } else if u.starts_with("rist://") {
            Some(Protocol::Rist)
        } else if u.starts_with("https://")
            || ["http://127.0.0.1", "http://localhost", "http://[::1]"]
                .iter()
                .any(|p| u.strip_prefix(p).is_some_and(|r| r.is_empty() || r.starts_with([':', '/'])))
        {
            // Plain http only on this computer: a token never crosses the network unencrypted.
            Some(Protocol::Whip)
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct Direct {
    id: String,
    out: *mut ffi::ObsOutput,
    service: *mut ffi::ObsService,
    /// WebRTC's own encoders (no B-frames, Opus); SRT and RIST share the stream's.
    own: Option<(*mut ffi::ObsEncoder, *mut ffi::ObsEncoder)>,
}

impl Streaming {
    pub fn stats(&self) -> StreamStats {
        output_stats(&self.studio.inner.api, self.out)
    }

    /// The reason OBS gave for the last failure, if any.
    pub fn last_error(&self) -> Option<String> {
        let e = text(self.studio.inner.api.obs_output_get_last_error(self.out));
        (!e.is_empty()).then_some(e)
    }

    /// Also sends this stream straight to `url` by SRT, RIST or WebRTC (WHIP), as OBS's own
    /// outputs do: SRT and RIST share this stream's encoding; WebRTC gets its own (no
    /// B-frames, Opus sound). `key` is the stream id, passphrase or bearer token (may be
    /// empty). One per id; adding an id again replaces it.
    pub fn add_direct(&mut self, id: &str, url: &str, key: &str) -> Result<(), StudioError> {
        let proto = Protocol::of(url)
            .ok_or_else(|| StudioError::Failed("url: an srt://, rist:// or https:// (WHIP) address.".into()))?;
        self.remove_direct(id);
        let a = &self.studio.inner.api;
        let settings = a.obs_data_create();
        let set = |k: &str, v: &str| {
            let (k, v) = (cstr(k), cstr(v));
            a.obs_data_set_string(settings, k.as_ptr(), v.as_ptr());
        };
        set("server", url.trim());
        let (service_id, output_id) = match proto {
            Protocol::Whip => {
                set("bearer_token", key);
                ("whip_custom", "whip_output")
            }
            Protocol::Srt | Protocol::Rist => {
                set("key", key);
                ("rtmp_custom", "ffmpeg_mpegts_muxer")
            }
        };
        let (sid, sname) = (cstr(service_id), cstr(&format!("ferrocast-direct-{id}")));
        let service = a.obs_service_create(sid.as_ptr(), sname.as_ptr(), settings, ptr::null_mut());
        a.obs_data_release(settings);
        let (oid, oname) = (cstr(output_id), cstr(&format!("ferrocast-direct-{id}")));
        let out = a.obs_output_create(oid.as_ptr(), oname.as_ptr(), ptr::null_mut(), ptr::null_mut());
        let mut d = Direct { id: id.to_string(), out, service, own: None };
        if service.is_null() || out.is_null() {
            self.release_direct(&mut d);
            return Err(StudioError::Failed(format!(
                "{} isn't available in this OBS",
                if proto == Protocol::Whip { "WebRTC (WHIP)" } else { "SRT and RIST" }
            )));
        }
        let (venc, aenc) = if proto == Protocol::Whip {
            let mut e = self.settings.encode.clone();
            e.bframes = false;
            match self.studio.encoders_for(&e, true) {
                Ok(pair) => {
                    d.own = Some(pair);
                    pair
                }
                Err(err) => {
                    self.release_direct(&mut d);
                    return Err(err);
                }
            }
        } else {
            (self.venc, self.aenc)
        };
        a.obs_output_set_video_encoder(out, venc);
        a.obs_output_set_audio_encoder(out, aenc, 0);
        a.obs_output_set_service(out, service);
        match self.settings.reconnect {
            Some((retries, every_s)) => a.obs_output_set_reconnect_settings(out, retries as c_int, every_s as c_int),
            None => a.obs_output_set_reconnect_settings(out, 0, 0),
        }
        if !a.obs_output_start(out) {
            let why = text(a.obs_output_get_last_error(out));
            self.release_direct(&mut d);
            return Err(StudioError::Failed(if why.is_empty() {
                "it didn't start: the server refused or couldn't be reached".into()
            } else {
                format!("it didn't start: {why}")
            }));
        }
        self.directs.push(d);
        Ok(())
    }

    /// Stops sending to one direct destination.
    pub fn remove_direct(&mut self, id: &str) {
        if let Some(i) = self.directs.iter().position(|d| d.id == id) {
            let mut d = self.directs.remove(i);
            self.release_direct(&mut d);
        }
    }

    /// How a direct destination is going (None: not sending to it).
    pub fn direct_stats(&self, id: &str) -> Option<StreamStats> {
        let d = self.directs.iter().find(|d| d.id == id)?;
        Some(output_stats(&self.studio.inner.api, d.out))
    }

    /// The ids sent to directly now.
    pub fn direct_ids(&self) -> Vec<String> {
        self.directs.iter().map(|d| d.id.clone()).collect()
    }

    fn release_direct(&self, d: &mut Direct) {
        let a = &self.studio.inner.api;
        if !d.out.is_null() {
            stop_output(a, d.out);
            a.obs_output_release(d.out);
            d.out = ptr::null_mut();
        }
        if !d.service.is_null() {
            a.obs_service_release(d.service);
            d.service = ptr::null_mut();
        }
        if let Some((v, au)) = d.own.take() {
            a.obs_encoder_release(v);
            a.obs_encoder_release(au);
        }
    }
}

/// An output's state now.
fn output_stats(a: &Api, out: *mut ffi::ObsOutput) -> StreamStats {
    StreamStats {
        active: a.obs_output_active(out),
        reconnecting: a.obs_output_reconnecting(out),
        bytes: a.obs_output_get_total_bytes(out),
        frames: u32::try_from(a.obs_output_get_total_frames(out)).unwrap_or(0),
        dropped: u32::try_from(a.obs_output_get_frames_dropped(out)).unwrap_or(0),
    }
}

/// Stops an output and waits up to 10 s for it.
fn stop_output(a: &Api, out: *mut ffi::ObsOutput) {
    a.obs_output_stop(out);
    let until = Instant::now() + Duration::from_secs(10);
    while a.obs_output_active(out) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        // The direct ones first: they share this stream's encoders.
        for mut d in std::mem::take(&mut self.directs) {
            self.release_direct(&mut d);
        }
        let a = &self.studio.inner.api;
        if !self.out.is_null() {
            stop_output(a, self.out);
            a.obs_output_release(self.out);
        }
        if !self.service.is_null() {
            a.obs_service_release(self.service);
        }
        a.obs_encoder_release(self.venc);
        a.obs_encoder_release(self.aenc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obs_app_paths() {
        let p = Paths::from_obs_app(Path::new("/Applications/OBS.app/Contents"), true);
        assert_eq!(p.libobs, Path::new("/Applications/OBS.app/Contents/Frameworks/libobs.framework/Versions/A/libobs"));
        assert!(p.renderer.ends_with("libobs-metal.dylib"));
        assert!(p.plugin_bin.contains("%module%.plugin/Contents/MacOS/"));
        let w = Paths::from_obs_windows(Path::new("C:/obs"));
        assert!(w.libobs.ends_with("bin/64bit/obs.dll") && w.renderer.ends_with("libobs-d3d11.dll"));
        assert_eq!(w.plugin_data, "C:/obs/data/obs-plugins/%module%/");
        assert!(p.data.ends_with('/'));
    }

    #[test]
    fn a_missing_engine_is_a_plain_error_and_never_blocks_a_retry() {
        let p = Paths::from_obs_app(Path::new("/nowhere/OBS.app/Contents"), true);
        let e = Studio::start(&p, Canvas::default(), Path::new("/tmp/fc-none")).unwrap_err();
        assert!(matches!(e, StudioError::Load(_)), "{e:?}");
        assert!(!RUNNING.load(Ordering::SeqCst), "a failed start frees the slot");
    }

    #[test]
    fn preview_sizes_are_device_pixels() {
        let r = Rect { x: 10.0, y: 20.0, w: 640.0, h: 360.0, vh: 800.0 };
        assert_eq!(px(r, 2.0), (1280, 720));
        assert_eq!(px(r, f64::NAN), (640, 360), "a bad scale is 1");
        assert_eq!(px(Rect { w: 0.0, h: -5.0, ..r }, 1.0), (1, 1), "never zero");
    }

    #[test]
    fn source_names_are_checked() {
        assert!(check_name("Webcam").is_ok());
        for bad in ["", " lead", "a\nb", &"x".repeat(101)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn option_kinds_are_named() {
        assert_eq!(kind_name(1), "bool");
        assert_eq!(kind_name(6), "list");
        assert_eq!(kind_name(13), "color_alpha");
        assert_eq!(kind_name(99), "other");
    }
}
