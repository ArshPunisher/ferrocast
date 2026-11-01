//! Recording and replay file names from OBS's pattern ("%CCYY-%MM-%DD %hh-%mm-%ss"), as
//! libobs's `os_generate_formatted_filename` makes them, without libobs.

use chrono::{DateTime, Local};

/// OBS's own short forms and the strftime each stands for; the plain strftime ones after.
const SPECS: [(&str, &str); 23] = [
    ("%CCYY", "%Y"),
    ("%YY", "%y"),
    ("%MM", "%m"),
    ("%DD", "%d"),
    ("%hh", "%H"),
    ("%mm", "%M"),
    ("%ss", "%S"),
    ("%%", "%%"),
    ("%a", "%a"),
    ("%A", "%A"),
    ("%b", "%b"),
    ("%B", "%B"),
    ("%d", "%d"),
    ("%H", "%H"),
    ("%I", "%I"),
    ("%m", "%m"),
    ("%M", "%M"),
    ("%p", "%p"),
    ("%S", "%S"),
    ("%y", "%y"),
    ("%Y", "%Y"),
    ("%z", "%z"),
    ("%Z", "%Z"),
];

/// What the pattern can say about the video (`%FPS`, `%CRES`, `%ORES`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct NameVideo {
    pub fps: u32,
    pub canvas: (u32, u32),
    pub output: (u32, u32),
}

/// A file name from `pattern` at `now`, with `extension`; spaces become underscores unless
/// `spaces`. Characters no file name may hold are left out, as libobs leaves them.
pub(crate) fn file_name(
    pattern: &str,
    extension: &str,
    spaces: bool,
    video: NameVideo,
    now: DateTime<Local>,
) -> String {
    let mut out = String::new();
    let mut rest = pattern;
    while !rest.is_empty() {
        if let Some((spec, f)) = SPECS.iter().find(|(s, _)| rest.starts_with(s)) {
            out.push_str(&now.format(f).to_string());
            rest = &rest[spec.len()..];
            continue;
        }
        let extra = [
            ("%FPS", video.fps.to_string()),
            ("%CRES", format!("{}x{}", video.canvas.0, video.canvas.1)),
            ("%ORES", format!("{}x{}", video.output.0, video.output.1)),
            ("%VF", "NV12".to_string()),
            ("%s", now.timestamp().to_string()),
        ];
        if let Some((spec, v)) = extra.iter().find(|(s, _)| rest.starts_with(s)) {
            out.push_str(v);
            rest = &rest[spec.len()..];
            continue;
        }
        let mut chars = rest.chars();
        let c = chars.next().unwrap_or_default();
        // An unknown %… loses its %, as in libobs.
        if c != '%' && !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') && !c.is_control() {
            out.push(c);
        }
        rest = chars.as_str();
    }
    if !spaces {
        out = out.replace(' ', "_");
    }
    let mut name: String = out.chars().take(255).collect();
    if !extension.is_empty() {
        name.push('.');
        name.push_str(extension);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn names_follow_obs_patterns() {
        let Some(at) = Local.with_ymd_and_hms(2026, 11, 1, 9, 5, 7).single() else { return };
        let v = NameVideo { fps: 60, canvas: (1920, 1080), output: (1280, 720) };
        assert_eq!(file_name("%CCYY-%MM-%DD %hh-%mm-%ss", "mkv", true, v, at), "2026-11-01 09-05-07.mkv");
        assert_eq!(file_name("%CCYY-%MM-%DD %hh-%mm-%ss", "mp4", false, v, at), "2026-11-01_09-05-07.mp4");
        assert_eq!(
            file_name("Replay %YY%MM%DD %FPS %CRES to %ORES", "mp4", true, v, at),
            "Replay 261101 60 1920x1080 to 1280x720.mp4"
        );
        assert_eq!(file_name("a/b:c %Q 100%%", "mp4", true, v, at), "abc Q 100%.mp4");
    }
}
