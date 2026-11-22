//! What a scene collection from an imported `.overlay` pack may hold.
//!
//! The pack's JSON is a stranger's: after parsing it is rebuilt from an allowlist before it
//! reaches the Studio, which would open any file path, load any page address and start any
//! helper (a VST host, a shader, a network fetcher) the JSON names. So:
//!
//! - only known source and filter kinds stay (the rest are left out, with a note),
//! - each kind keeps only the settings it is known by, and values are plain numbers, text and
//!   booleans (no nested objects where none belong),
//! - a media path stays only when it names a file inside the pack (the parser's placeholder for
//!   that file), anything else is emptied,
//! - a browser source loads `http(s)` pages on the public internet only, never a local file, a
//!   `data:` page, another app on this computer or the local network, and its CSS may not
//!   load anything.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::overlay_parse::{ASSET_PLACEHOLDER_PREFIX, OVERLAY_PLACEHOLDER_PREFIX};

/// What was changed, for the preview's notes and the report of missing files.
#[derive(Default)]
pub(super) struct Outcome {
    pub notes: Vec<String>,
    /// File references that were not in the pack (emptied).
    pub missing_assets: Vec<String>,
    /// Source kinds left out.
    pub left_out_kinds: Vec<String>,
}

/// Source kinds an imported pack may have. A browser source is only a page (checked below); a
/// capture asks for a device the user picks. `vlc_source` plays any path or address, so it is out.
fn source_kind_ok(kind: &str) -> bool {
    matches!(
        kind,
        "scene"
            | "group"
            | "image_source"
            | "ffmpeg_source"
            | "slideshow"
            | "color_source"
            | "color_source_v3"
            | "text_ft2_source"
            | "text_ft2_source_v2"
            | "text_gdiplus"
            | "text_gdiplus_v2"
            | "text_gdiplus_v3"
            | "browser_source"
            | "ferrocast_web"
            | "macos-avcapture"
            | "macos-avcapture-fast"
            | "av_capture_input"
            | "av_capture_input_v2"
            | "dshow_input"
            | "v4l2_input"
            | "screen_capture"
            | "game_capture"
            | "xshm_input"
            | "xcomposite_input"
            | "display_capture"
            | "monitor_capture"
            | "window_capture"
            | "coreaudio_input_capture"
            | "coreaudio_output_capture"
            | "wasapi_input_capture"
            | "wasapi_output_capture"
            | "pulse_input_capture"
            | "pulse_output_capture"
            | "sck_audio_capture"
    )
}

/// Filters that only change the picture or sound they are on. Ferrocast's own helpers
/// (`ferrocast_vst` loads native code, `ferrocast_shader` runs a program, `ferrocast_web_text`
/// fetches addresses) are never brought in from a file.
fn filter_kind_ok(kind: &str) -> bool {
    matches!(
        kind,
        "color_filter"
            | "color_filter_v2"
            | "color_key_filter"
            | "color_key_filter_v2"
            | "chroma_key_filter"
            | "chroma_key_filter_v2"
            | "luma_key_filter"
            | "luma_key_filter_v2"
            | "crop_filter"
            | "scroll_filter"
            | "sharpness_filter"
            | "sharpness_filter_v2"
            | "scale_filter"
            | "gain_filter"
            | "invert_polarity_filter"
            | "noise_gate_filter"
            | "noise_suppress_filter"
            | "noise_suppress_filter_v2"
            | "compressor_filter"
            | "limiter_filter"
            | "expander_filter"
            | "upward_compressor_filter"
            | "async_delay_filter"
            | "gpu_delay"
            | "mask_filter"
            | "mask_filter_v2"
            | "clut_filter"
            | "hdr_tonemap_filter"
    )
}

fn transition_kind_ok(kind: &str) -> bool {
    matches!(
        kind,
        "fade_transition"
            | "cut_transition"
            | "swipe_transition"
            | "slide_transition"
            | "fade_to_color_transition"
            | "wipe_transition"
            | "obs_stinger_transition"
            | "stinger_transition"
    )
}

/// Setting names that hold a file in the pack (when they hold anything).
const PATH_KEYS: [&str; 8] =
    ["file", "local_file", "path", "image_file", "luma_path", "mask_path", "image_path", "clut_path"];

