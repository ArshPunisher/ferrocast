//! A tiny VST3 gain plugin of our own, for testing the VST helper (`ferrocast-vsthost`) without
//! anyone else's plugin. Never shipped. Based on the `vst3` crate's gain example (MIT/Apache).
//!
//! Its state is 9 bytes: the gain (f64, little-endian), then a mode for tests: 0 plays, 1 hangs
//! in `process` (10 s), 2 aborts in `process` (a crash).

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, unsafe_code)]
#![allow(clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]

use std::ffi::{CString, c_char, c_void};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::{ptr, slice};

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComRef, ComWrapper, uid};

pub const NAME: &str = "Ferrocast Test Gain";
pub const VENDOR: &str = "Ferrocast tests";
/// The processor's class id, as the helper names it (32 hex digits).
pub const PROCESSOR_CID: TUID = uid(0x6E332252, 0x54224A00, 0xAA69301A, 0xF318797E);
const CONTROLLER_CID: TUID = uid(0x1BA8A477, 0xEE0A4A2D, 0x80F50D14, 0x13D2EAA1);

fn copy_cstring(src: &str, dst: &mut [c_char]) {
    let c = CString::new(src).unwrap_or_default();
    for (s, d) in c.as_bytes_with_nul().iter().zip(dst.iter_mut()) {
        *d = *s as c_char;
    }
    if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut len = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        len += 1;
    }
    if len < dst.len() {
        dst[len] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

/// Reads `n` bytes from a host's stream.
unsafe fn read_all(stream: *mut IBStream, n: usize) -> Option<Vec<u8>> {
    let s = unsafe { ComRef::from_raw(stream) }?;
    let mut buf = vec![0u8; n];
    let mut got = 0i32;
    let r = unsafe { s.read(buf.as_mut_ptr().cast(), n as i32, &mut got) };
    (r == kResultOk && got as usize == n).then_some(buf)
}

unsafe fn write_all(stream: *mut IBStream, bytes: &[u8]) -> tresult {
    let Some(s) = (unsafe { ComRef::from_raw(stream) }) else { return kInvalidArgument };
    let mut put = 0i32;
    let r = unsafe { s.write(bytes.as_ptr() as *mut c_void, bytes.len() as i32, &mut put) };
    if r == kResultOk && put as usize == bytes.len() { kResultOk } else { kResultFalse }
}

struct Processor {
    gain: AtomicU64,
    mode: AtomicU8,
}

impl Class for Processor {
    type Interfaces = (IComponent, IAudioProcessor, IProcessContextRequirements);
}

impl IPluginBaseTrait for Processor {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IComponentTrait for Processor {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        unsafe { *class_id = CONTROLLER_CID };
        kResultOk
    }
    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }
    unsafe fn getBusCount(&self, media: MediaType, dir: BusDirection) -> i32 {
        let _ = dir;
        if media as MediaTypes == MediaTypes_::kAudio { 1 } else { 0 }
    }
    unsafe fn getBusInfo(&self, media: MediaType, dir: BusDirection, index: i32, bus: *mut BusInfo) -> tresult {
        if media as MediaTypes != MediaTypes_::kAudio || index != 0 {
            return kInvalidArgument;
        }
        let bus = unsafe { &mut *bus };
        bus.mediaType = MediaTypes_::kAudio as MediaType;
        bus.direction = dir;
        bus.channelCount = 2;
        copy_wstring(if dir as BusDirections == BusDirections_::kInput { "Input" } else { "Output" }, &mut bus.name);
        bus.busType = BusTypes_::kMain as BusType;
        bus.flags = BusInfo_::BusFlags_::kDefaultActive;
        kResultOk
    }
    unsafe fn getRoutingInfo(&self, _i: *mut RoutingInfo, _o: *mut RoutingInfo) -> tresult {
        kNotImplemented
    }
    unsafe fn activateBus(&self, _m: MediaType, _d: BusDirection, _i: i32, _s: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let Some(b) = (unsafe { read_all(state, 9) }) else { return kResultFalse };
        let gain = f64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8]));
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
        self.mode.store(b[8], Ordering::Relaxed);
        kResultOk
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        let mut b = f64::from_bits(self.gain.load(Ordering::Relaxed)).to_le_bytes().to_vec();
        b.push(self.mode.load(Ordering::Relaxed));
        unsafe { write_all(state, &b) }
    }
}

