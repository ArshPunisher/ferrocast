//! Smart vertical on macOS (plan item 8, 29 Oct): Apple's Vision finds faces in a small
//! snapshot of a picture, on this Mac (nothing leaves the computer).

use objc2::AnyThread;
use objc2_foundation::{NSArray, NSData, NSDictionary};
use objc2_vision::{VNDetectFaceRectanglesRequest, VNImageRequestHandler, VNRequest};

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
    if w == 0 || h == 0 || rgba.len() != w as usize * h as usize * 4 {
        return Vec::new();
    }
    // Vision reads encoded images: a PNG of a small snapshot is cheap.
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let Ok(mut writer) = enc.write_header() else { return Vec::new() };
        if writer.write_image_data(rgba).is_err() {
            return Vec::new();
        }
    }
    let data = NSData::with_bytes(&png_bytes);
    let options = NSDictionary::new();
    let handler = VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &options);
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
}
