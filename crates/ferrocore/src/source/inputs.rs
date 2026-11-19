//! Keys and mouse buttons on stream (the OBS Input Overlay plugin, built in): the keys a layout
//! names, drawn as caps that light up while held. Looked at 60 times a second and drawn only
//! when something changes.
//!
//! Privacy by design: only the keys in the layout are ever looked at (never the whole keyboard,
//! so typing can't be read off the stream), nothing is kept, and nothing lights while a password
//! field has the keyboard: on a Mac any (Secure Input); on Windows a classic password box only
//! (Windows has nothing system-wide, so a browser's password field isn't seen there). A Mac asks for Input Monitoring first;
//! without it the caps show and never light.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Picture;
use crate::engine::Feed;
use crate::source::series::{Worker, spawn, wait};
use crate::source::text::{TextAlign, TextRenderer, TextStyle};

/// A key or mouse button the overlay can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Letter(char),
    Digit(char),
    Space,
    Shift,
    Ctrl,
    Alt,
    Tab,
    Esc,
    Enter,
    Up,
    Down,
    Left,
    Right,
    MouseLeft,
    MouseRight,
    MouseMiddle,
}

impl Key {
    /// Its name in a layout ("W", "Space", "LMB"), any case.
    pub fn parse(s: &str) -> Option<Key> {
        let up = s.to_ascii_uppercase();
        Some(match up.as_str() {
            "SPACE" => Key::Space,
            "SHIFT" => Key::Shift,
            "CTRL" | "CONTROL" => Key::Ctrl,
            "ALT" | "OPTION" => Key::Alt,
            "TAB" => Key::Tab,
            "ESC" | "ESCAPE" => Key::Esc,
            "ENTER" | "RETURN" => Key::Enter,
            "UP" | "↑" => Key::Up,
            "DOWN" | "↓" => Key::Down,
            "LEFT" | "←" => Key::Left,
            "RIGHT" | "→" => Key::Right,
            "LMB" | "MOUSE1" => Key::MouseLeft,
            "RMB" | "MOUSE2" => Key::MouseRight,
            "MMB" | "MOUSE3" => Key::MouseMiddle,
            _ => {
                let mut cs = up.chars();
                let (Some(c), None) = (cs.next(), cs.next()) else { return None };
                match c {
                    'A'..='Z' => Key::Letter(c),
                    '0'..='9' => Key::Digit(c),
                    _ => return None,
                }
            }
        })
    }

    /// What its cap says.
    pub fn label(self) -> String {
        match self {
            Key::Letter(c) | Key::Digit(c) => c.to_string(),
            Key::Space => "Space".into(),
            Key::Shift => "Shift".into(),
            Key::Ctrl => "Ctrl".into(),
            Key::Alt => "Alt".into(),
            Key::Tab => "Tab".into(),
            Key::Esc => "Esc".into(),
            Key::Enter => "Enter".into(),
            Key::Up => "↑".into(),
            Key::Down => "↓".into(),
            Key::Left => "←".into(),
            Key::Right => "→".into(),
            Key::MouseLeft => "L".into(),
            Key::MouseRight => "R".into(),
            Key::MouseMiddle => "M".into(),
        }
    }

    /// How many cap widths it takes.
    fn units(self) -> f32 {
        match self {
            Key::Space => 3.0,
            Key::Shift | Key::Ctrl | Key::Tab | Key::Enter => 1.5,
            _ => 1.0,
        }
    }
}

/// One place in a row: a key, or a gap one cap wide (`_`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    Key(Key),
    Gap,
}

/// The ready layouts, by name.
pub const LAYOUTS: &[(&str, &str, &str)] = &[
    ("wasd", "WASD and mouse", "_ W _ _ LMB RMB / A S D _ _ _ / Shift Space"),
    ("arrows", "Arrow keys", "_ Up _ / Left Down Right"),
    ("fps", "Shooter", "Q W E R / A S D F / Shift Ctrl Space"),
    ("mouse", "Mouse buttons", "LMB MMB RMB"),
];

