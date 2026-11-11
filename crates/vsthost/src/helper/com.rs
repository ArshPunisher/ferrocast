//! What a host hands a VST3 plugin: who's hosting it, messages between its two halves, the
//! parameter changes its window makes, and streams its state is read from and written to.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, Mutex};

use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper, Interface};

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        n += 1;
    }
    if n < dst.len() {
        dst[n] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

/// The same 16 bytes (a TUID is signed chars, a Guid bytes).
fn guid(t: &TUID) -> [u8; 16] {
    t.map(|b| b as u8)
}

/// Ferrocast, to the plugin; it makes the messages and attribute lists plugins ask a host for.
pub struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        // SAFETY: the plugin hands a String128 to fill.
        copy_wstring("Ferrocast", unsafe { &mut *name });
        kResultOk
    }

    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        // SAFETY: both ids are 16 bytes the plugin owns for the call.
        let (cid, iid) = unsafe { (guid(&*cid), guid(&*iid)) };
        let made = if cid == IMessage::IID && iid == IMessage::IID {
            ComWrapper::new(Message::default()).to_com_ptr::<IMessage>().map(|p| p.into_raw().cast::<c_void>())
        } else if cid == IAttributeList::IID && iid == IAttributeList::IID {
            ComWrapper::new(Attributes::default()).to_com_ptr::<IAttributeList>().map(|p| p.into_raw().cast::<c_void>())
        } else {
            None
        };
        match made {
            Some(p) => {
                // SAFETY: `obj` is where the plugin wants the new object; it now owns that reference.
                unsafe { *obj = p };
                kResultOk
            }
            None => {
                // SAFETY: as above.
                unsafe { *obj = std::ptr::null_mut() };
                kNoInterface
            }
        }
    }
}

/// A message between a plugin's processor and its controller.
#[derive(Default)]
pub struct Message {
    id: RefCell<Option<CString>>,
    attrs: RefCell<Option<vst3::ComPtr<IAttributeList>>>,
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.borrow().as_ref().map_or(std::ptr::null(), |c| c.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let id = (!id.is_null())
            // SAFETY: a NUL-ended string the plugin owns for the call; it's copied.
            .then(|| unsafe { CStr::from_ptr(id) }.to_owned());
        *self.id.borrow_mut() = id;
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        let mut a = self.attrs.borrow_mut();
        if a.is_none() {
            *a = ComWrapper::new(Attributes::default()).to_com_ptr::<IAttributeList>();
        }
        // Borrowed: the message keeps its reference.
        a.as_ref().map_or(std::ptr::null_mut(), |p| p.as_ptr())
    }
}

#[derive(Debug, Clone)]
enum Attr {
    Int(i64),
    Float(f64),
    Text(Vec<u16>),
    Bytes(Vec<u8>),
}

/// A message's values by name.
#[derive(Default)]
pub struct Attributes {
    map: RefCell<HashMap<CString, Attr>>,
}

impl Class for Attributes {
    type Interfaces = (IAttributeList,);
}

/// The id as an owned key (None for a null one).
fn key(id: AttrID) -> Option<CString> {
    // SAFETY: a NUL-ended id the plugin owns for the call; it's copied.
    (!id.is_null()).then(|| unsafe { CStr::from_ptr(id as *const c_char) }.to_owned())
}

