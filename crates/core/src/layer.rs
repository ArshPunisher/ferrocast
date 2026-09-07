//! Static overlay layers (image, text, shape, frame, webcam frame, brand mark), the style a
//! widget design gives a live widget, and the small value checks designs share: colours,
//! media references, font names and custom CSS.
//!
//! Every check returns a whole sentence that names the field, because the dashboard shows
//! it as is (API.md §1).

use serde_json::{Map, Value};

/// Longest custom CSS a design or layer may carry.
pub const MAX_CSS: usize = 20_000;
/// Longest text on a text layer.
pub const MAX_LAYER_TEXT: usize = 500;

pub(crate) type Check = Result<(), String>;

/// `#rgb`, `#rrggbb`, `#rrggbbaa` or `transparent`. Nothing that could break out of CSS.
pub fn is_color(s: &str) -> bool {
    if s == "transparent" {
        return true;
    }
    let Some(hex) = s.strip_prefix('#') else { return false };
    matches!(hex.len(), 3 | 4 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn color(path: &str, s: &str) -> Check {
    if is_color(s) { Ok(()) } else { Err(format!("{path}: \"{}\" isn't a colour; use #rrggbb.", clip(s))) }
}

/// A media id from the library (letters and digits) or a file bundled with the app
/// (`asset:alerts/coin.png`).
pub fn is_media_ref(s: &str) -> bool {
    if let Some(path) = s.strip_prefix("asset:") {
        return !path.is_empty()
            && path.len() <= 200
            && path.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"/_.-".contains(&b))
            && !path.contains("..")
            && !path.contains("//")
            && !path.starts_with('/');
    }
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub(crate) fn media_ref(path: &str, s: &str) -> Check {
    if is_media_ref(s) { Ok(()) } else { Err(format!("{path}: \"{}\" isn't a file from your media library.", clip(s))) }
}

/// A font family name: letters, digits, spaces and hyphens, 1 to 60 characters. Quotes and
/// semicolons can't get into the overlay's CSS this way.
pub fn is_font_family(s: &str) -> bool {
    let n = s.chars().count();
    (1..=60).contains(&n)
        && s.trim() == s
        && s.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '-')
        && !s.contains("  ")
}

pub(crate) fn font_family(path: &str, s: &str) -> Check {
    if is_font_family(s) {
        Ok(())
    } else {
        Err(format!("{path}: \"{}\" isn't a font name (letters, digits, spaces and hyphens).", clip(s)))
    }
}

/// Custom CSS stays inside the alert or widget it styles (Shadow DOM). It may not load
/// anything from the internet (that would tell a stranger when the stream is on), and old
/// browser hooks that run code are refused.
pub fn check_css(path: &str, css: &str) -> Check {
    if css.chars().count() > MAX_CSS {
        return Err(format!("{path}: custom CSS can be up to {MAX_CSS} characters."));
    }
    // Checked on what the browser will read: comments gone, escapes (`u\72l(`) decoded.
    let lower = normalize_css(css);
    if lower.contains("@import") {
        return Err(format!("{path}: @import isn't allowed; paste the CSS itself."));
    }
    for bad in ["expression(", "javascript:", "-moz-binding", "behavior:", "image-set(", "src("] {
        if lower.contains(bad) {
            return Err(format!("{path}: \"{bad}\" isn't allowed in custom CSS."));
        }
    }
    let mut rest = lower.as_str();
    while let Some(at) = rest.find("url(") {
        let inner = rest[at + 4..].trim_start().trim_start_matches(['"', '\'']);
        if !inner.starts_with("data:image/") {
            return Err(format!(
                "{path}: url() can only use data:image/… pictures; files from the internet aren't allowed."
            ));
        }
        rest = &rest[at + 4..];
    }
    Ok(())
}

/// Lowercase CSS with comments removed and backslash escapes decoded, the way a browser
/// tokenises it, so `u\72l(`, `@\69mport` or `ur\l(` can't sneak past the checks.
pub fn normalize_css(css: &str) -> String {
    // Comments are removed the way a browser reads them: a `/*` inside a quoted string is
    // text, not a comment, so `content:"/*"` can't hide a later `url(...)` from the check.
    let mut no_comments = String::with_capacity(css.len());
    let mut it = css.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = it.next() {
        if let Some(q) = quote {
            no_comments.push(c);
            if c == '\\' {
                no_comments.extend(it.next());
            } else if c == q || c == '\n' {
                quote = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            no_comments.push(c);
        } else if c == '/' && it.peek() == Some(&'*') {
            it.next();
            let mut prev = '\0';
            for n in it.by_ref() {
                if prev == '*' && n == '/' {
                    break;
                }
                prev = n;
            }
        } else {
            no_comments.push(c);
        }
    }
    let mut out = String::with_capacity(no_comments.len());
    let mut chars = no_comments.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
            hex.extend(chars.next());
        }
        if hex.is_empty() {
            // `\l` is just `l`; a backslash before a newline is a line continuation.
            if let Some(n) = chars.next()
                && n != '\n'
            {
                out.push(n);
            }
            continue;
        }
        if chars.peek().is_some_and(|w| w.is_whitespace()) {
            chars.next();
        }
        out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('\u{fffd}'));
    }
    out.to_lowercase()
}

