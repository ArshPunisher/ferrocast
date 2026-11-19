//! Pictures for the camera's readers: the frame fitted into the size an app picked, the
//! formats apps ask for, and the placeholder shown while Ferrocast isn't sending.
//!
//! NV12 here is what the engine gives: BT.709, limited range (black is Y 16, grey chroma 128).

/// Black, in limited range.
pub const BLACK_Y: u8 = 16;
/// No colour.
pub const GREY_UV: u8 = 128;
/// The placeholder's background (a dark grey).
const DARK_Y: u8 = 30;
/// Ferrocast's orange (#FF5A1F) in BT.709 limited range.
const ORANGE: (u8, u8, u8) = (120, 85, 203);

/// The part of a `dw`×`dh` frame a `sw`×`sh` picture fills when fitted (its shape kept, even
/// sides and place, centred): x, y, width, height.
pub fn fit_rect(sw: u32, sh: u32, dw: u32, dh: u32) -> (u32, u32, u32, u32) {
    if sw == 0 || sh == 0 || dw < 2 || dh < 2 {
        return (0, 0, dw & !1, dh & !1);
    }
    // Wider than the frame: full width, bars above and below; else full height.
    let (w, h) = if u64::from(sw) * u64::from(dh) >= u64::from(sh) * u64::from(dw) {
        (dw, ((u64::from(dw) * u64::from(sh) / u64::from(sw)) as u32).clamp(2, dh))
    } else {
        (((u64::from(dh) * u64::from(sw) / u64::from(sh)) as u32).clamp(2, dw), dh)
    };
    let (w, h) = (w & !1, h & !1);
    (((dw - w) / 2) & !1, ((dh - h) / 2) & !1, w, h)
}

/// A `w`×`h` NV12 frame filled black.
pub fn fill_black(dst: &mut [u8], w: u32, h: u32) {
    let ylen = w as usize * h as usize;
    dst[..ylen].fill(BLACK_Y);
    dst[ylen..ylen + ylen / 2].fill(GREY_UV);
}

