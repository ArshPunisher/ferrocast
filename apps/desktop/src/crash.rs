//! Crash reports on Windows: a native fault (an access violation in a capture or a sound
//! thread, where Rust's panic hook never runs) closed Ferrocast with nothing left to read.
//! Windows' last-chance handler writes a small minidump to `<data>/crashes` first, and the next
//! start logs where it is.
//!
//! Neither the faulting thread (`on_fault`) nor the waiting dump thread (`wait_and_dump`)
//! allocates on the heap or acquires Rust/CRT locks (`chrono` TZ locks, `std::fs::File::create`
//! UTF-16 allocation, `format!`, or `tracing`): all path prefix bytes are encoded as UTF-16 once
//! during `install()`, and the dump handler formats timestamps with `GetLocalTime` into a stack
//! `[u16; PATH_CAP]` buffer and opens the dump with `CreateFileW`.
#![allow(unsafe_code)]

#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::{CREATE_ALWAYS, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ};
#[cfg(windows)]
use windows::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION, MiniDumpWithIndirectlyReferencedMemory, MiniDumpWithThreadInfo,
    MiniDumpWithUnloadedModules, MiniDumpWriteDump, SetUnhandledExceptionFilter,
};
#[cfg(windows)]
use windows::Win32::System::SystemInformation::GetLocalTime;
#[cfg(windows)]
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, INFINITE, SetEvent, WaitForSingleObject,
};
#[cfg(windows)]
use windows::core::PCWSTR;

/// Dumps kept (the newest): each is a few MB.
#[cfg(windows)]
const KEEP: usize = 5;

/// Fixed UTF-16 buffer capacity for `<dir>\ferrocast <ver> YYYY-MM-DD HH-MM-SS.dmp\0`.
const PATH_CAP: usize = 1024;

/// Length of `"YYYY-MM-DD HH-MM-SS.dmp\0"` in UTF-16 code units.
const SUFFIX_LEN: usize = 24;

#[cfg(windows)]
struct SyncHandle(HANDLE);

#[cfg(windows)]
// SAFETY: Win32 manual/auto-reset event kernel HANDLEs created by `CreateEventW` are safe to wait
// on and signal from any thread in the process.
unsafe impl Send for SyncHandle {}
#[cfg(windows)]
// SAFETY: same kernel HANDLE thread-safety guarantee.
unsafe impl Sync for SyncHandle {}

#[cfg(windows)]
struct Dumper {
    /// Pre-encoded UTF-16 prefix `<data>/crashes\ferrocast <version> `.
    prefix: [u16; PATH_CAP],
    prefix_len: usize,
    fault_event: SyncHandle,
    done_event: SyncHandle,
    fault: AtomicPtr<EXCEPTION_POINTERS>,
    thread_id: AtomicU32,
    taken: AtomicBool,
    ready: AtomicBool,
}

#[cfg(windows)]
static DUMPER: OnceLock<Dumper> = OnceLock::new();

/// Writes 2 ASCII decimal digits (`00`..`99`) as UTF-16 without allocation.
fn write_2d(v: u16, dst: &mut [u16], at: usize) {
    let clamped = v % 100;
    dst[at] = u16::from(b'0' + (clamped / 10) as u8);
    dst[at + 1] = u16::from(b'0' + (clamped % 10) as u8);
}

/// Writes 4 ASCII decimal digits (`0000`..`9999`) as UTF-16 without allocation.
fn write_4d(v: u16, dst: &mut [u16], at: usize) {
    let clamped = v % 10_000;
    dst[at] = u16::from(b'0' + (clamped / 1000) as u8);
    dst[at + 1] = u16::from(b'0' + ((clamped / 100) % 10) as u8);
    dst[at + 2] = u16::from(b'0' + ((clamped / 10) % 10) as u8);
    dst[at + 3] = u16::from(b'0' + (clamped % 10) as u8);
}

/// Unboxed calendar stamp used to format a minidump filename without locks or heap allocations.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CrashStamp {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