fn clip(s: &str) -> String {
    s.chars().take(40).collect()
}

/// Reads `obj[key]` with a type check; `None` when missing or null.
fn opt<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

fn opt_str<'a>(obj: &'a Map<String, Value>, path: &str, key: &str) -> Result<Option<&'a str>, String> {
    match opt(obj, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("{path}.{key}: must be text.")),
    }
}

fn num_in(obj: &Map<String, Value>, path: &str, key: &str, lo: f64, hi: f64) -> Check {
    match opt(obj, key) {
        None => Ok(()),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() && (lo..=hi).contains(&n) => Ok(()),
            _ => Err(format!("{path}.{key}: must be a number from {lo} to {hi}.")),
        },
    }
}

fn one_of(obj: &Map<String, Value>, path: &str, key: &str, allowed: &[&str]) -> Check {
    match opt_str(obj, path, key)? {
        None => Ok(()),
        Some(s) if allowed.contains(&s) => Ok(()),
        Some(s) => Err(format!("{path}.{key}: \"{}\" isn't one of {}.", clip(s), allowed.join(", "))),
    }
}

fn opt_color(obj: &Map<String, Value>, path: &str, key: &str) -> Check {
    match opt_str(obj, path, key)? {
        None => Ok(()),
        Some(s) => color(&format!("{path}.{key}"), s),
    }
}

fn opt_bool(obj: &Map<String, Value>, path: &str, key: &str) -> Check {
    match opt(obj, key) {
        None | Some(Value::Bool(_)) => Ok(()),
        Some(_) => Err(format!("{path}.{key}: must be true or false.")),
    }
}

fn text_len(obj: &Map<String, Value>, path: &str, key: &str, max: usize) -> Check {
    match opt_str(obj, path, key)? {
        Some(s) if s.chars().count() > max => Err(format!("{path}.{key}: can be up to {max} characters.")),
        _ => Ok(()),
    }
}

pub const LAYER_TYPES: [&str; 6] = ["image", "text", "shape", "frame", "webcam", "brand"];
pub const SHADOWS: [&str; 4] = ["none", "soft", "hard", "glow"];
const LOOPS: [&str; 4] = ["none", "float", "pulse", "shimmer"];

/// Checks a `layer` widget's config.
pub fn validate(config: &Value) -> Check {
    let path = "config";
    let Some(obj) = config.as_object() else { return Err("config: must be an object.".into()) };
    let kind = opt_str(obj, path, "type")?.ok_or("config.type: say which kind of layer this is.")?;
    if !LAYER_TYPES.contains(&kind) {
        return Err(format!("config.type: \"{}\" isn't one of {}.", clip(kind), LAYER_TYPES.join(", ")));
    }
    match kind {
        "image" => {
            let m = opt_str(obj, path, "media")?.ok_or("config.media: pick a picture for this layer.")?;
            media_ref("config.media", m)?;
            one_of(obj, path, "fit", &["cover", "contain", "fill"])?;
            num_in(obj, path, "radius", 0.0, 2000.0)?;
        }
        "text" => {
            text_len(obj, path, "text", MAX_LAYER_TEXT)?;
            if let Some(f) = opt_str(obj, path, "font")? {
                font_family("config.font", f)?;
            }
            num_in(obj, path, "size", 6.0, 400.0)?;
            num_in(obj, path, "weight", 100.0, 900.0)?;
            opt_color(obj, path, "color")?;
            one_of(obj, path, "align", &["left", "center", "right"])?;
            opt_bool(obj, path, "uppercase")?;
            opt_bool(obj, path, "italic")?;
            num_in(obj, path, "letter_spacing", -0.2, 1.0)?;
            one_of(obj, path, "shadow", &SHADOWS)?;
            num_in(obj, path, "outline", 0.0, 20.0)?;
            opt_color(obj, path, "outline_color")?;
        }
        "shape" => {
            one_of(obj, path, "shape", &["rect", "ellipse", "line", "slant", "triangle"])?;
            opt_color(obj, path, "fill")?;
            opt_color(obj, path, "fill2")?;
            num_in(obj, path, "angle", 0.0, 360.0)?;
            opt_color(obj, path, "stroke")?;
            num_in(obj, path, "stroke_width", 0.0, 50.0)?;
            num_in(obj, path, "radius", 0.0, 2000.0)?;
        }
        "frame" | "webcam" => {
            one_of(obj, path, "style", &["line", "double", "corners", "notch", "glow"])?;
            opt_color(obj, path, "color")?;
            opt_color(obj, path, "accent")?;
            num_in(obj, path, "thickness", 1.0, 40.0)?;
            num_in(obj, path, "radius", 0.0, 400.0)?;
            text_len(obj, path, "label", 40)?;
        }
        _ => {
            one_of(obj, path, "variant", &["mark", "badge", "watermark"])?;
            opt_color(obj, path, "color")?;
        }
    }
    if let Some(anim) = opt(obj, "anim") {
        let a = anim.as_object().ok_or("config.anim: must be an object.")?;
        one_of(a, "config.anim", "in", &crate::design::ANIM_IN_NAMES)?;
        num_in(a, "config.anim", "delay_ms", 0.0, 10_000.0)?;
        one_of(a, "config.anim", "loop", &LOOPS)?;
    }
    if let Some(css) = opt_str(obj, path, "css")? {
        check_css("config.css", css)?;
    }
    Ok(())
}

