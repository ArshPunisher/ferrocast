//! Named shared memory on Windows: [`create`] for the side that makes it, [`open`] for the
//! other. The returned [`Region`] keeps the mapping alive.

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::core::{HSTRING, PCWSTR};

use super::{Backing, Region, SIZE};

/// What the frame server's region allows: SYSTEM, LocalService and administrators everything,
/// and the people signed in (interactive users) reading and writing, so Ferrocast can write to it.
pub const SDDL_SHARED: &str = "D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;IU)";

/// A mapped view and its handle, let go together.
pub(crate) struct Mapped {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: both came from a successful map below and are let go once, here.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

fn region_of(handle: HANDLE, write: bool) -> Result<Region, String> {
    let access = if write { FILE_MAP_ALL_ACCESS } else { FILE_MAP_READ };
    // SAFETY: maps `SIZE` bytes of a mapping that is at least that big (we made it so, or
    // check below by the header's own length check before reading the frames).
    let view = unsafe { MapViewOfFile(handle, access, 0, 0, SIZE) };
    if view.Value.is_null() {
        // SAFETY: read straight after the call it describes; then the handle (ours) is closed
        // and not used again.
        let why = unsafe { GetLastError() };
        // SAFETY: as above.
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(format!("the camera's memory couldn't be mapped ({why:?})"));
    }
    let base = view.Value.cast::<u8>();
    Ok(Region { base, len: SIZE, _backing: Backing::Mapped(Mapped { handle, view }) })
}

/// Makes (or opens, when it's there) the region called `name`, with the access `sddl` gives
/// (None: the default, the maker and administrators). True when it was already there.
pub fn create(name: &str, sddl: Option<&str>) -> Result<(Region, bool), String> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    if let Some(s) = sddl {
        // SAFETY: a valid SDDL string in; the descriptor is freed below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(s), SDDL_REVISION_1, &mut sd, None)
        }
        .map_err(|e| format!("the camera's memory access couldn't be set ({e})"))?;
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: a page-file mapping of SIZE bytes, named; `sa` lives through the call.
    let made = unsafe {
        CreateFileMappingW(
            HANDLE(-1isize as *mut _),
            (!sd.0.is_null()).then_some(&raw const sa),
            PAGE_READWRITE,
            0,
            SIZE as u32,
            &HSTRING::from(name),
        )
    };
    // SAFETY: read straight after the call it describes.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if !sd.0.is_null() {
        // SAFETY: allocated by ConvertStringSecurityDescriptor… above, freed once.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
    }
    let handle = made.map_err(|e| format!("the camera's memory couldn't be made ({e})"))?;
    Ok((region_of(handle, true)?, existed))
}

/// Opens the region called `name` that another process made (None when it isn't there).
pub fn open(name: &str, write: bool) -> Option<Region> {
    let access = if write { FILE_MAP_ALL_ACCESS } else { FILE_MAP_READ };
    // SAFETY: opens a named mapping by name; nothing else is passed.
    let handle = unsafe { OpenFileMappingW(access.0, false, PCWSTR(HSTRING::from(name).as_ptr())) }.ok()?;
    region_of(handle, write).ok()
}

// ------------------------------------------------------------------ registry

use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
    KEY_WRITE, REG_BINARY, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE, RRF_RT_REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};

use crate::regdata::{Entry, Value};

/// Whose `Software\Classes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// The user's (HKCU): no admin.
    User,
    /// The machine's (HKLM): admin.
    Machine,
}

/// Which registry view: 64-bit apps' or 32-bit apps' (WOW64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Bits64,
    Bits32,
}

fn hroot(root: Root) -> HKEY {
    match root {
        Root::User => HKEY_CURRENT_USER,
        Root::Machine => HKEY_LOCAL_MACHINE,
    }
}

fn sam(view: View) -> REG_SAM_FLAGS {
    match view {
        View::Bits64 => KEY_WOW64_64KEY,
        View::Bits32 => KEY_WOW64_32KEY,
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A key handle closed when dropped.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: opened below and closed once.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Writes every entry under `root`'s `Software\Classes`, in `view`.
pub fn write_entries(root: Root, view: View, entries: &[Entry]) -> Result<(), String> {
    for e in entries {
        let path = HSTRING::from(format!("Software\\Classes\\{}", e.key));
        let mut k = HKEY::default();
        // SAFETY: creates (or opens) a key under a predefined root; `k` is closed by `Key`.
        unsafe {
            RegCreateKeyExW(
                hroot(root),
                &path,
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE | sam(view),
                None,
                &mut k,
                None,
            )
        }
        .ok()
        .map_err(|x| format!("the camera's registry key {} couldn't be written ({x})", e.key))?;
        let k = Key(k);
        let name = e.name.map(HSTRING::from);
        let (kind, bytes): (REG_VALUE_TYPE, Vec<u8>) = match &e.value {
            Value::Text(t) => (REG_SZ, wide(t).iter().flat_map(|c| c.to_le_bytes()).collect()),
            Value::Binary(b) => (REG_BINARY, b.clone()),
        };
        let pname = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
        // SAFETY: writes `bytes` (alive through the call) as one value of an open key.
        unsafe { RegSetValueExW(k.0, pname, None, kind, Some(&bytes)) }
            .ok()
            .map_err(|x| format!("the camera's registry value under {} couldn't be written ({x})", e.key))?;
    }
    Ok(())
}

/// Deletes each key (and everything under it) under `root`'s `Software\Classes`, in `view`.
/// Keys that aren't there are fine.
pub fn delete_keys(root: Root, view: View, keys: &[String]) {
    let classes = HSTRING::from("Software\\Classes");
    let mut k = HKEY::default();
    // SAFETY: opens a key under a predefined root; closed by `Key`.
    if unsafe { RegOpenKeyExW(hroot(root), &classes, None, KEY_READ | KEY_WRITE | sam(view), &mut k) }.is_err() {
        return;
    }
    let k = Key(k);
    for key in keys {
        // SAFETY: deletes a subtree of an open key by name.
        let _ = unsafe { RegDeleteTreeW(k.0, &HSTRING::from(key.as_str())) };
    }
}

/// A text value under `root`'s `Software\Classes` (None: not there).
pub fn read_text(root: Root, view: View, key: &str, name: Option<&str>) -> Option<String> {
    let path = HSTRING::from(format!("Software\\Classes\\{key}"));
    let mut k = HKEY::default();
    // SAFETY: opens a key under a predefined root; closed by `Key`.
    unsafe { RegOpenKeyExW(hroot(root), &path, None, KEY_QUERY_VALUE | sam(view), &mut k) }.ok().ok()?;
    let k = Key(k);
    let name = name.map(HSTRING::from);
    let pname = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: reads one string value into `buf` (its size in bytes passed).
    unsafe {
        RegGetValueW(k.0, PCWSTR::null(), pname, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut len))
    }
    .ok()
    .ok()?;
    let n = (len as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}
