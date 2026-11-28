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
/// The options our own encoder offers for encoder `id` (the settings' "Your own"): x264's
/// preset and tune; the system's encoders take their settings from the plain ones.
pub fn own_encoder_options(id: &str) -> Vec<serde_json::Value> {
    if !id.to_ascii_lowercase().contains("x264") {
        return Vec::new();
    }
    let list = |key: &str, label: &str, help: &str, names: &[&str], default: &str| {
        serde_json::json!({
            "key": key, "label": label, "kind": "list", "help": help,
            "visible": true, "enabled": true, "min": null, "max": null, "step": null,
            "slider": false, "suffix": "", "default": default,
            "choices": names, "values": names, "group": []
        })
    };
    vec![
        list(
            "preset",
            "Preset",
            "Slower presets look better at the same size and use more of the computer.",
            &ferrocore::video::X264_PRESETS,
            "veryfast",
        ),
        list(
            "tune",
            "Tune",
            "What the picture is mostly like; leave it off if unsure.",
            &ferrocore::video::X264_TUNES,
            "",
        ),
    ]
}

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

type Slot = Arc<Mutex<Option<(Instant, u64, Option<Vec<OptionInfo>>)>>>;

fn slot(kind: &str) -> Slot {
    static SLOTS: Mutex<Option<HashMap<String, Slot>>> = Mutex::new(None);
    let mut all = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    all.get_or_insert_with(HashMap::new).entry(kind.to_string()).or_default().clone()
}

fn hotplug_epoch() -> u64 {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        ferrocore::camera::hotplug_epoch()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        0
    }
}

fn options_cached_with(
    kind: &str,
    epoch: u64,
    fetch: impl FnOnce(&str) -> Option<Vec<OptionInfo>>,
) -> Option<Vec<OptionInfo>> {
    if !lists_devices(kind) {
        return fetch(kind);
    }
    let slot = slot(kind);
    // Held while reading, so a second ask waits for this read and takes its answer.
    let mut held = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, cached_epoch, got)) = held.as_ref()
        && at.elapsed() < FRESH
        && *cached_epoch == epoch
    {
        return got.clone();
    }
    let got = fetch(kind);
    *held = Some((Instant::now(), epoch, got.clone()));
    got
}

