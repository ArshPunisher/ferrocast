//! Colour lookup tables for OBS's Apply LUT filter (color-grade-filter.c): a 512×512 PNG holding
//! a 64×64×64 cube, or an Adobe .cube file (1D or 3D, with its domain).

use std::path::Path;

use crate::AvError;

/// How OBS's PNG LUTs are laid out: 64 squares of 64×64.
const PNG_WIDTH: u32 = 64;

/// A LUT's values, as the card takes them.
#[derive(Debug, Clone, PartialEq)]
pub enum LutData {
    /// A PNG's pixels, RGBA in sRGB (read through an sRGB view, so filtering is in linear light).
    Srgb8(Vec<u8>),
    /// A .cube file's values made linear, RGBA as half floats.
    Half(Vec<u16>),
}

/// A loaded LUT.
#[derive(Debug, Clone, PartialEq)]
pub struct Lut {
    /// Entries a side (the cube's width, or a 1D table's length).
    pub size: u32,
    pub one_d: bool,
    pub data: LutData,
    pub domain_min: [f32; 3],
    pub domain_max: [f32; 3],
}

fn srgb_to_linear(u: f32) -> f32 {
    if u <= 0.04045 { u / 12.92 } else { ((u + 0.055) / 1.055).powf(2.4) }
}

impl Lut {
    /// Reads a .cube or a PNG (anything else is read as a picture).
    pub fn open(path: &Path) -> Result<Self, AvError> {
        let cube = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cube"));
        if cube {
            let text = std::fs::read_to_string(path)
                .map_err(|e| AvError::Source(format!("couldn't read the LUT {}: {e}", path.display())))?;
            Self::from_cube(&text)
        } else {
            let img = image::open(path)
                .map_err(|e| AvError::Source(format!("couldn't read the LUT {}: {e}", path.display())))?
                .to_rgba8();
            Self::from_png(img.width(), img.height(), img.as_raw())
        }
    }

    /// OBS's PNG layout: square z sits at column z % (width / 64), row z / (height / 64).
    pub fn from_png(width: u32, height: u32, rgba: &[u8]) -> Result<Self, AvError> {
        let n = PNG_WIDTH;
        if !width.is_multiple_of(n) || !height.is_multiple_of(n) || width * height != n * n * n {
            return Err(AvError::Source(format!("a PNG LUT is 512×512 (64 squares of 64×64), not {width}×{height}")));
        }
        if rgba.len() != (width * height * 4) as usize {
            return Err(AvError::BadPixels(width, height));
        }
        let (macro_w, macro_h) = (width / n, height / n);
        let mut out = Vec::with_capacity(rgba.len());
        for z in 0..n {
            // As color-grade-filter.c (it divides by the height in squares for the row).
            let (zx, zy) = ((z % macro_w) * n, (z / macro_h) * n);
            for y in 0..n {
                let row = width * (zy + y);
                for x in 0..n {
                    let i = ((row + zx + x) * 4) as usize;
                    out.extend_from_slice(&rgba[i..i + 4]);
                }
            }
        }
        Ok(Self { size: n, one_d: false, data: LutData::Srgb8(out), domain_min: [0.0; 3], domain_max: [1.0; 3] })
    }