/// Formats `<prefix>YYYY-MM-DD HH-MM-SS.dmp\0` into `dst` with zero heap allocations or locks.
pub(crate) fn format_dump_path_utf16(prefix: &[u16], stamp: CrashStamp, dst: &mut [u16; PATH_CAP]) -> bool {
    let n = prefix.len();
    if n + SUFFIX_LEN > PATH_CAP {
        return false;
    }
    dst[..n].copy_from_slice(prefix);
    write_4d(stamp.year, dst, n);
    dst[n + 4] = u16::from(b'-');
    write_2d(stamp.month, dst, n + 5);
    dst[n + 7] = u16::from(b'-');
    write_2d(stamp.day, dst, n + 8);
    dst[n + 10] = u16::from(b' ');
    write_2d(stamp.hour, dst, n + 11);
    dst[n + 13] = u16::from(b'-');
    write_2d(stamp.minute, dst, n + 14);
    dst[n + 16] = u16::from(b'-');
    write_2d(stamp.second, dst, n + 17);
    dst[n + 19] = u16::from(b'.');
    dst[n + 20] = u16::from(b'd');
    dst[n + 21] = u16::from(b'm');
    dst[n + 22] = u16::from(b'p');
    dst[n + 23] = 0;
    true
}

#[cfg(windows)]
fn encode_prefix_utf16(dir: &Path) -> Option<([u16; PATH_CAP], usize)> {
    use std::os::windows::ffi::OsStrExt;
    let file_prefix = dir.join(format!("ferrocast {} ", env!("CARGO_PKG_VERSION")));
    let units: Vec<u16> = file_prefix.as_os_str().encode_wide().collect();
    if units.len() + SUFFIX_LEN > PATH_CAP {
        return None;
    }
    let mut buf = [0u16; PATH_CAP];
    buf[..units.len()].copy_from_slice(&units);
    Some((buf, units.len()))
}

/// Turns crash reports on (once, at start), and logs any dump an earlier run left.
#[cfg(windows)]
pub fn install(data: &Path) {
    let dir = data.join("crashes");
    if std::fs::create_dir_all(&dir).is_err() {
        tracing::warn!(dir = %dir.display(), "no crash reports: the folder can't be made");
        return;
    }
    report_earlier(&dir);
    let Some((prefix, prefix_len)) = encode_prefix_utf16(&dir) else {
        tracing::warn!(dir = %dir.display(), "no crash reports: crash path is too long");
        return;
    };
    // SAFETY: valid null args create unnamed auto-reset/manual-reset events.
    let (Ok(fault_ev), Ok(done_ev)) = (unsafe {
        CreateEventW(None, false, false, PCWSTR::null())
            .and_then(|f| CreateEventW(None, true, false, PCWSTR::null()).map(|d| (f, d)))
    }) else {
        tracing::warn!("no crash reports: Win32 event creation failed");
        return;
    };
    let Ok(waiting) = std::thread::Builder::new().name("crash-report".into()).spawn(wait_and_dump) else {
        // SAFETY: both event handles were created above and aren't stored anywhere else.
        unsafe {
            let _ = CloseHandle(fault_ev);
            let _ = CloseHandle(done_ev);
        }
        tracing::warn!("no crash reports: no thread for them");
        return;
    };
    let dumper = Dumper {
        prefix,
        prefix_len,
        fault_event: SyncHandle(fault_ev),
        done_event: SyncHandle(done_ev),
        fault: AtomicPtr::new(std::ptr::null_mut()),
        thread_id: AtomicU32::new(0),
        taken: AtomicBool::new(false),
        ready: AtomicBool::new(false),
    };
    if DUMPER.set(dumper).is_err() {
        // SAFETY: `DUMPER` was already initialized; close the unused event handles we created.
        unsafe {
            let _ = CloseHandle(fault_ev);
            let _ = CloseHandle(done_ev);
        }
        return;
    }
    let _ = waiting;
    // SAFETY: `on_fault` is a plain function that lives as long as the process.
    unsafe { SetUnhandledExceptionFilter(Some(on_fault)) };
}

/// The faulting thread: record exception metadata atomically, signal the dump thread via a Win32
/// kernel event, and wait (30 s max) without allocating or acquiring any Rust/CRT locks.
#[cfg(windows)]
unsafe extern "system" fn on_fault(info: *const EXCEPTION_POINTERS) -> i32 {
    if let Some(d) = DUMPER.get()
        && !d.taken.swap(true, Ordering::SeqCst)
    {
        d.fault.store(info.cast_mut(), Ordering::Release);
        // SAFETY: no arguments; reads the calling thread's id.
        d.thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
        d.ready.store(true, Ordering::Release);
        // SAFETY: `fault_event` is an open Win32 event handle alive for process lifetime.
        let _ = unsafe { SetEvent(d.fault_event.0) };
        // SAFETY: waits at most 30 s for `done_event` without allocating or locking.
        let _ = unsafe { WaitForSingleObject(d.done_event.0, 30_000) };
    }
    // EXCEPTION_CONTINUE_SEARCH: Windows goes on and ends the process as before.
    0
}

