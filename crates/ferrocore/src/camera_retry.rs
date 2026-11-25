//! Pure state machine governing camera reconnect & waiting behavior.
//!
//! Kept free of OS and GPU dependencies so every timing transition (interruption, disconnect,
//! stall, exponential backoff capped at 5 s, off-air pause, hotplug wake, and recovery) can be
//! verified deterministically in unit tests without hardware.

/// Initial retry wait after a camera stop or failed open: 1 s.
pub const CAMERA_BACKOFF_INITIAL_NS: u64 = 1_000_000_000;
/// Maximum retry wait between camera reopen attempts: 5 s.
pub const CAMERA_BACKOFF_MAX_NS: u64 = 5_000_000_000;
/// Startup grace while a newly opened camera session hasn't produced its first frame yet: 10 s.
pub const CAMERA_FIRST_FRAME_NS: u64 = 10_000_000_000;
/// Stall threshold once a running camera has already produced frames: 5 s.
pub const CAMERA_STALL_NS: u64 = 5_000_000_000;
/// Sustained delivery window before consecutive failure count resets: 10 s.
pub const CAMERA_STABLE_NS: u64 = 10_000_000_000;
/// Grace window keeping an interrupted-but-connected session alive for `interruptionEnded:`: 15 s.
pub const CAMERA_INTERRUPTION_GRACE_NS: u64 = 15_000_000_000;
/// Fruitless zero-frame startup opens before an idle capture session stays open in no-signal mode.
pub const NO_SIGNAL_MAX_TRIES: u32 = 3;
/// Human-facing status message shown while waiting for a camera to return.
pub const WAITING_MESSAGE: &str = "Waiting for the camera";

/// Exponential backoff sequence for camera reopens: 1 s, 2 s, 4 s, then capped at 5 s.
pub const fn backoff_ns(failures: u32) -> u64 {
    let shift = if failures < 3 { failures } else { 3 };
    let wait = CAMERA_BACKOFF_INITIAL_NS << shift;
    if wait < CAMERA_BACKOFF_MAX_NS { wait } else { CAMERA_BACKOFF_MAX_NS }
}

/// High-level availability state of a selected camera source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraState {
    /// Session is healthy or opening for the first time without having dropped.
    Live,
    /// Camera is interrupted, disconnected, or waiting to come back (not a dead black source).
    Waiting,
}

/// Snapshot evaluated on each watchdog step for one camera source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraObservation {
    /// Whether the engine currently holds an open capture session handle (`Live::Camera`).
    pub running: bool,
    /// Total frames/samples received by the current capture session.
    pub frames: u64,
    /// True when the capture backend reported stop, disconnect, or error.
    pub ended: bool,
    /// True when the capture session is temporarily interrupted while the device remains connected.
    pub interrupted: bool,
    /// True when an asynchronous open/reopen worker is currently running.
    pub opening: bool,
    /// True while the camera source is visible in a shown/preview/output scene or on air.
    pub visible_or_on_air: bool,
    /// Monotonic timestamp in nanoseconds.
    pub now_ns: u64,
}

/// Action returned by [`CameraRetry::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraAction {
    /// Leave the current session/wait timer untouched.
    Keep,
    /// Drop any stopped session and reopen the camera now for the given reason.
    Reopen(&'static str),
}

/// Retry and waiting-state machine for a selected camera source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraRetry {
    state: CameraState,
    reason: Option<String>,
    tries: u32,
    next_try_ns: u64,
    count: u64,
    since_ns: u64,
    flowing_since_ns: u64,
    interrupted_since_ns: u64,
    was_visible: bool,
    ever_sent: bool,
    no_signal: bool,
}

impl CameraRetry {
    /// Starts healthy at `now_ns` when a camera source is created or opened cleanly.
    pub fn new(now_ns: u64) -> Self {
        Self {
            state: CameraState::Live,
            reason: None,
            tries: 0,
            next_try_ns: 0,
            count: 0,
            since_ns: now_ns,
            flowing_since_ns: 0,
            interrupted_since_ns: 0,
            was_visible: true,
            ever_sent: false,
            no_signal: false,
        }
    }

