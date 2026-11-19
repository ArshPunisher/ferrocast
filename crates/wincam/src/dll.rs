//! The DLL's exports: COM's class factory for both cameras, and registration.
//!
//! - `DllRegisterServer` (elevated, `regsvr32`): the Media Foundation camera for the machine —
//!   this DLL copied where the frame server (LocalService) can read it, its class in HKLM.
//! - `DllUnregisterServer`: undoes that.
//! - `DllInstall(TRUE, "user")` (`regsvr32 /n /i:user`): the DirectShow camera for this user
//!   (what Ferrocast itself writes; handy by hand, and for the 32-bit DLL).

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use ferrocast_camshare::regdata;
use ferrocast_camshare::win::{Root, View, delete_keys, write_entries};
use ferrocast_camshare::{CLSID_DSHOW, CLSID_MF};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_POINTER, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
    GetModuleHandleExW,
};
use windows::core::{BOOL, GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, Ref, implement};

/// Which camera a factory makes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    DirectShow,
    MediaFoundation,
}

#[implement(IClassFactory)]
struct Factory(Class);

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        iid: *const GUID,
        out: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if out.is_null() || iid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: the caller's out-pointer, cleared first as COM asks.
        unsafe { out.write(std::ptr::null_mut()) };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let obj = match self.0 {
            Class::DirectShow => crate::dshow::Filter::create(),
            // The frame server asks the class for IMFActivate, then activates the source.
            Class::MediaFoundation => crate::activate::Activate::create()?,
        };
        // SAFETY: hands the caller the interface it asked for (counted), or E_NOINTERFACE.
        unsafe { obj.query(iid, out) }.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// COM asks for a class's factory.
///
/// # Safety
/// COM's contract: valid GUID pointers and an out-pointer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if clsid.is_null() || iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM's out-pointer, cleared first.
    unsafe { out.write(std::ptr::null_mut()) };
    // SAFETY: COM's GUID.
    let class = match unsafe { (*clsid).to_u128() } {
        CLSID_DSHOW => Class::DirectShow,
        CLSID_MF => Class::MediaFoundation,
        _ => return CLASS_E_CLASSNOTAVAILABLE,
    };
    let factory: IClassFactory = Factory(class).into();
    // SAFETY: hands out the asked-for interface of the factory.
    unsafe { factory.query(iid, out) }
}

/// Never unloaded while the process lives: a sending thread may still be finishing.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

/// This DLL's own file.
fn own_path() -> Option<PathBuf> {
    let mut module = HMODULE::default();
    let here = own_path as *const ();
    // SAFETY: finds the module holding this function's code, without counting it.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(here.cast()),
            &mut module,
        )
    }
    .ok()?;
    let mut buf = vec![0u16; 32_768];
    // SAFETY: fills `buf` with the module's path.
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// The folder for the machine-wide copy: one every account (the frame server's too) can read.
fn machine_dir() -> PathBuf {
    let base = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .unwrap_or_else(|| "C:\\Program Files".into());
    Path::new(&base).join("Ferrocast Camera")
}

/// FNV-1a of the DLL's bytes: names each build's copy apart (not a security check).
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// Deletes a file now, or at the next restart when an app holds it.
fn remove_or_later(path: &Path) {
    if std::fs::remove_file(path).is_err() && path.exists() {
        // SAFETY: asks Windows to delete the file at the next restart.
        let _ = unsafe { MoveFileExW(&HSTRING::from(path.as_os_str()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    }
}

/// The camera DLLs in the machine folder.
fn machine_copies(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")))
        .collect()
}

/// The Media Foundation camera for the whole machine (admin): copies this DLL where the frame
/// server can read it and registers its class there. Ferrocast runs this once, elevated, and
/// again after an update.
///
/// Each build's copy has its own name (version and a hash of its bytes): a copy the frame
/// server still holds can't be overwritten, and keeping it would keep an old camera. Older
/// copies are deleted, at the next restart if they're in use.
pub fn install_machine() -> Result<PathBuf, String> {
    let from = own_path().ok_or("this DLL's path isn't known")?;
    let dir = machine_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} couldn't be made ({e})", dir.display()))?;
    let bytes = std::fs::read(&from).map_err(|e| format!("{} couldn't be read ({e})", from.display()))?;
    // A name not taken: a file there that can't be deleted is held by the frame server and may
    // be set to go at the next restart (camera off while in use), so it's never reused.
    let stem = format!("ferrocast_camera-{}-{:016x}", env!("CARGO_PKG_VERSION"), fnv(&bytes));
    let to = (0..100)
        .map(|n| dir.join(if n == 0 { format!("{stem}.dll") } else { format!("{stem}-{n}.dll") }))
        .find(|p| !p.exists() || std::fs::remove_file(p).is_ok())
        .ok_or_else(|| format!("{} is full of camera copies in use: restart Windows", dir.display()))?;
    std::fs::write(&to, &bytes).map_err(|e| format!("the camera couldn't be copied to {} ({e})", to.display()))?;
    write_entries(Root::Machine, View::Bits64, &regdata::mf_entries(&to.to_string_lossy()))?;
    for old in machine_copies(&dir) {
        if old != to {
            remove_or_later(&old);
        }
    }
    Ok(to)
}

/// Undoes [`install_machine`]: the class, then the copies (at the next restart if in use).
pub fn uninstall_machine() {
    delete_keys(Root::Machine, View::Bits64, &regdata::mf_keys());
    let dir = machine_dir();
    for old in machine_copies(&dir) {
        remove_or_later(&old);
    }
    let _ = std::fs::remove_dir(&dir);
}

/// `regsvr32` (elevated): the Media Foundation camera for the machine.
#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    if cfg!(target_pointer_width = "32") {
        // The frame server is 64-bit: only the 64-bit DLL serves it.
        return S_OK;
    }
    match install_machine() {
        Ok(_) => S_OK,
        Err(_) => E_FAIL,
    }
}

/// `regsvr32 /u` (elevated): removes it.
#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    if cfg!(target_pointer_width = "64") {
        uninstall_machine();
    }
    S_OK
}

/// `regsvr32 /n /i:user` (and `/u`): the DirectShow camera for this user, in this DLL's own
/// registry view (so the 32-bit DLL registers for 32-bit apps).
///
/// # Safety
/// `cmd` is null or a NUL-terminated string (regsvr32's contract).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllInstall(install: BOOL, cmd: PCWSTR) -> HRESULT {
    // SAFETY: the caller's string, or none.
    let cmd = if cmd.is_null() { String::new() } else { unsafe { cmd.to_string() }.unwrap_or_default() };
    if !cmd.eq_ignore_ascii_case("user") {
        return E_INVALIDARG;
    }
    let view = if cfg!(target_pointer_width = "32") { View::Bits32 } else { View::Bits64 };
    if install.as_bool() {
        let Some(path) = own_path() else { return E_FAIL };
        let formats = ferrocast_camshare::caps::FORMATS.map(|f| f.fourcc());
        match write_entries(Root::User, view, &regdata::dshow_entries(&path.to_string_lossy(), &formats)) {
            Ok(()) => S_OK,
            Err(_) => E_FAIL,
        }
    } else {
        delete_keys(Root::User, view, &regdata::dshow_keys());
        S_OK
    }
}
