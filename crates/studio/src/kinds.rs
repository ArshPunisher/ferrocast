//! The kinds of source, filter and transition Ferrocast's own engine runs, and each one's
//! settings form, with no libobs: the same ids and settings keys OBS uses (so collections go
//! both ways), drawn by the app's forms as libobs's properties were.

use serde_json::{Value, json};

use crate::{Kind, Kinds, OptionInfo};

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
    ];
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
        "ferrocast_shader" => vec![
            opt("shader_file", "Shader File", "path").file("WGSL Shaders (*.wgsl);;All Files (*.*)"),
            opt("shader_text", "Or Write One Here", "text").multiline().mono(),
            opt("param1", "Slider 1", "float").range(0.0, 1.0, 0.01).slider(),
            opt("param2", "Slider 2", "float").range(0.0, 1.0, 0.01).slider(),
            opt("param3", "Slider 3", "float").range(0.0, 1.0, 0.01).slider(),
            opt("param4", "Slider 4", "float").range(0.0, 1.0, 0.01).slider(),
        ],
        "slideshow" | "slideshow_v2" | "image_slide_show" => vec![
            opt("files", "Image Files", "editable_list").files(IMAGES),
            opt("slide_time", "Time Between Slides", "int").range(50.0, 3_600_000.0, 50.0).unit(" ms"),
            opt("transition", "Transition", "list").choices(&[("Fade", json!("fade")), ("Cut", json!("cut"))]),
            opt("transition_speed", "Transition Speed", "int").range(0.0, 3600000.0, 50.0).unit(" ms"),
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
            opt("text", "Text", "text").multiline(),
            opt("from_file", "Read from file", "bool"),
            opt("text_file", "Text File (UTF-8 or UTF-16)", "path").file("Text Files (*.txt);;All Files (*.*)"),
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
                let d: Vec<(String, Value)> = displays
                    .iter()
                    .enumerate()
                    .map(|(i, d)| (format!("Display {} ({}x{})", i + 1, d.width, d.height), json!(d.id)))
                    .collect();
                let w: Vec<(String, Value)> = windows
                    .iter()
                    .filter(|w| !w.title.is_empty())
                    .map(|w| (format!("[{}] {}", w.app, w.title), json!(w.id)))
                    .collect();
                v.push(opt("display", "Display", "list").choices(&as_refs(&d)));
                v.push(opt("window", "Window", "list").choices(&as_refs(&w)));
            }
            #[cfg(target_os = "macos")]
            v.push(opt("application", "Application", "list").choices(&as_refs(&app_choices())));
            v.push(opt("show_cursor", "Show Cursor", "bool"));
            v
        }
        "av_capture_input" | "av_capture_input_v2" => {
            #[allow(unused_mut)]
            let mut list: Vec<(String, Value)> = Vec::new();
            #[cfg(target_os = "macos")]
            list.extend(ferrocore::camera::cameras().into_iter().map(|d| (d.name, json!(d.id))));
            let l: Vec<(&str, Value)> = list.iter().map(|(a, b)| (a.as_str(), b.clone())).collect();
            vec![opt("device", "Device", "list").choices(&l)]
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
            v.push(opt("application", "Application", "list").choices(&as_refs(&app_choices())));
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
            vec![opt("video_device_id", "Device", "list").choices(&as_refs(&list))]
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
            vec![opt("device_id", "Device", "list").choices(&as_refs(&list))]
        }
        "pulse_input_capture" => {
            #[allow(unused_mut)]
            let mut list: Vec<(String, Value)> = vec![("Default".into(), json!("default"))];
            #[cfg(target_os = "linux")]
            list.extend(ferrocore::camera::microphones().into_iter().map(|d| (d.name, json!(d.id))));
            vec![opt("device_id", "Device", "list").choices(&as_refs(&list))]
        }
        "pulse_output_capture" => vec![opt("device_id", "Device", "list").choices(&[("Default", json!("default"))])],
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
            opt("local_file", "Local file", "path").file("HTML Files (*.html *.htm);;All Files (*.*)"),
            opt("url", "URL", "text"),
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
            opt("key_color", "Key Color", "color"),
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
            opt("key_color", "Key Color", "color"),
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
            opt("right", "Right", "int").range(-8192.0, 8192.0, 1.0),
            opt("bottom", "Bottom", "int").range(-8192.0, 8192.0, 1.0),
            opt("cx", "Width", "int").range(0.0, 8192.0, 1.0),
            opt("cy", "Height", "int").range(0.0, 8192.0, 1.0),
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
            opt("cx", "Width", "int").range(1.0, 8192.0, 1.0),
            opt("limit_cy", "Limit Height", "bool"),
            opt("cy", "Height", "int").range(1.0, 8192.0, 1.0),
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
            opt("blur", "Blur", "int").range(2.0, 100.0, 1.0).slider().unit(" px"),
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
        "color_source" | "color_source_v2" | "color_source_v3" => {
            json!({ "color": 0xFFD1_D1D1u32, "width": 0, "height": 0 })
        }
        "image_source" => json!({ "file": "", "unload": false, "linear_alpha": false }),
        "ndi_source" => json!({ "ndi_source_name": "" }),
        "ferrocast_shader" => json!({
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

#[cfg(test)]
mod tests {
    use super::*;

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