impl IAudioProcessorTrait for Processor {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut SpeakerArrangement,
        num_ins: i32,
        outputs: *mut SpeakerArrangement,
        num_outs: i32,
    ) -> tresult {
        let stereo = num_ins == 1
            && num_outs == 1
            && unsafe { *inputs == SpeakerArr::kStereo && *outputs == SpeakerArr::kStereo };
        if stereo { kResultTrue } else { kResultFalse }
    }
    unsafe fn getBusArrangement(&self, _dir: BusDirection, index: i32, arr: *mut SpeakerArrangement) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        unsafe { *arr = SpeakerArr::kStereo };
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        if size as SymbolicSampleSizes == SymbolicSampleSizes_::kSample32 { kResultOk } else { kNotImplemented }
    }
    unsafe fn getLatencySamples(&self) -> u32 {
        0
    }
    unsafe fn setupProcessing(&self, _setup: *mut ProcessSetup) -> tresult {
        kResultOk
    }
    unsafe fn setProcessing(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let d = unsafe { &*data };
        match self.mode.load(Ordering::Relaxed) {
            1 => std::thread::sleep(std::time::Duration::from_secs(10)),
            2 => std::process::abort(),
            _ => {}
        }
        // A parameter change from the window (id 0, the gain), its last point.
        if let Some(changes) = unsafe { ComRef::from_raw(d.inputParameterChanges) } {
            for i in 0..unsafe { changes.getParameterCount() } {
                if let Some(q) = unsafe { ComRef::from_raw(changes.getParameterData(i)) }
                    && unsafe { q.getParameterId() } == 0
                {
                    let (mut at, mut value) = (0, 0.0);
                    if unsafe { q.getPoint(q.getPointCount() - 1, &mut at, &mut value) } == kResultTrue {
                        // Normalised 0..1 → gain 0..2.
                        self.gain.store((value * 2.0).to_bits(), Ordering::Relaxed);
                    }
                }
            }
        }
        if d.numInputs != 1 || d.numOutputs != 1 {
            return kResultOk;
        }
        let gain = f64::from_bits(self.gain.load(Ordering::Relaxed)) as f32;
        let n = d.numSamples as usize;
        let (ins, outs) = unsafe { (&*d.inputs, &*d.outputs) };
        if ins.numChannels != 2 || outs.numChannels != 2 {
            return kResultOk;
        }
        let (i, o) = unsafe {
            (
                slice::from_raw_parts(ins.__field0.channelBuffers32, 2),
                slice::from_raw_parts(outs.__field0.channelBuffers32, 2),
            )
        };
        for c in 0..2 {
            let (src, dst) = unsafe { (slice::from_raw_parts(i[c], n), slice::from_raw_parts_mut(o[c], n)) };
            for (s, t) in src.iter().zip(dst.iter_mut()) {
                *t = s * gain;
            }
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

impl IProcessContextRequirementsTrait for Processor {
    unsafe fn getProcessContextRequirements(&self) -> u32 {
        0
    }
}

struct Controller {
    gain: std::cell::Cell<f64>,
}

impl Class for Controller {
    type Interfaces = (IEditController,);
}

impl IPluginBaseTrait for Controller {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for Controller {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        if let Some(b) = unsafe { read_all(state, 9) } {
            self.gain.set(f64::from_le_bytes(b[..8].try_into().unwrap_or([0; 8])) / 2.0);
        }
        kResultOk
    }
    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }
    unsafe fn getParameterCount(&self) -> i32 {
        1
    }
    unsafe fn getParameterInfo(&self, index: i32, info: *mut ParameterInfo) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }
        let info = unsafe { &mut *info };
        info.id = 0;
        copy_wstring("Gain", &mut info.title);
        copy_wstring("Gain", &mut info.shortTitle);
        copy_wstring("", &mut info.units);
        info.stepCount = 0;
        info.defaultNormalizedValue = 0.5;
        info.unitId = 0;
        info.flags = ParameterInfo_::ParameterFlags_::kCanAutomate;
        kResultOk
    }
    unsafe fn getParamStringByValue(&self, _id: u32, v: f64, s: *mut String128) -> tresult {
        copy_wstring(&format!("{:.2}", v * 2.0), unsafe { &mut *s });
        kResultOk
    }
    unsafe fn getParamValueByString(&self, _id: u32, _s: *mut TChar, _v: *mut f64) -> tresult {
        kNotImplemented
    }
    unsafe fn normalizedParamToPlain(&self, _id: u32, v: f64) -> f64 {
        v * 2.0
    }
    unsafe fn plainParamToNormalized(&self, _id: u32, v: f64) -> f64 {
        v / 2.0
    }
    unsafe fn getParamNormalized(&self, _id: u32) -> f64 {
        self.gain.get()
    }
    unsafe fn setParamNormalized(&self, _id: u32, v: f64) -> tresult {
        self.gain.set(v);
        kResultOk
    }
    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        kResultOk
    }
    unsafe fn createView(&self, name: *const c_char) -> *mut IPlugView {
        // SAFETY: the host's NUL-ended view name.
        if name.is_null() || unsafe { std::ffi::CStr::from_ptr(name) } != c"editor" {
            return ptr::null_mut();
        }
        ComWrapper::new(View).to_com_ptr::<IPlugView>().map_or(ptr::null_mut(), |p| p.into_raw())
    }
}

