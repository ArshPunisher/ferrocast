//! Live captions on macOS (plan item 7, 29 Oct): a Studio sound's audio, as libobs mixes it,
//! goes straight to Apple's Speech recognizer on this Mac (on-device only: nothing leaves the
//! computer), and its words come back as they're heard. One recognition runs about 50 s and
//! the next takes over, as Apple's requests are meant to be short.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_avf_audio::{AVAudioFormat, AVAudioPCMBuffer};
use objc2_foundation::{NSError, NSLocale};
use objc2_speech::{
    SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
    SFSpeechRecognitionTaskHint, SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus,
};

/// How long one recognition runs before the next takes over.
const TURN_SECS: u64 = 50;

/// Recognitions that fail this soon after starting, this many times in a row, stop captions
/// (Dictation switched off, permission taken back): restarting would only spin.
const QUICK_FAIL_SECS: u64 = 2;
const QUICK_FAILS: u32 = 5;

/// Where Speech recognition stands for this app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Allowed,
    /// Not asked yet: asking shows macOS's prompt.
    NotAsked,
    Denied,
    Restricted,
}

pub fn access() -> Access {
    // SAFETY: a class method with no arguments, callable from any thread.
    match unsafe { SFSpeechRecognizer::authorizationStatus() } {
        SFSpeechRecognizerAuthorizationStatus::Authorized => Access::Allowed,
        SFSpeechRecognizerAuthorizationStatus::Denied => Access::Denied,
        SFSpeechRecognizerAuthorizationStatus::Restricted => Access::Restricted,
        _ => Access::NotAsked,
    }
}

/// Shows macOS's prompt for Speech recognition (once); the answer comes on later reads of
/// [`access`]. Needs NSSpeechRecognitionUsageDescription in the app's Info.plist.
pub fn ask() {
    let done = RcBlock::new(|_: SFSpeechRecognizerAuthorizationStatus| {});
    // SAFETY: the block lives as long as macOS needs it (it retains it).
    unsafe { SFSpeechRecognizer::requestAuthorization(&done) };
}

/// The request audio is fed into, swapped for a new one each turn.
struct Feed {
    request: Option<Retained<SFSpeechAudioBufferRecognitionRequest>>,
    format: Retained<AVAudioFormat>,
}

// SAFETY: Apple documents appending to a buffer request as safe from any thread; the request
// and format are only touched under the Mutex.
unsafe impl Send for Feed {}

/// What the Feeder hands sound to.
struct Tap {
    feed: Mutex<Feed>,
}

/// A running caption recognizer for one source. Dropping it stops listening.
pub struct Listener {
    tap: Arc<Tap>,
    /// Only ever used on the main thread (macOS calls the handlers there).
    turn: Rc<RefCell<Turn>>,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener").finish_non_exhaustive()
    }
}

/// The recognition running now, and when it began.
struct Turn {
    recognizer: Retained<SFSpeechRecognizer>,
    task: Option<Retained<SFSpeechRecognitionTask>>,
    began: Instant,
    on_text: Rc<dyn Fn(String, bool) + 'static>,
    /// Failures in a row soon after starting; at `QUICK_FAILS` it stops.
    quick_fails: u32,
    gave_up: bool,
}

/// Why captions can't start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    NoAccess(Access),
    /// This Mac can't recognise its language on the device (System Settings, Keyboard,
    /// Dictation may download it).
    NotOnDevice,
    NoRecognizer,
}

impl Listener {
    /// Starts listening to mono sound at `rate` Hz handed to the returned `Feeder` (from any
    /// thread): Ferrocast's own engine's tap. Called on the main thread.
    pub fn start_fed(rate: u32, on_text: impl Fn(String, bool) + 'static) -> Result<(Listener, Feeder), Unavailable> {
        let a = access();
        if a != Access::Allowed {
            return Err(Unavailable::NoAccess(a));
        }
        // SAFETY: plain allocations and getters on objects made here, on the main thread.
        let recognizer = unsafe {
            let locale = NSLocale::currentLocale();
            SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &locale)
        }
        .ok_or(Unavailable::NoRecognizer)?;
        // SAFETY: getters on a live recognizer.
        if !unsafe { recognizer.supportsOnDeviceRecognition() } {
            return Err(Unavailable::NotOnDevice);
        }
        // SAFETY: a standard (float, non-interleaved) mono format.
        let format = unsafe {
            AVAudioFormat::initStandardFormatWithSampleRate_channels(AVAudioFormat::alloc(), f64::from(rate), 1)
        }
        .ok_or(Unavailable::NoRecognizer)?;
        let tap = Arc::new(Tap { feed: Mutex::new(Feed { request: None, format }) });
        let turn = Rc::new(RefCell::new(Turn {
            recognizer,
            task: None,
            began: Instant::now(),
            on_text: Rc::new(on_text),
            quick_fails: 0,
            gave_up: false,
        }));
        begin(&tap, &turn);
        let feeder = Feeder(tap.clone());
        Ok((Listener { tap, turn }, feeder))
    }

    /// False once recognition kept failing and stopped.
    pub fn working(&self) -> bool {
        !self.turn.borrow().gave_up
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Ok(mut f) = self.tap.feed.lock()
            && let Some(r) = f.request.take()
        {
            // SAFETY: ends a live request.
            unsafe { r.endAudio() };
        }
        if let Some(task) = self.turn.borrow_mut().task.take() {
            // SAFETY: cancels a live task.
            unsafe { task.cancel() };
        }
    }
}

