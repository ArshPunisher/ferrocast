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
use ferrocast_camshare::{CLSID_DSHOW, CLSID_MF, VERSION};
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
            Class::MediaFoundation => crate::mf::Source::create()?,
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

/// Where the machine-wide copy goes: a folder every account (the frame server's too) can read.
fn machine_copy() -> PathBuf {
    let base = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .unwrap_or_else(|| "C:\\Program Files".into());
    Path::new(&base).join("Ferrocast Camera").join(format!("ferrocast_camera_v{VERSION}.dll"))
}

/// The Media Foundation camera for the whole machine (admin): copies this DLL where the frame
/// server can read it and registers its class there. Ferrocast runs this once, elevated.
pub fn install_machine() -> Result<PathBuf, String> {
    let from = own_path().ok_or("this DLL's path isn't known")?;
    let to = machine_copy();
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{} couldn't be made ({e})", dir.display()))?;
    }
    if from != to {
        // In use by the frame server (an app has the camera open): the copy there stays, which
        // reads the same layout (the file name carries its version).
        if let Err(e) = std::fs::copy(&from, &to)
            && !to.exists()
        {
            return Err(format!("the camera couldn't be copied to {} ({e})", to.display()));
        }
    }
    write_entries(Root::Machine, View::Bits64, &regdata::mf_entries(&to.to_string_lossy()))?;
    Ok(to)
}

/// Undoes [`install_machine`]: the class, then the copy (at the next restart if it's in use).
pub fn uninstall_machine() {
    delete_keys(Root::Machine, View::Bits64, &regdata::mf_keys());
    let to = machine_copy();
    if std::fs::remove_file(&to).is_err() && to.exists() {
        // SAFETY: asks Windows to delete the file at the next restart.
        let _ = unsafe { MoveFileExW(&HSTRING::from(to.as_os_str()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    }
    if let Some(dir) = to.parent() {
        let _ = std::fs::remove_dir(dir);
    }
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
