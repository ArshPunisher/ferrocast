//! Copy 1440p NV12 into an IOSurface-backed CVPixelBuffer (CoreVideo only, no VideoToolbox).
use std::ffi::c_void;
use std::time::Instant;
type CF = *const c_void;
#[link(name = "CoreVideo", kind = "framework")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreate(a: CF, w: usize, h: usize, fmt: u32, attrs: CF, out: *mut CF) -> i32;
    fn CVPixelBufferLockBaseAddress(b: CF, f: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(b: CF, f: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(b: CF, p: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(b: CF, p: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(b: CF, p: usize) -> usize;
    fn CFRelease(c: CF);
    fn CFDictionaryCreate(a: CF, k: *const CF, v: *const CF, n: isize, kc: CF, vc: CF) -> CF;
    static kCVPixelBufferIOSurfacePropertiesKey: CF;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}
fn main() {
    let fresh = std::env::args().nth(1).as_deref() == Some("fresh");
    let (w, h) = (2560usize, 1440usize);
    let fmt = u32::from_be_bytes(*b"420v");
    unsafe {
        let empty = CFDictionaryCreate(std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, &raw const kCFTypeDictionaryKeyCallBacks as CF, &raw const kCFTypeDictionaryValueCallBacks as CF);
        let k = [kCVPixelBufferIOSurfacePropertiesKey];
        let v = [empty];
        let attrs = CFDictionaryCreate(std::ptr::null(), k.as_ptr(), v.as_ptr(), 1, &raw const kCFTypeDictionaryKeyCallBacks as CF, &raw const kCFTypeDictionaryValueCallBacks as CF);
        let mut buf: CF = std::ptr::null();
        let s = CVPixelBufferCreate(std::ptr::null(), w, h, fmt, attrs, &raw mut buf);
        assert_eq!(s, 0, "create {s}");
        let src: Vec<u8> = (0..w * h * 3 / 2).map(|i| i as u8).collect();
        let heap_dst = vec![0u8; w * h * 3 / 2];
        let mut heap = heap_dst;
        let n = 300;
        let mut t_cv = 0.0;
        let mut t_heap = 0.0;
        for _ in 0..n {
            let t0 = Instant::now();
            if fresh {
                CFRelease(buf);
                buf = std::ptr::null();
                CVPixelBufferCreate(std::ptr::null(), w, h, fmt, attrs, &raw mut buf);
            }
            CVPixelBufferLockBaseAddress(buf, 0);
            for p in 0..2 {
                let (dst, ds, rows) = (CVPixelBufferGetBaseAddressOfPlane(buf, p), CVPixelBufferGetBytesPerRowOfPlane(buf, p), CVPixelBufferGetHeightOfPlane(buf, p));
                let off = if p == 0 { 0 } else { w * h };
                for r in 0..rows {
                    std::ptr::copy_nonoverlapping(src.as_ptr().add(off + r * w), dst.add(r * ds), w.min(ds));
                }
            }
            CVPixelBufferUnlockBaseAddress(buf, 0);
            t_cv += t0.elapsed().as_secs_f64();
            let t1 = Instant::now();
            heap.copy_from_slice(std::hint::black_box(&src));
            std::hint::black_box(&heap);
            t_heap += t1.elapsed().as_secs_f64();
        }
        println!("into IOSurface CVPixelBuffer (row by row, locked): {:.2} ms/frame; into heap: {:.2} ms/frame", t_cv / n as f64 * 1e3, t_heap / n as f64 * 1e3);
    }
}
