//! The work on clips (trim, compile, captions, burn-in, upload) runs in the background, one at a
//! time, with progress and a way to cancel. A job is a record the dashboard reads; the work
//! itself is ffmpeg (or a request) that never touches a live output.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, Semaphore};

use crate::{Engine, EngineError, now};

/// A job that keeps running this long is stopped (a long compilation of long clips).
const JOB_LIMIT: Duration = Duration::from_secs(4 * 3600);
/// Finished jobs kept for the list.
const KEEP: usize = 50;
/// Jobs waiting or running at once. Each waiting upload holds an open file and a task, and a
/// queue behind a long compile could otherwise grow without end.
pub(crate) const MAX_ACTIVE_JOBS: usize = 20;

/// ffmpeg with no console window of its own on Windows.
pub(crate) fn hidden(ffmpeg: &Path) -> Command {
    Command::from(ferrocore::ffenc::hidden(std::process::Command::new(ffmpeg)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Trim,
    Compile,
    Captions,
    Burn,
    Upload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct JobError {
    pub message: String,
    pub hint: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct JobView {
    pub id: String,
    pub kind: JobKind,
    /// What it is doing, in words ("Trim Replay 21-00").
    pub label: String,
    pub state: JobState,
    /// 0 to 1.
    pub progress: f32,
    /// The clip it made (its id), when it made one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Anything else it gives back (a YouTube link).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JobError>,
    pub started: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
}

#[derive(Debug)]
struct Rec {
    view: JobView,
    cancel: Arc<Cancel>,
}

/// A job's stop flag, and the wake-up for what is waiting on it.
#[derive(Default, Debug)]
pub struct Cancel {
    flag: AtomicBool,
    wake: Notify,
}

impl Cancel {
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    /// Resolves when the job is cancelled.
    pub async fn wait(&self) {
        loop {
            let waiting = self.wake.notified();
            if self.is_set() {
                return;
            }
            waiting.await;
        }
    }
}

/// What a running job holds: its progress, its stop flag and the engine.
#[derive(Clone)]
pub struct JobCtx {
    pub engine: Engine,
    pub cancel: Arc<Cancel>,
    id: String,
}

impl std::fmt::Debug for JobCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobCtx").field("id", &self.id).finish_non_exhaustive()
    }
}

impl JobCtx {
    pub fn progress(&self, p: f32) {
        self.engine.studio().clips.jobs.update(&self.id, |v| v.progress = p.clamp(0.0, 1.0));
    }

    pub fn link(&self, url: &str) {
        let url = url.to_string();
        self.engine.studio().clips.jobs.update(&self.id, |v| v.link = Some(url.clone()));
    }
}

/// What a finished job gives back.
#[derive(Debug)]
pub struct Made {
    pub output: Option<String>,
}

#[derive(Debug)]
pub struct Jobs {
    recs: std::sync::Mutex<Vec<Rec>>,
    next: AtomicU64,
    /// One job at a time (ffmpeg shouldn't fight the live encoder for the machine).
    gate: Arc<Semaphore>,
}

impl Default for Jobs {
    fn default() -> Self {
        Self { recs: std::sync::Mutex::default(), next: AtomicU64::new(0), gate: Arc::new(Semaphore::new(1)) }
    }
}

impl Jobs {
    fn gate(&self) -> Arc<Semaphore> {
        self.gate.clone()
    }

    pub fn list(&self) -> Vec<JobView> {
        self.recs.lock().unwrap_or_else(|p| p.into_inner()).iter().rev().map(|r| r.view.clone()).collect()
    }

    pub fn get(&self, id: &str) -> Option<JobView> {
        self.recs.lock().unwrap_or_else(|p| p.into_inner()).iter().find(|r| r.view.id == id).map(|r| r.view.clone())
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut JobView)) {
        if let Some(r) = self.recs.lock().unwrap_or_else(|p| p.into_inner()).iter_mut().find(|r| r.view.id == id) {
            f(&mut r.view);
        }
    }

    /// Jobs waiting or running.
    fn active(&self) -> usize {
        self.recs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|r| matches!(r.view.state, JobState::Queued | JobState::Running))
            .count()
    }

    fn add(&self, kind: JobKind, label: String) -> (JobView, Arc<Cancel>) {
        let id = format!("job-{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let cancel = Arc::new(Cancel::default());
        let mut recs = self.recs.lock().unwrap_or_else(|p| p.into_inner());
        let view = JobView {
            id: id.clone(),
            kind,
            label,
            state: JobState::Queued,
            progress: 0.0,
            output: None,
            link: None,
            error: None,
            started: now().0,
            finished: None,
        };
        recs.push(Rec { view: view.clone(), cancel: cancel.clone() });
        // The oldest finished ones go.
        while recs.len() > KEEP {
            match recs.iter().position(|r| !matches!(r.view.state, JobState::Queued | JobState::Running)) {
                Some(i) => {
                    recs.remove(i);
                }
                None => break,
            }
        }
        (view, cancel)
    }

    /// Stops a job (waiting or running): its half-made file is removed by the work itself.
    pub fn cancel(&self, id: &str) -> Result<JobView, EngineError> {
        let recs = self.recs.lock().unwrap_or_else(|p| p.into_inner());
        let r = recs.iter().find(|r| r.view.id == id).ok_or_else(job_not_found)?;
        if matches!(r.view.state, JobState::Queued | JobState::Running) {
            r.cancel.set();
        }
        Ok(r.view.clone())
    }

    /// Takes a finished job off the list.
    pub fn dismiss(&self, id: &str) -> Result<(), EngineError> {
        let mut recs = self.recs.lock().unwrap_or_else(|p| p.into_inner());
        let i = recs.iter().position(|r| r.view.id == id).ok_or_else(job_not_found)?;
        if matches!(recs[i].view.state, JobState::Queued | JobState::Running) {
            return Err(EngineError::Conflict("That job is still running: cancel it first.".into()));
        }
        recs.remove(i);
        Ok(())
    }
}

fn job_not_found() -> EngineError {
    EngineError::Missing {
        code: "job_not_found",
        message: "That job isn't on the list.",
        hint: "Reload the list; it may have been cleared.",
    }
}

/// How an error reads on a job.
fn describe(e: &EngineError) -> JobError {
    match e {
        EngineError::Refused { message, hint, .. } => JobError { message: message.clone(), hint: hint.clone() },
        other => JobError {
            message: other.to_string(),
            hint: "Try again; if it keeps failing, check the clip plays.".into(),
        },
    }
}

impl Engine {
    pub fn clip_jobs(&self) -> Vec<JobView> {
        self.studio().clips.jobs.list()
    }

    pub fn clip_job(&self, id: &str) -> Result<JobView, EngineError> {
        self.studio().clips.jobs.get(id).ok_or_else(job_not_found)
    }

    pub fn cancel_clip_job(&self, id: &str) -> Result<JobView, EngineError> {
        self.studio().clips.jobs.cancel(id)
    }

    pub fn dismiss_clip_job(&self, id: &str) -> Result<(), EngineError> {
        self.studio().clips.jobs.dismiss(id)
    }

    /// Starts `work` in the background, after the ones before it → the job at once.
    pub(crate) fn start_clip_job<F, Fut>(&self, kind: JobKind, label: String, work: F) -> JobView
    where
        F: FnOnce(JobCtx) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Made, EngineError>> + Send + 'static,
    {
        let jobs = &self.studio().clips.jobs;
        let waiting = jobs.active();
        let (mut view, cancel) = jobs.add(kind, label);
        let id = view.id.clone();
        if waiting >= MAX_ACTIVE_JOBS {
            // Not started: a full queue says so at once (and holds nothing: no file, no task).
            let err = JobError {
                message: format!("There are already {MAX_ACTIVE_JOBS} jobs waiting."),
                hint: "Wait for some to finish, or cancel the ones you don't need.".into(),
            };
            jobs.update(&id, |v| {
                v.state = JobState::Failed;
                v.finished = Some(now().0);
                v.error = Some(err.clone());
            });
            view.state = JobState::Failed;
            view.error = Some(err);
            return view;
        }
        let engine = self.clone();
        let ctx = JobCtx { engine: engine.clone(), cancel: cancel.clone(), id: id.clone() };
        tokio::spawn(async move {
            let gate = engine.studio().clips.jobs.gate();
            // Waiting for its turn can be cancelled too.
            let permit = tokio::select! {
                p = gate.acquire_owned() => p.ok(),
                () = cancel.wait() => None,
            };
            let finish = |state: JobState, out: Option<Made>, err: Option<JobError>| {
                engine.studio().clips.jobs.update(&id, |v| {
                    v.state = state;
                    v.finished = Some(now().0);
                    if state == JobState::Done {
                        v.progress = 1.0;
                    }
                    v.output = out.and_then(|m| m.output);
                    v.error = err;
                });
            };
            let Some(_permit) = permit else {
                return finish(JobState::Cancelled, None, None);
            };
            if cancel.is_set() {
                return finish(JobState::Cancelled, None, None);
            }
            engine.studio().clips.jobs.update(&id, |v| v.state = JobState::Running);
            // The work stops itself when cancelled (so it can clean up); a cancel that it ignores
            // still ends at the time limit.
            let result = tokio::time::timeout(JOB_LIMIT, work(ctx)).await;
            match result {
                Ok(Ok(made)) => finish(JobState::Done, Some(made), None),
                Ok(Err(e)) if is_cancelled(&e) => finish(JobState::Cancelled, None, cancel_note(&e)),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "a clip job failed");
                    finish(JobState::Failed, None, Some(describe(&e)));
                }
                Err(_) => finish(
                    JobState::Failed,
                    None,
                    Some(JobError {
                        message: "That took too long and was stopped.".into(),
                        hint: "Try fewer or shorter clips.".into(),
                    }),
                ),
            }
        });
        view
    }
}