/// `options`, with device lists read at most once per [`FRESH`] (and refreshed immediately on
/// hot-plug connect/disconnect epoch changes): two asks at once (the dialog reading ahead while
/// you click) wait for the same read instead of each starting one. Call it off the window's thread
/// for device kinds.
pub fn options_cached(kind: &str) -> Option<Vec<OptionInfo>> {
    options_cached_with(kind, hotplug_epoch(), options)
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
        kind("color_source_v3", "Colour"),
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
            kind("color_key_filter_v2", "Colour Key"),
            kind("ferrocast_adjust", "Adjust"),
            kind("color_filter_v2", "Colour Correction"),
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
            kind("ferrocast_video_delay", "Video Delay"),
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
        // A VST3 sound plugin, run in its own locked-down helper (Mac first; docs/plan/VST.md).
        .chain(cfg!(target_os = "macos").then(|| kind("ferrocast_vst", "VST Plugin")))
        .collect(),
        transitions: vec![
            kind("cut_transition", "Cut"),
            kind("fade_transition", "Fade"),
            kind("swipe_transition", "Swipe"),
            kind("slide_transition", "Slide"),
            kind("fade_to_color_transition", "Fade to Colour"),
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
        default: None,
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

    fn hint(mut self, help: &str) -> Self {
        self.help = help.into();
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

/// The rate to switch to when a camera's `size` can't reach the saved `fps`: the highest of the
/// usual rates it does (0, its own, if none). None when nothing needs to change.
pub fn reachable_rate(modes: &[(u32, u32, f64)], size: Option<(u32, u32)>, fps: u64) -> Option<u64> {
    let (w, h) = size?;
    let top = modes.iter().filter(|m| m.0 == w && m.1 == h).map(|m| m.2).fold(0.0, f64::max);
    if fps == 0 || top <= 0.0 || fps as f64 <= top + 0.5 {
        return None;
    }
    Some([120u64, 60, 50, 30, 25, 24].into_iter().find(|&r| r as f64 <= top + 0.5).unwrap_or(0))
}

/// A list's choices: each label and the value it saves.
pub type Choices = Vec<(String, Value)>;

/// A camera's Size and Frame rate lists from the modes it really has (width, height, top
/// rate), so a capture card that sends at most 2560×1440 isn't offered 4K: its sizes, biggest
/// first, and the usual rates the picked size (any, when none is) reaches. "Its own" first in
/// both; a saved choice it doesn't list stays, so nothing changes under anyone.
pub fn real_size_rate(
    modes: &[(u32, u32, f64)],
    size: Option<(u32, u32)>,
    saved_fps: u64,
    own: Option<(u32, u32)>,
) -> (Choices, Choices) {
    // 16:9 first, tallest first (the sizes people stream at), then the other usual shapes by
    // size; none taller than 4:3 or wider than 2:1 (a card's 1920×2160 comes out squeezed).
    let wide = |w: u32, h: u32| (f64::from(w) / f64::from(h) / (16.0 / 9.0) - 1.0).abs() <= 0.02;
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    for m in modes {
        let r = f64::from(m.0) / f64::from(m.1.max(1));
        if m.0 >= 320 && (4.0 / 3.0 - 0.01..=2.0).contains(&r) && !sizes.contains(&(m.0, m.1)) {
            sizes.push((m.0, m.1));
        }
    }
    sizes.sort_by_key(|&(w, h)| (!wide(w, h), std::cmp::Reverse(u64::from(w) * u64::from(h)), std::cmp::Reverse(w)));
    sizes.truncate(10);
    if let Some(s) = size
        && !sizes.contains(&s)
    {
        sizes.push(s);
    }
    let size_list = std::iter::once(("Its own".to_string(), json!("")))
        .chain(sizes.iter().map(|(w, h)| (format!("{w}×{h}"), json!(format!("{w}x{h}")))))
        .collect();
    let top = modes.iter().filter(|m| size.is_none_or(|(w, h)| m.0 == w && m.1 == h)).map(|m| m.2).fold(0.0, f64::max);
    let reach = |r: u64| r as f64 <= top + 0.5;
    let mut rates: Vec<u64> = [24u64, 25, 30, 50, 60, 120].into_iter().filter(|&r| reach(r)).collect();
    if saved_fps > 0 && !rates.contains(&saved_fps) {
        rates.push(saved_fps);
        rates.sort_unstable();
    }
    // With no size picked, a rate its own size doesn't reach says which size it takes (the
    // biggest 16:9 mode that reaches it: 60 may mean 1360×768), as the camera picks.
    let reaches_at = |(w, h): (u32, u32), r: u64| modes.iter().any(|m| m.0 == w && m.1 == h && m.2 + 0.5 >= r as f64);
    let biggest = |r: u64| {
        if let Some(o) = own
            && reaches_at(o, r)
        {
            return Some(o);
        }
        modes
            .iter()
            .filter(|m| wide(m.0, m.1) && m.2 + 0.5 >= r as f64)
            .max_by_key(|m| u64::from(m.0) * u64::from(m.1))
            .map(|m| (m.0, m.1))
    };
    let full = own.or_else(|| biggest(0));
    // A saved rate this size can't reach says what it gets instead.
    let rate_list = std::iter::once(("Its own".to_string(), json!(0)))
        .chain(rates.into_iter().map(|r| {
            let label = if !reach(r) && top > 0.0 {
                format!("{r} (this size goes to {})", top.round())
            } else if size.is_none()
                && let Some((w, h)) = biggest(r)
                && Some((w, h)) != full
            {
                format!("{r} · {w}×{h}")
            } else {
                r.to_string()
            };
            (label, json!(r))
        }))
        .collect();
    (size_list, rate_list)
}

/// The running apps with a window or sound to capture, by name (their bundle ids kept): the
/// ones in the Dock, not macOS's helpers (loginwindow, WindowManager, "AutoFill (Google
/// Chrome)"), which have nothing to capture.
#[cfg(target_os = "macos")]
fn app_choices() -> Vec<(String, Value)> {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
    let docked: std::collections::HashSet<String> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| a.activationPolicy() == NSApplicationActivationPolicy::Regular)
        .filter_map(|a| a.bundleIdentifier().map(|b| b.to_string()))
        .collect();
    ferrocore::screen::apps()
        .map(|l| {
            l.into_iter()
                .filter(|a| docked.is_empty() || docked.contains(&a.id))
                .map(|a| (a.name, json!(a.id)))
                .collect()
        })
        .unwrap_or_default()
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

/// A source's or filter's settings form, each option with what it starts at; None for a kind
/// our engine doesn't run.
pub fn options(kind: &str) -> Option<Vec<OptionInfo>> {
    let mut opts = form(kind)?;
    if let Some(d) = defaults(kind) {
        for o in &mut opts {
            o.default = d.get(&o.key).cloned();
        }
    }
    Some(opts)
}

fn form(kind: &str) -> Option<Vec<OptionInfo>> {
    Some(match kind {
        "color_source" | "color_source_v2" | "color_source_v3" => vec![
            opt("color", "Colour", "color_alpha"),
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
            opt("color", "Colour", "color_alpha"),
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
            opt("color1", "Colour", "color_alpha"),
            opt("outline", "Outline", "bool"),
            opt("drop_shadow", "Shadow", "bool"),
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
                opt("color1", "Colour", "color_alpha"),
                opt("outline", "Outline", "bool"),
                opt("drop_shadow", "Shadow", "bool"),
            ]
        }
        // Its file is set by the app each time a replay is saved.
        "ferrocast_replay" => vec![opt("switch", "Show its scene while it plays", "bool")],
        // Nothing to set: its switch freezes and unfreezes.
        "ferrocast_freeze" => vec![],
        // Pictures held in memory: the longer and bigger, the more it takes (capped).
        "ferrocast_video_delay" => vec![
            opt("delay_ms", "Delay", "int").range(0.0, 20_000.0, 10.0).unit(" ms").hint(
                "Pictures wait in memory, as video. The longest that fits: about 5.7 s at 1080p30, 2.9 s at 1080p60, \
                 1.6 s at 1440p60, 0.7 s at 4K60 (a third of that for a picture with transparency). \
                 Past it the delay is cut short.",
            ),
            opt("audio", "Delay its sound too", "bool"),
        ],
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
                opt("shader_file", "Shader file", "path")
                    .file("WGSL Shaders (*.wgsl);;All Files (*.*)")
                    .when("preset", &own),
                opt("shader_text", "Or type one here", "text").multiline().mono().when("preset", &own),
                opt("param1", "Strength", "float").range(0.0, 1.0, 0.01).slider(),
                opt("param2", "Size or speed", "float").range(0.0, 1.0, 0.01).slider(),
                opt("param3", "Setting 3", "float").range(0.0, 1.0, 0.01).slider().when("preset", &own),
                opt("param4", "Setting 4", "float").range(0.0, 1.0, 0.01).slider().when("preset", &own),
            ]
        }
        "slideshow" | "slideshow_v2" | "image_slide_show" => vec![
            opt("files", "Images", "editable_list").files(IMAGES),
            opt("slide_time", "Each slide for", "int").range(50.0, 3_600_000.0, 50.0).unit(" ms"),
            opt("transition", "Transition", "list").choices(&[("Fade", json!("fade")), ("Cut", json!("cut"))]),
            opt("transition_speed", "Transition length", "int")
                .range(0.0, 3600000.0, 50.0)
                .unit(" ms")
                .when("transition", &[json!("fade")]),
            opt("loop", "Loop", "bool"),
            opt("randomize", "Shuffle", "bool"),
        ],
        "vlc_source" => vec![
            opt("playlist", "Playlist", "editable_list").files(MEDIA_FILES),
            opt("loop", "Loop", "bool"),
            opt("shuffle", "Shuffle playlist", "bool"),
        ],
        "image_source" => vec![
            opt("file", "Image", "path").file(IMAGES),
            opt("unload", "Free memory when hidden", "bool"),
            opt("linear_alpha", "Blend edges in linear light", "bool"),
        ],
        "text_ft2_source" | "text_ft2_source_v2" => vec![
            opt("font", "Font", "font"),
            opt("text", "Text", "text").multiline().when("from_file", &[json!(false)]),
            opt("from_file", "Read from file", "bool"),
            opt("text_file", "Text file", "path")
                .file("Text Files (*.txt);;All Files (*.*)")
                .when("from_file", &[json!(true)]),
            opt("antialiasing", "Smooth edges", "bool"),
            opt("color1", "First colour", "color_alpha"),
            opt("color2", "Second colour", "color_alpha"),
            opt("outline", "Outline", "bool"),
            opt("drop_shadow", "Shadow", "bool"),
            opt("custom_width", "Text box width", "int").range(0.0, 4096.0, 1.0),
            opt("word_wrap", "Wrap lines", "bool"),
            opt("align", "Align", "list").choices(&[
                ("Left", json!("left")),
                ("Centre", json!("center")),
                ("Right", json!("right")),
            ]),
            opt("bk_color", "Background", "color"),
            opt("bk_opacity", "Background opacity", "int").range(0.0, 100.0, 1.0).slider().unit("%"),
        ],
        "ffmpeg_source" => vec![
            opt("is_local_file", "From a file on this computer", "bool"),
            opt("local_file", "File", "path").file(MEDIA).when("is_local_file", &[json!(true)]),
            opt("input", "Address", "text").when("is_local_file", &[json!(false)]),
            opt("input_format", "Format", "text").when("is_local_file", &[json!(false)]),
            opt("reconnect_delay_sec", "Reconnect after", "int")
                .range(1.0, 60.0, 1.0)
                .unit(" s")
                .when("is_local_file", &[json!(false)]),
            opt("ffmpeg_options", "FFmpeg options", "text").when("is_local_file", &[json!(false)]),
            opt("looping", "Loop", "bool").when("is_local_file", &[json!(true)]),
            opt("restart_on_activate", "Start over when shown", "bool"),
            opt("close_when_inactive", "Let go of the file when hidden", "bool"),
            opt("clear_on_media_end", "Clear when it ends", "bool").when("is_local_file", &[json!(true)]),
            opt("hw_decode", "Hardware decoding", "bool"),
            opt("speed_percent", "Speed", "int")
                .range(1.0, 200.0, 1.0)
                .slider()
                .unit("%")
                .when("is_local_file", &[json!(true)]),
        ],
        "screen_capture" => {
            let mut v = vec![opt("type", "What to capture", "list").choices(&[
                ("A whole screen", json!(0)),
                ("One window", json!(1)),
                ("One app", json!(2)),
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
            v.push(opt("show_cursor", "Show cursor", "bool"));
            // Apps this display capture leaves out (bundle ids), as Privacy does for all.
            #[cfg(target_os = "macos")]
            v.push(opt("exclude_apps", "Leave out", "editable_list").when("type", &[json!(0)]));
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
                opt("type", "What to capture", "list")
                    .choices(&[("All the computer’s sound", json!(0)), ("One app’s sound", json!(1))]),
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
            vec![opt("monitor", "Display", "list").choices(&as_refs(&d)), opt("capture_cursor", "Show cursor", "bool")]
        }
        "window_capture" => {
            #[allow(unused_mut)]
            let mut w: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "windows")]
            if let Ok((_, windows)) = ferrocore::screen::shareable() {
                w.extend(windows.iter().map(|x| (format!("[{}] {}", x.app, x.title), json!(x.id))));
            }
            vec![opt("window", "Window", "list").choices(&as_refs(&w)), opt("cursor", "Show cursor", "bool")]
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
            vec![opt("screen", "Screen", "list").choices(&as_refs(&d)), opt("show_cursor", "Show cursor", "bool")]
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
            opt("is_local_file", "From a file on this computer", "bool"),
            opt("local_file", "File", "path")
                .file("HTML Files (*.html *.htm);;All Files (*.*)")
                .when("is_local_file", &[json!(true)]),
            opt("url", "Address", "text").when("is_local_file", &[json!(false)]),
            opt("width", "Width", "int").range(1.0, 8192.0, 1.0),
            opt("height", "Height", "int").range(1.0, 8192.0, 1.0),
            opt("fps", "Frame rate", "int").range(1.0, 60.0, 1.0),
            opt("css", "Your own CSS", "text").multiline(),
            opt("reroute_audio", "Control audio in Ferrocast", "bool"),
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
                v.push(opt("swipe_in", "Swipe in", "bool"));
            }
            v
        }
        "fade_to_color_transition" => vec![
            opt("color", "Colour", "color"),
            opt("switch_point", "Fully coloured at", "int").range(0.0, 100.0, 1.0).slider().unit("%"),
        ],
        "wipe_transition" => vec![
            opt("luma_image", "Image", "path").file("Image Files (*.png *.jpg *.jpeg *.bmp);;All Files (*.*)"),
            opt("luma_softness", "Softness", "float").range(0.0, 1.0, 0.01).slider(),
            opt("luma_invert", "Invert", "bool"),
        ],
        "obs_stinger_transition" => vec![
            opt("path", "Video", "path").file("Video Files (*.mp4 *.mov *.webm *.mkv);;All Files (*.*)"),
            opt("transition_point_type", "Switch scenes at", "list")
                .choices(&[("Time (milliseconds)", json!(0)), ("Frame", json!(1))]),
            opt("transition_point", "Switch point", "int").range(0.0, 60_000.0, 1.0),
        ],
        // Filters.
        "chroma_key_filter" | "chroma_key_filter_v2" => vec![
            opt("key_color_type", "Key colour", "list").choices(&[
                ("Green", json!("green")),
                ("Blue", json!("blue")),
                ("Magenta", json!("magenta")),
                ("Custom", json!("custom")),
            ]),
            opt("key_color", "Your colour", "color").when("key_color_type", &[json!("custom")]),
            opt("similarity", "Similarity", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("smoothness", "Smoothness", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("spill", "Spill reduction", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("opacity", "Opacity", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("contrast", "Contrast", "float").range(-4.0, 4.0, 0.01).slider(),
            opt("brightness", "Brightness", "float").range(-1.0, 1.0, 0.0001).slider(),
            opt("gamma", "Gamma", "float").range(-1.0, 1.0, 0.01).slider(),
        ],
        "color_key_filter" | "color_key_filter_v2" => vec![
            opt("key_color_type", "Key colour", "list").choices(&[
                ("Green", json!("green")),
                ("Blue", json!("blue")),
                ("Red", json!("red")),
                ("Magenta", json!("magenta")),
                ("Custom", json!("custom")),
            ]),
            opt("key_color", "Your colour", "color").when("key_color_type", &[json!("custom")]),
            opt("similarity", "Similarity", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("smoothness", "Smoothness", "int").range(1.0, 1000.0, 1.0).slider(),
            opt("opacity", "Opacity", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("contrast", "Contrast", "float").range(-4.0, 4.0, 0.01).slider(),
            opt("brightness", "Brightness", "float").range(-1.0, 1.0, 0.0001).slider(),
            opt("gamma", "Gamma", "float").range(-1.0, 1.0, 0.01).slider(),
        ],
        // A photo app's sliders: 0 in the middle leaves the picture as it is.
        "ferrocast_adjust" => vec![
            opt("exposure", "Exposure", "float").range(-3.0, 3.0, 0.05).slider().unit(" EV"),
            opt("contrast", "Contrast", "int").range(-100.0, 100.0, 1.0).slider(),
            opt("highlights", "Highlights", "int").range(-100.0, 100.0, 1.0).slider(),
            opt("shadows", "Shadows", "int").range(-100.0, 100.0, 1.0).slider(),
            opt("saturation", "Saturation", "int").range(-100.0, 100.0, 1.0).slider(),
            opt("vibrance", "Vibrance", "int").range(-100.0, 100.0, 1.0).slider(),
            opt("temperature", "Temperature", "int").range(-100.0, 100.0, 1.0).slider(),
            opt("tint", "Tint", "int").range(-100.0, 100.0, 1.0).slider(),
        ],
        "color_filter" | "color_filter_v2" => vec![
            opt("gamma", "Gamma", "float").range(-3.0, 3.0, 0.01).slider(),
            opt("contrast", "Contrast", "float").range(-4.0, 4.0, 0.01).slider(),
            opt("brightness", "Brightness", "float").range(-1.0, 1.0, 0.0001).slider(),
            opt("saturation", "Saturation", "float").range(-1.0, 5.0, 0.01).slider(),
            opt("hue_shift", "Hue", "float").range(-180.0, 180.0, 0.01).slider(),
            opt("opacity", "Opacity", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("color_multiply", "Multiply by colour", "color"),
            opt("color_add", "Add colour", "color"),
        ],
        "luma_key_filter" | "luma_key_filter_v2" => vec![
            opt("luma_max", "Brightest kept", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("luma_max_smooth", "Bright edge softness", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("luma_min", "Darkest kept", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("luma_min_smooth", "Dark edge softness", "float").range(0.0, 1.0, 0.0001).slider(),
        ],
        "sharpness_filter" | "sharpness_filter_v2" => {
            vec![opt("sharpness", "Sharpness", "float").range(0.0, 1.0, 0.01).slider()]
        }
        "crop_filter" => vec![
            opt("relative", "Trim each edge", "bool"),
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
                ("Cut out by its transparency", json!("mask_alpha_filter.effect")),
                ("Cut out by its brightness", json!("mask_color_filter.effect")),
                ("Darken (multiply)", json!("blend_mul_filter.effect")),
                ("Lighten (add)", json!("blend_add_filter.effect")),
                ("Subtract", json!("blend_sub_filter.effect")),
            ]),
            opt("image_path", "Image", "path").file(IMAGES),
            opt("color", "Colour", "color"),
            opt("opacity", "Opacity", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("stretch", "Stretch to fill", "bool"),
        ],
        "clut_filter" => vec![
            opt("image_path", "Image", "path").file("Image or cube files (*.png *.cube);;All Files (*.*)"),
            opt("clut_amount", "Amount", "float").range(0.0, 1.0, 0.0001).slider(),
            opt("passthrough_alpha", "Keep transparency", "bool"),
        ],
        "scroll_filter" => vec![
            opt("speed_x", "Sideways", "float").range(-500.0, 500.0, 1.0).slider().unit(" px/s"),
            opt("speed_y", "Up or down", "float").range(-500.0, 500.0, 1.0).slider().unit(" px/s"),
            opt("limit_cx", "Limit width", "bool"),
            opt("cx", "Width", "int").range(1.0, 8192.0, 1.0).when("limit_cx", &[json!(true)]),
            opt("limit_cy", "Limit height", "bool"),
            opt("cy", "Height", "int").range(1.0, 8192.0, 1.0).when("limit_cy", &[json!(true)]),
            opt("loop", "Loop", "bool"),
        ],
        "gain_filter" => vec![opt("db", "Gain", "float").range(-30.0, 30.0, 0.1).slider().unit(" dB")],
        "noise_gate_filter" => vec![
            opt("close_threshold", "Close below", "float").range(-96.0, 0.0, 0.1).slider().unit(" dB"),
            opt("open_threshold", "Open above", "float").range(-96.0, 0.0, 0.1).slider().unit(" dB"),
            opt("attack_time", "Attack", "int").range(0.0, 10000.0, 1.0).unit(" ms"),
            opt("hold_time", "Hold", "int").range(0.0, 10000.0, 1.0).unit(" ms"),
            opt("release_time", "Release", "int").range(0.0, 10000.0, 1.0).unit(" ms"),
        ],
        "compressor_filter" => vec![
            opt("ratio", "Ratio", "float").range(1.0, 32.0, 0.5).slider().unit(":1"),
            opt("threshold", "Threshold", "float").range(-60.0, 0.0, 0.1).slider().unit(" dB"),
            opt("attack_time", "Attack", "int").range(1.0, 500.0, 1.0).unit(" ms"),
            opt("release_time", "Release", "int").range(1.0, 1000.0, 1.0).unit(" ms"),
            opt("output_gain", "Volume after", "float").range(-32.0, 32.0, 0.1).slider().unit(" dB"),
            opt("sidechain_source", "Duck under", "list")
                .choices(&[("None", json!("none"))])
                .hint("Lower this sound while another source plays, like your mic."),
        ],
        "limiter_filter" => vec![
            opt("threshold", "Threshold", "float").range(-60.0, 0.0, 0.1).slider().unit(" dB"),
            opt("release_time", "Release", "int").range(1.0, 1000.0, 1.0).unit(" ms"),
        ],
        "expander_filter" => vec![
            opt("presets", "Preset", "list").choices(&[("Expander", json!("expander")), ("Gate", json!("gate"))]),
            opt("ratio", "Ratio", "float").range(1.0, 20.0, 0.1).slider().unit(":1"),
            opt("threshold", "Threshold", "float").range(-60.0, 0.0, 0.1).slider().unit(" dB"),
            opt("attack_time", "Attack", "int").range(1.0, 100.0, 1.0).unit(" ms"),
            opt("release_time", "Release", "int").range(1.0, 1000.0, 1.0).unit(" ms"),
            opt("output_gain", "Volume after", "float").range(-32.0, 32.0, 0.1).slider().unit(" dB"),
            opt("detector", "Detection", "list").choices(&[("RMS", json!("RMS")), ("Peak", json!("peak"))]),
        ],
        "upward_compressor_filter" => vec![
            opt("ratio", "Ratio", "float").range(0.0, 1.0, 0.01).slider().unit(":1"),
            opt("threshold", "Threshold", "float").range(-60.0, 0.0, 0.1).slider().unit(" dB"),
            opt("attack_time", "Attack", "int").range(1.0, 100.0, 1.0).unit(" ms"),
            opt("release_time", "Release", "int").range(1.0, 1000.0, 1.0).unit(" ms"),
            opt("output_gain", "Volume after", "float").range(-32.0, 32.0, 0.1).slider().unit(" dB"),
            opt("knee_width", "Knee width", "float").range(0.0, 20.0, 0.5).slider().unit(" dB"),
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
        // Picked from the plugins found (`GET …/vst/plugins`); its state is the plugin's own.
        "ferrocast_vst" => Vec::new(),
        "noise_suppress_filter" | "noise_suppress_filter_v2" => {
            vec![opt("suppress_level", "How much", "int").range(-60.0, 0.0, 1.0).slider().unit(" dB")]
        }
        "ferrocast_rounded_corners" => {
            vec![opt("radius", "Radius", "int").range(0.0, 1000.0, 1.0).slider().unit(" px")]
        }
        "ferrocast_border" => vec![
            opt("width", "Width", "int").range(0.0, 200.0, 1.0).slider().unit(" px"),
            opt("color", "Colour", "color_alpha"),
            opt("radius", "Radius", "int").range(0.0, 1000.0, 1.0).slider().unit(" px"),
        ],
        "ferrocast_shadow" => vec![
            opt("x", "Move right", "int").range(-200.0, 200.0, 1.0).unit(" px"),
            opt("y", "Move down", "int").range(-200.0, 200.0, 1.0).unit(" px"),
            opt("blur", "Blur", "int").range(0.0, 200.0, 1.0).slider().unit(" px"),
            opt("color", "Colour", "color_alpha"),
            opt("outside", "Keep its size", "bool")
                .hint("On: the shadow spills outside its box. Off: the picture shrinks to make room."),
        ],
        "ferrocast_shape" => vec![
            opt("shape", "Shape", "list").choices(&[
                ("Circle", json!(0)),
                ("Hexagon", json!(1)),
                ("Diamond", json!(2)),
                ("Star", json!(3)),
                ("Heart", json!(4)),
            ]),
            opt("feather", "Soft edge", "int").range(0.0, 200.0, 1.0).slider().unit(" px"),
            opt("stretch", "Stretch to fill", "bool"),
        ],
        "ferrocast_blur_area" => vec![
            opt("mode", "Hide it with", "list").choices(&[("Blur", json!(0)), ("Pixels", json!(1))]),
            opt("strength", "Strength", "int").range(1.0, 200.0, 1.0).slider().unit(" px"),
            opt("area_x", "From the left", "int").range(0.0, 100.0, 1.0).slider().unit("%"),
            opt("area_y", "From the top", "int").range(0.0, 100.0, 1.0).slider().unit("%"),
            opt("area_w", "Width", "int").range(1.0, 100.0, 1.0).slider().unit("%"),
            opt("area_h", "Height", "int").range(1.0, 100.0, 1.0).slider().unit("%"),
        ],
        "ferrocast_look" => vec![
            opt("look", "Look", "list").choices(&[
                ("Warm", json!(0)),
                ("Cool", json!(1)),
                ("Teal and orange", json!(2)),
                ("Black and white", json!(3)),
                ("Vintage", json!(4)),
                ("Punchy", json!(5)),
                ("Soft fade", json!(6)),
            ]),
            opt("strength", "Strength", "int").range(0.0, 100.0, 1.0).slider().unit("%"),
        ],
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
        "ferrocast_video_delay" => json!({ "delay_ms": 200, "audio": false }),
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
            "align": "left", "bk_color": 0xFF00_0000u32, "bk_opacity": 0,
        }),
        "ffmpeg_source" => json!({
            "is_local_file": true,
            "local_file": "",
            "input": "",
            "input_format": "",
            "reconnect_delay_sec": 2,
            "ffmpeg_options": "",
            "looping": false,
            "restart_on_activate": true,
            "close_when_inactive": false,
            "clear_on_media_end": true,
            "hw_decode": true,
            "speed_percent": 100,
        }),
        "screen_capture" => json!({ "type": 0, "show_cursor": true, "application": "", "exclude_apps": [] }),
        "av_capture_input" | "av_capture_input_v2" => json!({ "device": "", "resolution": "", "fps": 0 }),
        "coreaudio_input_capture" => json!({ "device_id": "default" }),
        "sck_audio_capture" | "coreaudio_output_capture" => json!({ "type": 0, "application": "" }),
        "monitor_capture" => json!({ "monitor": 0, "capture_cursor": true }),
        "window_capture" => json!({ "cursor": true }),
        "dshow_input" => json!({ "video_device_id": "", "resolution": "", "fps": 0 }),
        "wasapi_input_capture" | "wasapi_output_capture" => json!({ "device_id": "default" }),
        "wasapi_process_output_capture" => json!({ "window": "" }),
        "xshm_input" => json!({ "screen": 0, "show_cursor": true }),
        "v4l2_input" => json!({ "device_id": "", "resolution": "", "fps": 0 }),
        "pulse_input_capture" | "pulse_output_capture" => json!({ "device_id": "default" }),
        "browser_source" | "ferrocast_web" => json!({
            "is_local_file": false, "local_file": "", "url": "https://obsproject.com/browser-source",
            "width": 800, "height": 600, "fps": 30,
            "css": "body { background-color: rgba(0, 0, 0, 0); margin: 0px auto; overflow: hidden; }",
            "reroute_audio": false,
        }),
        "chroma_key_filter" | "chroma_key_filter_v2" => {
            json!({ "key_color_type": "green", "similarity": 400, "smoothness": 80, "spill": 100, "opacity": 1.0, "contrast": 0.0, "brightness": 0.0, "gamma": 0.0 })
        }
        "color_key_filter" | "color_key_filter_v2" => {
            json!({ "key_color_type": "green", "similarity": 80, "smoothness": 50, "opacity": 1.0, "contrast": 0.0, "brightness": 0.0, "gamma": 0.0 })
        }
        "ferrocast_adjust" => json!({
            "exposure": 0.0, "contrast": 0, "highlights": 0, "shadows": 0,
            "saturation": 0, "vibrance": 0, "temperature": 0, "tint": 0,
        }),
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
        "ferrocast_deepfilter" => json!({ "strength": 40 }),
        "ferrocast_vst" => json!({ "path": "", "cid": "", "name": "", "state": "" }),
        "noise_suppress_filter" | "noise_suppress_filter_v2" => json!({ "method": "rnnoise", "suppress_level": -30 }),
        "ferrocast_remove_background" => json!({ "mode": 0, "soft": 10, "blur": 24 }),
        "noise_gate_filter" => {
            json!({ "close_threshold": -32.0, "open_threshold": -26.0, "attack_time": 25, "hold_time": 200, "release_time": 150 })
        }
        "compressor_filter" => {
            json!({
                "ratio": 10.0,
                "threshold": -18.0,
                "attack_time": 6,
                "release_time": 60,
                "output_gain": 0.0,
                "sidechain_source": "none"
            })
        }
        "limiter_filter" => json!({ "threshold": -6.0, "release_time": 60 }),
        "expander_filter" => {
            json!({ "presets": "expander", "ratio": 2.0, "threshold": -40.0, "attack_time": 10, "release_time": 50, "output_gain": 0.0, "detector": "RMS" })
        }
        "upward_compressor_filter" => {
            json!({ "ratio": 0.5, "threshold": -20.0, "attack_time": 10, "release_time": 50, "output_gain": 0.0, "knee_width": 10.0 })
        }
        "basic_eq_filter" => json!({ "low": 0.0, "mid": 0.0, "high": 0.0 }),
        "invert_polarity_filter" => json!({}),
        "ferrocast_rounded_corners" => json!({ "radius": 24 }),
        "ferrocast_border" => json!({ "width": 6, "color": 0xFFFF_FFFFu32, "radius": 0 }),
        "ferrocast_shadow" => json!({ "x": 0, "y": 8, "blur": 24, "color": 0x8000_0000u32 }),
        "ferrocast_shape" => json!({ "shape": 0, "feather": 0, "stretch": false }),
        "ferrocast_blur_area" => {
            json!({ "mode": 0, "strength": 48, "area_x": 25, "area_y": 25, "area_w": 50, "area_h": 50 })
        }
        "ferrocast_look" => json!({ "look": 0, "strength": 100 }),
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

    #[test]
    fn a_camera_is_offered_only_the_modes_it_has() {
        // The owner's USB card: 2560×1440 at 30 at most, 1360×768 at 60, no 1080p60.
        let card = [
            (2560, 1440, 30.0),
            (1920, 1080, 30.0),
            (1360, 768, 60.0),
            (1280, 720, 60.0),
            (640, 480, 60.0),
            (160, 120, 30.0),
        ];
        let (sizes, rates) = real_size_rate(&card, None, 0, None);
        let labels: Vec<&str> = sizes.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Its own", "2560×1440", "1920×1080", "1360×768", "1280×720", "640×480"]);
        assert_eq!(sizes[1].1, json!("2560x1440"));
        // 16:9 first by size, then the rest; no squeezed or very wide modes.
        let odd = [
            (1920, 2160, 30.0),
            (2560, 1600, 30.0),
            (1280, 720, 60.0),
            (1280, 1024, 60.0),
            (4096, 1080, 30.0),
            (1280, 720, 30.0),
        ];
        let (odd_sizes, _) = real_size_rate(&odd, None, 0, None);
        let labels: Vec<&str> = odd_sizes.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Its own", "1280×720", "2560×1600"], "5:4 and 4096×1080 left out");
        let labels: Vec<&str> = rates.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Its own", "24", "25", "30", "50 · 1360×768", "60 · 1360×768"], "smaller to reach it");
        let rates: Vec<Value> = rates.into_iter().map(|(_, v)| v).collect();
        assert_eq!(rates, [json!(0), json!(24), json!(25), json!(30), json!(50), json!(60)]);
        // 1080p only goes to 30 on it.
        let (_, at) = real_size_rate(&card, Some((1920, 1080)), 0, None);
        assert_eq!(at.last().map(|(_, v)| v.clone()), Some(json!(30)));
        // A saved 4K and 60 it can't do stay listed.
        let (sizes, at) = real_size_rate(&card, Some((3840, 2160)), 60, None);
        assert!(sizes.iter().any(|(_, v)| v == "3840x2160"));
        assert!(at.iter().any(|(_, v)| *v == json!(60)));
        // A new size that can't reach the saved rate takes the best it can.
        // Its own 1080p: rates it reaches there stay plain; 60 says where it goes.
        let (_, at) = real_size_rate(&card, None, 0, Some((1920, 1080)));
        let labels: Vec<&str> = at.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Its own", "24", "25", "30", "50 · 1360×768", "60 · 1360×768"]);
        assert_eq!(reachable_rate(&card, Some((2560, 1440)), 50), Some(30));
        assert_eq!(reachable_rate(&card, Some((1280, 720)), 50), None);
        assert_eq!(reachable_rate(&card, None, 60), None);
        assert_eq!(reachable_rate(&card, Some((2560, 1440)), 0), None);
        // 1440p goes to 30 here: a saved 50 says so.
        let (_, at) = real_size_rate(&card, Some((2560, 1440)), 50, None);
        assert!(at.iter().any(|(l, v)| *v == json!(50) && l == "50 (this size goes to 30)"));
    }

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
                        // A shadow's `outside` is set when one is added, not a default: an older
                        // one without it keeps fitting into its box.
                        || matches!(o.key.as_str(), "text_file" | "display" | "window" | "key_color" | "from_file" | "outside"),
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
            // A mask and a LUT need their picture first, a VST its plugin.
            effects.iter().all(|s| s.contains("mask_filter") || s.contains("clut_filter") || s.contains("VST plugin")),
            "left out: {effects:?}"
        );
    }

    #[test]
    fn device_list_cache_refreshes_immediately_when_hotplug_epoch_changes() {
        let mut fetches = 0u32;
        let _ = options_cached_with("av_capture_input_v2", 10, |_| {
            fetches += 1;
            Some(Vec::new())
        });
        assert_eq!(fetches, 1);

        // Same hotplug epoch hits the cache without re-querying AVFoundation.
        let _ = options_cached_with("av_capture_input_v2", 10, |_| {
            fetches += 1;
            Some(Vec::new())
        });
        assert_eq!(fetches, 1);

        // Continuity Camera appear/disappear bumps the epoch -> immediate refresh before FRESH expires.
        let _ = options_cached_with("av_capture_input_v2", 11, |_| {
            fetches += 1;
            Some(Vec::new())
        });
        assert_eq!(fetches, 2);
    }
}
