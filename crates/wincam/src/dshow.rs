//! The DirectShow camera: a source filter with one output pin, which apps load into
//! themselves. The pin offers [`caps::offered`], agrees a mode with the app, and a thread sends
//! a frame each frame time from [`Feed`] (Ferrocast's picture, or the placeholder).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ferrocast_camshare::caps::{self, Mode};
use ferrocast_camshare::feed::Feed;
use ferrocast_camshare::{CLSID_DSHOW, FRIENDLY_NAME, NAME_LOCAL, now_ms};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK};
use windows::Win32::Media::DirectShow::{
    ALLOCATOR_PROPERTIES, AM_FILTER_MISC_FLAGS_IS_SOURCE, AMPROPERTY_PIN_CATEGORY, FILTER_INFO, FILTER_STATE,
    IAMFilterMiscFlags, IAMFilterMiscFlags_Impl, IAMStreamConfig, IAMStreamConfig_Impl, IBaseFilter, IBaseFilter_Impl,
    IEnumMediaTypes, IEnumMediaTypes_Impl, IEnumPins, IEnumPins_Impl, IFilterGraph, IMediaFilter_Impl, IMediaSample,
    IMemAllocator, IMemInputPin, IPin, IPin_Impl, PIN_DIRECTION, PIN_INFO, PINDIR_OUTPUT, State_Paused, State_Running,
    State_Stopped, VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::KernelStreaming::{IKsPropertySet, IKsPropertySet_Impl};
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, AMPROPSETID_Pin, CLSID_MemoryAllocator, PIN_CATEGORY_CAPTURE,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoUninitialize,
    IPersist_Impl,
};
use windows::core::{GUID, HRESULT, IUnknownImpl, Interface, OutRef, PCWSTR, PWSTR, Ref, implement};

use crate::media;

// DirectShow's results (vfwmsgs.h).
const VFW_E_NOT_CONNECTED: HRESULT = HRESULT(0x8004_0209_u32 as i32);
const VFW_E_ALREADY_CONNECTED: HRESULT = HRESULT(0x8004_0204_u32 as i32);
const VFW_E_NO_ACCEPTABLE_TYPES: HRESULT = HRESULT(0x8004_0207_u32 as i32);
const VFW_E_NOT_STOPPED: HRESULT = HRESULT(0x8004_0224_u32 as i32);
const VFW_E_NOT_FOUND: HRESULT = HRESULT(0x8004_0216_u32 as i32);
const VFW_E_TYPE_NOT_ACCEPTED: HRESULT = HRESULT(0x8004_022A_u32 as i32);
const E_PROP_SET_UNSUPPORTED: HRESULT = HRESULT(0x8007_0492_u32 as i32);
const E_PROP_ID_UNSUPPORTED: HRESULT = HRESULT(0x8007_0490_u32 as i32);
/// KSPROPERTY_SUPPORT_GET.
const SUPPORT_GET: u32 = 1;

/// The pin's id and name.
const PIN_NAME: &str = "Capture";

/// A COM pointer moved to the sending thread. DirectShow's allocators, input pins and clocks
/// are free-threaded (the filter graph's rule for streaming threads).
struct Sent<T>(T);
// SAFETY: see above: these objects are called from streaming threads by design.
unsafe impl<T> Send for Sent<T> {}

/// What the filter and its pin share.
struct State {
    state: FILTER_STATE,
    /// The graph we're in, not counted (DirectShow's rule: the graph holds us).
    graph: *mut std::ffi::c_void,
    name: Vec<u16>,
    clock: Option<IReferenceClock>,
    /// Run's start time (stream time 0 on the clock).
    start: i64,
    /// The filter itself, not counted (the pin hands it out in QueryPinInfo).
    filter: *mut std::ffi::c_void,
    connected: Option<IPin>,
    input: Option<IMemInputPin>,
    allocator: Option<IMemAllocator>,
    mode: Option<Mode>,
    /// The mode an app asked for with IAMStreamConfig::SetFormat (offered first).
    wanted: Option<Mode>,
    sender: Option<Sender>,
}

// SAFETY: the raw pointers are only handed out (counted) under the lock, and the COM pointers
// are the free-threaded DirectShow kind.
unsafe impl Send for State {}

type Shared = Arc<Mutex<State>>;

fn lock(s: &Shared) -> MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

