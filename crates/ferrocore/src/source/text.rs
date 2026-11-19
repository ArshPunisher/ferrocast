//! The Text source, drawn by our engine (OBS's Text FreeType 2 and GDI+ sources, in one): any
//! installed font (with emoji, right-to-left scripts and fallback fonts found by cosmic-text),
//! bold and italic, a colour or a top-to-bottom gradient, an outline, a drop shadow, a
//! background, word wrap at a width, alignment, and a chat-log mode that keeps the last lines.

use cosmic_text::{Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache, Weight, Wrap};

use crate::Picture;

/// How the text looks. Colours are straight RGBA.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// A font family name; empty for the system's sans-serif.
    pub font: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: [u8; 4],
    /// A second colour: the text fades from `color` at the top to this at the bottom.
    pub gradient: Option<[u8; 4]>,
    /// Outline width (pixels) and colour.
    pub outline: Option<(f32, [u8; 4])>,
    /// Drop shadow offset (pixels, right and down) and colour.
    pub shadow: Option<((i32, i32), [u8; 4])>,
    pub background: Option<[u8; 4]>,
    /// Wrap at this width (pixels); the picture is then that wide.
    pub wrap_width: Option<u32>,
    pub align: TextAlign,
    /// Only the last this many lines (OBS's chat log mode).
    pub last_lines: Option<usize>,
    /// Space around the text, pixels.
    pub padding: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font: String::new(),
            size: 48.0,
            bold: false,
            italic: false,
            color: [255, 255, 255, 255],
            gradient: None,
            outline: None,
            shadow: None,
            background: None,
            wrap_width: None,
            align: TextAlign::Left,
            last_lines: None,
            padding: 0,
        }
    }
}

/// Draws text. Keeps the system's fonts loaded (finding them takes a moment the first time).
pub struct TextRenderer {
    fonts: FontSystem,
    cache: SwashCache,
}

impl std::fmt::Debug for TextRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextRenderer").finish_non_exhaustive()
    }
}

impl Default for TextRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// The largest text picture (a runaway chat log can't make a huge texture).
const MAX_SIDE: usize = 8192;

impl TextRenderer {
    pub fn new() -> Self {
        Self { fonts: FontSystem::new(), cache: SwashCache::new() }
    }

    /// The text as a picture (straight alpha), just big enough for it, its outline and shadow.
    pub fn render(&mut self, text: &str, style: &TextStyle) -> Picture {
        let text = match style.last_lines {
            Some(n) => {
                let lines: Vec<&str> = text.lines().collect();
                lines[lines.len().saturating_sub(n)..].join("\n")
            }
            None => text.to_string(),
        };
        let size = style.size.clamp(1.0, 1000.0);
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, (size * 1.2).ceil()));
        buffer.set_wrap(if style.wrap_width.is_some() { Wrap::WordOrGlyph } else { Wrap::None });
        buffer.set_size(style.wrap_width.map(|w| w as f32), None);
        let mut attrs = Attrs::new();
        if !style.font.is_empty() {
            attrs = attrs.family(Family::Name(&style.font));
        }
        if style.bold {
            attrs = attrs.weight(Weight::BOLD);
        }
        if style.italic {
            attrs = attrs.style(Style::Italic);
        }
        let align = match style.align {
            TextAlign::Left => Align::Left,
            TextAlign::Center => Align::Center,
            TextAlign::Right => Align::Right,
        };
        buffer.set_text(&text, &attrs, Shaping::Advanced, Some(align));
        buffer.shape_until_scroll(&mut self.fonts, false);
        let (mut tw, mut th) = (0f32, 0f32);
        for run in buffer.layout_runs() {
            tw = tw.max(run.line_w);
            th = th.max(run.line_top + run.line_height);
        }
        if let Some(w) = style.wrap_width {
            tw = w as f32;
        }
        let outline = style.outline.map_or(0.0, |(w, _)| w.max(0.0));
        let (sx, sy) = style.shadow.map_or((0, 0), |((x, y), _)| (x, y));
        let edge = outline.ceil() as usize + style.padding as usize;
        let w = ((tw.ceil() as usize) + 2 * edge + sx.unsigned_abs() as usize).clamp(1, MAX_SIDE);
        let h = ((th.ceil() as usize) + 2 * edge + sy.unsigned_abs() as usize).clamp(1, MAX_SIDE);
        // Where the text sits: inside the edge, moved away from a shadow cast up or left.
        let (ox, oy) = (edge as i32 + (-sx).max(0), edge as i32 + (-sy).max(0));

        // Coverage of all glyphs, 0 to 1 (outline and shadow follow it); the text's own colour
        // goes where plain glyphs are; colour glyphs (emoji) keep their own colours.
        let mut cover = vec![0f32; w * h];
        let mut plain = vec![0f32; w * h];
        let mut colour = vec![[0f32; 4]; w * h];
        buffer.draw(&mut self.fonts, &mut self.cache, Color::rgba(255, 255, 255, 255), |x, y, gw, gh, c| {
            let a = f32::from(c.a()) / 255.0;
            let tinted = (c.r(), c.g(), c.b()) != (255, 255, 255);
            for dy in 0..gh as i32 {
                for dx in 0..gw as i32 {
                    let (px, py) = (x + dx + ox, y + dy + oy);
                    if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                        let i = py as usize * w + px as usize;
                        cover[i] = (cover[i] + a).min(1.0);
                        if tinted {
                            colour[i] =
                                [f32::from(c.r()) / 255.0, f32::from(c.g()) / 255.0, f32::from(c.b()) / 255.0, a];
                        } else {
                            plain[i] = (plain[i] + a).min(1.0);
                        }
                    }
                }
            }
        });
        let ring = if outline > 0.0 { dilate(&cover, w, h, outline) } else { cover.clone() };
        let mut out = vec![[0f32; 4]; w * h];
        let over = |dst: &mut [f32; 4], c: [u8; 4], a: f32| {
            let sa = f32::from(c[3]) / 255.0 * a;
            if sa <= 0.0 {
                return;
            }
            let da = dst[3];
            let oa = sa + da * (1.0 - sa);
            for k in 0..3 {
                dst[k] = (f32::from(c[k]) / 255.0 * sa + dst[k] * da * (1.0 - sa)) / oa.max(1e-6);
            }
            dst[3] = oa;
        };
        if let Some(bg) = style.background {
            for px in &mut out {
                over(px, bg, 1.0);
            }
        }
        if let Some((_, sc)) = style.shadow {
            for y in 0..h {
                for x in 0..w {
                    let (fx, fy) = (x as i32 - sx, y as i32 - sy);
                    if fx >= 0 && fy >= 0 && (fx as usize) < w && (fy as usize) < h {
                        over(&mut out[y * w + x], sc, ring[fy as usize * w + fx as usize]);
                    }
                }
            }
        }
        if let Some((_, oc)) = style.outline {
            for (i, px) in out.iter_mut().enumerate() {
                over(px, oc, ring[i]);
            }
        }
        let top = oy as f32;
        let span = th.max(1.0);
        for y in 0..h {
            let color = match style.gradient {
                Some(c2) => {
                    let t = ((y as f32 - top) / span).clamp(0.0, 1.0);
                    let mut c = [0u8; 4];
                    for k in 0..4 {
                        c[k] = (f32::from(style.color[k]) * (1.0 - t) + f32::from(c2[k]) * t).round() as u8;
                    }
                    c
                }
                None => style.color,
            };
            for x in 0..w {
                let i = y * w + x;
                over(&mut out[i], color, plain[i]);
                let [r, g, b, a] = colour[i];
                if a > 0.0 {
                    over(&mut out[i], [to8(r), to8(g), to8(b), 255], a);
                }
            }
        }
        let rgba = out.iter().flat_map(|p| [to8(p[0]), to8(p[1]), to8(p[2]), to8(p[3])]).collect();
        Picture { width: w as u32, height: h as u32, rgba, premultiplied: false }
    }
}

