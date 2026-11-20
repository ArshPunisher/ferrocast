//! Apple's media frameworks (VideoToolbox, AudioToolbox, CoreMedia, CoreVideo), called directly:
//! the few C functions the encoders need, declared here. Every unsafe block says why it's sound.
#![allow(unsafe_code)]

pub(crate) mod aac;
pub mod camera;
pub mod media;
pub(crate) mod power;
pub mod screen;
pub(crate) mod vt;
pub mod zerocopy;

use std::ffi::{c_char, c_void};
use std::ptr;

pub(crate) type CFTypeRef = *const c_void;
pub(crate) type CFStringRef = *const c_void;
pub(crate) type CFDictionaryRef = *const c_void;
pub(crate) type CFMutableDictionaryRef = *mut c_void;
pub(crate) type CFAllocatorRef = *const c_void;
pub(crate) type OSStatus = i32;

#[repr(C)]
pub(crate) struct CFDictionaryKeyCallBacks {
    _private: [u8; 0],
}
#[repr(C)]
pub(crate) struct CFDictionaryValueCallBacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kCFBooleanTrue: CFTypeRef;
    pub(crate) static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
    pub(crate) fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreateMutable(
        alloc: CFAllocatorRef,
        capacity: isize,
        keys: *const CFDictionaryKeyCallBacks,
        values: *const CFDictionaryValueCallBacks,
    ) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(dict: CFMutableDictionaryRef, key: *const c_void, value: *const c_void);
    pub(crate) fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFNumberCreate(alloc: CFAllocatorRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFStringCreateWithCString(alloc: CFAllocatorRef, s: *const c_char, encoding: u32) -> CFStringRef;
    pub(crate) fn CFDataGetLength(data: CFTypeRef) -> isize;
    pub(crate) fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    pub(crate) fn CFArrayGetCount(array: CFTypeRef) -> isize;
    static kCFTypeArrayCallBacks: CFDictionaryValueCallBacks;
    pub(crate) fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const CFTypeRef,
        n: isize,
        callbacks: *const CFDictionaryValueCallBacks,
    ) -> CFTypeRef;
    pub(crate) fn CFArrayGetValueAtIndex(array: CFTypeRef, i: isize) -> *const c_void;
    #[cfg(test)]
    pub(crate) fn CFGetRetainCount(cf: CFTypeRef) -> isize;
}

#[repr(C)]
struct TimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut TimebaseInfo) -> i32;
}

/// The Mac's tick-to-nanosecond ratio, asked once (it never changes while the machine runs): every
/// `host_ns` call asked for it again, two system calls a time, from capture callbacks too.
fn timebase() -> Option<(u32, u32)> {
    static TIMEBASE: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut tb = TimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: a plain system call; `tb` is ours.
        let ok = unsafe { mach_timebase_info(&raw mut tb) } == 0;
        (ok && tb.denom != 0).then_some((tb.numer, tb.denom))
    })
}