const MAX_TEXT: usize = 4000;
const MAX_NAME: usize = 200;

/// A setting name that could name a file, address or command, that isn't one of the known
/// ones handled by kind: dropped rather than guessed at.
fn risky_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    ["path", "file", "url", "uri", "input", "playlist", "command", "exec", "script", "shader", "source_file", "options"]
        .iter()
        .any(|w| k.contains(w))
}

struct Cx<'a> {
    n_assets: usize,
    resolve: &'a dyn Fn(&str) -> Option<usize>,
    out: Outcome,
    seen_kinds: BTreeSet<String>,
}

impl Cx<'_> {
    /// A file reference that may stay: the parser's placeholder for a file in the pack. A plain
    /// reference that names a pack file becomes its placeholder; anything else is emptied.
    fn pack_file(&mut self, raw: &str) -> String {
        let raw = raw.trim();
        if raw.is_empty() {
            return String::new();
        }
        if let Some(rest) = raw.strip_prefix(ASSET_PLACEHOLDER_PREFIX)
            && let Ok(idx) = rest.parse::<usize>()
            && idx < self.n_assets
        {
            return raw.to_string();
        }
        if let Some(idx) = (self.resolve)(raw) {
            return format!("{ASSET_PLACEHOLDER_PREFIX}{idx}");
        }
        if !raw.to_ascii_lowercase().starts_with("http") && !self.out.missing_assets.iter().any(|m| m == raw) {
            self.out.missing_assets.push(raw.chars().take(200).collect());
        }
        String::new()
    }

    fn note(&mut self, msg: String) {
        if self.out.notes.len() < 200 && !self.out.notes.contains(&msg) {
            self.out.notes.push(msg);
        }
    }

    fn left_out_kind(&mut self, kind: &str) {
        if self.seen_kinds.insert(kind.to_string()) {
            self.out.left_out_kinds.push(kind.to_string());
        }
    }
}

/// Whether `url` is a page a browser source may load: `http(s)` on a public host, no login in the
/// address. `localhost`, loopback, link-local (cloud metadata), private and shared ranges, and the
/// numeric spellings of those (`2130706433`, `0x7f.1`) are the user's own computer or network.
pub(super) fn public_page_url(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.len() > 2048 {
        return None;
    }
    let parsed = reqwest::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    // `host_str` is the address as a browser reads it: `2130706433` and `0x7f.1` are already
    // `127.0.0.1` here, and an IPv6 address comes in brackets.
    let host = parsed.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            if !public_v4(ip) {
                return None;
            }
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            let seg = ip.segments();
            let bad = ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || ip.to_ipv4_mapped().is_some_and(|v| !public_v4(v));
            if bad {
                return None;
            }
        }
        Err(_) => {
            let local = host == "localhost"
                || host.ends_with(".localhost")
                || host.ends_with(".local")
                || host.ends_with(".internal")
                || host.ends_with(".lan")
                || host.ends_with(".home.arpa")
                || !host.contains('.');
            if local {
                return None;
            }
        }
    }
    Some(url.to_string())
}

fn public_v4(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || o[0] >= 240)
}

/// A capture setting that says which window, application, display, device or process to take.
fn names_what_to_capture(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "window",
        "owner",
        "application",
        "app_",
        "_app",
        "device",
        "display",
        "monitor",
        "screen",
        "uuid",
        "bundle",
        "game",
        "executable",
        "process",
        "camera",
        "microphone",
        "_id",
        "id_",
    ]
    .iter()
    .any(|w| k.contains(w))
        || k == "id"
        || k == "app"
}

fn scalar_ok(v: &Value) -> bool {
    match v {
        Value::Bool(_) | Value::Number(_) => true,
        Value::String(s) => s.chars().count() <= MAX_TEXT,
        _ => false,
    }
}

