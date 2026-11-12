//! One VST3 effect, set up as the SDK's own hosts do it: its component and processor, its
//! controller (the same object or its own), joined, stereo in and out, 32-bit floats, then
//! running. Blocks go through it; its state can be read and set.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};

use super::com::{Changes, Handler, HostApp, Pending, Stream};
use super::module::{Module, cid_of, iid_of, unsafe_zeroed};

pub struct Instance {
    pub name: String,
    /// Its own delay, in samples.
    pub latency: u32,
    /// Its window changed something since the state was last read.
    pub changed: Arc<AtomicBool>,
    pending: Pending,
    max_block: usize,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    pub controller: Option<ComPtr<IEditController>>,
    /// The controller is an object of its own (initialised and let go on its own).
    separate: bool,
    joined: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    _host: ComPtr<IHostApplication>,
    _handler: Option<ComPtr<IComponentHandler>>,
    // Last: the library goes after everything made from it.
    _module: Module,
}

impl std::fmt::Debug for Instance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Instance").field("name", &self.name).field("latency", &self.latency).finish()
    }
}

/// A host's FUnknown pointer for an object we hold.
fn unknown<I: vst3::Interface>(p: &ComPtr<I>) -> *mut FUnknown {
    p.as_ptr().cast()
}

impl Instance {
    /// Makes the effect `cid` of `module` at `rate` for blocks up to `max_block`, with `state`
    /// (component, controller) when there is one.
    #[allow(unsafe_code)]
    pub fn new(
        module: Module,
        cid: &str,
        name: String,
        rate: u32,
        max_block: usize,
        state: Option<(&[u8], &[u8])>,
    ) -> Result<Instance, String> {
        let cid = cid_of(cid).ok_or("a class id isn't 32 hex digits")?;
        let host = ComWrapper::new(HostApp).to_com_ptr::<IHostApplication>().ok_or("no host object")?;
        let f = &module.factory;
        let mut obj: *mut std::ffi::c_void = std::ptr::null_mut();
        let iid = iid_of::<IComponent>();
        // SAFETY: the factory makes a class it listed; `obj` gets one reference.
        let r = unsafe { f.createInstance(cid.as_ptr(), iid.as_ptr(), &mut obj) };
        // SAFETY: an owned IComponent reference, or null.
        let component = (r == kResultOk).then(|| unsafe { ComPtr::from_raw(obj.cast::<IComponent>()) }).flatten();
        let component = component.ok_or("it wouldn't make that effect")?;
        // SAFETY: the host object outlives the component (kept in the instance).
        if unsafe { component.initialize(unknown(&host)) } != kResultOk {
            return Err("it wouldn't start".into());
        }
        let processor = component.cast::<IAudioProcessor>().ok_or("it doesn't process sound")?;
        // Its controller: the same object, or one the factory makes.
        let (controller, separate) = match component.cast::<IEditController>() {
            Some(c) => (Some(c), false),
            None => {
                let mut ccid: TUID = [0; 16];
                // SAFETY: a TUID for the answer.
                let made = if unsafe { component.getControllerClassId(&mut ccid) } == kResultOk {
                    let mut o: *mut std::ffi::c_void = std::ptr::null_mut();
                    let iid = iid_of::<IEditController>();
                    // SAFETY: as for the component.
                    let r = unsafe { f.createInstance(ccid.as_ptr(), iid.as_ptr(), &mut o) };
                    // SAFETY: an owned reference, or null.
                    (r == kResultOk).then(|| unsafe { ComPtr::from_raw(o.cast::<IEditController>()) }).flatten()
                } else {
                    None
                };
                // SAFETY: as for the component.
                let made = made.filter(|c| unsafe { c.initialize(unknown(&host)) } == kResultOk);
                (made, true)
            }
        };
        // Its two halves talk to each other directly.
        let joined = if separate {
            controller.as_ref().and_then(|c| {
                let a = component.cast::<IConnectionPoint>()?;
                let b = c.cast::<IConnectionPoint>()?;
                // SAFETY: both are alive while joined (kept in the instance).
                unsafe {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                }
                Some((a, b))
            })
        } else {
            None
        };
        let pending: Pending = Arc::default();
        let changed = Arc::new(AtomicBool::new(false));
        let handler = ComWrapper::new(Handler { pending: pending.clone(), changed: changed.clone() })
            .to_com_ptr::<IComponentHandler>();
        if let (Some(c), Some(h)) = (&controller, &handler) {
            // SAFETY: the handler outlives the controller's use of it (kept in the instance).
            unsafe { c.setComponentHandler(h.as_ptr()) };
        }
        let mut me = Instance {
            name,
            latency: 0,
            changed,
            pending,
            max_block,
            component,
            processor,
            controller,
            separate,
            joined,
            _host: host,
            _handler: handler,
            _module: module,
        };
        if let Some((comp, ctrl)) = state {
            me.set_state(comp, ctrl)?;
        }
        me.start(rate)?;
        Ok(me)
    }