/// The most keys a layout may name.
pub const MOST_KEYS: usize = 24;

/// Rows of caps from a layout ("_ W _ / A S D"): rows split by `/`, keys by spaces.
pub fn layout(text: &str) -> Result<Vec<Vec<Cap>>, String> {
    let mut rows = Vec::new();
    let mut keys = 0;
    for row in text.split('/') {
        let mut caps = Vec::new();
        for word in row.split_whitespace() {
            if word == "_" {
                caps.push(Cap::Gap);
                continue;
            }
            let k = Key::parse(word).ok_or_else(|| format!("\"{word}\" isn't a key the overlay knows"))?;
            keys += 1;
            caps.push(Cap::Key(k));
        }
        if !caps.is_empty() {
            rows.push(caps);
        }
    }
    if keys == 0 {
        return Err("the layout names no keys".into());
    }
    if keys > MOST_KEYS {
        return Err(format!("a layout shows up to {MOST_KEYS} keys"));
    }
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq)]
pub struct InputSpec {
    pub rows: Vec<Vec<Cap>>,
    /// A cap's side, pixels.
    pub cap: u32,
    /// Straight RGBA: a cap at rest, a cap held, its label.
    pub rest: [u8; 4],
    pub held: [u8; 4],
    pub text: [u8; 4],
}

/// Says whether keys are held now. The engine asks the computer; tests hand in their own.
pub trait Keys: Send + 'static {
    fn held(&mut self, k: Key) -> bool;
}

/// The keys of `spec` held now, in its order.
fn look(spec: &InputSpec, keys: &mut dyn Keys) -> Vec<bool> {
    spec.rows
        .iter()
        .flatten()
        .filter_map(|c| match c {
            Cap::Key(k) => Some(keys.held(*k)),
            Cap::Gap => None,
        })
        .collect()
}

/// The overlay's size for `spec`.
pub fn size(spec: &InputSpec) -> (u32, u32) {
    let cap = spec.cap.clamp(16, 256) as f32;
    let gap = (cap * 0.12).round();
    let widest = spec
        .rows
        .iter()
        .map(|r| r.iter().map(|c| if let Cap::Key(k) = c { k.units() } else { 1.0 }).sum::<f32>() * (cap + gap) - gap)
        .fold(cap, f32::max);
    let tall = spec.rows.len().max(1) as f32 * (cap + gap) - gap;
    (widest.ceil() as u32, tall.ceil() as u32)
}

fn fill_round(p: &mut Picture, x0: u32, y0: u32, w: u32, h: u32, r: u32, c: [u8; 4]) {
    let r = r.min(w / 2).min(h / 2) as i64;
    for y in 0..h {
        for x in 0..w {
            // Outside a corner's circle: left clear.
            let (dx, dy) = (
                (r - x as i64).max(x as i64 - (w as i64 - 1 - r)).max(0),
                (r - y as i64).max(y as i64 - (h as i64 - 1 - r)).max(0),
            );
            if dx * dx + dy * dy > r * r {
                continue;
            }
            let (px, py) = (x0 + x, y0 + y);
            if px >= p.width || py >= p.height {
                continue;
            }
            let i = ((py * p.width + px) * 4) as usize;
            p.rgba[i..i + 4].copy_from_slice(&c);
        }
    }
}

/// `src` (straight alpha) over `dst` at (x, y).
fn over(dst: &mut Picture, src: &Picture, x: i64, y: i64) {
    for sy in 0..src.height as i64 {
        for sx in 0..src.width as i64 {
            let (dx, dy) = (x + sx, y + sy);
            if dx < 0 || dy < 0 || dx >= dst.width as i64 || dy >= dst.height as i64 {
                continue;
            }
            let s = ((sy * src.width as i64 + sx) * 4) as usize;
            let d = ((dy * dst.width as i64 + dx) * 4) as usize;
            let a = f32::from(src.rgba[s + 3]) / 255.0;
            if a == 0.0 {
                continue;
            }
            let da = f32::from(dst.rgba[d + 3]) / 255.0;
            let out = a + da * (1.0 - a);
            for k in 0..3 {
                let c = (f32::from(src.rgba[s + k]) * a + f32::from(dst.rgba[d + k]) * da * (1.0 - a)) / out.max(1e-6);
                dst.rgba[d + k] = c.round().clamp(0.0, 255.0) as u8;
            }
            dst.rgba[d + 3] = (out * 255.0).round() as u8;
        }
    }
}

