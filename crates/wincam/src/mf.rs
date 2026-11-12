//! The Media Foundation camera (Windows 11): a live media source with one video stream, which
//! the camera frame server loads when Ferrocast calls `MFCreateVirtualCamera`. Every camera app
//! then sees "Ferrocast Camera". Each sample asked for is filled from [`Feed`] at once (the
//! frame server paces the requests).
//!
//! Media Foundation's functions are looked up when first needed rather than imported, so the
//! DLL still loads where Media Foundation isn't installed (Windows "N" editions), for the
//! DirectShow camera.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use ferrocast_camshare::caps::{self, Mode};
use ferrocast_camshare::feed::{Feed, Format};
use ferrocast_camshare::{NAME_GLOBAL, NAME_LOCAL, Reader, now_ms};
use windows::Win32::Foundation::{E_NOTIMPL, E_POINTER, HMODULE};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFGetService, IMFGetService_Impl, IMFMediaBuffer, IMFMediaEvent,
    IMFMediaEventGenerator_Impl, IMFMediaEventQueue, IMFMediaSource, IMFMediaSource_Impl, IMFMediaSourceEx,
    IMFMediaSourceEx_Impl, IMFMediaStream, IMFMediaStream_Impl, IMFMediaStream2, IMFMediaStream2_Impl, IMFMediaType,
    IMFPresentationDescriptor, IMFSample, IMFSampleAllocatorControl, IMFSampleAllocatorControl_Impl,
    IMFStreamDescriptor, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MEMediaSample, MENewStream, MESourceStarted,
    MESourceStopped, MEStreamStarted, MEStreamStopped, MEUpdatedStream, MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
    MF_DEVICESTREAM_FRAMESERVER_SHARED, MF_DEVICESTREAM_STREAM_CATEGORY, MF_DEVICESTREAM_STREAM_ID,
    MF_E_INVALID_STATE_TRANSITION, MF_E_MEDIA_SOURCE_WRONGSTATE, MF_E_SHUTDOWN, MF_E_UNSUPPORTED_SERVICE,
    MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_DEFAULT_STRIDE, MF_MT_FIXED_SIZE_SAMPLES, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE,
    MF_STREAM_STATE, MF_STREAM_STATE_RUNNING, MF_STREAM_STATE_STOPPED, MFFrameSourceTypes_Color, MFMEDIASOURCE_IS_LIVE,
    MFMediaType_Video, MFSampleAllocatorUsage, MFSampleAllocatorUsage_UsesCustomAllocator, MFSampleExtension_Token,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HRESULT, IUnknown, Interface, Ref, implement, s, w};

/// ERROR_SET_NOT_FOUND as an HRESULT: what a camera without KS properties answers.
const SET_NOT_FOUND: HRESULT = HRESULT(0x8007_0492_u32 as i32);

// ------------------------------------------------------------------ Media Foundation, looked up

type CreateEventQueue = unsafe extern "system" fn(*mut *mut std::ffi::c_void) -> HRESULT;
type CreateAttributes = unsafe extern "system" fn(*mut *mut std::ffi::c_void, u32) -> HRESULT;
type CreateMediaType = unsafe extern "system" fn(*mut *mut std::ffi::c_void) -> HRESULT;
type CreateStreamDescriptor =
    unsafe extern "system" fn(u32, u32, *const *mut std::ffi::c_void, *mut *mut std::ffi::c_void) -> HRESULT;
type CreatePresentationDescriptor =
    unsafe extern "system" fn(u32, *const *mut std::ffi::c_void, *mut *mut std::ffi::c_void) -> HRESULT;
type CreateSample = unsafe extern "system" fn(*mut *mut std::ffi::c_void) -> HRESULT;
type CreateMemoryBuffer = unsafe extern "system" fn(u32, *mut *mut std::ffi::c_void) -> HRESULT;
type GetSystemTime = unsafe extern "system" fn() -> i64;

/// mfplat.dll's functions this source uses.
struct Mf {
    event_queue: CreateEventQueue,
    attributes: CreateAttributes,
    media_type: CreateMediaType,
    stream_descriptor: CreateStreamDescriptor,
    presentation_descriptor: CreatePresentationDescriptor,
    sample: CreateSample,
    memory_buffer: CreateMemoryBuffer,
    system_time: GetSystemTime,
}