/// A new recognition: a fresh request fed from now on, its words passed on, and the next turn
/// begun when it finishes (or fails).
fn begin(tap: &Arc<Tap>, turn: &Rc<RefCell<Turn>>) {
    // SAFETY: a new request, configured before it's used.
    let request = unsafe {
        let r = SFSpeechAudioBufferRecognitionRequest::new();
        r.setShouldReportPartialResults(true);
        r.setRequiresOnDeviceRecognition(true);
        r.setTaskHint(SFSpeechRecognitionTaskHint::Dictation);
        r
    };
    let (tap2, turn2) = (Arc::downgrade(tap), Rc::downgrade(turn));
    let handler = RcBlock::new(move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
        let (Some(tap), Some(turn)) = (tap2.upgrade(), turn2.upgrade()) else { return };
        let mut finished = !error.is_null();
        if let Some(r) = NonNull::new(result) {
            // SAFETY: macOS hands a live result for the duration of the call.
            let r = unsafe { r.as_ref() };
            // SAFETY: getters on that live result.
            let (text, last) = unsafe { (r.bestTranscription().formattedString().to_string(), r.isFinal()) };
            let on_text = turn.borrow().on_text.clone();
            on_text(text, last);
            finished |= last;
        }
        let long = turn.borrow().began.elapsed().as_secs() >= TURN_SECS;
        if long && !finished {
            // Its time is up: ending the audio brings its final words, then the next turn.
            if let Ok(f) = tap.feed.lock()
                && let Some(r) = &f.request
            {
                // SAFETY: ends a live request.
                unsafe { r.endAudio() };
            }
        }
        if finished {
            let mut t = turn.borrow_mut();
            if !error.is_null() && t.began.elapsed().as_secs() < QUICK_FAIL_SECS {
                t.quick_fails += 1;
            } else {
                t.quick_fails = 0;
            }
            if t.quick_fails >= QUICK_FAILS {
                t.gave_up = true;
                t.task = None;
                drop(t);
                tracing::warn!("live captions stopped: speech recognition kept failing");
                if let Ok(mut f) = tap.feed.lock() {
                    f.request = None;
                }
                return;
            }
            drop(t);
            begin(&tap, &turn);
        }
    });
    let mut t = turn.borrow_mut();
    // SAFETY: the recognizer and request are live; macOS retains the handler block.
    let task = unsafe { t.recognizer.recognitionTaskWithRequest_resultHandler(&request, &handler) };
    t.task = Some(task);
    t.began = Instant::now();
    drop(t);
    if let Ok(mut f) = tap.feed.lock() {
        f.request = Some(request);
    }
}

/// Hands mono sound to a `Listener` started with `start_fed`, from any thread.
#[derive(Clone)]
pub struct Feeder(Arc<Tap>);

impl std::fmt::Debug for Feeder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feeder").finish_non_exhaustive()
    }
}

impl Feeder {
    /// Appends `samples` (mono, the listener's rate) to what's being recognised.
    pub fn feed(&self, samples: &[f32]) {
        append(&self.0, samples);
    }
}

/// Appends mono samples to the current request.
fn append(tap: &Tap, samples: &[f32]) {
    let Ok(n) = u32::try_from(samples.len()) else { return };
    if n == 0 {
        return;
    }
    let Ok(f) = tap.feed.lock() else { return };
    let Some(request) = &f.request else { return };
    // SAFETY: a new buffer the size of the chunk; its one channel is filled before it's
    // appended, and the request copies it.
    unsafe {
        let Some(buf) = AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(AVAudioPCMBuffer::alloc(), &f.format, n)
        else {
            return;
        };
        buf.setFrameLength(n);
        let chans = buf.floatChannelData();
        if chans.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(samples.as_ptr(), (*chans).as_ptr(), samples.len());
        request.appendAudioPCMBuffer(&buf);
    }
}