/// The caps drawn with `held` lit; `labels` are the keys' labels drawn once.
pub fn draw(spec: &InputSpec, held: &[bool], labels: &[Picture]) -> Picture {
    let (w, h) = size(spec);
    let mut p = Picture::solid(w, h, [0, 0, 0, 0]);
    let cap = spec.cap.clamp(16, 256) as f32;
    let gap = (cap * 0.12).round();
    let mut n = 0;
    for (ri, row) in spec.rows.iter().enumerate() {
        let mut x = 0.0f32;
        let y = ri as f32 * (cap + gap);
        for c in row {
            match c {
                Cap::Gap => x += cap + gap,
                Cap::Key(k) => {
                    let wide = k.units() * (cap + gap) - gap;
                    let lit = held.get(n).copied().unwrap_or(false);
                    fill_round(
                        &mut p,
                        x as u32,
                        y as u32,
                        wide as u32,
                        cap as u32,
                        (cap * 0.18) as u32,
                        if lit { spec.held } else { spec.rest },
                    );
                    if let Some(l) = labels.get(n) {
                        let lx = x as i64 + (wide as i64 - i64::from(l.width)) / 2;
                        let ly = y as i64 + (cap as i64 - i64::from(l.height)) / 2;
                        over(&mut p, l, lx, ly);
                    }
                    n += 1;
                    x += wide + gap;
                }
            }
        }
    }
    p
}

/// The computer's keyboard and mouse, as far as the layout's keys go.
#[derive(Debug)]
pub struct System;

