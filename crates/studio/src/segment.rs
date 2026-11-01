//! The person in a picture (Remove background): Apple's Vision finds them (person segmentation),
//! on this Mac; nothing leaves the computer.

/// A person mask: one byte a pixel, 255 a person.
#[derive(Debug)]
pub(crate) struct Mask {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

/// Vision's mask of the person in `rgba` (`w`×`h`), or None when it finds nothing to say.
pub(crate) fn person_mask(rgba: &[u8], w: u32, h: u32) -> Option<Mask> {
    // Vision makes Objective-C objects each time: each round lets go of its own.
    objc2::rc::autoreleasepool(|_| ask_vision(rgba, w, h))
}

fn ask_vision(rgba: &[u8], w: u32, h: u32) -> Option<Mask> {
    use objc2::AnyThread;
    use objc2_core_video::{
        CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSArray, NSData, NSDictionary};
    use objc2_vision::{
        VNGeneratePersonSegmentationRequest, VNGeneratePersonSegmentationRequestQualityLevel, VNImageRequestHandler,
        VNRequest,
    };
    if w == 0 || h == 0 || rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().ok()?.write_image_data(rgba).ok()?;
    }
    let data = NSData::with_bytes(&png_bytes);
    let handler =
        VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &data, &NSDictionary::new());
    // SAFETY: a plain new request; its quality and output format set before it runs.
    let request = unsafe { VNGeneratePersonSegmentationRequest::new() };
    // SAFETY: setters on a live request, before it runs.
    unsafe {
        request.setQualityLevel(VNGeneratePersonSegmentationRequestQualityLevel::Balanced);
        // kCVPixelFormatType_OneComponent8 ('L008'): one byte a pixel.
        request.setOutputPixelFormat(0x4C30_3038);
    }
    let as_request: &VNRequest = &request;
    handler.performRequests_error(&NSArray::from_slice(&[as_request])).ok()?;
    // SAFETY: results of a request that has run, read on the same thread.
    let found = unsafe { request.results() }?;
    let obs = found.firstObject()?;
    // SAFETY: a getter on a live observation.
    let buf = unsafe { obs.pixelBuffer() };
    let (mw, mh) = (CVPixelBufferGetWidth(&buf), CVPixelBufferGetHeight(&buf));
    // SAFETY: locked for reading, read row by row within the buffer's own sizes, then unlocked.
    unsafe {
        if CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(&buf) as *const u8;
        let row = CVPixelBufferGetBytesPerRow(&buf);
        let mut px = Vec::with_capacity(mw * mh);
        if !base.is_null() && row >= mw {
            for y in 0..mh {
                px.extend_from_slice(std::slice::from_raw_parts(base.add(y * row), mw));
            }
        }
        CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags::ReadOnly);
        (px.len() == mw * mh).then_some(Mask { w: mw as u32, h: mh as u32, px })
    }
}