/// The plain values of `src`, leaving out file, address and command-like names (those are
/// handled by kind) and anything in `skip`.
fn plain_settings(src: &Map<String, Value>, skip: &[&str]) -> Map<String, Value> {
    src.iter()
        .filter(|(k, v)| scalar_ok(v) && !skip.contains(&k.as_str()) && !risky_key(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn number_object(v: Option<&Value>) -> Option<Value> {
    let o = v?.as_object()?;
    let m: Map<String, Value> = o.iter().filter(|(_, v)| v.is_number()).map(|(k, v)| (k.clone(), v.clone())).collect();
    (!m.is_empty()).then_some(Value::Object(m))
}

fn clean_font(v: Option<&Value>) -> Option<Value> {
    let o = v?.as_object()?;
    let m: Map<String, Value> = o
        .iter()
        .filter(|(k, v)| matches!(k.as_str(), "face" | "size" | "flags" | "style") && scalar_ok(v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!m.is_empty()).then_some(Value::Object(m))
}

/// The settings of a filter or transition: plain values, and the pack's own files.
fn clean_generic(cx: &mut Cx, src: Option<&Value>) -> Value {
    let Some(o) = src.and_then(Value::as_object) else { return Value::Object(Map::new()) };
    let mut out = plain_settings(o, &PATH_KEYS);
    for k in PATH_KEYS {
        if let Some(raw) = o.get(k).and_then(Value::as_str) {
            let kept = cx.pack_file(raw);
            out.insert(k.into(), Value::String(kept));
        }
    }
    Value::Object(out)
}

fn clean_items(items: &Value, left_out: &BTreeSet<String>) -> Value {
    let list = items.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut out = Vec::new();
    for it in list.iter().take(5000) {
        let Some(o) = it.as_object() else { continue };
        if o.get("name").and_then(Value::as_str).is_some_and(|n| left_out.contains(n)) {
            continue;
        }
        let mut m = plain_settings(o, &[]);
        for k in ["pos", "scale", "bounds"] {
            if let Some(v) = number_object(o.get(k)) {
                m.insert(k.into(), v);
            }
        }
        out.push(Value::Object(m));
    }
    Value::Array(out)
}

fn clean_settings(cx: &mut Cx, name: &str, kind: &str, raw: Option<&Value>, left_out: &BTreeSet<String>) -> Value {
    let empty = Map::new();
    let o = raw.and_then(Value::as_object).unwrap_or(&empty);
    let mut out;
    match kind {
        "scene" | "group" => {
            out = plain_settings(o, &[]);
            if let Some(items) = o.get("items") {
                out.insert("items".into(), clean_items(items, left_out));
            }
            if let Some(v) = number_object(o.get("custom_size")) {
                out.insert("custom_size".into(), v);
            }
        }
        "browser_source" | "ferrocast_web" => {
            out = plain_settings(o, &["css", "is_local_file", "local_file"]);
            out.insert("is_local_file".into(), Value::Bool(false));
            let url = o.get("url").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let own_overlay = url.starts_with(OVERLAY_PLACEHOLDER_PREFIX)
                && url.len() < 80
                && url[OVERLAY_PLACEHOLDER_PREFIX.len()..]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            let had_address = !url.is_empty() || o.get("is_local_file").and_then(Value::as_bool) == Some(true);
            let kept = if own_overlay { url } else { public_page_url(&url).unwrap_or_default() };
            if kept.is_empty() && had_address {
                cx.note(format!(
                    "“{name}” is a browser source whose page isn't a public web page (a local file, another app on this computer or network, or not http/https), so its address was left empty."
                ));
            }
            out.insert("url".into(), Value::String(kept));
            if let Some(css) = o.get("css").and_then(Value::as_str) {
                if ferrocast_core::layer::check_css("css", css).is_ok() {
                    out.insert("css".into(), Value::String(css.to_string()));
                } else {
                    out.insert("css".into(), Value::String(String::new()));
                    cx.note(format!("“{name}”: its custom CSS loads things from the internet, so it was left out."));
                }
            }
        }
        "ffmpeg_source" => {
            // Always a file in the pack: a stream address or a path on this computer is never opened.
            out = plain_settings(o, &["is_local_file", "local_file", "input", "ffmpeg_options"]);
            out.insert("is_local_file".into(), Value::Bool(true));
            let file = cx.pack_file(o.get("local_file").and_then(Value::as_str).unwrap_or(""));
            out.insert("local_file".into(), Value::String(file));
        }
        "image_source" => {
            out = plain_settings(o, &["file"]);
            let file = cx.pack_file(o.get("file").and_then(Value::as_str).unwrap_or(""));
            out.insert("file".into(), Value::String(file));
        }
        "slideshow" => {
            out = plain_settings(o, &["files"]);
            let mut files = Vec::new();
            for f in o.get("files").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default().iter().take(500) {
                let raw = f.get("value").and_then(Value::as_str).or_else(|| f.as_str()).unwrap_or("");
                let kept = cx.pack_file(raw);
                if !kept.is_empty() {
                    let mut m = Map::new();
                    m.insert("value".into(), Value::String(kept));
                    for k in ["hidden", "selected"] {
                        if let Some(b) = f.get(k).and_then(Value::as_bool) {
                            m.insert(k.into(), Value::Bool(b));
                        }
                    }
                    files.push(Value::Object(m));
                }
            }
            out.insert("files".into(), Value::Array(files));
        }
        "text_ft2_source" | "text_ft2_source_v2" | "text_gdiplus" | "text_gdiplus_v2" | "text_gdiplus_v3" => {
            // Text is shown as it is: never read from a file on this computer.
            out = plain_settings(o, &["font"]);
            out.insert("from_file".into(), Value::Bool(false));
            out.insert("read_from_file".into(), Value::Bool(false));
            out.remove("from_file");
            out.remove("read_from_file");
            if let Some(f) = clean_font(o.get("font")) {
                out.insert("font".into(), f);
            }
        }
        "color_source" | "color_source_v3" => {
            out = plain_settings(o, &[])
                .into_iter()
                .filter(|(k, _)| matches!(k.as_str(), "color" | "width" | "height"))
                .collect();
        }
        _ => {
            // Captures and audio: what to capture is the user's to pick after the import. A pack
            // that names a window, app, display or device (`owner_name: "1Password"`) would put
            // it on air as soon as the scene is, so those settings are dropped; harmless ones
            // (cursor, preset) stay.
            let all = plain_settings(o, &[]);
            let before = all.len();
            out = all.into_iter().filter(|(k, _)| !names_what_to_capture(k)).collect();
            if out.len() < before {
                cx.note(format!(
                    "“{name}” will ask which window, display or device to use: the pack's choice isn't kept."
                ));
            }
        }
    }
    Value::Object(out)
}

fn clean_filters(cx: &mut Cx, owner: &str, raw: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    for f in raw.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default().iter().take(50) {
        let Some(o) = f.as_object() else { continue };
        let id = o.get("id").or_else(|| o.get("type")).and_then(Value::as_str).unwrap_or("");
        if !filter_kind_ok(id) {
            cx.left_out_kind(id);
            cx.note(format!(
                "Left out a “{id}” filter on “{owner}”: this kind of filter isn't allowed in an imported pack."
            ));
            continue;
        }
        let mut m = Map::new();
        let name = o.get("name").and_then(Value::as_str).unwrap_or(id);
        m.insert("name".into(), Value::String(name.chars().take(MAX_NAME).collect()));
        m.insert("id".into(), Value::String(id.to_string()));
        m.insert("versioned_id".into(), Value::String(id.to_string()));
        m.insert("enabled".into(), Value::Bool(o.get("enabled").and_then(Value::as_bool).unwrap_or(true)));
        m.insert("settings".into(), clean_generic(cx, o.get("settings")));
        out.push(Value::Object(m));
    }
    out
}

/// Rebuilds `doc` (an OBS-style scene collection from a pack) so it holds only what is allowed
/// above. `resolve` finds a pack file by the reference a source used; `n_assets` is how many
/// files the pack has (the parser's placeholders count from 0).
pub(super) fn make_safe(doc: &mut Value, n_assets: usize, resolve: &dyn Fn(&str) -> Option<usize>) -> Outcome {
    let mut cx = Cx { n_assets, resolve, out: Outcome::default(), seen_kinds: BTreeSet::new() };
    let Some(root) = doc.as_object() else { return cx.out };

    // Which sources are left out, first: scenes mustn't keep items for them.
    let mut left_out = BTreeSet::new();
    let list = root.get("sources").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for s in list {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("");
        if name.chars().count() > MAX_NAME && source_kind_ok(s.get("id").and_then(Value::as_str).unwrap_or("")) {
            // Not shortened (scene items refer to it by name): left out, with its scene items.
            left_out.insert(name.to_string());
            let short: String = name.chars().take(40).collect();
            cx.note(format!("Left out “{short}…”: its name is longer than {MAX_NAME} characters."));
        }
    }
    for s in list {
        let kind = s.get("id").and_then(Value::as_str).unwrap_or("");
        let name = s.get("name").and_then(Value::as_str).unwrap_or("");
        if !source_kind_ok(kind) {
            left_out.insert(name.to_string());
            cx.left_out_kind(kind);
            let shown = if kind.is_empty() { "(no kind)" } else { kind };
            cx.note(format!("Left out “{name}”: sources of kind “{shown}” aren't allowed in an imported pack."));
        }
    }

    let mut sources = Vec::new();
    for s in list {
        let Some(o) = s.as_object() else { continue };
        let kind = o.get("id").and_then(Value::as_str).unwrap_or("");
        let name = o.get("name").and_then(Value::as_str).unwrap_or("");
        if !source_kind_ok(kind) || left_out.contains(name) {
            continue;
        }
        let mut m = plain_settings(o, &["id", "versioned_id", "name"]);
        m.insert("name".into(), Value::String(name.chars().take(MAX_NAME).collect()));
        m.insert("id".into(), Value::String(kind.to_string()));
        m.insert("versioned_id".into(), Value::String(kind.to_string()));
        m.insert("settings".into(), clean_settings(&mut cx, name, kind, o.get("settings"), &left_out));
        let filters = clean_filters(&mut cx, name, o.get("filters"));
        if !filters.is_empty() {
            m.insert("filters".into(), Value::Array(filters));
        }
        if let Some(mix) = number_object(o.get("mixers")) {
            m.insert("mixers".into(), mix);
        }
        sources.push(Value::Object(m));
    }

    let mut transitions = Vec::new();
    for t in root.get("transitions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default().iter().take(50) {
        let Some(o) = t.as_object() else { continue };
        let id = o.get("id").and_then(Value::as_str).unwrap_or("");
        if !transition_kind_ok(id) {
            cx.left_out_kind(id);
            cx.note(format!("Left out a “{id}” transition: this kind isn't allowed in an imported pack."));
            continue;
        }
        let mut m = plain_settings(o, &["id", "name"]);
        m.insert("id".into(), Value::String(id.to_string()));
        let tn = o.get("name").and_then(Value::as_str).unwrap_or(id);
        m.insert("name".into(), Value::String(tn.chars().take(MAX_NAME).collect()));
        m.insert("settings".into(), clean_generic(&mut cx, o.get("settings")));
        transitions.push(Value::Object(m));
    }

    let mut out = plain_settings(root, &[]);
    if let Some(order) = root.get("scene_order").and_then(Value::as_array) {
        let names: Vec<Value> = order
            .iter()
            .filter_map(|e| e.get("name").and_then(Value::as_str))
            .map(|n| serde_json::json!({ "name": n.chars().take(MAX_NAME).collect::<String>() }))
            .collect();
        out.insert("scene_order".into(), Value::Array(names));
    }
    out.insert("sources".into(), Value::Array(sources));
    out.insert("transitions".into(), Value::Array(transitions));
    *doc = Value::Object(out);
    cx.out
}

#[cfg(test)]
mod tests {
    use super::public_page_url;

    #[test]
    fn page_addresses() {
        for ok in
            ["https://example.com/a?b=1", "http://example.org", "https://8.8.8.8/x", "https://sub.example.co.uk:8443/p"]
        {
            assert!(public_page_url(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            "file:///etc/passwd",
            "https://localhost/",
            "https://printer.local/",
            "http://127.0.0.1/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://172.16.0.1/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://metadata/",
            "https://u:p@example.com/",
        ] {
            assert!(public_page_url(bad).is_none(), "{bad}");
        }
    }
}