// The system's own calls for key state (no Rust binding covers them): small and checked.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod mac {
    use super::Key;

    // CGEventSourceStateID: what the hardware says now.
    const HID: i32 = 1;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
    }
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn IsSecureEventInputEnabled() -> bool;
    }

    /// The Mac's virtual key code (US layout positions, as games use them).
    fn code(k: Key) -> Option<u16> {
        const LETTERS: [u16; 26] =
            [0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16, 6];
        const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
        Some(match k {
            Key::Letter(c) => LETTERS[(c as u8 - b'A') as usize],
            Key::Digit(c) => DIGITS[(c as u8 - b'0') as usize],
            Key::Space => 49,
            Key::Shift => 56,
            Key::Ctrl => 59,
            Key::Alt => 58,
            Key::Tab => 48,
            Key::Esc => 53,
            Key::Enter => 36,
            Key::Up => 126,
            Key::Down => 125,
            Key::Left => 123,
            Key::Right => 124,
            _ => return None,
        })
    }

    pub fn allowed() -> bool {
        // SAFETY: plain calls with no arguments.
        unsafe { CGPreflightListenEventAccess() }
    }

    pub fn ask() {
        // SAFETY: shows the system's prompt once; no arguments.
        unsafe {
            CGRequestListenEventAccess();
        }
    }

    pub fn held(k: Key) -> bool {
        // SAFETY: plain calls on numbers; a password field has the keyboard: nothing lights.
        unsafe {
            if IsSecureEventInputEnabled() {
                return false;
            }
            match k {
                Key::MouseLeft => CGEventSourceButtonState(HID, 0),
                Key::MouseRight => CGEventSourceButtonState(HID, 1),
                Key::MouseMiddle => CGEventSourceButtonState(HID, 2),
                _ => code(k).is_some_and(|c| CGEventSourceKeyState(HID, c)),
            }
        }
    }
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod win {
    use super::Key;

    #[repr(C)]
    #[derive(Default)]
    struct Rect {
        l: i32,
        t: i32,
        r: i32,
        b: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct GuiThreadInfo {
        size: u32,
        flags: u32,
        active: isize,
        focus: isize,
        capture: isize,
        menu_owner: isize,
        move_size: isize,
        caret: isize,
        caret_rect: Rect,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(vk: i32) -> i16;
        fn GetGUIThreadInfo(thread: u32, info: *mut GuiThreadInfo) -> i32;
        fn GetWindowLongW(window: isize, index: i32) -> i32;
    }

    /// A password box has the keyboard (a classic edit control with ES_PASSWORD): keys stay
    /// dark. Windows has no system-wide Secure Input like a Mac's, so a browser's password
    /// field isn't seen; the overlay's key list and its (i) say so.
    fn password_focused() -> bool {
        const GWL_STYLE: i32 = -16;
        const ES_PASSWORD: i32 = 0x20;
        let mut info = GuiThreadInfo { size: std::mem::size_of::<GuiThreadInfo>() as u32, ..Default::default() };
        // SAFETY: `info` is sized as the call wants; thread 0 is the foreground one.
        unsafe {
            GetGUIThreadInfo(0, &mut info) != 0
                && info.focus != 0
                && GetWindowLongW(info.focus, GWL_STYLE) & ES_PASSWORD != 0
        }
    }

    fn vk(k: Key) -> i32 {
        match k {
            Key::Letter(c) | Key::Digit(c) => c as i32,
            Key::Space => 0x20,
            Key::Shift => 0x10,
            Key::Ctrl => 0x11,
            Key::Alt => 0x12,
            Key::Tab => 0x09,
            Key::Esc => 0x1B,
            Key::Enter => 0x0D,
            Key::Up => 0x26,
            Key::Down => 0x28,
            Key::Left => 0x25,
            Key::Right => 0x27,
            Key::MouseLeft => 0x01,
            Key::MouseRight => 0x02,
            Key::MouseMiddle => 0x04,
        }
    }

    pub fn held(k: Key) -> bool {
        if password_focused() {
            return false;
        }
        // SAFETY: a plain call on a key code.
        unsafe { GetAsyncKeyState(vk(k)) as u16 & 0x8000 != 0 }
    }
}

impl Keys for System {
    fn held(&mut self, k: Key) -> bool {
        #[cfg(target_os = "macos")]
        return mac::allowed() && mac::held(k);
        #[cfg(target_os = "windows")]
        return win::held(k);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = k;
            false
        }
    }
}

/// On a Mac, asks for Input Monitoring once if it isn't allowed yet.
pub fn ask_once() {
    #[cfg(target_os = "macos")]
    {
        static ASKED: std::sync::Once = std::sync::Once::new();
        ASKED.call_once(|| {
            if !mac::allowed() {
                mac::ask();
            }
        });
    }
}