fn to8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Coverage grown by `r` pixels: each pixel takes the most of any within a disc, its edge soft.
fn dilate(src: &[f32], w: usize, h: usize, r: f32) -> Vec<f32> {
    let ri = r.ceil() as i32;
    let mut out = vec![0f32; w * h];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut m = 0f32;
            for dy in -ri..=ri {
                for dx in -ri..=ri {
                    let d = ((dx * dx + dy * dy) as f32).sqrt();
                    let weight = (r + 0.5 - d).clamp(0.0, 1.0);
                    if weight <= 0.0 {
                        continue;
                    }
                    let (px, py) = (x + dx, y + dy);
                    if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                        m = m.max(src[py as usize * w + px as usize] * weight);
                    }
                }
            }
            out[y as usize * w + x as usize] = m;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(p: &Picture) -> usize {
        p.rgba.chunks(4).filter(|c| c[3] > 128).count()
    }

    #[test]
    fn text_has_size_and_ink() {
        let mut r = TextRenderer::new();
        let p = r.render("Ferrocast", &TextStyle::default());
        assert!(p.width > 150 && p.height >= 48, "{}x{}", p.width, p.height);
        assert!(lit(&p) > 500);
        // Longer text is wider.
        let q = r.render("Ferrocore", &TextStyle::default());
        assert!(q.width > p.width);
    }

    #[test]
    fn outline_and_shadow_add_ink_around() {
        let mut r = TextRenderer::new();
        let plain = r.render("Hi", &TextStyle::default());
        let fancy = r.render(
            "Hi",
            &TextStyle {
                outline: Some((4.0, [0, 0, 0, 255])),
                shadow: Some(((6, 6), [0, 0, 0, 200])),
                ..Default::default()
            },
        );
        assert!(fancy.width >= plain.width + 14 && fancy.height >= plain.height + 14);
        assert!(lit(&fancy) > lit(&plain) * 2);
    }

    #[test]
    fn wrapping_makes_lines_and_chat_mode_keeps_the_last() {
        let mut r = TextRenderer::new();
        let wrapped =
            r.render("one two three four five six seven", &TextStyle { wrap_width: Some(200), ..Default::default() });
        assert_eq!(wrapped.width, 200);
        assert!(wrapped.height > 48 * 2);
        let chat = r.render("a\nb\nc\nd", &TextStyle { last_lines: Some(2), ..Default::default() });
        let all = r.render("a\nb\nc\nd", &TextStyle::default());
        assert!(chat.height < all.height);
    }

    #[test]
    fn gradients_run_top_to_bottom() {
        let mut r = TextRenderer::new();
        let p = r.render(
            "████",
            &TextStyle { color: [255, 0, 0, 255], gradient: Some([0, 0, 255, 255]), ..Default::default() },
        );
        let w = p.width as usize;
        let ink: Vec<(usize, &[u8])> = p.rgba.chunks(4).enumerate().filter(|(_, c)| c[3] == 255).collect();
        let (top, bottom) = (ink.first().map(|(i, c)| (i / w, c[0])), ink.last().map(|(i, c)| (i / w, c[0])));
        if let (Some((ty, tr)), Some((by, br))) = (top, bottom) {
            assert!(by > ty && tr > br, "red fades from the top");
        }
    }
}
