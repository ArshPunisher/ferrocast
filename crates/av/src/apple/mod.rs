//! Apple's media frameworks (VideoToolbox, AudioToolbox, CoreMedia, CoreVideo), called directly:
//! the few C functions the encoders need, declared here. Every unsafe block says why it's sound.
#![allow(unsafe_code)]

pub(crate) mod aac;
pub mod camera;
pub mod media;
pub mod screen;
pub(crate) mod vt;

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
    fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const CFTypeRef,
        n: isize,
        callbacks: *const CFDictionaryValueCallBacks,
    ) -> CFTypeRef;
    pub(crate) fn CFArrayGetValueAtIndex(array: CFTypeRef, i: isize) -> *const c_void;
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

/// The host clock in nanoseconds (what CoreMedia's host clock and capture times use).
pub(crate) fn host_ns() -> u64 {
    let mut tb = TimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: both are plain system calls; `tb` is ours.
    let (t, ok) = unsafe { (mach_absolute_time(), mach_timebase_info(&raw mut tb) == 0) };
    if !ok || tb.denom == 0 {
        return t;
    }
    (u128::from(t) * u128::from(tb.numer) / u128::from(tb.denom)) as u64
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