fn mf() -> windows::core::Result<&'static Mf> {
    static MF: OnceLock<Option<Mf>> = OnceLock::new();
    MF.get_or_init(|| {
        // SAFETY: loads a system DLL by name; kept loaded for the process's life.
        let lib: HMODULE = unsafe { LoadLibraryW(w!("mfplat.dll")) }.ok()?;
        macro_rules! get {
            ($name:literal, $t:ty) => {{
                // SAFETY: an export of mfplat.dll with this documented signature.
                let f = unsafe { GetProcAddress(lib, s!($name)) }?;
                // SAFETY: as above: the same function, typed.
                unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $t>(f) }
            }};
        }
        Some(Mf {
            event_queue: get!("MFCreateEventQueue", CreateEventQueue),
            attributes: get!("MFCreateAttributes", CreateAttributes),
            media_type: get!("MFCreateMediaType", CreateMediaType),
            stream_descriptor: get!("MFCreateStreamDescriptor", CreateStreamDescriptor),
            presentation_descriptor: get!("MFCreatePresentationDescriptor", CreatePresentationDescriptor),
            sample: get!("MFCreateSample", CreateSample),
            memory_buffer: get!("MFCreateMemoryBuffer", CreateMemoryBuffer),
            system_time: get!("MFGetSystemTime", GetSystemTime),
        })
    })
    .as_ref()
    .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_NOINTERFACE))
}

/// Takes an out-pointer a creating function filled into `T`.
///
/// # Safety
/// `raw` must be null or a counted pointer to a `T`.
unsafe fn take<T: Interface>(hr: HRESULT, raw: *mut std::ffi::c_void) -> windows::core::Result<T> {
    hr.ok()?;
    if raw.is_null() {
        return Err(E_POINTER.into());
    }
    // SAFETY: the caller's word: one counted T, now ours.
    Ok(unsafe { T::from_raw(raw) })
}

fn new_attributes(n: u32) -> windows::core::Result<IMFAttributes> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: mfplat's function, filling `raw`.
    let hr = unsafe { (mf()?.attributes)(&mut raw, n) };
    // SAFETY: an IMFAttributes on success.
    unsafe { take(hr, raw) }
}

fn new_event_queue() -> windows::core::Result<IMFMediaEventQueue> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: as above.
    let hr = unsafe { (mf()?.event_queue)(&mut raw) };
    // SAFETY: an IMFMediaEventQueue on success.
    unsafe { take(hr, raw) }
}

/// An NV12 type of `m`'s size and rate.
fn media_type(m: &Mode) -> windows::core::Result<IMFMediaType> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: as above.
    let hr = unsafe { (mf()?.media_type)(&mut raw) };
    // SAFETY: an IMFMediaType on success.
    let t: IMFMediaType = unsafe { take(hr, raw) }?;
    let pack = |hi: u32, lo: u32| (u64::from(hi) << 32) | u64::from(lo);
    let (num, den) = (10_000_000u32, m.interval.max(1) as u32);
    // SAFETY: setting attributes of our own type.
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(m.width, m.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(num, den))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        t.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        t.SetUINT32(&MF_MT_SAMPLE_SIZE, m.frame_len() as u32)?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, m.width)?;
    }
    Ok(t)
}

/// The mode a media type is (NV12 only: the frame server converts for apps).
fn mode_of(t: &IMFMediaType) -> Option<Mode> {
    // SAFETY: reading attributes of a media type.
    let (size, rate) = unsafe { (t.GetUINT64(&MF_MT_FRAME_SIZE).ok()?, t.GetUINT64(&MF_MT_FRAME_RATE).ok()) };
    let (num, den) = rate.map_or((30, 1), |r| ((r >> 32) as u32, r as u32));
    let m = Mode {
        width: (size >> 32) as u32,
        height: size as u32,
        format: Format::Nv12,
        interval: caps::interval(num, den),
    };
    caps::acceptable(&m).then_some(m)
}

/// The feed for the frame server: the region it makes in `Global\` for Ferrocast to write
/// into; else (made in an app, as the tests do) the session's own.
fn new_feed() -> Feed {
    match ferrocast_camshare::win::create(NAME_GLOBAL, Some(ferrocast_camshare::win::SDDL_SHARED)) {
        Ok((region, _)) => Feed::with_reader(Reader::new_made(region)),
        Err(_) => Feed::new(|| ferrocast_camshare::win::open(NAME_LOCAL, false)),
    }
}

// ------------------------------------------------------------------ the source

