//! The kinds of source, filter and transition Ferrocast's own engine runs, and each one's
//! settings form, with no libobs: the same ids and settings keys OBS uses (so collections go
//! both ways), drawn by the app's forms as libobs's properties were.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{Kind, Kinds, OptionInfo};

/// Kinds whose form lists what's on this computer now (cameras, screens, windows, mics, NDI):
/// reading those can take seconds (on Windows FFmpeg asks DirectShow for the cameras), so
/// they're read off the window's thread and kept a moment for the next ask.
pub fn lists_devices(kind: &str) -> bool {
    matches!(
        kind,
        "ndi_source"
            | "screen_capture"
            | "av_capture_input"
            | "av_capture_input_v2"
            | "coreaudio_input_capture"
            | "sck_audio_capture"
            | "coreaudio_output_capture"
            | "monitor_capture"
            | "window_capture"
            | "dshow_input"
            | "xshm_input"
            | "v4l2_input"
            | "pulse_input_capture"
            | "wasapi_process_output_capture"
            | "wasapi_input_capture"
    )
}

/// How long a device list read is given again without reading the devices anew: long enough
/// that the dialog's read-ahead and the click that follows share one read, short enough that a
/// camera plugged in shows the next time you look.
const FRESH: Duration = Duration::from_millis(1500);

type Slot = Arc<Mutex<Option<(Instant, Option<Vec<OptionInfo>>)>>>;

fn slot(kind: &str) -> Slot {
    static SLOTS: Mutex<Option<HashMap<String, Slot>>> = Mutex::new(None);
    let mut all = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    all.get_or_insert_with(HashMap::new).entry(kind.to_string()).or_default().clone()
}

/// `options`, with device lists read at most once per [`FRESH`]: two asks at once (the
/// dialog reading ahead while you click) wait for the same read instead of each starting one.
/// Call it off the window's thread for device kinds.
pub fn options_cached(kind: &str) -> Option<Vec<OptionInfo>> {
    if !lists_devices(kind) {
        return options(kind);
    }
    let slot = slot(kind);
    // Held while reading, so a second ask waits for this read and takes its answer.
    let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, got)) = held.as_ref()
        && at.elapsed() < FRESH
    {
        return got.clone();
    }
    let got = options(kind);
    *held = Some((Instant::now(), got.clone()));
    got
}

/// Each display's name as macOS shows it ("VG271U M3"), read on the main thread by the app
/// and kept here, so a display list read on another thread still names them.
static SCREEN_NAMES: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);