pub(crate) fn cancelled() -> EngineError {
    EngineError::Refused {
        status: 409,
        code: "job_cancelled",
        message: "Cancelled.".into(),
        hint: String::new(),
        retry_after: None,
    }
}

/// A cancel that says something (an upload stopped while its last piece was on its way).
pub(crate) fn cancelled_with(note: &str, hint: &str) -> EngineError {
    EngineError::Refused {
        status: 409,
        code: "job_cancelled",
        message: note.into(),
        hint: hint.into(),
        retry_after: None,
    }
}

fn cancel_note(e: &EngineError) -> Option<JobError> {
    match e {
        EngineError::Refused { code: "job_cancelled", message, hint, .. } if message != "Cancelled." => {
            Some(JobError { message: message.clone(), hint: hint.clone() })
        }
        _ => None,
    }
}

pub(crate) fn is_cancelled(e: &EngineError) -> bool {
    matches!(e, EngineError::Refused { code: "job_cancelled", .. })
}

/// Runs ffmpeg with `args` (after the program's own `-progress` flags), writing `part`, then
/// renames it to `out` (never over a file that is there). `total_ms` turns the progress into a
/// share. A cancel stops ffmpeg and removes the half-made file.
pub(crate) async fn run_ffmpeg(
    ctx: &JobCtx,
    ffmpeg: &Path,
    args: Vec<OsString>,
    total_ms: u64,
    part: &Path,
    out: &Path,
    work_dir: Option<&Path>,
) -> Result<(), EngineError> {
    let _ = tokio::fs::remove_file(part).await;
    // However this ends (an error, a cancel, the future dropped), no half-made file stays.
    let mut guard = PartGuard(Some(part.to_path_buf()));
    let mut cmd = hidden(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-progress", "pipe:1", "-nostats"])
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(d) = work_dir {
        cmd.current_dir(d);
    }
    let mut child = cmd.spawn().map_err(|e| ffmpeg_trouble(format!("FFmpeg didn't start: {e}")))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = async {
        if let Some(o) = stdout {
            let mut lines = BufReader::new(o).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // `out_time_us` (and the misnamed `out_time_ms`) are microseconds.
                if let Some(v) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms="))
                    && let Ok(us) = v.trim().parse::<i64>()
                    && total_ms > 0
                {
                    ctx.progress((us.max(0) as f32 / 1000.0) / total_ms as f32);
                }
            }
        }
    };
    let errs = async {
        let mut text = String::new();
        if let Some(e) = stderr {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            let _ = e.take(64 * 1024).read_to_end(&mut buf).await;
            text = String::from_utf8_lossy(&buf).into_owned();
        }
        text
    };
    let work = async {
        let (_, text) = tokio::join!(reader, errs);
        let status = child.wait().await;
        (status, text)
    };
    let done = tokio::select! {
        r = work => Some(r),
        () = ctx.cancel.wait() => None,
    };
    let Some((status, text)) = done else {
        let _ = child.kill().await;
        let _ = tokio::fs::remove_file(part).await;
        return Err(cancelled());
    };
    match status {
        Ok(s) if s.success() => {}
        _ => {
            let _ = tokio::fs::remove_file(part).await;
            tracing::warn!(error = %text.trim(), "ffmpeg failed on a clip job");
            return Err(ffmpeg_trouble_from(&text));
        }
    }
    // The finished file takes its name only if nothing is there.
    let (p, o) = (part.to_path_buf(), out.to_path_buf());
    let moved = tokio::task::spawn_blocking(move || super::move_without_replacing(&p, &o))
        .await
        .map_err(|e| EngineError::Internal(e.to_string()))?;
    moved?;
    guard.0 = None;
    Ok(())
}