impl IAttributeListTrait for Attributes {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        self.map.borrow_mut().insert(k, Attr::Int(value));
        kResultOk
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match key(id).and_then(|k| self.map.borrow().get(&k).cloned()) {
            Some(Attr::Int(v)) => {
                // SAFETY: an int64 for the answer.
                unsafe { *value = v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        self.map.borrow_mut().insert(k, Attr::Float(value));
        kResultOk
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match key(id).and_then(|k| self.map.borrow().get(&k).cloned()) {
            Some(Attr::Float(v)) => {
                // SAFETY: an f64 for the answer.
                unsafe { *value = v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut text = Vec::new();
        // SAFETY: a NUL-ended UTF-16 string the plugin owns for the call (bounded, in case).
        unsafe {
            let mut p = string;
            while *p != 0 && text.len() < 1 << 16 {
                text.push(*p);
                p = p.add(1);
            }
        }
        self.map.borrow_mut().insert(k, Attr::Text(text));
        kResultOk
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let Some(Attr::Text(t)) = key(id).and_then(|k| self.map.borrow().get(&k).cloned()) else {
            return kResultFalse;
        };
        let room = size_in_bytes as usize / 2;
        if room == 0 || string.is_null() {
            return kInvalidArgument;
        }
        let n = t.len().min(room - 1);
        // SAFETY: the plugin's buffer holds `room` TChars; n + 1 of them are written.
        unsafe {
            for (i, c) in t.iter().take(n).enumerate() {
                *string.add(i) = *c as TChar;
            }
            *string.add(n) = 0;
        }
        kResultOk
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        let bytes = if data.is_null() || size_in_bytes == 0 {
            Vec::new()
        } else {
            // SAFETY: the plugin's `size_in_bytes` bytes, read for the call; they're copied.
            unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size_in_bytes as usize) }.to_vec()
        };
        self.map.borrow_mut().insert(k, Attr::Bytes(bytes));
        kResultOk
    }

    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        let map = self.map.borrow();
        let Some(Attr::Bytes(b)) = map.get(&k) else { return kResultFalse };
        // SAFETY: pointers for the answer. The bytes stay put while the list lives and nothing
        // sets that id again (as the SDK's own list promises).
        unsafe {
            *data = b.as_ptr().cast();
            *size_in_bytes = b.len() as u32;
        }
        kResultOk
    }
}

/// Parameter changes the plugin's window made, waiting for the next block.
pub type Pending = Arc<Mutex<Vec<(u32, f64)>>>;

/// What the controller tells the host: an edit goes to the processor at the next block, and the
/// state counts as changed.
pub struct Handler {
    pub pending: Pending,
    pub changed: Arc<std::sync::atomic::AtomicBool>,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        if let Ok(mut p) = self.pending.lock() {
            p.push((id, value));
        }
        self.changed.store(true, std::sync::atomic::Ordering::Release);
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        self.changed.store(true, std::sync::atomic::Ordering::Release);
        kResultOk
    }

    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        self.changed.store(true, std::sync::atomic::Ordering::Release);
        kResultOk
    }
}

/// One parameter's changes in a block.
pub struct Queue {
    id: u32,
    points: RefCell<Vec<(i32, f64)>>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.points.borrow().len() as int32
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        let Some((at, v)) = usize::try_from(index).ok().and_then(|i| self.points.borrow().get(i).copied()) else {
            return kInvalidArgument;
        };
        // SAFETY: two values for the answer.
        unsafe {
            *offset = at;
            *value = v;
        }
        kResultTrue
    }

    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        let mut p = self.points.borrow_mut();
        p.push((offset, value));
        if !index.is_null() {
            // SAFETY: where the plugin wants the point's index.
            unsafe { *index = p.len() as int32 - 1 };
        }
        kResultOk
    }
}

/// A block's parameter changes, in or out.
#[derive(Default)]
pub struct Changes {
    queues: RefCell<Vec<vst3::ComPtr<IParamValueQueue>>>,
    ids: RefCell<Vec<u32>>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl Changes {
    /// Changes from the window's edits: each parameter's last value, at the block's start.
    pub fn of(edits: &[(u32, f64)]) -> Changes {
        let c = Changes::default();
        for &(id, v) in edits {
            let at = c.ids.borrow().iter().position(|&i| i == id);
            match at {
                Some(i) => {
                    if let Some(q) = c.queues.borrow().get(i) {
                        // SAFETY: our own queue; a point at offset 0 replaces nothing, it's added.
                        unsafe { q.addPoint(0, v, std::ptr::null_mut()) };
                    }
                }
                None => {
                    if let Some(q) = ComWrapper::new(Queue { id, points: RefCell::new(vec![(0, v)]) })
                        .to_com_ptr::<IParamValueQueue>()
                    {
                        c.queues.borrow_mut().push(q);
                        c.ids.borrow_mut().push(id);
                    }
                }
            }
        }
        c
    }
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> int32 {
        self.queues.borrow().len() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.queues.borrow().get(i).map(|q| q.as_ptr()))
            .unwrap_or(std::ptr::null_mut())
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: the plugin's parameter id for the call.
        let id = unsafe { *id };
        let at = self.ids.borrow().iter().position(|&i| i == id);
        let i = match at {
            Some(i) => i,
            None => {
                let Some(q) =
                    ComWrapper::new(Queue { id, points: RefCell::new(Vec::new()) }).to_com_ptr::<IParamValueQueue>()
                else {
                    return std::ptr::null_mut();
                };
                self.queues.borrow_mut().push(q);
                self.ids.borrow_mut().push(id);
                self.ids.borrow().len() - 1
            }
        };
        if !index.is_null() {
            // SAFETY: where the plugin wants the queue's index.
            unsafe { *index = i as int32 };
        }
        self.queues.borrow().get(i).map_or(std::ptr::null_mut(), |q| q.as_ptr())
    }
}

/// A state, in memory, read or written by the plugin.
#[derive(Default)]
pub struct Stream {
    pub bytes: RefCell<Vec<u8>>,
    at: Cell<usize>,
}

impl Stream {
    pub fn of(bytes: &[u8]) -> Stream {
        Stream { bytes: RefCell::new(bytes.to_vec()), at: Cell::new(0) }
    }
}