    /// An Adobe .cube file: LUT_1D_SIZE or LUT_3D_SIZE, DOMAIN_MIN/MAX, then the entries.
    pub fn from_cube(text: &str) -> Result<Self, AvError> {
        let (mut size_1d, mut size_3d) = (0u32, 0u32);
        let (mut domain_min, mut domain_max) = ([0f32; 3], [1f32; 3]);
        let mut values: Vec<[f32; 3]> = Vec::new();
        let three = |rest: &str| -> Option<[f32; 3]> {
            let mut it = rest.split_whitespace().map(str::parse::<f32>);
            match (it.next(), it.next(), it.next()) {
                (Some(Ok(a)), Some(Ok(b)), Some(Ok(c))) => Some([a, b, c]),
                _ => None,
            }
        };
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = three(line) {
                values.push(v);
            } else if !values.is_empty() {
                // Metadata ends where the entries begin.
                continue;
            } else if let Some(rest) = line.strip_prefix("DOMAIN_MIN") {
                domain_min = three(rest).unwrap_or(domain_min);
            } else if let Some(rest) = line.strip_prefix("DOMAIN_MAX") {
                domain_max = three(rest).unwrap_or(domain_max);
            } else if let Some(rest) = line.strip_prefix("LUT_1D_SIZE") {
                size_1d = rest.trim().parse().unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("LUT_3D_SIZE") {
                size_3d = rest.trim().parse().unwrap_or(0);
            }
        }
        if (0..3).any(|i| domain_min[i] >= domain_max[i]) {
            return Err(AvError::Source("the .cube LUT's domain is empty".into()));
        }
        let (size, one_d) = match (size_1d, size_3d) {
            // A 1D table is one row of a texture: 8,192 is as wide as every card goes.
            (s, _) if (2..=8_192).contains(&s) => (s, true),
            (_, s) if (2..=256).contains(&s) => (s, false),
            _ => return Err(AvError::Source("the .cube LUT has no usable LUT_1D_SIZE or LUT_3D_SIZE".into())),
        };
        let count = if one_d { size as usize } else { (size as usize).pow(3) };
        if values.len() < count {
            return Err(AvError::Source(format!("the .cube LUT has {} of its {count} entries", values.len())));
        }
        let one = half::f16::from_f32(1.0).to_bits();
        let data = values[..count]
            .iter()
            .flat_map(|v| {
                let h = |x: f32| half::f16::from_f32(srgb_to_linear(x)).to_bits();
                [h(v[0]), h(v[1]), h(v[2]), one]
            })
            .collect();
        Ok(Self { size, one_d, data: LutData::Half(data), domain_min, domain_max })
    }

    /// Whether the domain is the whole of 0 to 1.
    pub fn full_domain(&self) -> bool {
        self.domain_min.iter().all(|v| *v <= 0.0) && self.domain_max.iter().all(|v| *v >= 1.0)
    }

    /// Where a colour lands in the table (OBS's clut_scale and clut_offset).
    pub fn scale_offset(&self) -> ([f32; 3], [f32; 3]) {
        let w = self.size as f32;
        if let LutData::Srgb8(_) = self.data {
            return ([1.0 - 1.0 / w; 3], [0.5 / w; 3]);
        }
        let (mut scale, mut offset) = ([0f32; 3], [0f32; 3]);
        for i in 0..3 {
            let s = (w - 1.0) / (self.domain_max[i] - self.domain_min[i]);
            scale[i] = s / w;
            offset[i] = (-self.domain_min[i] * s + 0.5) / w;
        }
        (scale, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cube_file_reads() {
        let text = "TITLE \"t\"\nLUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\n\
                    0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";
        let l = Lut::from_cube(text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((l.size, l.one_d), (2, false));
        let LutData::Half(d) = &l.data else { panic!("not half floats") };
        assert_eq!(d.len(), 32);
        assert_eq!(half::f16::from_bits(d[4]).to_f32(), 1.0);
        let (s, o) = l.scale_offset();
        assert_eq!((s[0], o[0]), (0.5, 0.25));
        assert!(Lut::from_cube("LUT_3D_SIZE 2\n0 0 0\n").is_err(), "too few entries");
    }

    #[test]
    fn a_png_lut_is_cut_into_squares() {
        let mut px = vec![0u8; 512 * 512 * 4];
        // Square 9 is the second row's second square: its first pixel at (64, 64).
        let i = (64 * 512 + 64) * 4;
        px[i] = 200;
        let l = Lut::from_png(512, 512, &px).unwrap_or_else(|e| panic!("{e}"));
        let LutData::Srgb8(d) = &l.data else { panic!("not sRGB") };
        assert_eq!(d[(9 * 64 * 64) * 4], 200);
        assert!(Lut::from_png(256, 256, &px[..256 * 256 * 4]).is_err());
    }
}