/// Removes a half-made file when dropped, unless it was given up (`.0 = None`).
struct PartGuard(Option<PathBuf>);

impl Drop for PartGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[allow(dead_code)]
fn first_line(t: &str) -> String {
    let l = t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it stopped without saying why");
    l.chars().take(160).collect()
}

/// What ffmpeg said, as the trouble: a full disk is said as that (with what to do), otherwise the
/// last line that says what went wrong (the early ones are often warnings).
pub(crate) fn ffmpeg_trouble_from(stderr: &str) -> EngineError {
    let lower = stderr.to_lowercase();
    if lower.contains("no space left on device") || lower.contains("disk full") || lower.contains("not enough space") {
        return EngineError::Refused {
            status: 507,
            code: "disk_full",
            message: "The disk is full, so the clip couldn't be saved.".into(),
            hint: "Free some space in the recordings folder, then try again.".into(),
            retry_after: None,
        };
    }
    let last = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("it stopped without saying why");
    ffmpeg_trouble(last.chars().take(160).collect())
}

fn ffmpeg_trouble(why: String) -> EngineError {
    EngineError::Refused {
        status: 422,
        code: "ffmpeg_failed",
        message: format!("FFmpeg couldn't do that: {why}."),
        hint: "Check the clip plays, then try again.".into(),
        retry_after: None,
    }
}

/// A file name for the new clip in `root`: `stem` as asked, "stem 2"… when that is taken (so a
/// job never fails for a name already there).
pub(crate) fn free_stem(root: &Path, stem: &str, ext: &str) -> String {
    for n in 1..1000 {
        let s = if n == 1 { stem.to_string() } else { format!("{stem} {n}") };
        if std::fs::symlink_metadata(root.join(format!("{s}.{ext}"))).is_err() {
            return s;
        }
    }
    stem.to_string()
}

/// `<root>/<stem>.<ext>.part`: where a clip is written until it is whole.
pub(crate) fn part_path(out: &Path) -> PathBuf {
    let mut name = out.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    out.with_file_name(name)
}