    /// Starts already waiting at `now_ns` (e.g. an iPhone Continuity Camera asleep when loaded).
    pub fn waiting(now_ns: u64, retry_in_ns: u64, reason: impl Into<String>) -> Self {
        Self {
            state: CameraState::Waiting,
            reason: Some(reason.into()),
            tries: 1,
            next_try_ns: now_ns.saturating_add(retry_in_ns),
            count: 0,
            since_ns: now_ns,
            flowing_since_ns: 0,
            interrupted_since_ns: 0,
            was_visible: true,
            ever_sent: false,
            no_signal: false,
        }
    }

    /// Current high-level camera state.
    pub fn state(&self) -> CameraState {
        self.state
    }

    /// True while the camera is waiting to reconnect instead of treating it as dead.
    pub fn is_waiting(&self) -> bool {
        self.state == CameraState::Waiting
    }

    /// True when a healthy 0-frame capture session stays open quietly waiting for a signal.
    pub fn is_no_signal(&self) -> bool {
        self.no_signal
    }

    /// True once this source has delivered at least one frame.
    pub fn ever_sent(&self) -> bool {
        self.ever_sent
    }

    /// Status string exposed while waiting.
    pub fn waiting_message(&self) -> Option<&'static str> {
        self.is_waiting().then_some(WAITING_MESSAGE)
    }

    /// Internal diagnostic detail for why the camera entered waiting state, if any.
    pub fn detail(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Consecutive reconnect attempts made since the last stable recovery.
    pub fn tries(&self) -> u32 {
        self.tries
    }

    /// Earliest monotonic timestamp (ns) when the next reopen is allowed.
    pub fn next_try_ns(&self) -> u64 {
        self.next_try_ns
    }

    /// Clears backoff immediately when a matching hot-plug connect event arrives.
    pub fn on_hotplug(&mut self, now_ns: u64) {
        self.next_try_ns = now_ns;
        self.tries = 0;
        self.no_signal = false;
        self.interrupted_since_ns = 0;
        self.since_ns = now_ns;
    }

    /// Clears waiting state once a fresh video frame renders on the source.
    pub fn on_frame_uploaded(&mut self, now_ns: u64) {
        self.state = CameraState::Live;
        self.reason = None;
        self.ever_sent = true;
        self.no_signal = false;
        self.interrupted_since_ns = 0;
        self.since_ns = now_ns;
    }

    /// Marks an asynchronous reopen as launched; returns `prev_tries` for [`Self::on_reopen_finished`].
    pub fn on_reopen_started(&mut self, now_ns: u64) -> u32 {
        let prev = self.tries;
        self.state = CameraState::Waiting;
        self.no_signal = false;
        self.interrupted_since_ns = 0;
        if self.reason.is_none() {
            self.reason = Some(WAITING_MESSAGE.into());
        }
        self.next_try_ns = now_ns.saturating_add(backoff_ns(prev));
        self.tries = prev.saturating_add(1);
        self.flowing_since_ns = 0;
        self.count = 0;
        self.since_ns = now_ns;
        prev
    }

    /// Records failure of the asynchronous initial open so try count and backoff advance cleanly.
    pub fn on_initial_open_failed(&mut self, now_ns: u64, err: Option<&str>) {
        let prev = self.on_reopen_started(now_ns);
        self.on_reopen_finished(prev, false, now_ns, err);
    }

    /// Advances exponential backoff without resetting status when a source's worker cap is saturated.
    pub fn defer_backoff(&mut self, now_ns: u64) {
        let prev = self.tries;
        self.next_try_ns = now_ns.saturating_add(backoff_ns(prev));
        self.tries = prev.saturating_add(1);
        self.since_ns = now_ns;
    }

    /// Updates state after an asynchronous reopen finishes.
    ///
    /// A successful open while waiting keeps `"waiting for the camera"` until frames actually resume
    /// arriving, avoiding a false Live flicker if Continuity Camera drops right before frame 1.
    pub fn on_reopen_finished(&mut self, prev_tries: u32, opened: bool, now_ns: u64, err: Option<&str>) {
        self.next_try_ns = now_ns.saturating_add(backoff_ns(prev_tries));
        self.count = 0;
        self.interrupted_since_ns = 0;
        self.since_ns = now_ns;
        if !opened {
            self.state = CameraState::Waiting;
            self.reason = Some(err.unwrap_or(WAITING_MESSAGE).to_string());
        }
    }

    /// Advances the policy for one observation and decides whether to reopen now.
    pub fn step(&mut self, obs: CameraObservation) -> CameraAction {
        if !obs.visible_or_on_air {
            self.was_visible = false;
            if !obs.running || obs.ended || obs.interrupted {
                self.no_signal = false;
                self.state = CameraState::Waiting;
                self.flowing_since_ns = 0;
                if self.reason.is_none() {
                    self.reason = Some(WAITING_MESSAGE.into());
                }
                if obs.interrupted && obs.running && !obs.ended {
                    if self.interrupted_since_ns == 0 {
                        self.interrupted_since_ns = obs.now_ns.max(1);
                    }
                    self.since_ns = obs.now_ns;
                } else {
                    self.interrupted_since_ns = 0;
                }
            } else {
                self.interrupted_since_ns = 0;
                if obs.frames != self.count {
                    self.count = obs.frames;
                    self.since_ns = obs.now_ns;
                    if obs.frames > 0 {
                        self.ever_sent = true;
                        self.no_signal = false;
                        self.state = CameraState::Live;
                        self.reason = None;
                    }
                }
            }
            return CameraAction::Keep;
        }

        if !self.was_visible {
            self.was_visible = true;
            if self.state == CameraState::Waiting || !obs.running || obs.ended {
                self.next_try_ns = obs.now_ns;
                self.tries = 0;
                self.since_ns = obs.now_ns;
            }
        }

        if obs.opening {
            return CameraAction::Keep;
        }

        if !obs.running {
            self.no_signal = false;
            self.interrupted_since_ns = 0;
            self.state = CameraState::Waiting;
            self.flowing_since_ns = 0;
            if self.reason.is_none() {
                self.reason = Some(WAITING_MESSAGE.into());
            }
            return if obs.now_ns >= self.next_try_ns {
                CameraAction::Reopen("the camera isn't running")
            } else {
                CameraAction::Keep
            };
        }

        if obs.frames != self.count {
            self.count = obs.frames;
            self.since_ns = obs.now_ns;
            if obs.frames > 0 {
                let woke_from_no_signal = self.no_signal;
                self.ever_sent = true;
                self.no_signal = false;
                self.interrupted_since_ns = 0;
                self.state = CameraState::Live;
                self.reason = None;
                if self.flowing_since_ns == 0 {
                    self.flowing_since_ns = obs.now_ns.max(1);
                }
                if woke_from_no_signal || obs.now_ns.saturating_sub(self.flowing_since_ns) >= CAMERA_STABLE_NS {
                    self.tries = 0;
                    self.next_try_ns = 0;
                }
            }
        }

        if obs.ended {
            self.no_signal = false;
            self.interrupted_since_ns = 0;
            self.flowing_since_ns = 0;
            self.state = CameraState::Waiting;
            if self.reason.is_none() {
                self.reason = Some(WAITING_MESSAGE.into());
            }
            return if obs.now_ns >= self.next_try_ns {
                CameraAction::Reopen("the camera stopped or was interrupted")
            } else {
                CameraAction::Keep
            };
        }

        if obs.interrupted {
            self.no_signal = false;
            self.flowing_since_ns = 0;
            self.state = CameraState::Waiting;
            if self.reason.is_none() {
                self.reason = Some(WAITING_MESSAGE.into());
            }
            if self.interrupted_since_ns == 0 {
                self.interrupted_since_ns = obs.now_ns.max(1);
            }
            self.since_ns = obs.now_ns;
            if obs.now_ns.saturating_sub(self.interrupted_since_ns) < CAMERA_INTERRUPTION_GRACE_NS {
                return CameraAction::Keep;
            }
            return if obs.now_ns >= self.next_try_ns {
                CameraAction::Reopen("the camera interruption didn't end")
            } else {
                CameraAction::Keep
            };
        } else if self.interrupted_since_ns != 0 {
            self.interrupted_since_ns = 0;
            self.since_ns = obs.now_ns;
        }

        let limit = if obs.frames > 0 { CAMERA_STALL_NS } else { CAMERA_FIRST_FRAME_NS };
        if obs.now_ns.saturating_sub(self.since_ns) > limit {
            self.flowing_since_ns = 0;
            if !self.ever_sent && self.tries >= NO_SIGNAL_MAX_TRIES {
                // Keep a healthy 0-frame capture session open quietly (e.g. powered-off console
                // on an HDMI capture card) until a signal arrives or the device disconnects.
                self.no_signal = true;
                self.state = CameraState::Live;
                self.reason = None;
                return CameraAction::Keep;
            }
            self.state = CameraState::Waiting;
            if self.reason.is_none() {
                self.reason = Some(WAITING_MESSAGE.into());
            }
            let why = if obs.frames > 0 { "the camera stopped sending" } else { "the camera hasn't sent a frame yet" };
            return if obs.now_ns >= self.next_try_ns { CameraAction::Reopen(why) } else { CameraAction::Keep };
        }

        CameraAction::Keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    fn obs(running: bool, frames: u64, ended: bool, now_ns: u64) -> CameraObservation {
        CameraObservation {
            running,
            frames,
            ended,
            interrupted: false,
            opening: false,
            visible_or_on_air: true,
            now_ns,
        }
    }

    #[test]
    fn backoff_follows_1_2_4_then_caps_at_5_seconds() {
        assert_eq!(backoff_ns(0), SEC);
        assert_eq!(backoff_ns(1), 2 * SEC);
        assert_eq!(backoff_ns(2), 4 * SEC);
        assert_eq!(backoff_ns(3), 5 * SEC);
        assert_eq!(backoff_ns(4), 5 * SEC);
        assert_eq!(backoff_ns(50), 5 * SEC);
    }

    #[test]
    fn interrupted_camera_keeps_retrying_forever_on_backoff_and_recovers() {
        let mut policy = CameraRetry::new(0);
        // Normal frame delivery keeps state Live.
        assert_eq!(policy.step(obs(true, 10, false, SEC)), CameraAction::Keep);
        assert!(!policy.is_waiting());

        // iPhone locks / interrupts session at t=2s -> immediate reopen, Waiting state.
        assert!(matches!(policy.step(obs(true, 10, true, 2 * SEC)), CameraAction::Reopen(_)));
        assert!(policy.is_waiting());
        let p0 = policy.on_reopen_started(2 * SEC);
        policy.on_reopen_finished(p0, false, 2 * SEC, Some("not connected"));

        // Must not hammer during the 1 s backoff window.
        assert_eq!(
            policy.step(CameraObservation { opening: true, ..obs(false, 0, false, 2 * SEC) }),
            CameraAction::Keep
        );
        assert_eq!(policy.step(obs(false, 0, false, 2 * SEC + 500_000_000)), CameraAction::Keep);

        // Drives retries past 3 attempts (1 s -> 2 s -> 4 s -> 5 s -> 5 s ... forever).
        let mut t = 3 * SEC;
        let expected_waits = [2 * SEC, 4 * SEC, 5 * SEC, 5 * SEC, 5 * SEC];
        for wait in expected_waits {
            assert!(matches!(policy.step(obs(false, 0, false, t)), CameraAction::Reopen(_)), "due at {t}");
            let p = policy.on_reopen_started(t);
            policy.on_reopen_finished(p, false, t, Some("asleep"));
            assert_eq!(policy.step(obs(false, 0, false, t + wait - 1)), CameraAction::Keep, "no hammer before {wait}");
            t += wait;
        }
        assert!(policy.is_waiting());

        // Camera returns at t: open succeeds, remains Waiting until frame arrives, then Live.
        assert!(matches!(policy.step(obs(false, 0, false, t)), CameraAction::Reopen(_)));
        let p_ok = policy.on_reopen_started(t);
        policy.on_reopen_finished(p_ok, true, t, None);
        assert!(policy.is_waiting(), "stays waiting until first picture arrives");
        assert_eq!(policy.step(obs(true, 1, false, t + 50_000_000)), CameraAction::Keep);
        assert!(!policy.is_waiting());
        assert_eq!(policy.state(), CameraState::Live);
    }

    #[test]
    fn hidden_or_off_air_pauses_retries_and_resumes_when_shown() {
        let mut policy = CameraRetry::new(0);
        let p = policy.on_reopen_started(SEC);
        policy.on_reopen_finished(p, false, SEC, Some("disconnected"));
        assert!(policy.is_waiting());

        // Off-air / hidden: never reopens regardless of elapsed time.
        for s in [2, 10, 60] {
            assert_eq!(
                policy.step(CameraObservation { visible_or_on_air: false, ..obs(false, 0, false, s * SEC) }),
                CameraAction::Keep
            );
        }

        // Back on air: retries immediately on the very next check.
        assert!(matches!(policy.step(obs(false, 0, false, 61 * SEC)), CameraAction::Reopen(_)));
    }

    #[test]
    fn hotplug_clears_backoff_and_stable_frames_reset_tries() {
        let mut policy = CameraRetry::waiting(10 * SEC, 5 * SEC, "disconnected");
        assert_eq!(policy.step(obs(false, 0, false, 11 * SEC)), CameraAction::Keep);
        policy.on_hotplug(11 * SEC);
        assert!(matches!(policy.step(obs(false, 0, false, 11 * SEC)), CameraAction::Reopen(_)));

        let p = policy.on_reopen_started(11 * SEC);
        policy.on_reopen_finished(p, true, 11 * SEC, None);
        assert!(policy.tries() > 0);
        // Sustained frames across >= 10 s reset failure count back to 0.
        assert_eq!(policy.step(obs(true, 1, false, 12 * SEC)), CameraAction::Keep);
        assert_eq!(policy.step(obs(true, 300, false, 22 * SEC)), CameraAction::Keep);
        assert_eq!(policy.tries(), 0);
    }

    #[test]
    fn healthy_zero_frame_capture_card_stops_reopening_after_three_tries_and_wakes_on_frame() {
        let mut policy = CameraRetry::new(0);
        // Three 10 s zero-frame startup windows reopen; once tries reaches 3 on a healthy open
        // session (PS5/Switch powered off), the session stays open quietly without 10 s churn.
        let mut t = 0;
        for _ in 0..3 {
            t += CAMERA_FIRST_FRAME_NS + 1;
            assert!(matches!(policy.step(obs(true, 0, false, t)), CameraAction::Reopen(_)));
            let p = policy.on_reopen_started(t);
            policy.on_reopen_finished(p, true, t, None);
        }
        assert_eq!(policy.tries(), 3);

        t += CAMERA_FIRST_FRAME_NS + 1;
        assert_eq!(
            policy.step(obs(true, 0, false, t)),
            CameraAction::Keep,
            "healthy 0-frame session must stay open without reopening"
        );
        for extra in [20 * SEC, 60 * SEC, 600 * SEC] {
            assert_eq!(policy.step(obs(true, 0, false, t + extra)), CameraAction::Keep);
        }

        // Console switches on later: frames flow on the existing open session and reset tries.
        t += 600 * SEC;
        assert_eq!(policy.step(obs(true, 12, false, t)), CameraAction::Keep);
        assert_eq!(policy.state(), CameraState::Live);
        assert_eq!(policy.tries(), 0);

        // Once frames have flowed, a later stall resumes reopening instead of latching no-signal.
        assert!(matches!(policy.step(obs(true, 12, false, t + CAMERA_STALL_NS + 1)), CameraAction::Reopen(_)));
    }

    #[test]
    fn interrupted_session_waits_for_grace_window_before_reopening() {
        let mut policy = CameraRetry::new(0);
        assert_eq!(policy.step(obs(true, 10, false, SEC)), CameraAction::Keep);

        // Session interrupted on connected device: marks Waiting immediately without tearing down.
        let int_obs = |frames: u64, now_ns: u64| CameraObservation {
            running: true,
            frames,
            ended: false,
            interrupted: true,
            opening: false,
            visible_or_on_air: true,
            now_ns,
        };
        assert_eq!(policy.step(int_obs(10, 2 * SEC)), CameraAction::Keep);
        assert!(policy.is_waiting());
        assert_eq!(policy.step(int_obs(10, 2 * SEC + CAMERA_STALL_NS + SEC)), CameraAction::Keep);
        assert_eq!(policy.step(int_obs(10, 2 * SEC + CAMERA_INTERRUPTION_GRACE_NS - 1)), CameraAction::Keep);

        // Interruption ends before grace expires: keeps session open and resumes cleanly on frame.
        assert_eq!(policy.step(obs(true, 10, false, 10 * SEC)), CameraAction::Keep);
        assert_eq!(policy.step(obs(true, 11, false, 10 * SEC + 100_000_000)), CameraAction::Keep);
        assert!(!policy.is_waiting());

        // Stuck interrupted past grace window triggers a single reopen.
        assert_eq!(policy.step(int_obs(11, 20 * SEC)), CameraAction::Keep);
        assert!(matches!(policy.step(int_obs(11, 20 * SEC + CAMERA_INTERRUPTION_GRACE_NS)), CameraAction::Reopen(_)));
    }
}