/// Checks the `style` a widget design gives a live widget (goal bar, label, event list…).
pub fn validate_widget_style(style: &Value) -> Check {
    let path = "config.style";
    let Some(obj) = style.as_object() else { return Err("config.style: must be an object.".into()) };
    if let Some(v) = opt_str(obj, path, "variant")?
        && (v.is_empty() || v.len() > 24 || !v.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    {
        return Err("config.style.variant: use a short name in lowercase letters.".into());
    }
    if let Some(f) = opt_str(obj, path, "font")? {
        font_family("config.style.font", f)?;
    }
    for c in ["text", "accent", "background", "track", "muted"] {
        opt_color(obj, path, c)?;
    }
    num_in(obj, path, "radius", 0.0, 64.0)?;
    num_in(obj, path, "font_size", 8.0, 120.0)?;
    opt_bool(obj, path, "uppercase")?;
    one_of(obj, path, "brand", &crate::design::BRAND_SPOT_NAMES)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn colours_media_and_fonts_refuse_css_breakouts() {
        for ok in ["#fff", "#ffa424", "#ffa42480", "transparent", "#ABCD"] {
            assert!(is_color(ok), "{ok}");
        }
        for bad in ["", "red", "#ff", "#ggg", "#fff;}body{", "rgb(0,0,0)", "#ffa424 !important"] {
            assert!(!is_color(bad), "{bad}");
        }
        for ok in ["01hx2k3m4n5p6q7r8s9t0vwxyz", "asset:alerts/coin.png", "asset:themes/forge-1.webp"] {
            assert!(is_media_ref(ok), "{ok}");
        }
        for bad in [
            "",
            "../etc/passwd",
            "asset:../x",
            "asset:/abs",
            "asset:a//b",
            "asset:UP.png",
            "a b",
            "x".repeat(65).as_str(),
        ] {
            assert!(!is_media_ref(bad), "{bad}");
        }
        for ok in ["Bricolage Grotesque", "Press Start 2P", "Noto Sans Devanagari", "JetBrains-Mono"] {
            assert!(is_font_family(ok), "{ok}");
        }
        for bad in ["", " Lead", "A;B", "x'y", "a\"b", "Comic}body{color:red", "two  spaces", &"F".repeat(61)] {
            assert!(!is_font_family(bad), "{bad}");
        }
    }

    #[test]
    fn css_can_style_but_never_fetch() {
        assert!(check_css("css", ".al-title { color: red }").is_ok());
        assert!(check_css("css", "div { background: url(\"data:image/png;base64,AAAA\") }").is_ok());
        for bad in [
            "@import url(https://x.test/a.css);",
            "div{background:url(https://tracker.test/p.gif)}",
            "div{background:URL( //tracker.test/p.gif)}",
            "div{background:url('javascript:alert(1)')}",
            "div{width:expression(alert(1))}",
            "div{-moz-binding:url(x)}",
            "div{background:u\\72l(https://t.test/p.gif)}",
            "div{background:\\75 rl(//t.test/p.gif)}",
            "@\\69mport 'x.css';",
            "div{background:ur\\l(https://t.test)}",
            "div{background:image-set('https://t.test/a.png' 1x)}",
            "div{background:/* hi */url(https://t.test)}",
        ] {
            let e = check_css("look.css", bad).unwrap_err();
            assert!(e.starts_with("look.css:"), "{e}");
        }
        assert!(check_css("css", &"a".repeat(MAX_CSS + 1)).is_err());
    }

    #[test]
    fn a_comment_opener_inside_a_string_cannot_hide_a_fetch() {
        let sneaky = ".x{content:\"/*\"} .y{background:url(https://tracker.test/p.gif)} /* */";
        assert!(check_css("css", sneaky).is_err(), "the url() after the string is still seen");
        let single = ".x{content:'/*'} .y{background:url(//t.test/p.gif)}";
        assert!(check_css("css", single).is_err());
        // a real comment is still removed, and strings survive whole
        assert_eq!(normalize_css("a{/* hi */color:red}"), "a{color:red}");
        assert_eq!(normalize_css(".x{content:\"/* not a comment */\"}"), ".x{content:\"/* not a comment */\"}");
        assert_eq!(normalize_css(".x{content:\"a\\\"/*\"}"), ".x{content:\"a\"/*\"}");
    }

    #[test]
    fn every_layer_kind_checks_its_own_fields() {
        let ok = [
            json!({ "type": "image", "media": "asset:themes/forge.webp", "fit": "cover", "radius": 12 }),
            json!({ "type": "text", "text": "Starting soon", "font": "Bricolage Grotesque", "size": 96, "weight": 800,
                    "color": "#eef0f4", "align": "center", "shadow": "soft", "anim": { "in": "rise", "delay_ms": 300, "loop": "none" } }),
            json!({ "type": "shape", "shape": "slant", "fill": "#d9142f", "fill2": "#7a0a1a", "angle": 90, "stroke": null }),
            json!({ "type": "frame", "style": "notch", "color": "#ffa424", "thickness": 4, "radius": 16, "label": "LIVE" }),
            json!({ "type": "webcam", "style": "corners", "color": "#33d4ff" }),
            json!({ "type": "brand", "variant": "badge" }),
        ];
        for c in ok {
            validate(&c).unwrap_or_else(|e| panic!("{c}: {e}"));
        }
        let bad = [
            (json!({}), "config.type"),
            (json!({ "type": "iframe" }), "config.type"),
            (json!({ "type": "image" }), "config.media"),
            (json!({ "type": "image", "media": "https://x.test/a.png" }), "config.media"),
            (json!({ "type": "text", "text": "x".repeat(501) }), "config.text"),
            (json!({ "type": "text", "color": "red;}" }), "config.color"),
            (json!({ "type": "text", "font": "a'b" }), "config.font"),
            (json!({ "type": "text", "size": 9999 }), "config.size"),
            (json!({ "type": "text", "size": "big" }), "config.size"),
            (json!({ "type": "shape", "shape": "star" }), "config.shape"),
            (json!({ "type": "frame", "label": "x".repeat(41) }), "config.label"),
            (json!({ "type": "brand", "variant": "huge" }), "config.variant"),
            (json!({ "type": "text", "anim": { "in": "explode" } }), "config.anim.in"),
            (json!({ "type": "text", "anim": "rise" }), "config.anim"),
            (json!({ "type": "text", "css": "@import url(x)" }), "config.css"),
            (json!({ "type": "text", "uppercase": "yes" }), "config.uppercase"),
        ];
        for (c, field) in bad {
            let e = validate(&c).unwrap_err();
            assert!(e.starts_with(field), "{c} -> {e}");
            assert!(e.ends_with('.'), "whole sentence: {e}");
        }
    }

    #[test]
    fn widget_styles_are_checked() {
        validate_widget_style(&json!({ "variant": "segments", "font": "Hanken Grotesk", "text": "#fff",
            "accent": "#ffa424", "track": "#16191e", "radius": 12, "font_size": 22, "brand": "corner" }))
        .unwrap();
        assert!(
            validate_widget_style(&json!({ "variant": "Big Bar" })).unwrap_err().starts_with("config.style.variant")
        );
        assert!(validate_widget_style(&json!({ "accent": "url(x)" })).is_err());
        assert!(validate_widget_style(&json!({ "brand": "everywhere" })).is_err());
        assert!(validate_widget_style(&json!("bar")).is_err());
    }
}