/// Watches the keys into `feed` until stopped, with the engine's `text` renderer.
pub(crate) fn input_overlay(
    spec: InputSpec,
    mut keys: Box<dyn Keys>,
    feed: Feed,
    text: Arc<Mutex<TextRenderer>>,
) -> Worker {
    spawn("ferrocast-input-overlay", move |stop: &AtomicBool| {
        let cap = spec.cap.clamp(16, 256) as f32;
        let labels: Vec<Picture> = {
            let mut t = text.lock().unwrap_or_else(|e| e.into_inner());
            spec.rows
                .iter()
                .flatten()
                .filter_map(|c| match c {
                    Cap::Key(k) => Some(*k),
                    Cap::Gap => None,
                })
                .map(|k| {
                    let small = matches!(k.label().chars().count(), 2..);
                    let style = TextStyle {
                        size: if small { cap * 0.3 } else { cap * 0.42 },
                        bold: true,
                        color: spec.text,
                        align: TextAlign::Center,
                        ..TextStyle::default()
                    };
                    t.render(&k.label(), &style)
                })
                .collect()
        };
        let mut shown: Option<Vec<bool>> = None;
        loop {
            let now = look(&spec, keys.as_mut());
            if shown.as_ref() != Some(&now) {
                feed.picture(draw(&spec, &now, &labels));
                shown = Some(now);
            }
            if !wait(stop, Duration::from_millis(16)) {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layout_reads_its_keys_and_refuses_others() {
        let rows = layout("_ W _ / a s d / Shift Space LMB").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec![Cap::Gap, Cap::Key(Key::Letter('W')), Cap::Gap]);
        assert_eq!(rows[1][0], Cap::Key(Key::Letter('A')), "any case");
        assert_eq!(rows[2], vec![Cap::Key(Key::Shift), Cap::Key(Key::Space), Cap::Key(Key::MouseLeft)]);
        for (_, _, l) in LAYOUTS {
            assert!(layout(l).is_ok(), "{l}");
        }
        assert!(layout("W @").is_err_and(|e| e.contains("\"@\"")));
        assert!(layout("F13").is_err(), "only the keys it knows");
        assert!(layout(" / _ ").is_err_and(|e| e.contains("no keys")));
        assert!(layout("").is_err());
        let many = vec!["A"; MOST_KEYS + 1].join(" ");
        assert!(layout(&many).is_err_and(|e| e.contains("up to")), "never the whole keyboard");
    }

    fn spec(text: &str) -> InputSpec {
        InputSpec {
            rows: layout(text).unwrap_or_else(|e| panic!("{e}")),
            cap: 50,
            rest: [40, 40, 40, 200],
            held: [255, 90, 31, 255],
            text: [255, 255, 255, 255],
        }
    }

    #[test]
    fn held_keys_light_up() {
        let s = spec("A _ Space");
        let (w, h) = size(&s);
        // 1 + 1 (gap) + 3 caps of 50 with gaps of 6.
        assert_eq!((w, h), (5 * 56 - 6, 50));
        let p = draw(&s, &[true, false], &[]);
        let at = |x: u32, y: u32| p.rgba[((y * p.width + x) * 4) as usize..][..4].to_vec();
        assert_eq!(at(25, 25), vec![255, 90, 31, 255], "A held");
        assert_eq!(at(80, 25), vec![0, 0, 0, 0], "the gap is clear");
        assert_eq!(at(200, 25), vec![40, 40, 40, 200], "Space at rest");
        assert_eq!(at(0, 0), vec![0, 0, 0, 0], "round corners");
    }

    struct Fake(Vec<Key>);
    impl Keys for Fake {
        fn held(&mut self, k: Key) -> bool {
            self.0.contains(&k)
        }
    }

    #[test]
    fn only_the_layouts_keys_are_looked_at() {
        struct Spy(Arc<Mutex<Vec<Key>>>);
        impl Keys for Spy {
            fn held(&mut self, k: Key) -> bool {
                self.0.lock().unwrap_or_else(|e| e.into_inner()).push(k);
                false
            }
        }
        let asked = Arc::new(Mutex::new(Vec::new()));
        let s = spec("W / A S D");
        assert_eq!(look(&s, &mut Spy(asked.clone())), vec![false; 4]);
        assert_eq!(
            *asked.lock().unwrap_or_else(|e| e.into_inner()),
            [Key::Letter('W'), Key::Letter('A'), Key::Letter('S'), Key::Letter('D')]
        );
    }

    #[test]
    fn the_worker_draws_again_only_when_keys_change() {
        let fonts = Arc::new(Mutex::new(TextRenderer::new()));
        let feed = Feed::default();
        let w = input_overlay(spec("W A"), Box::new(Fake(vec![Key::Letter('A')])), feed.clone(), fonts);
        std::thread::sleep(Duration::from_millis(200));
        let p = feed.take_picture().unwrap_or_else(|| panic!("a first picture"));
        assert_eq!((p.width, p.height), (106, 50));
        std::thread::sleep(Duration::from_millis(100));
        assert!(feed.take_picture().is_none(), "nothing changed: nothing drawn");
        drop(w);
    }
}