/// Reads the displays' names (main thread only; elsewhere this keeps the last ones).
#[cfg(target_os = "macos")]
pub fn read_screen_names() {
    let names = crate::ffi::projector_window::screen_names();
    if !names.is_empty() {
        *SCREEN_NAMES.lock().unwrap_or_else(|e| e.into_inner()) = Some(names);
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn screen_names() -> HashMap<u32, String> {
    #[cfg(target_os = "macos")]
    read_screen_names();
    SCREEN_NAMES.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

fn kind(id: &str, name: &str) -> Kind {
    Kind { id: id.into(), name: name.into() }
}

/// Every kind our engine runs on this computer.
pub fn kinds() -> Kinds {
    let mut inputs = vec![
        kind("color_source_v3", "Color Source"),
        kind("image_source", "Image"),
        kind("text_ft2_source_v2", "Text"),
        kind("slideshow", "Image Slide Show"),
        kind("ndi_source", "NDI Source"),
        kind("ferrocast_clone", "Source Clone"),
        kind("ferrocast_countdown", "Countdown"),
        kind("ferrocast_visualizer", "Audio Visualizer"),
        kind("ferrocast_web_text", "Web Text"),
        kind("ferrocast_inputs", "Input Overlay"),
    ];
    // Where media files report how far they've played (the replay's scene goes back when it ends).
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        inputs.push(kind("ferrocast_replay", "Instant Replay"));
    }
    if cfg!(target_os = "macos") {
        inputs.extend([
            kind("ffmpeg_source", "Media Source"),
            kind("vlc_source", "VLC Video Source"),
            kind("screen_capture", "macOS Screen Capture"),
            kind("av_capture_input_v2", "Video Capture Device"),
            kind("coreaudio_input_capture", "Audio Input Capture"),
            kind("sck_audio_capture", "macOS Audio Capture"),
            kind("browser_source", "Browser"),
        ]);
    }
    if cfg!(target_os = "windows") {
        inputs.extend([
            kind("ffmpeg_source", "Media Source"),
            kind("vlc_source", "VLC Video Source"),
            kind("monitor_capture", "Display Capture"),
            kind("window_capture", "Window Capture"),
            kind("dshow_input", "Video Capture Device"),
            kind("wasapi_input_capture", "Audio Input Capture"),
            kind("wasapi_output_capture", "Audio Output Capture"),
            kind("wasapi_process_output_capture", "Application Audio Capture"),
            // A hidden Edge draws it (`ffi::headless`, since 2 Nov), but it wasn't listed, so
            // Add a source had no web page or overlay to add on Windows (10 Nov).
            kind("browser_source", "Browser"),
        ]);
    }
    if cfg!(target_os = "linux") {
        inputs.extend([
            kind("ffmpeg_source", "Media Source"),
            kind("vlc_source", "VLC Video Source"),
            kind("xshm_input", "Screen Capture (XSHM)"),
            kind("v4l2_input", "Video Capture Device (V4L2)"),
            kind("pulse_input_capture", "Audio Input Capture (PulseAudio)"),
            kind("pulse_output_capture", "Audio Output Capture (PulseAudio)"),
        ]);
    }
    Kinds {
        inputs,
        filters: vec![
            kind("chroma_key_filter_v2", "Chroma Key"),
            kind("color_key_filter_v2", "Color Key"),
            kind("color_filter_v2", "Color Correction"),
            kind("luma_key_filter_v2", "Luma Key"),
            kind("sharpness_filter_v2", "Sharpen"),
            kind("crop_filter", "Crop/Pad"),
            kind("mask_filter_v2", "Image Mask/Blend"),
            kind("clut_filter", "Apply LUT"),
            kind("scroll_filter", "Scroll"),
            kind("ferrocast_rounded_corners", "Rounded Corners"),
            kind("ferrocast_border", "Border"),
            kind("ferrocast_shadow", "Shadow"),
            kind("ferrocast_shape", "Shape"),
            kind("ferrocast_blur_area", "Blur or Pixelate"),
            kind("ferrocast_look", "Look"),
            kind("ferrocast_shader", "Shader Effect"),
            kind("ferrocast_freeze", "Freeze"),
            kind("gain_filter", "Gain"),
            kind("noise_gate_filter", "Noise Gate"),
            kind("compressor_filter", "Compressor"),
            kind("limiter_filter", "Limiter"),
            kind("expander_filter", "Expander"),
            kind("upward_compressor_filter", "Upward Compressor"),
            kind("basic_eq_filter", "3-Band Equalizer"),
            kind("noise_suppress_filter_v2", "Noise Suppression"),
            kind("invert_polarity_filter", "Invert Polarity"),
        ]
        .into_iter()
        // Remove background: Apple's Vision finds the person (a Mac for now).
        .chain(
            cfg!(any(target_os = "macos", target_os = "windows"))
                .then(|| kind("ferrocast_remove_background", "Remove Background")),
        )
        // Strong noise removal (DeepFilterNet), in builds that have it.
        .chain(cfg!(feature = "deepfilter").then(|| kind("ferrocast_deepfilter", "Strong Noise Removal")))
        .collect(),
        transitions: vec![
            kind("cut_transition", "Cut"),
            kind("fade_transition", "Fade"),
            kind("swipe_transition", "Swipe"),
            kind("slide_transition", "Slide"),
            kind("fade_to_color_transition", "Fade to Color"),
            kind("wipe_transition", "Luma Wipe"),
            kind("obs_stinger_transition", "Stinger"),
            kind("ferrocast_move", "Move"),
        ],
        encoders: if cfg!(target_os = "macos") {
            vec![
                kind("com.apple.videotoolbox.videoencoder.ave.avc", "Apple VT H264 Hardware Encoder"),
                kind("com.apple.videotoolbox.videoencoder.ave.hevc", "Apple VT HEVC Hardware Encoder"),
                kind("obs_x264", "x264"),
            ]
        } else {
            // Through FFmpeg: the graphics card's encoder that works here, or x264.
            vec![kind("ffmpeg_hardware", "Hardware (NVENC, AMF or Quick Sync)"), kind("obs_x264", "x264")]
        },
        outputs: Vec::new(),
    }
}

/// One option, drawn as `kind` (OBS's: bool, int, float, text, path, list, color, font…).
fn opt(key: &str, label: &str, kind: &'static str) -> OptionInfo {
    OptionInfo {
        key: key.into(),
        label: label.into(),
        kind,
        help: String::new(),
        visible: true,
        enabled: true,
        min: None,
        max: None,
        step: None,
        slider: false,
        suffix: String::new(),
        choices: Vec::new(),
        values: Vec::new(),
        group: Vec::new(),
        checkable: false,
        path: "",
        filter: String::new(),
        text_type: "",
        monospace: false,
        shown_when: None,
    }
}

impl OptionInfo {
    fn range(mut self, min: f64, max: f64, step: f64) -> Self {
        (self.min, self.max, self.step) = (Some(min), Some(max), Some(step));
        self
    }

    fn slider(mut self) -> Self {
        self.slider = true;
        self
    }

    fn unit(mut self, suffix: &str) -> Self {
        self.suffix = suffix.into();
        self
    }

    fn choices(mut self, list: &[(&str, Value)]) -> Self {
        self.choices = list.iter().map(|(c, _)| (*c).to_string()).collect();
        self.values = list.iter().map(|(_, v)| v.clone()).collect();
        self
    }

    fn file(mut self, filter: &str) -> Self {
        self.path = "file";
        self.filter = filter.into();
        self
    }

    /// A list of files (OBS's editable list of files).
    fn files(mut self, filter: &str) -> Self {
        self.path = "files";
        self.filter = filter.into();
        self
    }

    fn mono(mut self) -> Self {
        self.monospace = true;
        self
    }

    fn multiline(mut self) -> Self {
        self.text_type = "multiline";
        self
    }

    /// Shown only while `key` holds one of `values`.
    fn when(mut self, key: &str, values: &[Value]) -> Self {
        self.shown_when = Some(crate::ShownWhen { key: key.into(), values: values.to_vec() });
        self
    }
}

/// A camera's size and frame rate, after its device: its own, or one asked (the nearest it
/// has is used).
fn camera_size_rate() -> [OptionInfo; 2] {
    [
        opt("resolution", "Size", "list").choices(&[
            ("Its own", json!("")),
            ("720p", json!("1280x720")),
            ("1080p", json!("1920x1080")),
            ("4K", json!("3840x2160")),
        ]),
        opt("fps", "Frame rate", "list").choices(&[("Its own", json!(0)), ("30", json!(30)), ("60", json!(60))]),
    ]
}

/// The running apps with a window or sound to capture, by name (their bundle ids kept).
#[cfg(target_os = "macos")]
fn app_choices() -> Vec<(String, Value)> {
    ferrocore::screen::apps().map(|l| l.into_iter().map(|a| (a.name, json!(a.id))).collect()).unwrap_or_default()
}

/// Owned choices as the borrowed pairs `choices` takes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn as_refs(l: &[(String, Value)]) -> Vec<(&str, Value)> {
    l.iter().map(|(a, b)| (a.as_str(), b.clone())).collect()
}

const MEDIA_FILES: &str = "Media Files (*.mp4 *.m4v *.mov *.mkv *.webm *.flv *.ts *.avi *.mp3 *.m4a *.aac *.wav *.ogg *.opus *.flac);;All Files (*.*)";
const IMAGES: &str = "Image Files (*.bmp *.tga *.png *.jpeg *.jpg *.jxr *.gif *.psd *.webp);;All Files (*.*)";
const MEDIA: &str =
    "Media Files (*.mp4 *.m4v *.mov *.mkv *.ts *.webm *.mp3 *.aac *.wav *.m4a *.flac *.ogg);;All Files (*.*)";

/// A source's or filter's settings form; None for a kind our engine doesn't run.
pub fn options(kind: &str) -> Option<Vec<OptionInfo>> {
    Some(match kind {
        "color_source" | "color_source_v2" | "color_source_v3" => vec![
            opt("color", "Color", "color_alpha"),
            opt("width", "Width", "int").range(0.0, 16384.0, 1.0),
            opt("height", "Height", "int").range(0.0, 16384.0, 1.0),
        ],
        "ndi_source" => {
            // The sources on the network now (a moment's look), when NDI is installed.
            let found: Vec<(String, Value)> =
                crate::ffi::ndi::sources(1500).unwrap_or_default().into_iter().map(|n| (n.clone(), json!(n))).collect();
            vec![opt("ndi_source_name", "Source name", "list").choices(&as_refs(&found))]
        }
        // Its Listen to list is the sound on air and the collection's sources with sound
        // (Stage::options fills them in).
        "ferrocast_visualizer" => vec![
            opt("source", "Listen to", "list").choices(&[("Sound on air", json!(""))]),
            opt("bars", "Bars", "int").range(8.0, 64.0, 1.0).slider(),
            opt("width", "Width", "int").range(16.0, 3840.0, 1.0),
            opt("height", "Height", "int").range(8.0, 2160.0, 1.0),
            opt("color", "Color", "color_alpha"),
            opt("mirror", "From the middle", "bool"),
        ],
        "ferrocast_inputs" => {
            let mut layouts: Vec<(String, Value)> = ferrocore::source::inputs::LAYOUTS
                .iter()
                .map(|(id, name, _)| ((*name).to_string(), json!(id)))
                .collect();
            layouts.push(("Your own".into(), json!("custom")));
            vec![
                opt("layout", "Keys", "list").choices(&as_refs(&layouts)),
                opt("keys", "Your keys (W / A S D)", "text").when("layout", &[json!("custom")]),
                opt("size", "Size", "int").range(16.0, 256.0, 1.0).slider().unit("px"),
                opt("color_rest", "Key", "color_alpha"),
                opt("color_held", "Held", "color_alpha"),
                opt("color_text", "Letters", "color_alpha"),
            ]
        }
        "ferrocast_web_text" => vec![
            opt("url", "Address", "text"),
            opt("path", "Value (data.viewers)", "text"),
            opt("words", "Text ({} is the value)", "text"),
            opt("every", "Every", "int").range(5.0, 3600.0, 1.0).unit("s"),
            opt("font", "Font", "font"),
            opt("color1", "Color", "color_alpha"),
            opt("outline", "Outline", "bool"),
            opt("drop_shadow", "Drop Shadow", "bool"),
        ],
        "ferrocast_countdown" => {
            let set_time = [json!("for")];
            vec![
                opt("mode", "Count", "list")
                    .choices(&[("For a set time", json!("for")), ("Until a time of day", json!("until"))]),
                opt("minutes", "Minutes", "int").range(0.0, 1440.0, 1.0).when("mode", &set_time),
                opt("seconds", "Seconds", "int").range(0.0, 59.0, 1.0).when("mode", &set_time),
                opt("until", "Time of day (21:30)", "text").when("mode", &[json!("until")]),
                opt("before", "Before the time", "text"),
                opt("end", "When it ends", "text"),
                opt("font", "Font", "font"),
                opt("color1", "Color", "color_alpha"),
                opt("outline", "Outline", "bool"),
                opt("drop_shadow", "Drop Shadow", "bool"),
            ]
        }
        // Its file is set by the app each time a replay is saved.
        "ferrocast_replay" => vec![opt("switch", "Show its scene while it plays", "bool")],
        // Nothing to set: its switch freezes and unfreezes.
        "ferrocast_freeze" => vec![],
        // Its choices are the collection's sources and scenes (Stage::options fills them in).
        "ferrocast_clone" => {
            vec![opt("clone", "Source", "list").choices(&[]), opt("filters", "Include its filters", "bool")]
        }
        "ferrocast_shader" => {
            // The gallery first; one's own code only for "Your own".
            let mut presets: Vec<(String, Value)> = vec![("Your own".into(), json!("custom"))];
            presets.extend(ferrocore::shaderfx::PRESETS.iter().map(|(id, name, _)| ((*name).to_string(), json!(id))));
            // Installed plugins' effects that are on, after the ready-made ones.
            presets.extend(
                ferrocore::shaderfx::plugins()
                    .into_iter()
                    .map(|p| (format!("{} (plugin)", p.name), json!(ferrocore::shaderfx::plugin_preset(&p.id)))),
            );
            let own = [json!("custom")];
            vec![
                opt("preset", "Effect", "list").choices(&as_refs(&presets)),
                opt("shader_file", "Shader File", "path")
                    .file("WGSL Shaders (*.wgsl);;All Files (*.*)")
                    .when("preset", &own),
                opt("shader_text", "Or Write One Here", "text").multiline().mono().when("preset", &own),
                opt("param1", "Strength", "float").range(0.0, 1.0, 0.01).slider(),
                opt("param2", "Size or speed", "float").range(0.0, 1.0, 0.01).slider(),
                opt("param3", "Slider 3", "float").range(0.0, 1.0, 0.01).slider().when("preset", &own),
                opt("param4", "Slider 4", "float").range(0.0, 1.0, 0.01).slider().when("preset", &own),
            ]
        }
        "slideshow" | "slideshow_v2" | "image_slide_show" => vec![
            opt("files", "Image Files", "editable_list").files(IMAGES),
            opt("slide_time", "Time Between Slides", "int").range(50.0, 3_600_000.0, 50.0).unit(" ms"),
            opt("transition", "Transition", "list").choices(&[("Fade", json!("fade")), ("Cut", json!("cut"))]),
            opt("transition_speed", "Transition Speed", "int")
                .range(0.0, 3600000.0, 50.0)
                .unit(" ms")
                .when("transition", &[json!("fade")]),
            opt("loop", "Loop", "bool"),
            opt("randomize", "Randomize Playback", "bool"),
        ],
        "vlc_source" => vec![
            opt("playlist", "Playlist", "editable_list").files(MEDIA_FILES),
            opt("loop", "Loop Playlist", "bool"),
            opt("shuffle", "Shuffle playlist", "bool"),
        ],
        "image_source" => vec![
            opt("file", "Image File", "path").file(IMAGES),
            opt("unload", "Unload image when not showing", "bool"),
            opt("linear_alpha", "Apply alpha in linear space", "bool"),
        ],
        "text_ft2_source" | "text_ft2_source_v2" => vec![
            opt("font", "Font", "font"),
            opt("text", "Text", "text").multiline().when("from_file", &[json!(false)]),
            opt("from_file", "Read from file", "bool"),
            opt("text_file", "Text File (UTF-8 or UTF-16)", "path")
                .file("Text Files (*.txt);;All Files (*.*)")
                .when("from_file", &[json!(true)]),
            opt("antialiasing", "Antialiasing", "bool"),
            opt("color1", "Color 1", "color_alpha"),
            opt("color2", "Color 2", "color_alpha"),
            opt("outline", "Outline", "bool"),
            opt("drop_shadow", "Drop Shadow", "bool"),
            opt("custom_width", "Custom text width", "int").range(0.0, 4096.0, 1.0),
            opt("word_wrap", "Word Wrap", "bool"),
        ],
        "ffmpeg_source" => vec![
            opt("is_local_file", "Local File", "bool"),
            opt("local_file", "Local File", "path").file(MEDIA),
            opt("looping", "Loop", "bool"),
            opt("restart_on_activate", "Restart playback when source becomes active", "bool"),
            opt("close_when_inactive", "Close file when inactive", "bool"),
            opt("speed_percent", "Speed", "int").range(1.0, 200.0, 1.0).slider().unit("%"),
        ],
        "screen_capture" => {
            let mut v = vec![opt("type", "Method", "list").choices(&[
                ("Display Capture", json!(0)),
                ("Window Capture", json!(1)),
                ("Application Capture", json!(2)),
            ])];
            #[cfg(target_os = "macos")]
            if let Ok((displays, windows)) = ferrocore::screen::shareable() {
                let names = screen_names();
                let d: Vec<(String, Value)> =
                    displays.iter().enumerate().map(|(i, d)| (display_label(i, d, &names), json!(d.id))).collect();
                let w: Vec<(String, Value)> = windows
                    .iter()
                    .filter(|w| !w.title.is_empty())
                    .map(|w| (format!("[{}] {}", w.app, w.title), json!(w.id)))
                    .collect();
                // Display Capture and Application Capture pick a display, Window Capture a
                // window, Application Capture an app.
                v.push(opt("display", "Display", "list").choices(&as_refs(&d)).when("type", &[json!(0), json!(2)]));
                v.push(opt("window", "Window", "list").choices(&as_refs(&w)).when("type", &[json!(1)]));
            }
            #[cfg(target_os = "macos")]
            v.push(
                opt("application", "Application", "list").choices(&as_refs(&app_choices())).when("type", &[json!(2)]),
            );
            v.push(opt("show_cursor", "Show Cursor", "bool"));
            v
        }
        "av_capture_input" | "av_capture_input_v2" => {
            #[allow(unused_mut)]
            let mut list: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "macos")]
            list.extend(ferrocore::camera::cameras().into_iter().map(|d| (d.name, json!(d.id))));
            let l: Vec<(&str, Value)> = list.iter().map(|(a, b)| (a.as_str(), b.clone())).collect();
            let mut v = vec![opt("device", "Device", "list").choices(&l)];
            v.extend(camera_size_rate());
            v
        }
        "coreaudio_input_capture" => {
            #[allow(unused_mut)]
            let mut list: Vec<(String, Value)> = vec![("Default".into(), json!("default"))];
            #[cfg(target_os = "macos")]
            list.extend(ferrocore::camera::microphones().into_iter().map(|d| (d.name, json!(d.id))));
            let l: Vec<(&str, Value)> = list.iter().map(|(a, b)| (a.as_str(), b.clone())).collect();
            vec![opt("device_id", "Device", "list").choices(&l)]
        }
        "sck_audio_capture" | "coreaudio_output_capture" => {
            #[allow(unused_mut)]
            let mut v = vec![
                opt("type", "Method", "list")
                    .choices(&[("Desktop Audio Capture", json!(0)), ("Application Audio Capture", json!(1))]),
            ];
            #[cfg(target_os = "macos")]
            v.push(
                opt("application", "Application", "list").choices(&as_refs(&app_choices())).when("type", &[json!(1)]),
            );
            v
        }
        // Windows (OBS's own keys).
        "monitor_capture" => {
            #[allow(unused_mut)]
            let mut d: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "windows")]
            if let Ok((displays, _)) = ferrocore::screen::shareable() {
                d.extend(
                    displays.iter().map(|x| (format!("Display {} ({}x{})", x.id + 1, x.width, x.height), json!(x.id))),
                );
            }
            vec![
                opt("monitor", "Display", "list").choices(&as_refs(&d)),
                opt("capture_cursor", "Capture Cursor", "bool"),
            ]
        }
        "window_capture" => {
            #[allow(unused_mut)]
            let mut w: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "windows")]
            if let Ok((_, windows)) = ferrocore::screen::shareable() {
                w.extend(windows.iter().map(|x| (format!("[{}] {}", x.app, x.title), json!(x.id))));
            }
            vec![opt("window", "Window", "list").choices(&as_refs(&w)), opt("cursor", "Capture Cursor", "bool")]
        }
        "dshow_input" => {
            #[allow(unused_mut)]
            let mut list: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "windows")]
            list.extend(ferrocore::camera::cameras().into_iter().map(|d| (d.name, json!(d.id))));
            let mut v = vec![opt("video_device_id", "Device", "list").choices(&as_refs(&list))];
            v.extend(camera_size_rate());
            v
        }
        // Linux (OBS's own keys).
        "xshm_input" => {
            #[allow(unused_mut)]
            let mut d: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "linux")]
            if let Ok((displays, _)) = ferrocore::screen::shareable() {
                d.extend(
                    displays.iter().map(|x| (format!("Display {} ({}x{})", x.id + 1, x.width, x.height), json!(x.id))),
                );
            }
            vec![opt("screen", "Screen", "list").choices(&as_refs(&d)), opt("show_cursor", "Capture Cursor", "bool")]
        }
        "v4l2_input" => {
            #[allow(unused_mut)]
            let mut list: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "linux")]
            list.extend(ferrocore::camera::cameras().into_iter().map(|d| (d.name, json!(d.id))));
            let mut v = vec![opt("device_id", "Device", "list").choices(&as_refs(&list))];
            v.extend(camera_size_rate());
            v
        }
        "pulse_input_capture" => {
            #[allow(unused_mut)]
            let mut list: Vec<(String, Value)> = vec![("Default".into(), json!("default"))];
            #[cfg(target_os = "linux")]
            list.extend(ferrocore::camera::microphones().into_iter().map(|d| (d.name, json!(d.id))));
            vec![opt("device_id", "Device", "list").choices(&as_refs(&list))]
        }
        "pulse_output_capture" => vec![opt("device_id", "Device", "list").choices(&[("Default", json!("default"))])],
        "wasapi_process_output_capture" => {
            // OBS's value: "title:class:program.exe" (only the program is read here).
            #[allow(unused_mut)]
            let mut w: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "windows")]
            if let Ok((_, windows)) = ferrocore::screen::shareable() {
                w.extend(windows.iter().filter(|x| !x.app.is_empty()).map(|x| {
                    let value = format!("{}::{}.exe", x.title.replace(':', "#3A"), x.app);
                    (format!("[{}.exe]: {}", x.app, x.title), json!(value))
                }));
            }
            vec![opt("window", "Window", "list").choices(&as_refs(&w))]
        }
        "wasapi_input_capture" => {
            #[allow(unused_mut)]
            let mut list: Vec<(String, Value)> = vec![("Default".into(), json!("default"))];
            #[cfg(target_os = "windows")]
            list.extend(ferrocore::camera::microphones().into_iter().map(|d| (d.name, json!(d.id))));
            vec![opt("device_id", "Device", "list").choices(&as_refs(&list))]
        }
        "wasapi_output_capture" => {
            vec![opt("device_id", "Device", "list").choices(&[("Default", json!("default"))])]
        }
        "browser_source" | "ferrocast_web" => vec![
            opt("is_local_file", "Local file", "bool"),
            opt("local_file", "Local file", "path")
                .file("HTML Files (*.html *.htm);;All Files (*.*)")
                .when("is_local_file", &[json!(true)]),
            opt("url", "URL", "text").when("is_local_file", &[json!(false)]),
            opt("width", "Width", "int").range(1.0, 8192.0, 1.0),
            opt("height", "Height", "int").range(1.0, 8192.0, 1.0),
            opt("fps", "FPS", "int").range(1.0, 60.0, 1.0),
            opt("css", "Custom CSS", "text").multiline(),
        ],
        // Transitions.
        "swipe_transition" | "slide_transition" => {
            let mut v = vec![opt("direction", "Direction", "list").choices(&[
                ("Left", json!("left")),
                ("Right", json!("right")),
                ("Up", json!("up")),
                ("Down", json!("down")),
            ])];
            if kind == "swipe_transition" {
                v.push(opt("swipe_in", "Swipe In", "bool"));
            }
            v
        }
        "fade_to_color_transition" => vec![
            opt("color", "Color", "color"),
            opt("switch_point", "Peak Color Point", "int").range(0.0, 100.0, 1.0).slider().unit("%"),
        ],
        "wipe_transition" => vec![
            opt("luma_image", "Image", "path").file("Image Files (*.png *.jpg *.jpeg *.bmp);;All Files (*.*)"),
            opt("luma_softness", "Softness", "float").range(0.0, 1.0, 0.01).slider(),
            opt("luma_invert", "Invert", "bool"),
        ],
        "obs_stinger_transition" => vec![
            opt("path", "Video File", "path").file("Video Files (*.mp4 *.mov *.webm *.mkv);;All Files (*.*)"),
            opt("transition_point_type", "Transition Point Type", "list")
                .choices(&[("Time (milliseconds)", json!(0)), ("Frame", json!(1))]),
            opt("transition_point", "Transition Point", "int").range(0.0, 60_000.0, 1.0),
        ],
        // Filters.
        "chroma_key_filter" | "chroma_key_filter_v2" => vec![
            opt("key_color_type", "Key Color Type", "list").choices(&[
                ("Green", json!("green")),
                ("Blue", json!("blue")),
                ("Magenta", json!("magenta")),
                ("Custom", json!("custom")),
            ]),
            opt("key_color", "Key Color", "color").when("key_color_type", &[json!("custom")]),
            opt("similarity", "Similarity", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("smoothness", "Smoothness", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("spill", "Key Color Spill Reduction", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("opacity", "Opacity", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("contrast", "Contrast", "float").range(-4.0, 4.0, 0.01).slider(),
            opt("brightness", "Brightness", "float").range(-1.0, 1.0, 0.0001).slider(),
            opt("gamma", "Gamma", "float").range(-1.0, 1.0, 0.01).slider(),
        ],
        "color_key_filter" | "color_key_filter_v2" => vec![
            opt("key_color_type", "Key Color Type", "list").choices(&[
                ("Green", json!("green")),
                ("Blue", json!("blue")),
                ("Red", json!("red")),
                ("Magenta", json!("magenta")),
                ("Custom", json!("custom")),
            ]),
            opt("key_color", "Key Color", "color").when("key_color_type", &[json!("custom")]),
            opt("similarity", "Similarity", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("smoothness", "Smoothness", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("opacity", "Opacity", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("contrast", "Contrast", "float").range(-4.0, 4.0, 0.01).slider(),
            opt("brightness", "Brightness", "float").range(-1.0, 1.0, 0.0001).slider(),
            opt("gamma", "Gamma", "float").range(-1.0, 1.0, 0.01).slider(),
        ],
        "color_filter" | "color_filter_v2" => vec![
            opt("gamma", "Gamma", "float").range(-3.0, 3.0, 0.01).slider(),
            opt("contrast", "Contrast", "float").range(-4.0, 4.0, 0.01).slider(),
            opt("brightness", "Brightness", "float").range(-1.0, 1.0, 0.0001).slider(),
            opt("saturation", "Saturation", "float").range(-1.0, 5.0, 0.01).slider(),
            opt("hue_shift", "Hue Shift", "float").range(-180.0, 180.0, 0.01).slider(),
            opt("opacity", "Opacity", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("color_multiply", "Color Multiply", "color"),
            opt("color_add", "Color Add", "color"),
        ],
        "luma_key_filter" | "luma_key_filter_v2" => vec![
            opt("luma_max", "Luma Max", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("luma_max_smooth", "Luma Max Smooth", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("luma_min", "Luma Min", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("luma_min_smooth", "Luma Min Smooth", "float").range(0.0, 1.0, 0.0001).slider(),
        ],
        "sharpness_filter" | "sharpness_filter_v2" => {
            vec![opt("sharpness", "Sharpness", "float").range(0.0, 1.0, 0.01).slider()]
        }
        "crop_filter" => vec![
            opt("relative", "Relative", "bool"),
            opt("left", "Left", "int").range(-8192.0, 8192.0, 1.0),
            opt("top", "Top", "int").range(-8192.0, 8192.0, 1.0),
            // Relative trims all four edges; otherwise it keeps a box of a width and height.
            opt("right", "Right", "int").range(-8192.0, 8192.0, 1.0).when("relative", &[json!(true)]),
            opt("bottom", "Bottom", "int").range(-8192.0, 8192.0, 1.0).when("relative", &[json!(true)]),
            opt("cx", "Width", "int").range(0.0, 8192.0, 1.0).when("relative", &[json!(false)]),
            opt("cy", "Height", "int").range(0.0, 8192.0, 1.0).when("relative", &[json!(false)]),
        ],
        "mask_filter" | "mask_filter_v2" => vec![
            opt("type", "Type", "list").choices(&[
                ("Alpha Mask (Alpha Channel)", json!("mask_alpha_filter.effect")),
                ("Alpha Mask (Color Channel)", json!("mask_color_filter.effect")),
                ("Blend (Multiply)", json!("blend_mul_filter.effect")),
                ("Blend (Addition)", json!("blend_add_filter.effect")),
                ("Blend (Subtraction)", json!("blend_sub_filter.effect")),
            ]),
            opt("image_path", "Path", "path").file(IMAGES),
            opt("color", "Color", "color"),
            opt("opacity", "Opacity", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("stretch", "Stretch Image (discard image aspect ratio)", "bool"),
        ],
        "clut_filter" => vec![
            opt("image_path", "Path", "path").file("Image or cube files (*.png *.cube);;All Files (*.*)"),
            opt("clut_amount", "Amount", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("passthrough_alpha", "Passthrough Alpha", "bool"),
        ],
        "scroll_filter" => vec![
            opt("speed_x", "Horizontal Speed", "float").range(-500.0, 500.0, 1.0).slider(),
            opt("speed_y", "Vertical Speed", "float").range(-500.0, 500.0, 1.0).slider(),
            opt("limit_cx", "Limit Width", "bool"),
            opt("cx", "Width", "int").range(1.0, 8192.0, 1.0).when("limit_cx", &[json!(true)]),
            opt("limit_cy", "Limit Height", "bool"),
            opt("cy", "Height", "int").range(1.0, 8192.0, 1.0).when("limit_cy", &[json!(true)]),
            opt("loop", "Loop", "bool"),
        ],
        "gain_filter" => vec![opt("db", "Gain", "float").range(-30.0, 30.0, 0.1).slider().unit(" dB")],
        "noise_gate_filter" => vec![
            opt("close_threshold", "Close Threshold", "float").range(-96.0, 0.0, 0.1).slider().unit(" dB"),
            opt("open_threshold", "Open Threshold", "float").range(-96.0, 0.0, 0.1).slider().unit(" dB"),
            opt("attack_time", "Attack Time", "int").range(0.0, 10000.0, 1.0).unit(" ms"),
            opt("hold_time", "Hold Time", "int").range(0.0, 10000.0, 1.0).unit(" ms"),
            opt("release_time", "Release Time", "int").range(0.0, 10000.0, 1.0).unit(" ms"),
        ],
        "compressor_filter" => vec![
            opt("ratio", "Ratio", "float").range(1.0, 32.0, 0.5).slider().unit(":1"),
            opt("threshold", "Threshold", "float").range(-60.0, 0.0, 0.1).slider().unit(" dB"),
            opt("attack_time", "Attack", "int").range(1.0, 500.0, 1.0).unit(" ms"),
            opt("release_time", "Release", "int").range(1.0, 1000.0, 1.0).unit(" ms"),
            opt("output_gain", "Output Gain", "float").range(-32.0, 32.0, 0.1).slider().unit(" dB"),
        ],
        "limiter_filter" => vec![
            opt("threshold", "Threshold", "float").range(-60.0, 0.0, 0.1).slider().unit(" dB"),
            opt("release_time", "Release", "int").range(1.0, 1000.0, 1.0).unit(" ms"),
        ],
        "expander_filter" | "upward_compressor_filter" => vec![
            opt("ratio", "Ratio", "float").range(1.0, 20.0, 0.1).slider().unit(":1"),
            opt("threshold", "Threshold", "float").range(-60.0, 0.0, 0.1).slider().unit(" dB"),
            opt("attack_time", "Attack", "int").range(1.0, 100.0, 1.0).unit(" ms"),
            opt("release_time", "Release", "int").range(1.0, 1000.0, 1.0).unit(" ms"),
            opt("output_gain", "Output Gain", "float").range(-32.0, 32.0, 0.1).slider().unit(" dB"),
        ],
        "basic_eq_filter" => vec![
            opt("low", "Low", "float").range(-20.0, 20.0, 0.1).slider().unit(" dB"),
            opt("mid", "Mid", "float").range(-20.0, 20.0, 0.1).slider().unit(" dB"),
            opt("high", "High", "float").range(-20.0, 20.0, 0.1).slider().unit(" dB"),
        ],
        "invert_polarity_filter" => Vec::new(),
        "ferrocast_remove_background" => vec![
            opt("mode", "Background", "list").choices(&[("Remove it", json!(0)), ("Blur it", json!(1))]),
            opt("soft", "Soft edge", "int").range(0.0, 40.0, 1.0).slider().unit("%"),
            opt("blur", "Blur", "int").range(2.0, 100.0, 1.0).slider().unit(" px").when("mode", &[json!(1)]),
        ],
        "ferrocast_deepfilter" => vec![opt("strength", "Strength", "int").range(1.0, 100.0, 1.0).slider()],
        "noise_suppress_filter" | "noise_suppress_filter_v2" => {
            vec![opt("suppress_level", "Suppression Level", "int").range(-60.0, 0.0, 1.0).slider().unit(" dB")]
        }
        "ferrocast_rounded_corners" => {
            vec![opt("radius", "Radius", "int").range(0.0, 1000.0, 1.0).slider().unit(" px")]
        }
        "ferrocast_border" => vec![
            opt("width", "Width", "int").range(0.0, 200.0, 1.0).slider().unit(" px"),
            opt("color", "Color", "color_alpha"),
            opt("radius", "Radius", "int").range(0.0, 1000.0, 1.0).slider().unit(" px"),
        ],
        "ferrocast_shadow" => vec![
            opt("x", "Offset X", "int").range(-200.0, 200.0, 1.0).unit(" px"),
            opt("y", "Offset Y", "int").range(-200.0, 200.0, 1.0).unit(" px"),
            opt("blur", "Blur", "int").range(0.0, 200.0, 1.0).slider().unit(" px"),
            opt("color", "Color", "color_alpha"),
        ],
        "ferrocast_shape" | "ferrocast_blur_area" | "ferrocast_look" => Vec::new(),
        _ => return None,
    })
}

/// A kind's settings to start with (the keys its form shows, at OBS's defaults).
pub fn defaults(kind: &str) -> Option<Value> {
    Some(match kind {
        "color_source_v3" => json!({ "color": 0xFFD1_D1D1u32, "width": 0, "height": 0 }),
        "color_source" | "color_source_v2" => json!({ "color": 0xFFFF_FFFFu32, "width": 0, "height": 0 }),
        "image_source" => json!({ "file": "", "unload": false, "linear_alpha": false }),
        "ndi_source" => json!({ "ndi_source_name": "" }),
        "ferrocast_clone" => json!({ "clone": "", "filters": true }),
        "ferrocast_freeze" => json!({}),
        "ferrocast_replay" => json!({ "local_file": "", "switch": true }),
        "ferrocast_visualizer" => json!({
            "source": "", "bars": 32, "width": 800, "height": 200, "color": 0xFFFF_FFFFu32, "mirror": false,
        }),
        "ferrocast_inputs" => json!({
            "layout": "wasd", "keys": "_ W _ / A S D", "size": 64,
            "color_rest": 0xCC28_2828u32, "color_held": 0xFF1F_5AFFu32, "color_text": 0xFFFF_FFFFu32,
        }),
        "ferrocast_web_text" => json!({
            "url": "https://", "path": "", "words": "{}", "every": 30,
            "font": { "face": "Helvetica", "size": 72, "style": "Bold", "flags": 1 },
            "color1": 0xFFFF_FFFFu32, "color2": 0xFFFF_FFFFu32, "outline": true, "drop_shadow": false,
        }),
        "ferrocast_countdown" => json!({
            "mode": "for", "minutes": 5, "seconds": 0, "until": "21:00",
            "before": "Starting in ", "end": "Starting now",
            "font": { "face": "Helvetica", "size": 96, "style": "Bold", "flags": 1 },
            "color1": 0xFFFF_FFFFu32, "color2": 0xFFFF_FFFFu32, "outline": true, "drop_shadow": false,
        }),
        "ferrocast_shader" => json!({
            "preset": "custom",
            "shader_file": "",
            "shader_text": "// Each point of the picture: pic(uv) reads it; time(), size() and params() help.\nfn effect(uv: vec2<f32>) -> vec4<f32> {\n    let c = pic(uv);\n    let gray = dot(c.rgb, vec3<f32>(0.299, 0.587, 0.114));\n    return vec4<f32>(mix(c.rgb, vec3<f32>(gray), params().x), c.a);\n}\n",
            "param1": 0.5, "param2": 0.5, "param3": 0.5, "param4": 0.5,
        }),
        "slideshow" | "slideshow_v2" | "image_slide_show" => json!({
            "files": [], "slide_time": 8000, "transition": "fade", "transition_speed": 700, "loop": true, "randomize": false,
        }),
        "vlc_source" => json!({ "playlist": [], "loop": true, "shuffle": false }),
        "text_ft2_source" | "text_ft2_source_v2" => json!({
            "font": { "face": "Helvetica", "size": 256, "style": "Regular", "flags": 0 },
            "text": "", "color1": 0xFFFF_FFFFu32, "color2": 0xFFFF_FFFFu32, "outline": false,
            "drop_shadow": false, "word_wrap": false, "custom_width": 0, "antialiasing": true,
        }),
        "ffmpeg_source" => {
            json!({ "is_local_file": true, "local_file": "", "looping": false, "restart_on_activate": true, "close_when_inactive": false, "speed_percent": 100 })
        }
        "screen_capture" => json!({ "type": 0, "show_cursor": true, "application": "" }),
        "av_capture_input" | "av_capture_input_v2" => json!({ "device": "" }),
        "coreaudio_input_capture" => json!({ "device_id": "default" }),
        "sck_audio_capture" | "coreaudio_output_capture" => json!({ "type": 0, "application": "" }),
        "monitor_capture" => json!({ "monitor": 0, "capture_cursor": true }),
        "window_capture" => json!({ "cursor": true }),
        "dshow_input" => json!({ "video_device_id": "" }),
        "wasapi_input_capture" | "wasapi_output_capture" => json!({ "device_id": "default" }),
        "wasapi_process_output_capture" => json!({ "window": "" }),
        "xshm_input" => json!({ "screen": 0, "show_cursor": true }),
        "v4l2_input" => json!({ "device_id": "" }),
        "pulse_input_capture" | "pulse_output_capture" => json!({ "device_id": "default" }),
        "browser_source" | "ferrocast_web" => json!({
            "is_local_file": false, "local_file": "", "url": "https://obsproject.com/browser-source",
            "width": 800, "height": 600, "fps": 30,
            "css": "body { background-color: rgba(0, 0, 0, 0); margin: 0px auto; overflow: hidden; }",
        }),
        "chroma_key_filter" | "chroma_key_filter_v2" => {
            json!({ "key_color_type": "green", "similarity": 400, "smoothness": 80, "spill": 100, "opacity": 1.0, "contrast": 0.0, "brightness": 0.0, "gamma": 0.0 })
        }
        "color_key_filter" | "color_key_filter_v2" => {
            json!({ "key_color_type": "green", "similarity": 80, "smoothness": 50, "opacity": 1.0, "contrast": 0.0, "brightness": 0.0, "gamma": 0.0 })
        }
        "color_filter" | "color_filter_v2" => {
            json!({ "gamma": 0.0, "contrast": 0.0, "brightness": 0.0, "saturation": 0.0, "hue_shift": 0.0, "opacity": 1.0, "color_multiply": 0x00FF_FFFFu32, "color_add": 0 })
        }
        "luma_key_filter" | "luma_key_filter_v2" => {
            json!({ "luma_max": 1.0, "luma_max_smooth": 0.0, "luma_min": 0.0, "luma_min_smooth": 0.0 })
        }
        "sharpness_filter" | "sharpness_filter_v2" => json!({ "sharpness": 0.08 }),
        "crop_filter" => json!({ "relative": true, "left": 0, "top": 0, "right": 0, "bottom": 0, "cx": 0, "cy": 0 }),
        "swipe_transition" => json!({ "direction": "left", "swipe_in": false }),
        "slide_transition" => json!({ "direction": "left" }),
        "fade_to_color_transition" => json!({ "color": 0xFF00_0000u32, "switch_point": 50 }),
        "wipe_transition" => json!({ "luma_image": "", "luma_softness": 0.03, "luma_invert": false }),
        "obs_stinger_transition" => json!({ "path": "", "transition_point_type": 0, "transition_point": 500 }),
        "mask_filter" | "mask_filter_v2" => {
            json!({ "type": "mask_color_filter.effect", "image_path": "", "color": 0x00FF_FFFFu32, "opacity": 1.0, "stretch": false })
        }
        "clut_filter" => json!({ "image_path": "", "clut_amount": 1.0, "passthrough_alpha": false }),
        "scroll_filter" => {
            json!({ "speed_x": 0.0, "speed_y": 0.0, "limit_cx": false, "cx": 100, "limit_cy": false, "cy": 100, "loop": true })
        }
        "gain_filter" => json!({ "db": 0.0 }),
        "ferrocast_deepfilter" => json!({ "strength": 100 }),
        "noise_suppress_filter" | "noise_suppress_filter_v2" => json!({ "method": "rnnoise", "suppress_level": -30 }),
        "ferrocast_remove_background" => json!({ "mode": 0, "soft": 10, "blur": 24 }),
        "noise_gate_filter" => {
            json!({ "close_threshold": -32.0, "open_threshold": -26.0, "attack_time": 25, "hold_time": 200, "release_time": 150 })
        }
        "compressor_filter" => {
            json!({ "ratio": 10.0, "threshold": -18.0, "attack_time": 6, "release_time": 60, "output_gain": 0.0 })
        }
        "limiter_filter" => json!({ "threshold": -6.0, "release_time": 60 }),
        "expander_filter" => {
            json!({ "ratio": 2.0, "threshold": -40.0, "attack_time": 10, "release_time": 50, "output_gain": 0.0 })
        }
        "upward_compressor_filter" => {
            json!({ "ratio": 0.5, "threshold": -20.0, "attack_time": 10, "release_time": 50, "output_gain": 0.0 })
        }
        "basic_eq_filter" => json!({ "low": 0.0, "mid": 0.0, "high": 0.0 }),
        "invert_polarity_filter" => json!({}),
        "ferrocast_rounded_corners" => json!({ "radius": 24 }),
        "ferrocast_border" => json!({ "width": 6, "color": 0xFFFF_FFFFu32, "radius": 0 }),
        "ferrocast_shadow" => json!({ "x": 0, "y": 8, "blur": 24, "color": 0x8000_0000u32 }),
        "ferrocast_shape" | "ferrocast_blur_area" | "ferrocast_look" => json!({}),
        _ => return None,
    })
}

/// A display as OBS names it ("VG271U M3: 2560x1440 @ 0,0"), or "Display 2 (2560x1440)" when
/// macOS gives no name.
#[cfg(target_os = "macos")]
fn display_label(i: usize, d: &ferrocore::screen::Display, names: &std::collections::HashMap<u32, String>) -> String {
    match names.get(&d.id).filter(|n| !n.trim().is_empty()) {
        Some(n) => format!("{n}: {}x{} @ {},{}", d.width, d.height, d.x, d.y),
        None => format!("Display {} ({}x{})", i + 1, d.width, d.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An option shown only for some values of another names one in its own form, with values
    /// that option can hold (else it would never show).
    #[test]
    fn shown_when_names_an_option_of_its_form() {
        let k = kinds();
        for id in k.inputs.iter().chain(&k.filters).chain(&k.transitions).map(|k| k.id.as_str()) {
            let form = options(id).unwrap_or_default();
            for o in &form {
                let Some(w) = &o.shown_when else { continue };
                let by = form.iter().find(|x| x.key == w.key);
                let by = by.unwrap_or_else(|| panic!("{id}: {} is shown by {}, not in its form", o.key, w.key));
                for v in &w.values {
                    let can = if by.kind == "bool" { v.is_boolean() } else { by.values.contains(v) };
                    assert!(can, "{id}: {} is shown when {} is {v}, which it can't be", o.key, w.key);
                }
            }
        }
        // A Window Capture's form keeps the Display list for the other two methods.
        let form = options("screen_capture").unwrap_or_default();
        let show = |key: &str| form.iter().find(|o| o.key == key).and_then(|o| o.shown_when.clone());
        assert_eq!(show("show_cursor"), None);
        #[cfg(target_os = "macos")]
        assert_eq!(show("application").map(|w| w.values), Some(vec![json!(2)]));
    }

    #[test]
    fn every_kind_has_a_form_and_defaults_our_engine_reads() {
        let k = kinds();
        for id in k.inputs.iter().chain(&k.filters).map(|k| k.id.as_str()) {
            assert!(options(id).is_some(), "{id} has a form");
            let d = defaults(id).unwrap_or_else(|| panic!("{id} has defaults"));
            for o in options(id).unwrap_or_default() {
                assert!(
                    d.get(&o.key).is_some()
                        || matches!(o.key.as_str(), "text_file" | "display" | "window" | "key_color" | "from_file"),
                    "{id}: {} has a default",
                    o.key
                );
            }
        }
        // A source with a filter of every kind at its defaults: our engine brings each over
        // (nothing left out but what needs a file chosen first).
        let filters: Vec<Value> = k
            .filters
            .iter()
            .map(|f| json!({ "id": f.id, "versioned_id": f.id, "name": f.name, "settings": defaults(&f.id) }))
            .collect();
        let sources = vec![
            json!({ "id": "color_source_v3", "name": "C", "settings": defaults("color_source_v3"), "filters": filters }),
        ];
        let got = ferrocore::obs::import(&json!({ "sources": sources }), (1920, 1080));
        let effects: Vec<String> = got.skipped.clone();
        assert!(
            // A mask and a LUT need their picture first.
            effects.iter().all(|s| s.contains("mask_filter") || s.contains("clut_filter")),
            "left out: {effects:?}"
        );
    }
}