/// `ticks` of the host clock in nanoseconds.
fn ticks_to_ns(ticks: u64, tb: Option<(u32, u32)>) -> u64 {
    match tb {
        Some((numer, denom)) if numer != denom => (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64,
        _ => ticks,
    }
}

/// The host clock in nanoseconds (what CoreMedia's host clock and capture times use).
pub(crate) fn host_ns() -> u64 {
    // SAFETY: a plain system call.
    let t = unsafe { mach_absolute_time() };
    ticks_to_ns(t, timebase())
}

/// A Core Foundation object this code owns (one retain), released when dropped.
#[derive(Debug)]
pub(crate) struct Owned(pub(crate) CFTypeRef);

impl Owned {
    /// Takes ownership of a +1 reference (from a Create or Copy call); None when null.
    pub(crate) fn new(r: CFTypeRef) -> Option<Self> {
        (!r.is_null()).then_some(Self(r))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `Owned` holds exactly one retain on a live CF object, given up once here.
        unsafe { CFRelease(self.0) }
    }
}

/// kCFNumberSInt32Type, kCFNumberFloat64Type.
const SINT32: isize = 3;
const FLOAT64: isize = 6;

pub(crate) fn int(v: i32) -> Option<Owned> {
    // SAFETY: CFNumberCreate copies the i32 it's pointed at; the result is +1 (Owned).
    Owned::new(unsafe { CFNumberCreate(ptr::null(), SINT32, (&raw const v).cast()) })
}

pub(crate) fn float(v: f64) -> Option<Owned> {
    // SAFETY: as `int`, for an f64.
    Owned::new(unsafe { CFNumberCreate(ptr::null(), FLOAT64, (&raw const v).cast()) })
}

/// A CFString from Rust text (no NULs).
pub(crate) fn string(s: &std::ffi::CStr) -> Option<Owned> {
    /// kCFStringEncodingUTF8.
    const UTF8: u32 = 0x0800_0100;
    // SAFETY: `s` is a valid NUL-terminated string; CFStringCreateWithCString copies it (+1).
    Owned::new(unsafe { CFStringCreateWithCString(ptr::null(), s.as_ptr(), UTF8) })
}

/// A CFArray of CF objects (retained by it).
pub(crate) fn array(values: &[&Owned]) -> Option<Owned> {
    let raw: Vec<CFTypeRef> = values.iter().map(|v| v.0).collect();
    // SAFETY: each value is a live CF object; the standard callbacks make the array retain them;
    // the result is +1 (Owned).
    Owned::new(unsafe {
        CFArrayCreate(ptr::null(), raw.as_ptr(), raw.len() as isize, &raw const kCFTypeArrayCallBacks)
    })
}

/// A mutable CFDictionary holding CF objects (retained by it).
#[derive(Debug)]
pub(crate) struct Dict(pub(crate) Owned);

impl Dict {
    pub(crate) fn new() -> Option<Self> {
        // SAFETY: the standard CF callbacks make the dictionary retain and release its keys and
        // values; the result is +1 (Owned).
        let d = unsafe {
            CFDictionaryCreateMutable(
                ptr::null(),
                0,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        };
        Owned::new(d).map(Self)
    }

    pub(crate) fn set(&self, key: CFTypeRef, value: CFTypeRef) {
        if key.is_null() || value.is_null() {
            return;
        }
        // SAFETY: the dictionary is live and mutable; key and value are live CF objects, which it
        // retains.
        unsafe { CFDictionarySetValue(self.0.0.cast_mut(), key, value) }
    }
}

/// Bytes of a CFData (copied).
pub(crate) fn data_bytes(data: CFTypeRef) -> Vec<u8> {
    if data.is_null() {
        return Vec::new();
    }
    // SAFETY: `data` is a live CFData; its byte pointer is valid for its length while we copy.
    unsafe {
        let n = usize::try_from(CFDataGetLength(data)).unwrap_or(0);
        let p = CFDataGetBytePtr(data);
        if p.is_null() || n == 0 { Vec::new() } else { std::slice::from_raw_parts(p, n).to_vec() }
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    /// The cached ratio is the one the system gives, the clock goes forward, and the sums are right
    /// for a ratio that isn't 1 (Apple silicon's is 125/3) and for one that is (Intel's).
    #[test]
    fn the_host_clock_uses_the_systems_ratio_asked_once() {
        let mut tb = TimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: a plain system call; `tb` is ours.
        assert_eq!(unsafe { mach_timebase_info(&raw mut tb) }, 0);
        assert_eq!(timebase(), Some((tb.numer, tb.denom)));
        assert_eq!(timebase(), timebase(), "the same answer every time");
        let a = host_ns();
        let b = host_ns();
        assert!(b >= a && b - a < 1_000_000_000);
        assert_eq!(ticks_to_ns(24, Some((125, 3))), 1_000);
        assert_eq!(
            ticks_to_ns(24_000_000_000_000, Some((125, 3))),
            1_000_000_000_000_000,
            "no overflow in a day's ticks"
        );
        assert_eq!(ticks_to_ns(777, Some((1, 1))), 777);
        assert_eq!(ticks_to_ns(777, None), 777, "no ratio known: the ticks as they are, as before");
    }
}