impl Class for Stream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for Stream {
    unsafe fn read(&self, buffer: *mut c_void, n: int32, read: *mut int32) -> tresult {
        let b = self.bytes.borrow();
        let at = self.at.get().min(b.len());
        let n = usize::try_from(n).unwrap_or(0).min(b.len() - at);
        if n > 0 && !buffer.is_null() {
            // SAFETY: the plugin's buffer holds at least the `n` bytes it asked for.
            unsafe { std::ptr::copy_nonoverlapping(b[at..].as_ptr(), buffer.cast::<u8>(), n) };
        }
        self.at.set(at + n);
        if !read.is_null() {
            // SAFETY: where the plugin wants the count.
            unsafe { *read = n as int32 };
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, n: int32, written: *mut int32) -> tresult {
        let n = usize::try_from(n).unwrap_or(0);
        if n > crate::proto::MAX_FRAME {
            return kOutOfMemory;
        }
        let mut b = self.bytes.borrow_mut();
        let at = self.at.get();
        if b.len() < at + n {
            b.resize(at + n, 0);
        }
        if n > 0 && !buffer.is_null() {
            // SAFETY: the plugin's `n` bytes, read for the call.
            unsafe { std::ptr::copy_nonoverlapping(buffer.cast::<u8>(), b[at..].as_mut_ptr(), n) };
        }
        self.at.set(at + n);
        if !written.is_null() {
            // SAFETY: where the plugin wants the count.
            unsafe { *written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let len = self.bytes.borrow().len() as i64;
        let base = match mode as u32 {
            m if m == IBStream_::IStreamSeekMode_::kIBSeekSet => 0,
            m if m == IBStream_::IStreamSeekMode_::kIBSeekCur => self.at.get() as i64,
            m if m == IBStream_::IStreamSeekMode_::kIBSeekEnd => len,
            _ => return kInvalidArgument,
        };
        let to = (base + pos).clamp(0, len.max(base + pos).max(0));
        self.at.set(to as usize);
        if !result.is_null() {
            // SAFETY: where the plugin wants the new place.
            unsafe { *result = to };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: where the plugin wants the place.
        unsafe { *pos = self.at.get() as i64 };
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// An edit in the plugin's window is kept for the next block, as a parameter change at its
    /// start (each parameter's points in order), and marks the state as changed.
    #[test]
    #[allow(unsafe_code)]
    fn window_edits_reach_the_next_block() {
        let pending: Pending = Arc::default();
        let changed = Arc::new(AtomicBool::new(false));
        let h = ComWrapper::new(Handler { pending: pending.clone(), changed: changed.clone() });
        let h = h.to_com_ptr::<IComponentHandler>().unwrap_or_else(|| panic!("a handler"));
        // SAFETY: our own handler.
        unsafe {
            h.performEdit(0, 0.25);
            h.performEdit(3, 1.0);
            h.performEdit(0, 0.75);
        }
        assert!(changed.load(Ordering::Acquire));
        let edits = pending.lock().map(|p| p.clone()).unwrap_or_default();
        assert_eq!(edits, vec![(0, 0.25), (3, 1.0), (0, 0.75)]);
        let c = ComWrapper::new(Changes::of(&edits));
        let c = c.to_com_ptr::<IParameterChanges>().unwrap_or_else(|| panic!("changes"));
        // SAFETY: our own objects, read back.
        unsafe {
            assert_eq!(c.getParameterCount(), 2, "one queue per parameter");
            let q = vst3::ComRef::from_raw(c.getParameterData(0)).unwrap_or_else(|| panic!("a queue"));
            assert_eq!((q.getParameterId(), q.getPointCount()), (0, 2));
            let (mut at, mut v) = (0, 0.0);
            assert_eq!(q.getPoint(1, &mut at, &mut v), kResultTrue);
            assert_eq!((at, v), (0, 0.75), "its last value last");
            assert!(c.getParameterData(5).is_null());
        }
    }

    /// A state stream reads back what was written, and seeks.
    #[test]
    #[allow(unsafe_code)]
    fn a_stream_reads_what_was_written() {
        let s = ComWrapper::new(Stream::default());
        let p = s.to_com_ptr::<IBStream>().unwrap_or_else(|| panic!("a stream"));
        let mut data = *b"hello";
        let (mut n, mut pos) = (0i32, 0i64);
        // SAFETY: our own stream and buffers of the sizes given.
        unsafe {
            assert_eq!(p.write(data.as_mut_ptr().cast(), 5, &mut n), kResultOk);
            assert_eq!(n, 5);
            p.seek(1, IBStream_::IStreamSeekMode_::kIBSeekSet as i32, &mut pos);
            let mut back = [0u8; 8];
            p.read(back.as_mut_ptr().cast(), 8, &mut n);
            assert_eq!((n, &back[..4]), (4, &b"ello"[..]), "only what's there");
        }
    }
}
