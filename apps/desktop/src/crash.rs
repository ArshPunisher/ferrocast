//! Crash reports on Windows: a native fault (an access violation in a capture or a sound
//! thread, where Rust's panic hook never runs) closed Ferrocast with nothing left to read,
//! and finding the Nov 2026 Chrome window crash took an outside tool (ProcDump). Now
//! Windows' last-chance handler writes a small minidump to `<data>/crashes` first, and the
//! next start logs where it is.
//!
//! The dump is written from a thread of its own, made at start and waiting: the faulting
//! thread may hold the heap's or the log's lock, so it only stores the fault, wakes that
//! thread and waits for it. Not caught: a fast-fail (the heap reporting its own corruption,
//! as that Chrome crash did; Rust's abort), which ends the process before any handler runs.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION, MiniDumpWithIndirectlyReferencedMemory, MiniDumpWithThreadInfo,
    MiniDumpWithUnloadedModules, MiniDumpWriteDump, SetUnhandledExceptionFilter,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};

/// Dumps kept (the newest): each is a few MB.
const KEEP: usize = 5;

struct Dumper {
    dir: PathBuf,
    thread: std::thread::Thread,
    fault: AtomicPtr<EXCEPTION_POINTERS>,
    thread_id: AtomicU32,
    taken: AtomicBool,
    done: AtomicBool,
}

static DUMPER: OnceLock<Dumper> = OnceLock::new();

/// Turns crash reports on (once, at start), and logs any dump an earlier run left.
pub fn install(data: &Path) {
    let dir = data.join("crashes");
    if std::fs::create_dir_all(&dir).is_err() {
        tracing::warn!(dir = %dir.display(), "no crash reports: the folder can't be made");
        return;
    }
    report_earlier(&dir);
    let Ok(waiting) = std::thread::Builder::new().name("crash-report".into()).spawn(wait_and_dump) else {
        tracing::warn!("no crash reports: no thread for them");
        return;
    };
    let dumper = Dumper {
        dir,
        thread: waiting.thread().clone(),
        fault: AtomicPtr::new(std::ptr::null_mut()),
        thread_id: AtomicU32::new(0),
        taken: AtomicBool::new(false),
        done: AtomicBool::new(false),
    };
    if DUMPER.set(dumper).is_err() {
        return;
    }
    // SAFETY: `on_fault` is a plain function that lives as long as the process.
    unsafe { SetUnhandledExceptionFilter(Some(on_fault)) };
}

/// The faulting thread: hand the fault over and wait (30 s at most) for the dump.
unsafe extern "system" fn on_fault(info: *const EXCEPTION_POINTERS) -> i32 {
    if let Some(d) = DUMPER.get()
        && !d.taken.swap(true, Ordering::SeqCst)
    {
        d.fault.store(info.cast_mut(), Ordering::SeqCst);
        // SAFETY: no arguments; reads the calling thread's id.
        d.thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        d.thread.unpark();
        for _ in 0..600 {
            if d.done.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    // EXCEPTION_CONTINUE_SEARCH: Windows goes on and ends the process as before.
    0
}

/// The waiting thread: writes the dump once a fault comes.
fn wait_and_dump() {
    let d = loop {
        std::thread::park();
        if let Some(d) = DUMPER.get()
            && !d.fault.load(Ordering::SeqCst).is_null()
        {
            break d;
        }
    };
    let when = chrono::Local::now().format("%Y-%m-%d %H-%M-%S");
    let path = d.dir.join(format!("ferrocast {} {when}.dmp", env!("CARGO_PKG_VERSION")));
    if let Ok(file) = std::fs::File::create(&path) {
        use std::os::windows::io::AsRawHandle;
        let info = MINIDUMP_EXCEPTION_INFORMATION {
            ThreadId: d.thread_id.load(Ordering::SeqCst),
            ExceptionPointers: d.fault.load(Ordering::SeqCst),
            ClientPointers: false.into(),
        };
        // Stacks, threads and the memory they point at: enough to see where and why.
        let kind = MiniDumpWithThreadInfo | MiniDumpWithIndirectlyReferencedMemory | MiniDumpWithUnloadedModules;
        // SAFETY: the file handle is open for writing until `file` drops below, and `info`
        // points at the fault Windows passed to `on_fault`, which waits on this.
        let _ = unsafe {
            MiniDumpWriteDump(
                GetCurrentProcess(),
                GetCurrentProcessId(),
                HANDLE(file.as_raw_handle()),
                kind,
                Some(&raw const info),
                None,
                None,
            )
        };
    }
    d.done.store(true, Ordering::SeqCst);
}

/// Logs dumps left by an earlier run (once each: they're renamed) and keeps the newest few.
fn report_earlier(dir: &Path) {
    for path in dumps_in(dir) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if let Some(stem) = name.strip_suffix(".dmp")
            && !stem.ends_with(" seen")
        {
            let seen = path.with_file_name(format!("{stem} seen.dmp"));
            let shown = if std::fs::rename(&path, &seen).is_ok() { seen } else { path.clone() };
            tracing::error!(file = %shown.display(), "Ferrocast crashed last time: a crash report was saved");
        }
    }
    let dumps = dumps_in(dir);
    for old in dumps.iter().take(dumps.len().saturating_sub(KEEP)) {
        let _ = std::fs::remove_file(old);
    }
}

/// The dumps in `dir`, oldest first.
fn dumps_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut dumps: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "dmp"))
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
        .collect();
    dumps.sort();
    dumps.into_iter().map(|(_, p)| p).collect()
}

/// Test builds only: `FERROCAST_TEST_CRASH=1` faults on purpose a few seconds after start, to
/// check a report is written (tools/windows-smoke.ps1 -Crash).
#[cfg(debug_assertions)]
pub fn test_crash_if_asked() {
    if std::env::var_os("FERROCAST_TEST_CRASH").is_none() {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(5));
        tracing::warn!("FERROCAST_TEST_CRASH: faulting on purpose");
        // SAFETY: deliberately not: a write to an address no process has (0x10; not null, which
        // debug builds' own checks would turn into a panic), so Windows raises an access
        // violation as a real fault would.
        unsafe { std::ptr::write_volatile(0x10 as *mut u32, 1) };
    });
}