struct SourceState {
    shut: bool,
    /// The stream was selected at the last Start (its next Start is an update, not new).
    streaming: bool,
    /// The stream object, made once.
    stream: Option<IMFMediaStream>,
}

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl, IMFSampleAllocatorControl)]
pub(crate) struct Source {
    events: IMFMediaEventQueue,
    attributes: IMFAttributes,
    pd: IMFPresentationDescriptor,
    state: Mutex<SourceState>,
    shared: Arc<StreamShared>,
}

/// What the stream needs of the source.
struct StreamShared {
    events: IMFMediaEventQueue,
    attributes: IMFAttributes,
    sd: IMFStreamDescriptor,
    inner: Mutex<StreamInner>,
}

// SAFETY: Media Foundation's event queues, attribute stores and descriptors are free-threaded
// (the frame server calls sources from its own worker threads); the rest is behind a Mutex.
unsafe impl Send for StreamShared {}
// SAFETY: as above.
unsafe impl Sync for StreamShared {}

struct StreamInner {
    state: MF_STREAM_STATE,
    shut: bool,
    /// The source, not counted (it holds the stream); handed out counted.
    source: *mut std::ffi::c_void,
    feed: Option<Feed>,
    nv12: Vec<u8>,
}

// SAFETY: the raw source pointer is only handed out (counted) under the lock while the source
// is alive (it's cleared at shutdown); the rest is plain data and free-threaded MF objects.
unsafe impl Send for StreamInner {}

impl Source {
    pub(crate) fn create() -> windows::core::Result<IUnknown> {
        let native = new_feed().peek(now_ms());
        // Ferrocast's own size first when it's sending, then the common ones, all NV12.
        let modes: Vec<Mode> = caps::offered(native).into_iter().filter(|m| m.format == Format::Nv12).collect();
        let types: Vec<Option<IMFMediaType>> = modes.iter().map(|m| media_type(m).ok()).collect();
        let api = mf()?;
        let mut raw = std::ptr::null_mut();
        let ptrs: Vec<*mut std::ffi::c_void> =
            types.iter().map(|t| t.as_ref().map_or(std::ptr::null_mut(), Interface::as_raw)).collect();
        // SAFETY: mfplat's function over our types (alive through the call).
        let hr = unsafe { (api.stream_descriptor)(0, ptrs.len() as u32, ptrs.as_ptr(), &mut raw) };
        // SAFETY: an IMFStreamDescriptor on success.
        let sd: IMFStreamDescriptor = unsafe { take(hr, raw) }?;
        // SAFETY: the descriptor's own type handler; the first type is the current one.
        unsafe {
            let handler = sd.GetMediaTypeHandler()?;
            if let Some(Some(first)) = types.first() {
                handler.SetCurrentMediaType(first)?;
            }
        }
        let mut raw = std::ptr::null_mut();
        let sdp = sd.as_raw();
        // SAFETY: as above, one stream.
        let hr = unsafe { (api.presentation_descriptor)(1, &sdp, &mut raw) };
        // SAFETY: an IMFPresentationDescriptor on success.
        let pd: IMFPresentationDescriptor = unsafe { take(hr, raw) }?;
        // SAFETY: selecting our one stream.
        unsafe { pd.SelectStream(0) }?;
        // What the frame server reads to know it's a colour camera stream it may share.
        let sattrs = new_attributes(4)?;
        // SAFETY: setting attributes of our own store.
        unsafe {
            sattrs.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
            sattrs.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
            sattrs.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
            sattrs.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
        }
        let shared = Arc::new(StreamShared {
            events: new_event_queue()?,
            attributes: sattrs,
            sd,
            inner: Mutex::new(StreamInner {
                state: MF_STREAM_STATE_STOPPED,
                shut: false,
                source: std::ptr::null_mut(),
                feed: None,
                nv12: Vec::new(),
            }),
        });
        let source = windows_core::ComObject::new(Source {
            events: new_event_queue()?,
            attributes: new_attributes(1)?,
            pd,
            state: Mutex::new(SourceState { shut: false, streaming: false, stream: None }),
            shared: shared.clone(),
        });
        // Not counted: the source owns the stream, which hands the source out counted.
        inner(&shared).source = source.as_interface::<IMFMediaSourceEx>().as_raw();
        let stream: IMFMediaStream2 = Stream { shared }.into();
        let stream: IMFMediaStream = stream.into();
        source.lock().stream = Some(stream);
        Ok(source.to_interface::<IUnknown>())
    }