/// An editor with nothing in it, 320 by 200: enough for the host to make, show and close a
/// window round it.
struct View;

impl Class for View {
    type Interfaces = (IPlugView,);
}

impl IPlugViewTrait for View {
    unsafe fn isPlatformTypeSupported(&self, t: FIDString) -> tresult {
        // SAFETY: the host's NUL-ended type name.
        let t = unsafe { std::ffi::CStr::from_ptr(t) };
        if t == c"NSView" || t == c"HWND" { kResultTrue } else { kResultFalse }
    }
    unsafe fn attached(&self, parent: *mut c_void, _t: FIDString) -> tresult {
        if parent.is_null() { kInvalidArgument } else { kResultOk }
    }
    unsafe fn removed(&self) -> tresult {
        kResultOk
    }
    unsafe fn onWheel(&self, _d: f32) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyDown(&self, _k: char16, _c: int16, _m: int16) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyUp(&self, _k: char16, _c: int16, _m: int16) -> tresult {
        kResultFalse
    }
    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        // SAFETY: the host's rect to fill.
        unsafe { *size = ViewRect { left: 0, top: 0, right: 320, bottom: 200 } };
        kResultOk
    }
    unsafe fn onSize(&self, _s: *mut ViewRect) -> tresult {
        kResultOk
    }
    unsafe fn onFocus(&self, _f: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setFrame(&self, _f: *mut IPlugFrame) -> tresult {
        kResultOk
    }
    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }
    unsafe fn checkSizeConstraint(&self, _r: *mut ViewRect) -> tresult {
        kResultOk
    }
}

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory,);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let info = unsafe { &mut *info };
        copy_cstring(VENDOR, &mut info.vendor);
        copy_cstring("", &mut info.url);
        copy_cstring("", &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }
    unsafe fn countClasses(&self) -> i32 {
        2
    }
    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        let info = unsafe { &mut *info };
        let (cid, category) = match index {
            0 => (PROCESSOR_CID, "Audio Module Class"),
            1 => (CONTROLLER_CID, "Component Controller Class"),
            _ => return kInvalidArgument,
        };
        info.cid = cid;
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_cstring(category, &mut info.category);
        copy_cstring(NAME, &mut info.name);
        kResultOk
    }
    unsafe fn createInstance(&self, cid: FIDString, iid: FIDString, obj: *mut *mut c_void) -> tresult {
        let made = match unsafe { *(cid as *const TUID) } {
            PROCESSOR_CID => {
                ComWrapper::new(Processor { gain: AtomicU64::new(1.0f64.to_bits()), mode: AtomicU8::new(0) })
                    .to_com_ptr::<FUnknown>()
            }
            CONTROLLER_CID => ComWrapper::new(Controller { gain: std::cell::Cell::new(0.5) }).to_com_ptr::<FUnknown>(),
            _ => None,
        };
        let Some(made) = made else { return kInvalidArgument };
        let p = made.as_ptr();
        unsafe { ((*(*p).vtbl).queryInterface)(p, iid as *mut TUID, obj) }
    }
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn InitDll() -> bool {
    true
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn ExitDll() -> bool {
    true
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "system" fn bundleEntry(_bundle: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "system" fn bundleExit() -> bool {
    true
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
extern "system" fn ModuleEntry(_lib: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
extern "system" fn ModuleExit() -> bool {
    true
}

#[unsafe(no_mangle)]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    ComWrapper::new(Factory).to_com_ptr::<IPluginFactory>().map_or(ptr::null_mut(), |p| p.into_raw())
}
