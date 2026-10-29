//! Smart vertical on macOS (plan item 8, 29 Oct): Apple's Vision finds faces in a small
//! snapshot of a picture, on this Mac (nothing leaves the computer).

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSData, NSDictionary};
use objc2_vision::{
    VNDetectFaceRectanglesRequest, VNGenerateAttentionBasedSaliencyImageRequest, VNImageRequestHandler, VNRequest,
};

/// A face's box, 0 to 1 of the picture, from its top left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Face {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The faces in an RGBA picture `w`×`h` (rows top first), biggest first. Empty when there are
/// none, or the picture can't be read.
pub fn faces(rgba: &[u8], w: u32, h: u32) -> Vec<Face> {
    let Some(handler) = handler(rgba, w, h) else { return Vec::new() };
    // SAFETY: a plain new request with no completion handler.
    let request = unsafe { VNDetectFaceRectanglesRequest::new() };
    let as_request: &VNRequest = &request;
    let list = NSArray::from_slice(&[as_request]);
    if handler.performRequests_error(&list).is_err() {
        return Vec::new();
    }
    // SAFETY: results of a request that has run, read on the same thread.
    let Some(found) = (unsafe { request.results() }) else { return Vec::new() };
    let mut out: Vec<Face> = found
        .iter()
        .map(|o| {
            // SAFETY: a getter on a live observation.
            let b = unsafe { o.boundingBox() };
            // Vision's origin is the bottom left.
            Face { x: b.origin.x, y: 1.0 - b.origin.y - b.size.height, w: b.size.width, h: b.size.height }
        })
        .filter(|f| f.w > 0.0 && f.h > 0.0)
        .collect();
    out.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));
    out
}

/// Where the eye goes in an RGBA picture (Vision's attention-based saliency): the box round
/// what stands out, biggest first. Empty for a picture with nothing standing out.
pub fn busy(rgba: &[u8], w: u32, h: u32) -> Vec<Face> {
    let Some(handler) = handler(rgba, w, h) else { return Vec::new() };
    // SAFETY: a plain new request with no completion handler.
    let request = unsafe { VNGenerateAttentionBasedSaliencyImageRequest::new() };
    let as_request: &VNRequest = &request;
    if handler.performRequests_error(&NSArray::from_slice(&[as_request])).is_err() {
        return Vec::new();
    }
    // SAFETY: results of a request that has run, read on the same thread.
    let Some(found) = (unsafe { request.results() }) else { return Vec::new() };
    let mut out = Vec::new();
    for o in found.iter() {
        // SAFETY: a getter on a live observation.
        let Some(objects) = (unsafe { o.salientObjects() }) else { continue };
        for r in objects.iter() {
            // SAFETY: a getter on a live observation.
            let b = unsafe { r.boundingBox() };
            if b.size.width > 0.0 && b.size.height > 0.0 {
                out.push(Face {
                    x: b.origin.x,
                    y: 1.0 - b.origin.y - b.size.height,
                    w: b.size.width,
                    h: b.size.height,
                });
            }
        }
    }
    out.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));
    out
}

/// A Vision handler for an RGBA picture `w`×`h` (rows top first), or None when it can't be
/// read. Vision reads encoded images: a PNG of a small snapshot is cheap.
fn handler(rgba: &[u8], w: u32, h: u32) -> Option<Retained<VNImageRequestHandler>> {
    if w == 0 || h == 0 || rgba.len() != w as usize * h as usize * 4 {
        return None;
    }
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    let data = NSData::with_bytes(&png_bytes);
    let options = NSDictionary::new();
    Some(VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_find_is_no_faces() {
        assert!(faces(&[0; 64 * 36 * 4], 64, 36).is_empty());
        assert!(faces(&[255; 64 * 36 * 4], 64, 36).is_empty());
        // The wrong size, or none, is refused before Vision sees it.
        assert!(faces(&[0; 10], 64, 36).is_empty());
        assert!(faces(&[], 0, 0).is_empty());
    }

    /// Needs a picture of a face: `FACE_PNG=/path/face.png cargo test -- --ignored`.
    #[test]
    #[ignore = "needs FACE_PNG, a picture with one face"]
    fn finds_the_face_in_a_portrait() {
        let path = std::env::var("FACE_PNG").expect("FACE_PNG");
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size()];
        let info = r.next_frame(&mut buf).unwrap();
        let px = &buf[..info.buffer_size()];
        let rgba: Vec<u8> = match info.color_type {
            png::ColorType::Rgba => px.to_vec(),
            png::ColorType::Rgb => px.chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
            other => panic!("{other:?}"),
        };
        let found = faces(&rgba, info.width, info.height);
        eprintln!("{found:?}");
        let f = found.first().expect("a face");
        // The Mona Lisa's face is in the top half, about in the middle.
        assert!(f.y + f.h < 0.6 && f.x > 0.25 && f.x + f.w < 0.75, "{f:?}");
    }

    #[test]
    fn a_bright_square_on_black_is_where_the_eye_goes() {
        let (w, h) = (160u32, 90u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 30..60 {
            for x in 110..140 {
                let i = ((y * w + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&[255, 200, 0, 255]);
            }
        }
        for i in (3..px.len()).step_by(4) {
            px[i] = 255;
        }
        let found = busy(&px, w, h);
        let b = found.first().expect("something stands out");
        let mid = b.x + b.w / 2.0;
        assert!(mid > 0.55, "the square is on the right: {b:?}");
        assert!(busy(&[0; 10], w, h).is_empty());
    }
}