/// The feed for this process: the shared memory Ferrocast made in this session.
fn new_feed() -> Feed {
    Feed::new(|| ferrocast_camshare::win::open(NAME_LOCAL, false))
}

/// What to offer now: Ferrocast's own size first when it's sending, the app's choice before all.
fn offered(s: &State) -> Vec<Mode> {
    let mut list = caps::offered(new_feed().peek(now_ms()));
    if let Some(w) = s.wanted {
        list.retain(|m| *m != w);
        list.insert(0, w);
    }
    list
}

/// The thread sending frames while the graph is paused or running.
struct Sender {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Sender {
    fn start(shared: &Shared, input: IMemInputPin, allocator: IMemAllocator, mode: Mode) -> Sender {
        let stop = Arc::new(AtomicBool::new(false));
        let (input, allocator) = (Sent(input), Sent(allocator));
        let (s, stop2) = (shared.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("ferrocast-camera".into())
            .spawn(move || {
                // SAFETY: COM for this thread, undone at its end.
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                send_frames(&s, &stop2, &input.0, &allocator.0, mode);
                drop((input, allocator));
                if com {
                    // SAFETY: paired with the CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
            })
            .ok();
        Sender { stop, thread }
    }

    /// Stops the thread (the allocator must be decommitted first, so a wait for a buffer ends).
    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The sending loop: one frame each frame time, until stopped or the app stops taking them.
fn send_frames(s: &Shared, stop: &AtomicBool, input: &IMemInputPin, allocator: &IMemAllocator, mode: Mode) {
    let mut feed = new_feed();
    let every = Duration::from_nanos(mode.interval.max(1) as u64 * 100);
    let mut next = Instant::now();
    let mut n: i64 = 0;
    while !stop.load(Ordering::Acquire) {
        let mut sample: Option<IMediaSample> = None;
        // SAFETY: waits for a free buffer; fails once the allocator is decommitted (stopping).
        if unsafe { allocator.GetBuffer(&mut sample, None, None, 0) }.is_err() {
            break;
        }
        let Some(sample) = sample else { break };
        // SAFETY: the buffer's own pointer and size.
        let (ptr, size) = unsafe { (sample.GetPointer(), sample.GetSize()) };
        let Ok(ptr) = ptr else { break };
        let len = mode.frame_len();
        if ptr.is_null() || (size as usize) < len {
            break;
        }
        // SAFETY: the sample's buffer is ours to fill until Receive, `size` bytes long.
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
        feed.fill(now_ms(), (mode.width, mode.height), mode.format, buf);
        // Stream time: the clock since Run when running with one, else frame by frame.
        let (running, clock, start) = {
            let st = lock(s);
            (st.state == State_Running, st.clock.clone(), st.start)
        };
        let t0 = match (running, clock) {
            // SAFETY: a clock's time.
            (true, Some(c)) => unsafe { c.GetTime() }.map_or(n * mode.interval, |now| (now - start).max(0)),
            _ => n * mode.interval,
        };
        let t1 = t0 + mode.interval;
        // SAFETY: setting the sample's own fields.
        unsafe {
            let _ = sample.SetTime(Some(&t0), Some(&t1));
            let _ = sample.SetSyncPoint(true);
            let _ = sample.SetDiscontinuity(n == 0);
            let _ = sample.SetActualDataLength(len as i32);
        }
        // SAFETY: hands the sample on; the app may hold it (it's counted).
        if let Err(e) = unsafe { input.Receive(&sample) } {
            // The app stopped taking frames (stopping, or gone): wait for the graph to stop us.
            if e.code() != S_FALSE {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        drop(sample);
        n += 1;
        next += every;
        let now = Instant::now();
        match next.checked_duration_since(now) {
            Some(rest) => std::thread::sleep(rest),
            // Late (the app held a buffer): start counting again from now, not catch up.
            None => next = now,
        }
    }
}

// ------------------------------------------------------------------ the filter

#[implement(IBaseFilter, IAMFilterMiscFlags)]
pub(crate) struct Filter {
    shared: Shared,
    pin: IPin,
}

impl Filter {
    /// A new camera filter, as an IUnknown for the class factory.
    pub(crate) fn create() -> windows::core::IUnknown {
        let shared: Shared = Arc::new(Mutex::new(State {
            state: State_Stopped,
            graph: std::ptr::null_mut(),
            name: Vec::new(),
            clock: None,
            start: 0,
            filter: std::ptr::null_mut(),
            connected: None,
            input: None,
            allocator: None,
            mode: None,
            wanted: None,
            sender: None,
        }));
        let pin: IPin = Pin { shared: shared.clone() }.into();
        let filter: IBaseFilter = Filter { shared: shared.clone(), pin }.into();
        lock(&shared).filter = filter.as_raw();
        filter.into()
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        let sender = {
            let mut st = lock(&self.shared);
            st.filter = std::ptr::null_mut();
            st.graph = std::ptr::null_mut();
            if let Some(a) = &st.allocator {
                // SAFETY: ends any wait for a buffer before the thread is joined.
                let _ = unsafe { a.Decommit() };
            }
            st.sender.take()
        };
        if let Some(s) = sender {
            s.stop();
        }
    }
}

impl IPersist_Impl for Filter_Impl {
    fn GetClassID(&self) -> windows::core::Result<GUID> {
        Ok(GUID::from_u128(CLSID_DSHOW))
    }
}

impl IMediaFilter_Impl for Filter_Impl {
    fn Stop(&self) -> windows::core::Result<()> {
        let sender = {
            let mut st = lock(&self.shared);
            st.state = State_Stopped;
            if let Some(a) = &st.allocator {
                // SAFETY: wakes the sending thread out of GetBuffer.
                let _ = unsafe { a.Decommit() };
            }
            st.sender.take()
        };
        if let Some(s) = sender {
            s.stop();
        }
        Ok(())
    }

    fn Pause(&self) -> windows::core::Result<()> {
        let mut st = lock(&self.shared);
        if st.state == State_Stopped
            && let (Some(input), Some(alloc), Some(mode)) = (st.input.clone(), st.allocator.clone(), st.mode)
        {
            // SAFETY: buffers become available to GetBuffer.
            unsafe { alloc.Commit() }?;
            st.sender = Some(Sender::start(&self.shared, input, alloc, mode));
        }
        st.state = State_Paused;
        Ok(())
    }

    fn Run(&self, tstart: i64) -> windows::core::Result<()> {
        if lock(&self.shared).state == State_Stopped {
            self.Pause()?;
        }
        let mut st = lock(&self.shared);
        st.start = tstart;
        st.state = State_Running;
        Ok(())
    }

    fn GetState(&self, _timeout: u32) -> windows::core::Result<FILTER_STATE> {
        Ok(lock(&self.shared).state)
    }

    fn SetSyncSource(&self, clock: Ref<'_, IReferenceClock>) -> windows::core::Result<()> {
        lock(&self.shared).clock = clock.cloned();
        Ok(())
    }

    fn GetSyncSource(&self) -> windows::core::Result<IReferenceClock> {
        // None is a null clock and S_OK, as DirectShow expects.
        lock(&self.shared).clock.clone().ok_or_else(|| windows::core::Error::from_hresult(S_OK))
    }
}

impl IBaseFilter_Impl for Filter_Impl {
    fn EnumPins(&self) -> windows::core::Result<IEnumPins> {
        Ok(PinList { pin: self.pin.clone(), at: Mutex::new(0) }.into())
    }

    fn FindPin(&self, id: &PCWSTR) -> windows::core::Result<IPin> {
        // SAFETY: a caller's string, read up to its NUL.
        let id = unsafe { id.to_string() }.unwrap_or_default();
        if id == PIN_NAME { Ok(self.pin.clone()) } else { Err(VFW_E_NOT_FOUND.into()) }
    }

    fn QueryFilterInfo(&self, info: *mut FILTER_INFO) -> windows::core::Result<()> {
        // SAFETY: the caller's struct to fill.
        let info = unsafe { info.as_mut() }.ok_or(windows::core::Error::from(E_POINTER))?;
        let st = lock(&self.shared);
        let n = st.name.len().min(127);
        info.achName = [0; 128];
        info.achName[..n].copy_from_slice(&st.name[..n]);
        // The graph, counted for the caller (as the rules say).
        // SAFETY: the graph outlives its filters; borrowed then counted.
        let graph = unsafe { IFilterGraph::from_raw_borrowed(&st.graph) }.cloned();
        info.pGraph = std::mem::ManuallyDrop::new(graph);
        Ok(())
    }

    fn JoinFilterGraph(&self, graph: Ref<'_, IFilterGraph>, name: &PCWSTR) -> windows::core::Result<()> {
        let mut st = lock(&self.shared);
        st.graph = graph.as_ref().map_or(std::ptr::null_mut(), |g| g.as_raw());
        // SAFETY: a caller's string, read up to its NUL (or none).
        st.name = if name.is_null() { Vec::new() } else { unsafe { name.as_wide() }.to_vec() };
        Ok(())
    }

    fn QueryVendorInfo(&self) -> windows::core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }
}

impl IAMFilterMiscFlags_Impl for Filter_Impl {
    fn GetMiscFlags(&self) -> u32 {
        AM_FILTER_MISC_FLAGS_IS_SOURCE.0 as u32
    }
}

// ------------------------------------------------------------------ the pin

#[implement(IPin, IAMStreamConfig, IKsPropertySet)]
struct Pin {
    shared: Shared,
}

/// Agrees `m` with the app's input pin `to`: the connection, then the buffers. Undone on failure.
fn try_connect(me: &IPin, to: &IPin, m: &Mode) -> windows::core::Result<(IMemInputPin, IMemAllocator)> {
    let mut mt = AM_MEDIA_TYPE::default();
    // SAFETY: `mt` is ours; its format block is freed below.
    unsafe { media::write(&mut mt, m) }?;
    // SAFETY: offers the type; the app copies what it keeps.
    let agreed = unsafe { to.ReceiveConnection(me, &mt) };
    // SAFETY: ours, written above.
    unsafe { media::free_format(&mut mt) };
    agreed?;
    let buffers = || -> windows::core::Result<(IMemInputPin, IMemAllocator)> {
        let input: IMemInputPin = to.cast()?;
        // The app's allocator if it has one, else DirectShow's own.
        // SAFETY: plain calls on the app's pin and DirectShow's class.
        let alloc = unsafe { input.GetAllocator() }.or_else(|_| unsafe {
            CoCreateInstance::<_, IMemAllocator>(&CLSID_MemoryAllocator, None, CLSCTX_INPROC_SERVER)
        })?;
        // SAFETY: as above.
        let need = unsafe { input.GetAllocatorRequirements() }.unwrap_or_default();
        let want = ALLOCATOR_PROPERTIES {
            cBuffers: need.cBuffers.max(3),
            cbBuffer: (m.frame_len() as i32).max(need.cbBuffer),
            cbAlign: need.cbAlign.max(1),
            cbPrefix: need.cbPrefix.max(0),
        };
        // SAFETY: as above.
        let got = unsafe { alloc.SetProperties(&want) }?;
        if (got.cbBuffer as usize) < m.frame_len() {
            return Err(E_FAIL.into());
        }
        // SAFETY: as above.
        unsafe { input.NotifyAllocator(&alloc, false) }?;
        Ok((input, alloc))
    };
    buffers().inspect_err(|_| {
        // SAFETY: undoes the app's half of the connection.
        let _ = unsafe { to.Disconnect() };
    })
}

impl IPin_Impl for Pin_Impl {
    fn Connect(&self, to: Ref<'_, IPin>, pmt: *const AM_MEDIA_TYPE) -> windows::core::Result<()> {
        let to = to.ok()?.clone();
        let me: IPin = self.to_interface();
        let candidates = {
            let st = lock(&self.shared);
            if st.connected.is_some() {
                return Err(VFW_E_ALREADY_CONNECTED.into());
            }
            if st.state != State_Stopped {
                return Err(VFW_E_NOT_STOPPED.into());
            }
            // A full type from the app: only that one. Else ours, best first.
            // SAFETY: the caller's type, or null.
            match unsafe { media::read(pmt, caps::interval(30, 1)) } {
                Some(m) => vec![m],
                None if pmt.is_null() => offered(&st),
                // Partly given (say, the format only): ours that match it.
                None => {
                    // SAFETY: not null (checked).
                    let sub = unsafe { (*pmt).subtype };
                    let list = offered(&st);
                    let any = sub == GUID::zeroed();
                    list.into_iter()
                        .filter(|m| {
                            any || media::guid(ferrocast_camshare::regdata::media_subtype(m.format.fourcc())) == sub
                        })
                        .collect()
                }
            }
        };
        for m in candidates {
            if let Ok((input, alloc)) = try_connect(&me, &to, &m) {
                let mut st = lock(&self.shared);
                st.connected = Some(to);
                st.input = Some(input);
                st.allocator = Some(alloc);
                st.mode = Some(m);
                return Ok(());
            }
        }
        Err(VFW_E_NO_ACCEPTABLE_TYPES.into())
    }

    fn ReceiveConnection(&self, _from: Ref<'_, IPin>, _pmt: *const AM_MEDIA_TYPE) -> windows::core::Result<()> {
        // An output pin: it connects, it isn't connected to.
        Err(E_UNEXPECTED.into())
    }

    fn Disconnect(&self) -> windows::core::Result<()> {
        let mut st = lock(&self.shared);
        if st.state != State_Stopped {
            return Err(VFW_E_NOT_STOPPED.into());
        }
        if st.connected.take().is_none() {
            return Err(windows::core::Error::from_hresult(S_FALSE));
        }
        st.input = None;
        st.allocator = None;
        st.mode = None;
        Ok(())
    }

    fn ConnectedTo(&self) -> windows::core::Result<IPin> {
        lock(&self.shared).connected.clone().ok_or_else(|| VFW_E_NOT_CONNECTED.into())
    }

    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> windows::core::Result<()> {
        if pmt.is_null() {
            return Err(E_POINTER.into());
        }
        let mode = lock(&self.shared).mode;
        match mode {
            // SAFETY: the caller's struct to fill.
            Some(m) => unsafe { media::write(pmt, &m) },
            None => {
                // SAFETY: as above: an empty type, as the rules ask when not connected.
                unsafe { pmt.write(AM_MEDIA_TYPE::default()) };
                Err(VFW_E_NOT_CONNECTED.into())
            }
        }
    }

    fn QueryPinInfo(&self, info: *mut PIN_INFO) -> windows::core::Result<()> {
        // SAFETY: the caller's struct to fill.
        let info = unsafe { info.as_mut() }.ok_or(windows::core::Error::from(E_POINTER))?;
        let st = lock(&self.shared);
        // SAFETY: the filter outlives its pin's use; borrowed then counted for the caller.
        let filter = unsafe { IBaseFilter::from_raw_borrowed(&st.filter) }.cloned();
        info.pFilter = std::mem::ManuallyDrop::new(filter);
        info.dir = PINDIR_OUTPUT;
        info.achName = [0; 128];
        for (i, c) in PIN_NAME.encode_utf16().enumerate() {
            info.achName[i] = c;
        }
        Ok(())
    }

    fn QueryDirection(&self) -> windows::core::Result<PIN_DIRECTION> {
        Ok(PINDIR_OUTPUT)
    }

    fn QueryId(&self) -> windows::core::Result<PWSTR> {
        let wide: Vec<u16> = PIN_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a CoTaskMemAlloc'd copy, the caller frees it.
        let p = unsafe { CoTaskMemAlloc(wide.len() * 2) }.cast::<u16>();
        if p.is_null() {
            return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
        }
        // SAFETY: `p` holds `wide.len()` u16s.
        unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len()) };
        Ok(PWSTR(p))
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        // SAFETY: the caller's type, or null.
        if unsafe { media::read(pmt, caps::interval(30, 1)) }.is_some() { S_OK } else { S_FALSE }
    }

    fn EnumMediaTypes(&self) -> windows::core::Result<IEnumMediaTypes> {
        let list = offered(&lock(&self.shared));
        Ok(TypeList { list: Arc::new(list), at: Mutex::new(0) }.into())
    }

    fn QueryInternalConnections(&self, _pins: OutRef<'_, IPin>, _n: *mut u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EndOfStream(&self) -> windows::core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn BeginFlush(&self) -> windows::core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn EndFlush(&self) -> windows::core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn NewSegment(&self, _start: i64, _stop: i64, _rate: f64) -> windows::core::Result<()> {
        Ok(())
    }
}

impl IAMStreamConfig_Impl for Pin_Impl {
    fn SetFormat(&self, pmt: *const AM_MEDIA_TYPE) -> windows::core::Result<()> {
        let mut st = lock(&self.shared);
        if pmt.is_null() {
            st.wanted = None;
            return Ok(());
        }
        // SAFETY: the caller's type.
        let m = unsafe { media::read(pmt, caps::interval(30, 1)) }
            .ok_or(windows::core::Error::from(VFW_E_TYPE_NOT_ACCEPTED))?;
        if st.connected.is_some() && st.mode != Some(m) {
            // Another type while connected would need a reconnect: say so, as cameras do.
            return Err(VFW_E_ALREADY_CONNECTED.into());
        }
        st.wanted = Some(m);
        Ok(())
    }

    fn GetFormat(&self) -> windows::core::Result<*mut AM_MEDIA_TYPE> {
        let st = lock(&self.shared);
        let m = st.mode.or(st.wanted).unwrap_or_else(|| offered(&st)[0]);
        media::alloc(&m)
    }

    fn GetNumberOfCapabilities(&self, count: *mut i32, size: *mut i32) -> windows::core::Result<()> {
        if count.is_null() || size.is_null() {
            return Err(E_POINTER.into());
        }
        let n = offered(&lock(&self.shared)).len() as i32;
        // SAFETY: the caller's ints.
        unsafe {
            count.write(n);
            size.write(std::mem::size_of::<VIDEO_STREAM_CONFIG_CAPS>() as i32);
        }
        Ok(())
    }

    fn GetStreamCaps(&self, index: i32, pmt: *mut *mut AM_MEDIA_TYPE, scc: *mut u8) -> windows::core::Result<()> {
        if pmt.is_null() || scc.is_null() {
            return Err(E_POINTER.into());
        }
        let list = offered(&lock(&self.shared));
        let m = *usize::try_from(index).ok().and_then(|i| list.get(i)).ok_or(windows::core::Error::from(S_FALSE))?;
        let (w, h) = (m.width as i32, m.height as i32);
        let size = windows::Win32::Foundation::SIZE { cx: w, cy: h };
        let caps = VIDEO_STREAM_CONFIG_CAPS {
            guid: windows::Win32::Media::MediaFoundation::FORMAT_VideoInfo,
            InputSize: size,
            MinCroppingSize: size,
            MaxCroppingSize: size,
            CropGranularityX: 1,
            CropGranularityY: 1,
            CropAlignX: 1,
            CropAlignY: 1,
            MinOutputSize: size,
            MaxOutputSize: size,
            OutputGranularityX: 1,
            OutputGranularityY: 1,
            MinFrameInterval: m.interval,
            MaxFrameInterval: m.interval,
            MinBitsPerSecond: (m.frame_len() as i32).saturating_mul(8).saturating_mul(m.fps() as i32),
            MaxBitsPerSecond: (m.frame_len() as i32).saturating_mul(8).saturating_mul(m.fps() as i32),
            ..Default::default()
        };
        let mt = media::alloc(&m)?;
        // SAFETY: the caller's pointer and its caps struct (sized by GetNumberOfCapabilities).
        unsafe {
            pmt.write(mt);
            scc.cast::<VIDEO_STREAM_CONFIG_CAPS>().write_unaligned(caps);
        }
        Ok(())
    }
}

impl IKsPropertySet_Impl for Pin_Impl {
    fn Set(
        &self,
        _set: *const GUID,
        _id: u32,
        _i: *const std::ffi::c_void,
        _ni: u32,
        _d: *const std::ffi::c_void,
        _nd: u32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Get(
        &self,
        set: *const GUID,
        id: u32,
        _instance: *const std::ffi::c_void,
        _ninstance: u32,
        data: *mut std::ffi::c_void,
        ndata: u32,
        returned: *mut u32,
    ) -> windows::core::Result<()> {
        // SAFETY: the caller's GUID.
        if unsafe { set.as_ref() } != Some(&AMPROPSETID_Pin) {
            return Err(E_PROP_SET_UNSUPPORTED.into());
        }
        if id != AMPROPERTY_PIN_CATEGORY.0 as u32 {
            return Err(E_PROP_ID_UNSUPPORTED.into());
        }
        if !returned.is_null() {
            // SAFETY: the caller's int.
            unsafe { returned.write(std::mem::size_of::<GUID>() as u32) };
        }
        if data.is_null() {
            return Ok(());
        }
        if (ndata as usize) < std::mem::size_of::<GUID>() {
            return Err(E_INVALIDARG.into());
        }
        // The pin's category: a capture pin (how apps know it's a camera).
        // SAFETY: the caller's buffer, big enough (checked).
        unsafe { data.cast::<GUID>().write_unaligned(PIN_CATEGORY_CAPTURE) };
        Ok(())
    }

    fn QuerySupported(&self, set: *const GUID, id: u32) -> windows::core::Result<u32> {
        // SAFETY: the caller's GUID.
        if unsafe { set.as_ref() } != Some(&AMPROPSETID_Pin) {
            return Err(E_PROP_SET_UNSUPPORTED.into());
        }
        if id != AMPROPERTY_PIN_CATEGORY.0 as u32 {
            return Err(E_PROP_ID_UNSUPPORTED.into());
        }
        Ok(SUPPORT_GET)
    }
}

// ------------------------------------------------------------------ enumerators

#[implement(IEnumPins)]
struct PinList {
    pin: IPin,
    at: Mutex<usize>,
}

impl IEnumPins_Impl for PinList_Impl {
    fn Next(&self, n: u32, pins: *mut Option<IPin>, fetched: *mut u32) -> HRESULT {
        if pins.is_null() || (n != 1 && fetched.is_null()) {
            return E_POINTER;
        }
        let mut at = self.at.lock().unwrap_or_else(|p| p.into_inner());
        let mut got = 0;
        if n > 0 && *at == 0 {
            // SAFETY: the caller's array of at least `n` slots.
            unsafe { pins.write(Some(self.pin.clone())) };
            *at = 1;
            got = 1;
        }
        if !fetched.is_null() {
            // SAFETY: the caller's int.
            unsafe { fetched.write(got) };
        }
        if got == n { S_OK } else { S_FALSE }
    }

    fn Skip(&self, n: u32) -> windows::core::Result<()> {
        let mut at = self.at.lock().unwrap_or_else(|p| p.into_inner());
        *at = at.saturating_add(n as usize);
        if *at <= 1 { Ok(()) } else { Err(windows::core::Error::from_hresult(S_FALSE)) }
    }

    fn Reset(&self) -> windows::core::Result<()> {
        *self.at.lock().unwrap_or_else(|p| p.into_inner()) = 0;
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumPins> {
        let at = *self.at.lock().unwrap_or_else(|p| p.into_inner());
        Ok(PinList { pin: self.pin.clone(), at: Mutex::new(at) }.into())
    }
}

#[implement(IEnumMediaTypes)]
struct TypeList {
    list: Arc<Vec<Mode>>,
    at: Mutex<usize>,
}

impl IEnumMediaTypes_Impl for TypeList_Impl {
    fn Next(&self, n: u32, types: *mut *mut AM_MEDIA_TYPE, fetched: *mut u32) -> HRESULT {
        if types.is_null() || (n != 1 && fetched.is_null()) {
            return E_POINTER;
        }
        let mut at = self.at.lock().unwrap_or_else(|p| p.into_inner());
        let mut got = 0u32;
        while got < n && *at < self.list.len() {
            match media::alloc(&self.list[*at]) {
                // SAFETY: the caller's array of at least `n` slots.
                Ok(mt) => unsafe { types.add(got as usize).write(mt) },
                Err(e) => return e.code(),
            }
            *at += 1;
            got += 1;
        }
        if !fetched.is_null() {
            // SAFETY: the caller's int.
            unsafe { fetched.write(got) };
        }
        if got == n { S_OK } else { S_FALSE }
    }

    fn Skip(&self, n: u32) -> windows::core::Result<()> {
        let mut at = self.at.lock().unwrap_or_else(|p| p.into_inner());
        *at = at.saturating_add(n as usize);
        if *at <= self.list.len() { Ok(()) } else { Err(windows::core::Error::from_hresult(S_FALSE)) }
    }

    fn Reset(&self) -> windows::core::Result<()> {
        *self.at.lock().unwrap_or_else(|p| p.into_inner()) = 0;
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumMediaTypes> {
        let at = *self.at.lock().unwrap_or_else(|p| p.into_inner());
        Ok(TypeList { list: self.list.clone(), at: Mutex::new(at) }.into())
    }
}

/// The camera's name, for whoever asks (FriendlyName in the registry says the same).
#[allow(dead_code)]
pub(crate) const NAME: &str = FRIENDLY_NAME;