/// Scales one plane of `comps` interleaved components (1 for Y, 2 for CbCr) from `src`
/// (`sw`×`sh`, rows packed) into the `rw`×`rh` rectangle at (`rx`,`ry`) of `dst` (rows `dstride`
/// bytes), bilinear.
#[allow(clippy::too_many_arguments)]
fn scale_plane(
    src: &[u8],
    (sw, sh): (usize, usize),
    dst: &mut [u8],
    dstride: usize,
    (rx, ry, rw, rh): (usize, usize, usize, usize),
    comps: usize,
) {
    if sw == 0 || sh == 0 || rw == 0 || rh == 0 {
        return;
    }
    // 16.16 fixed point, sampling pixel centres.
    let step_x = ((sw as u64) << 16) / rw as u64;
    let step_y = ((sh as u64) << 16) / rh as u64;
    let xs: Vec<(usize, usize, u32)> = (0..rw)
        .map(|x| {
            let fx = ((x as u64 * step_x) + step_x / 2).saturating_sub(1 << 15);
            let x0 = ((fx >> 16) as usize).min(sw - 1);
            (x0, (x0 + 1).min(sw - 1), (fx & 0xFFFF) as u32)
        })
        .collect();
    for y in 0..rh {
        let fy = ((y as u64 * step_y) + step_y / 2).saturating_sub(1 << 15);
        let y0 = ((fy >> 16) as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = (fy & 0xFFFF) as u32;
        let (r0, r1) = (&src[y0 * sw * comps..(y0 + 1) * sw * comps], &src[y1 * sw * comps..(y1 + 1) * sw * comps]);
        let out = &mut dst[(ry + y) * dstride + rx * comps..(ry + y) * dstride + (rx + rw) * comps];
        for (x, &(x0, x1, wx)) in xs.iter().enumerate() {
            for c in 0..comps {
                let (a, b) = (u32::from(r0[x0 * comps + c]), u32::from(r0[x1 * comps + c]));
                let (p, q) = (u32::from(r1[x0 * comps + c]), u32::from(r1[x1 * comps + c]));
                let top = a * (65536 - wx) + b * wx;
                let bottom = p * (65536 - wx) + q * wx;
                let v = (u64::from(top) * u64::from(65536 - wy) + u64::from(bottom) * u64::from(wy) + (1 << 31)) >> 32;
                out[x * comps + c] = v as u8;
            }
        }
    }
}

/// `src` (NV12, `sw`×`sh`) fitted into `dst` (NV12, `dw`×`dh`): its shape kept, black bars
/// where the shapes differ. The same size is a copy.
pub fn fit_nv12(src: &[u8], (sw, sh): (u32, u32), dst: &mut [u8], (dw, dh): (u32, u32)) {
    let (slen, dlen) = (super::nv12_len(sw, sh), super::nv12_len(dw, dh));
    if src.len() < slen || dst.len() < dlen {
        return;
    }
    if (sw, sh) == (dw, dh) {
        dst[..dlen].copy_from_slice(&src[..slen]);
        return;
    }
    let (rx, ry, rw, rh) = fit_rect(sw, sh, dw, dh);
    if (rw, rh) != (dw, dh) {
        fill_black(dst, dw, dh);
    }
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let (rx, ry, rw, rh) = (rx as usize, ry as usize, rw as usize, rh as usize);
    let (sy, suv) = src[..slen].split_at(sw * sh);
    let (dy, duv) = dst[..dlen].split_at_mut(dw * dh);
    scale_plane(sy, (sw, sh), dy, dw, (rx, ry, rw, rh), 1);
    scale_plane(suv, (sw / 2, sh / 2), duv, dw, (rx / 2, ry / 2, rw / 2, rh / 2), 2);
}

/// NV12 → YUY2 (Y0 U Y1 V), `w`×`h`; `dst` is `w`×`h`×2.
pub fn nv12_to_yuy2(src: &[u8], w: u32, h: u32, dst: &mut [u8]) {
    let (w, h) = (w as usize, h as usize);
    if src.len() < w * h * 3 / 2 || dst.len() < w * h * 2 {
        return;
    }
    let (y, uv) = src.split_at(w * h);
    for row in 0..h {
        let yr = &y[row * w..(row + 1) * w];
        let cr = &uv[(row / 2) * w..(row / 2 + 1) * w];
        let out = &mut dst[row * w * 2..(row + 1) * w * 2];
        for x in (0..w).step_by(2) {
            out[x * 2] = yr[x];
            out[x * 2 + 1] = cr[x];
            out[x * 2 + 2] = yr[x + 1];
            out[x * 2 + 3] = cr[x + 1];
        }
    }
}

/// NV12 → I420 (Y, then U, then V planes), `w`×`h`.
pub fn nv12_to_i420(src: &[u8], w: u32, h: u32, dst: &mut [u8]) {
    let (w, h) = (w as usize, h as usize);
    let (ylen, clen) = (w * h, w * h / 4);
    if src.len() < ylen * 3 / 2 || dst.len() < ylen + 2 * clen {
        return;
    }
    dst[..ylen].copy_from_slice(&src[..ylen]);
    let (u, v) = dst[ylen..ylen + 2 * clen].split_at_mut(clen);
    for (i, pair) in src[ylen..ylen + 2 * clen].as_chunks::<2>().0.iter().enumerate() {
        u[i] = pair[0];
        v[i] = pair[1];
    }
}

/// What the camera shows while Ferrocast isn't sending: dark grey with an orange ring in the
/// middle (Ferrocast's colour), NV12 `w`×`h`. Never a frozen frame.
pub fn placeholder(w: u32, h: u32) -> Vec<u8> {
    let (w, h) = ((w & !1).max(2) as usize, (h & !1).max(2) as usize);
    let mut out = vec![DARK_Y; w * h];
    out.resize(w * h * 3 / 2, GREY_UV);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let outer = (w.min(h) as f32 / 9.0).max(2.0);
    let inner = outer * 0.62;
    let ring = |x: f32, y: f32| {
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        d <= outer && d >= inner
    };
    for y in 0..h {
        for x in 0..w {
            if ring(x as f32 + 0.5, y as f32 + 0.5) {
                out[y * w + x] = ORANGE.0;
            }
        }
    }
    let ylen = w * h;
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            if ring(x as f32 * 2.0 + 1.0, y as f32 * 2.0 + 1.0) {
                out[ylen + y * w + x * 2] = ORANGE.1;
                out[ylen + y * w + x * 2 + 1] = ORANGE.2;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nv12_len;

    /// An NV12 frame whose Y is `f(x, y)` and chroma is (`u`, `v`).
    fn make(w: u32, h: u32, f: impl Fn(u32, u32) -> u8, u: u8, v: u8) -> Vec<u8> {
        let mut out: Vec<u8> = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| f(x, y)).collect();
        for _ in 0..(w * h / 4) {
            out.push(u);
            out.push(v);
        }
        out
    }

    #[test]
    fn fitting_keeps_the_shape_with_bars() {
        assert_eq!(fit_rect(1920, 1080, 1280, 720), (0, 0, 1280, 720));
        assert_eq!(fit_rect(1920, 1080, 640, 480), (0, 60, 640, 360), "bars above and below");
        assert_eq!(fit_rect(608, 1080, 1920, 1080), (656, 0, 608, 1080), "bars at the sides");
        assert_eq!(fit_rect(0, 0, 640, 360), (0, 0, 640, 360));
        let (x, y, w, h) = fit_rect(1726, 1080, 1280, 720);
        assert!(x % 2 == 0 && y % 2 == 0 && w % 2 == 0 && h % 2 == 0, "even, for NV12's chroma");
    }

    #[test]
    fn the_same_size_is_copied_exactly() {
        let src = make(64, 36, |x, y| ((x * 3 + y) % 256) as u8, 90, 160);
        let mut dst = vec![0; nv12_len(64, 36)];
        fit_nv12(&src, (64, 36), &mut dst, (64, 36));
        assert_eq!(src, dst);
    }

    #[test]
    fn a_smaller_size_keeps_flat_areas_and_colour() {
        let src = make(128, 72, |_, _| 200, 90, 160);
        let mut dst = vec![0; nv12_len(64, 36)];
        fit_nv12(&src, (128, 72), &mut dst, (64, 36));
        assert!(dst[..64 * 36].iter().all(|&v| v == 200));
        assert!(dst[64 * 36..].as_chunks::<2>().0.iter().all(|p| *p == [90, 160]));
    }

    #[test]
    fn a_ramp_scaled_stays_a_ramp() {
        let src = make(256, 2, |x, _| x as u8, 128, 128);
        let mut dst = vec![0; nv12_len(128, 2)];
        fit_nv12(&src, (256, 2), &mut dst, (128, 2));
        let row = &dst[..128];
        assert!(row.windows(2).all(|p| p[1] >= p[0]), "still rising");
        assert!(row[0] < 4 && row[127] > 250, "ends kept: {} {}", row[0], row[127]);
    }

    #[test]
    fn another_shape_gets_black_bars() {
        let src = make(64, 64, |_, _| 235, 128, 128);
        let mut dst = vec![0; nv12_len(128, 64)];
        fit_nv12(&src, (64, 64), &mut dst, (128, 64));
        let px = |x: usize, y: usize| dst[y * 128 + x];
        assert_eq!(px(0, 10), BLACK_Y, "bar on the left");
        assert_eq!(px(127, 10), BLACK_Y, "bar on the right");
        assert_eq!(px(64, 32), 235, "the picture in the middle");
    }

    #[test]
    fn short_buffers_are_left_alone() {
        let mut dst = vec![7; 10];
        fit_nv12(&[1; 5], (64, 36), &mut dst, (64, 36));
        assert!(dst.iter().all(|&v| v == 7));
    }

    #[test]
    fn yuy2_and_i420_carry_the_same_picture() {
        // 4×2: Y 0..8, chroma pairs (10,20) and (30,40).
        let src = vec![0, 1, 2, 3, 4, 5, 6, 7, 10, 20, 30, 40];
        let mut yuy2 = vec![0; 16];
        nv12_to_yuy2(&src, 4, 2, &mut yuy2);
        assert_eq!(yuy2, [0, 10, 1, 20, 2, 30, 3, 40, 4, 10, 5, 20, 6, 30, 7, 40]);
        let mut i420 = vec![0; 12];
        nv12_to_i420(&src, 4, 2, &mut i420);
        assert_eq!(i420, [0, 1, 2, 3, 4, 5, 6, 7, 10, 30, 20, 40]);
    }

    #[test]
    fn the_placeholder_is_dark_with_an_orange_ring() {
        let p = placeholder(640, 360);
        assert_eq!(p.len(), nv12_len(640, 360));
        assert_eq!(p[0], DARK_Y, "dark corner");
        assert_eq!(p[180 * 640 + 320], DARK_Y, "the ring's middle is empty");
        let ring_x = 320 + (360.0f32 / 9.0 * 0.8) as usize;
        assert_eq!(p[180 * 640 + ring_x], ORANGE.0, "orange on the ring");
        let ylen = 640 * 360;
        let c = ylen + 90 * 640 + (ring_x / 2) * 2;
        assert_eq!((p[c], p[c + 1]), (ORANGE.1, ORANGE.2));
        assert_eq!(placeholder(3, 3).len(), nv12_len(2, 2), "odd sizes made even");
    }
}