    /// Stereo in and out, its buses on, set up for this rate and block, then running.
    #[allow(unsafe_code)]
    fn start(&mut self, rate: u32) -> Result<(), String> {
        let c = &self.component;
        // SAFETY: the component's own bus counts.
        let (ins, outs) = unsafe {
            (
                c.getBusCount(MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection),
                c.getBusCount(MediaTypes_::kAudio as MediaType, BusDirections_::kOutput as BusDirection),
            )
        };
        if ins < 1 || outs < 1 {
            return Err("it doesn't take sound in (an instrument?)".into());
        }
        let mut i_arr = vec![SpeakerArr::kStereo; ins as usize];
        let mut o_arr = vec![SpeakerArr::kStereo; outs as usize];
        // Side-chain and other extra buses get nothing (left as the plugin has them).
        // SAFETY: arrays of the counts given.
        unsafe { self.processor.setBusArrangements(i_arr.as_mut_ptr(), 1, o_arr.as_mut_ptr(), 1) };
        // SAFETY: bus 0 each way.
        unsafe {
            c.activateBus(MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection, 0, 1);
            c.activateBus(MediaTypes_::kAudio as MediaType, BusDirections_::kOutput as BusDirection, 0, 1);
        }
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: self.max_block as int32,
            sampleRate: f64::from(rate),
        };
        // SAFETY: a setup struct for the call.
        if unsafe { self.processor.setupProcessing(&mut setup) } != kResultOk {
            return Err("it won't run at this rate".into());
        }
        // SAFETY: set up above.
        if unsafe { c.setActive(1) } != kResultOk {
            return Err("it wouldn't turn on".into());
        }
        // SAFETY: as above; some plugins say no here and still process.
        unsafe { self.processor.setProcessing(1) };
        // SAFETY: a plain getter.
        self.latency = unsafe { self.processor.getLatencySamples() };
        Ok(())
    }

    /// One block through it, in place (at most `max_block` samples).
    #[allow(unsafe_code)]
    pub fn process(&self, left: &mut [f32], right: &mut [f32]) -> Result<(), String> {
        let n = left.len().min(right.len()).min(self.max_block);
        let mut in_l = left[..n].to_vec();
        let mut in_r = right[..n].to_vec();
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];
        let mut in_ptrs = [in_l.as_mut_ptr(), in_r.as_mut_ptr()];
        let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
        let mut ins = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 { channelBuffers32: in_ptrs.as_mut_ptr() },
        };
        let mut outs = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 { channelBuffers32: out_ptrs.as_mut_ptr() },
        };
        let edits: Vec<(u32, f64)> = self.pending.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default();
        let changes = (!edits.is_empty())
            .then(|| ComWrapper::new(Changes::of(&edits)).to_com_ptr::<IParameterChanges>())
            .flatten();
        let mut data: ProcessData = unsafe_zeroed();
        data.processMode = ProcessModes_::kRealtime as int32;
        data.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as int32;
        data.numSamples = n as int32;
        data.numInputs = 1;
        data.numOutputs = 1;
        data.inputs = &mut ins;
        data.outputs = &mut outs;
        data.inputParameterChanges = changes.as_ref().map_or(std::ptr::null_mut(), |c| c.as_ptr());
        // SAFETY: every pointer in `data` is to buffers and objects alive for the call.
        let r = unsafe { self.processor.process(&mut data) };
        if r != kResultOk {
            return Err(format!("it said {r} to a block"));
        }
        left[..n].copy_from_slice(&out_l);
        right[..n].copy_from_slice(&out_r);
        Ok(())
    }

    /// Its state: the component's, then the controller's.
    #[allow(unsafe_code)]
    pub fn state(&self) -> (Vec<u8>, Vec<u8>) {
        let read = |get: &dyn Fn(*mut IBStream) -> tresult| -> Vec<u8> {
            let s = ComWrapper::new(Stream::default());
            match s.to_com_ptr::<IBStream>() {
                Some(p) if get(p.as_ptr()) == kResultOk => s.bytes.borrow().clone(),
                _ => Vec::new(),
            }
        };
        // SAFETY: a stream of ours for the plugin to write into.
        let comp = read(&|st| unsafe { self.component.getState(st) });
        let ctrl = match (&self.controller, self.separate) {
            // SAFETY: as above.
            (Some(c), true) => read(&|st| unsafe { c.getState(st) }),
            _ => Vec::new(),
        };
        (comp, ctrl)
    }

    /// Sets its state (component, controller), and tells the controller the component's.
    #[allow(unsafe_code)]
    pub fn set_state(&self, comp: &[u8], ctrl: &[u8]) -> Result<(), String> {
        let stream = |b: &[u8]| ComWrapper::new(Stream::of(b)).to_com_ptr::<IBStream>();
        if !comp.is_empty() {
            let s = stream(comp).ok_or("no stream")?;
            // SAFETY: a stream of ours for the plugin to read.
            if unsafe { self.component.setState(s.as_ptr()) } != kResultOk {
                return Err("it didn't take its saved settings".into());
            }
            if let (Some(c), Some(s)) = (&self.controller, stream(comp)) {
                // SAFETY: as above.
                unsafe { c.setComponentState(s.as_ptr()) };
            }
        }
        if !ctrl.is_empty()
            && self.separate
            && let (Some(c), Some(s)) = (&self.controller, stream(ctrl))
        {
            // SAFETY: as above.
            unsafe { c.setState(s.as_ptr()) };
        }
        Ok(())
    }
}

impl Drop for Instance {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: the SDK's order for letting a plugin go.
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            if let Some((a, b)) = self.joined.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = &self.controller {
                c.setComponentHandler(std::ptr::null_mut());
                if self.separate {
                    c.terminate();
                }
            }
            self.component.terminate();
        }
    }
}