    fn lock(&self) -> MutexGuard<'_, SourceState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // Released without Shutdown: the stream (which an app may still hold) must not hand
        // out a source that's gone.
        let mut s = inner(&self.shared);
        s.source = std::ptr::null_mut();
        s.shut = true;
    }
}

fn inner(s: &StreamShared) -> MutexGuard<'_, StreamInner> {
    s.inner.lock().unwrap_or_else(|p| p.into_inner())
}

impl IMFMediaEventGenerator_Impl for Source_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
        // SAFETY: the queue's own call (it waits when asked to, outside our lock).
        unsafe { self.events.GetEvent(flags.0) }
    }

    fn BeginGetEvent(&self, cb: Ref<'_, IMFAsyncCallback>, state: Ref<'_, IUnknown>) -> windows::core::Result<()> {
        // SAFETY: as above.
        unsafe { self.events.BeginGetEvent(cb.as_ref(), state.as_ref()) }
    }

    fn EndGetEvent(&self, result: Ref<'_, IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        // SAFETY: as above.
        unsafe { self.events.EndGetEvent(result.as_ref()) }
    }

    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        hr: HRESULT,
        value: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        // SAFETY: as above, passing the caller's values on.
        unsafe { self.events.QueueEventParamVar(met, ext, hr, value) }
    }
}

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> windows::core::Result<u32> {
        if self.lock().shut {
            return Err(MF_E_SHUTDOWN.into());
        }
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> windows::core::Result<IMFPresentationDescriptor> {
        if self.lock().shut {
            return Err(MF_E_SHUTDOWN.into());
        }
        // SAFETY: a copy of ours for the caller.
        unsafe { self.pd.Clone() }
    }

    fn Start(
        &self,
        pd: Ref<'_, IMFPresentationDescriptor>,
        _format: *const GUID,
        _at: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        let mut st = self.lock();
        if st.shut {
            return Err(MF_E_SHUTDOWN.into());
        }
        let pd = pd.ok()?;
        // The stream asked for, with the type the caller set on it.
        let mut selected = windows::core::BOOL::default();
        let mut sd: Option<IMFStreamDescriptor> = None;
        // SAFETY: reading the caller's descriptor.
        unsafe { pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd) }?;
        let sd = sd.ok_or(windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG))?;
        // SAFETY: as above.
        let mode =
            unsafe { sd.GetMediaTypeHandler().and_then(|h| h.GetCurrentMediaType()) }.ok().and_then(|t| mode_of(&t));
        let stream = st.stream.clone().ok_or(windows::core::Error::from(MF_E_MEDIA_SOURCE_WRONGSTATE))?;
        let selected = selected.as_bool();
        {
            let mut s = inner(&self.shared);
            // A stream the caller left out stays stopped and gives no samples.
            s.state = if selected { MF_STREAM_STATE_RUNNING } else { MF_STREAM_STATE_STOPPED };
            if selected && s.feed.is_none() {
                s.feed = Some(new_feed());
            }
            if let Some(m) = mode {
                s.nv12.resize(m.frame_len(), 0);
            }
        }
        let was = std::mem::replace(&mut st.streaming, selected);
        // MESourceStarted carries when it started (VT_I8, the clock samples are stamped with).
        let at = mf().map(|api| {
            // SAFETY: mfplat's clock.
            let now = unsafe { (api.system_time)() };
            PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_I8,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 { hVal: now },
                    }),
                },
            }
        })?;
        // SAFETY: queuing events on our own queues (the values are copied).
        unsafe {
            if selected {
                // New when it wasn't streaming before (first Start, or deselected last time).
                let kind = if was { MEUpdatedStream } else { MENewStream };
                self.events.QueueEventParamUnk(kind.0 as u32, &GUID::zeroed(), HRESULT(0), &stream)?;
            }
            self.events.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), HRESULT(0), &at)?;
            if selected {
                self.shared.events.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), HRESULT(0), &at)?;
            }
        }
        Ok(())
    }

    fn Stop(&self) -> windows::core::Result<()> {
        if self.lock().shut {
            return Err(MF_E_SHUTDOWN.into());
        }
        inner(&self.shared).state = MF_STREAM_STATE_STOPPED;
        let none = std::ptr::null();
        // SAFETY: queuing events on our own queues.
        unsafe {
            self.shared.events.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), HRESULT(0), none)?;
            self.events.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), HRESULT(0), none)?;
        }
        Ok(())
    }

    fn Pause(&self) -> windows::core::Result<()> {
        // A live camera doesn't pause.
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }

    fn Shutdown(&self) -> windows::core::Result<()> {
        let mut st = self.lock();
        if st.shut {
            return Ok(());
        }
        st.shut = true;
        st.stream = None;
        {
            let mut s = inner(&self.shared);
            s.shut = true;
            s.source = std::ptr::null_mut();
            s.feed = None;
        }
        // SAFETY: ends both queues (waiting callers get MF_E_SHUTDOWN).
        unsafe {
            let _ = self.shared.events.Shutdown();
            let _ = self.events.Shutdown();
        }
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> windows::core::Result<IMFAttributes> {
        Ok(self.attributes.clone())
    }

    fn GetStreamAttributes(&self, id: u32) -> windows::core::Result<IMFAttributes> {
        if id != 0 {
            return Err(windows::Win32::Foundation::E_INVALIDARG.into());
        }
        Ok(self.shared.attributes.clone())
    }

    fn SetD3DManager(&self, _manager: Ref<'_, IUnknown>) -> windows::core::Result<()> {
        // Frames are in system memory: nothing to do with a device.
        Ok(())
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(
        &self,
        _service: *const GUID,
        _iid: *const GUID,
        _out: *mut *mut std::ffi::c_void,
    ) -> windows::core::Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for Source_Impl {
    fn KsProperty(
        &self,
        _p: *const KSIDENTIFIER,
        _pl: u32,
        _d: *mut std::ffi::c_void,
        _dl: u32,
        _r: *mut u32,
    ) -> windows::core::Result<()> {
        Err(SET_NOT_FOUND.into())
    }

    fn KsMethod(
        &self,
        _p: *const KSIDENTIFIER,
        _pl: u32,
        _d: *mut std::ffi::c_void,
        _dl: u32,
        _r: *mut u32,
    ) -> windows::core::Result<()> {
        Err(SET_NOT_FOUND.into())
    }

    fn KsEvent(
        &self,
        _p: *const KSIDENTIFIER,
        _pl: u32,
        _d: *mut std::ffi::c_void,
        _dl: u32,
        _r: *mut u32,
    ) -> windows::core::Result<()> {
        Err(SET_NOT_FOUND.into())
    }
}

impl IMFSampleAllocatorControl_Impl for Source_Impl {
    fn SetDefaultAllocator(&self, _stream: u32, _allocator: Ref<'_, IUnknown>) -> windows::core::Result<()> {
        // We make our own samples (system memory).
        Ok(())
    }

    fn GetAllocatorUsage(
        &self,
        stream: u32,
        input: *mut u32,
        usage: *mut MFSampleAllocatorUsage,
    ) -> windows::core::Result<()> {
        if input.is_null() || usage.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: the caller's out-values.
        unsafe {
            input.write(stream);
            usage.write(MFSampleAllocatorUsage_UsesCustomAllocator);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ the stream

#[implement(IMFMediaStream2, IKsControl)]
struct Stream {
    shared: Arc<StreamShared>,
}

impl Stream {
    /// One sample of what to show now, NV12 at the stream's current type.
    fn sample(&self) -> windows::core::Result<IMFSample> {
        // SAFETY: reading our own descriptor's current type.
        let t = unsafe { self.shared.sd.GetMediaTypeHandler().and_then(|h| h.GetCurrentMediaType()) }?;
        let m = mode_of(&t).ok_or(windows::core::Error::from(MF_E_MEDIA_SOURCE_WRONGSTATE))?;
        let api = mf()?;
        let len = m.frame_len();
        let mut raw = std::ptr::null_mut();
        // SAFETY: mfplat's function.
        let hr = unsafe { (api.memory_buffer)(len as u32, &mut raw) };
        // SAFETY: an IMFMediaBuffer on success.
        let buffer: IMFMediaBuffer = unsafe { take(hr, raw) }?;
        {
            let mut s = inner(&self.shared);
            let s = &mut *s;
            let feed = s.feed.get_or_insert_with(new_feed);
            let mut ptr = std::ptr::null_mut();
            let mut max = 0u32;
            // SAFETY: locks the buffer we just made, `max` bytes, until Unlock below.
            unsafe { buffer.Lock(&mut ptr, Some(&mut max), None) }?;
            if !ptr.is_null() && max as usize >= len {
                // SAFETY: the locked buffer, `len` bytes of it ours to fill.
                let dst = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
                feed.fill(now_ms(), (m.width, m.height), Format::Nv12, dst);
            }
            // SAFETY: paired with Lock above.
            unsafe {
                buffer.Unlock()?;
                buffer.SetCurrentLength(len as u32)?;
            }
        }
        let mut raw = std::ptr::null_mut();
        // SAFETY: mfplat's function.
        let hr = unsafe { (api.sample)(&mut raw) };
        // SAFETY: an IMFSample on success.
        let sample: IMFSample = unsafe { take(hr, raw) }?;
        // SAFETY: filling our own sample; the time is the system clock frame servers expect.
        unsafe {
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((api.system_time)())?;
            sample.SetSampleDuration(m.interval)?;
        }
        Ok(sample)
    }
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
        // SAFETY: the queue's own call.
        unsafe { self.shared.events.GetEvent(flags.0) }
    }

    fn BeginGetEvent(&self, cb: Ref<'_, IMFAsyncCallback>, state: Ref<'_, IUnknown>) -> windows::core::Result<()> {
        // SAFETY: as above.
        unsafe { self.shared.events.BeginGetEvent(cb.as_ref(), state.as_ref()) }
    }

    fn EndGetEvent(&self, result: Ref<'_, IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        // SAFETY: as above.
        unsafe { self.shared.events.EndGetEvent(result.as_ref()) }
    }

    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        hr: HRESULT,
        value: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        // SAFETY: as above.
        unsafe { self.shared.events.QueueEventParamVar(met, ext, hr, value) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> windows::core::Result<IMFMediaSource> {
        let s = inner(&self.shared);
        if s.shut {
            return Err(MF_E_SHUTDOWN.into());
        }
        // SAFETY: the source is alive while not shut down; borrowed then counted for the caller.
        unsafe { IMFMediaSourceEx::from_raw_borrowed(&s.source) }
            .and_then(|x| x.cast::<IMFMediaSource>().ok())
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> windows::core::Result<IMFStreamDescriptor> {
        if inner(&self.shared).shut {
            return Err(MF_E_SHUTDOWN.into());
        }
        Ok(self.shared.sd.clone())
    }

    fn RequestSample(&self, token: Ref<'_, IUnknown>) -> windows::core::Result<()> {
        {
            let s = inner(&self.shared);
            if s.shut {
                return Err(MF_E_SHUTDOWN.into());
            }
            if s.state != MF_STREAM_STATE_RUNNING {
                return Err(MF_E_MEDIA_SOURCE_WRONGSTATE.into());
            }
        }
        let sample = self.sample()?;
        if let Some(t) = token.as_ref() {
            // SAFETY: the frame server's token, carried back on the sample.
            unsafe { sample.SetUnknown(&MFSampleExtension_Token, t) }?;
        }
        // SAFETY: queuing the sample on our own queue.
        unsafe { self.shared.events.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), HRESULT(0), &sample) }
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> windows::core::Result<()> {
        let mut s = inner(&self.shared);
        if s.shut {
            return Err(MF_E_SHUTDOWN.into());
        }
        s.state = value;
        Ok(())
    }

    fn GetStreamState(&self) -> windows::core::Result<MF_STREAM_STATE> {
        let s = inner(&self.shared);
        if s.shut {
            return Err(MF_E_SHUTDOWN.into());
        }
        Ok(s.state)
    }
}

impl IKsControl_Impl for Stream_Impl {
    fn KsProperty(
        &self,
        _p: *const KSIDENTIFIER,
        _pl: u32,
        _d: *mut std::ffi::c_void,
        _dl: u32,
        _r: *mut u32,
    ) -> windows::core::Result<()> {
        Err(SET_NOT_FOUND.into())
    }

    fn KsMethod(
        &self,
        _p: *const KSIDENTIFIER,
        _pl: u32,
        _d: *mut std::ffi::c_void,
        _dl: u32,
        _r: *mut u32,
    ) -> windows::core::Result<()> {
        Err(SET_NOT_FOUND.into())
    }

    fn KsEvent(
        &self,
        _p: *const KSIDENTIFIER,
        _pl: u32,
        _d: *mut std::ffi::c_void,
        _dl: u32,
        _r: *mut u32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}