/// The waiting dump thread: formats the dump filename with `GetLocalTime` on the stack, opens it
/// via `CreateFileW`, and writes the minidump with zero heap allocations or Rust/CRT locks.
#[cfg(windows)]
fn wait_and_dump() {
    let d = loop {
        if let Some(d) = DUMPER.get() {
            if d.ready.load(Ordering::Acquire) {
                break d;
            }
            // SAFETY: `fault_event` is alive for process lifetime.
            let _ = unsafe { WaitForSingleObject(d.fault_event.0, INFINITE) };
            if d.ready.load(Ordering::Acquire) {
                break d;
            }
        } else {
            std::thread::yield_now();
        }
    };
    // SAFETY: returns current local calendar fields by value with no CRT locks or allocations.
    let now = unsafe { GetLocalTime() };
    let mut path = [0u16; PATH_CAP];
    let prefix = &d.prefix[..d.prefix_len];
    let stamp = CrashStamp {
        year: now.wYear,
        month: now.wMonth,
        day: now.wDay,
        hour: now.wHour,
        minute: now.wMinute,
        second: now.wSecond,
    };
    if format_dump_path_utf16(prefix, stamp, &mut path) {
        // SAFETY: `path` is NUL-terminated UTF-16 on this stack frame.
        if let Ok(file) = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_READ,
                None,
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        } {
            let fault_ptr = d.fault.load(Ordering::Acquire);
            let info = (!fault_ptr.is_null()).then(|| MINIDUMP_EXCEPTION_INFORMATION {
                ThreadId: d.thread_id.load(Ordering::Acquire),
                ExceptionPointers: fault_ptr,
                ClientPointers: false.into(),
            });
            // Stacks, threads and the memory they point at: enough to see where and why.
            let kind = MiniDumpWithThreadInfo | MiniDumpWithIndirectlyReferencedMemory | MiniDumpWithUnloadedModules;
            // SAFETY: `file` is open for writing until `CloseHandle` below, and `info` points at the
            // fault Windows passed to `on_fault`, which waits on `done_event`.
            let _ = unsafe {
                MiniDumpWriteDump(
                    GetCurrentProcess(),
                    GetCurrentProcessId(),
                    file,
                    kind,
                    info.as_ref().map(std::ptr::from_ref),
                    None,
                    None,
                )
            };
            // SAFETY: `file` is a valid non-invalid Win32 handle opened above.
            let _ = unsafe { CloseHandle(file) };
        }
    }
    // SAFETY: `done_event` is alive for process lifetime.
    let _ = unsafe { SetEvent(d.done_event.0) };
}

/// Logs dumps left by an earlier run (once each: they're renamed) and keeps the newest few.
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(all(windows, debug_assertions))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_dump_path_formatting_writes_exact_utf16_without_overflow() {
        let prefix: Vec<u16> =
            r"C:\Users\Streamer\AppData\Local\Ferrocast\crashes\ferrocast 3.4.0 ".encode_utf16().collect();
        let stamp = CrashStamp { year: 2026, month: 11, day: 20, hour: 2, minute: 5, second: 9 };
        let mut dst = [0xFFFFu16; PATH_CAP];
        assert!(format_dump_path_utf16(&prefix, stamp, &mut dst));
        let nul = dst.iter().position(|&c| c == 0).unwrap_or_else(|| panic!("expected NUL terminator"));
        let decoded = String::from_utf16(&dst[..nul]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            decoded,
            r"C:\Users\Streamer\AppData\Local\Ferrocast\crashes\ferrocast 3.4.0 2026-11-20 02-05-09.dmp"
        );

        let too_long = vec![b'a' as u16; PATH_CAP - SUFFIX_LEN + 1];
        assert!(!format_dump_path_utf16(&too_long, stamp, &mut dst));
    }
}
