//! Small Windows calls: folders, free space, running copies of the app, shortcuts, saved
//! sign-ins, opening links. Every `unsafe` block of setup is in this file.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Security::Credentials::{CRED_FLAGS, CREDENTIALW, CredDeleteW, CredEnumerateW, CredFree, CRED_TYPE_GENERIC};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, STGM_READ};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Programs, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem, IShellLinkW,
    KF_FLAG_DEFAULT, SHCreateItemFromParsingName, SHGetKnownFolderPath, SHStrDupW, SIGDN_FILESYSPATH, SLGP_RAWPATH, ShellExecuteW, ShellLink,
};
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{HTCAPTION, MB_ICONERROR, MB_OK, MessageBoxW, SW_MINIMIZE, SW_SHOWNORMAL, SendMessageW, ShowWindow, WM_NCLBUTTONDOWN};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// A shell folder (Desktop, Start menu Programs), following redirection (OneDrive desktops).
fn known(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = PathBuf::from(OsString::from_wide(p.as_wide()));
        CoTaskMemFree(Some(p.0 as *const _));
        Some(path)
    }
}

pub fn desktop_dir() -> Option<PathBuf> {
    known(&FOLDERID_Desktop)
}

pub fn programs_dir() -> Option<PathBuf> {
    known(&FOLDERID_Programs)
}

/// Free bytes for this user on the drive holding `path` (or its nearest existing parent).
pub fn free_space(path: &Path) -> Option<u64> {
    let mut at = path;
    while !at.exists() {
        at = at.parent()?;
    }
    let w = wide(at.as_os_str());
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None).ok()? };
    Some(free)
}

/// `C:` for `C:\Users\...`.
pub fn drive_of(path: &Path) -> String {
    match path.components().next() {
        Some(std::path::Component::Prefix(p)) => p.as_os_str().to_string_lossy().trim_end_matches('\\').to_string(),
        _ => "the drive".into(),
    }
}

/// Programs running from inside `dir`, other than this one: (process id, full path).
pub fn running_in(dir: &Path) -> Vec<(u32, PathBuf)> {
    let me = std::process::id();
    let dir = normalize(dir);
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            if e.th32ProcessID != me
                && let Some(path) = image_of(e.th32ProcessID)
                    && normalize(&path).starts_with(&dir) {
                        out.push((e.th32ProcessID, path));
                    }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn image_of(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| PathBuf::from(OsString::from_wide(&buf[..len as usize])))
    }
}

/// Lower-case with `\` separators and no trailing one, for comparing Windows paths.
fn normalize(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    PathBuf::from(s.trim_end_matches('\\').to_string() + "\\")
}

/// Waits up to `wait` for programs running from `dir` to quit on their own (the app closes
/// itself after starting an update), then ends what's left. True when none are left.
pub fn close_running(dir: &Path, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    while Instant::now() < until && !running_in(dir).is_empty() {
        std::thread::sleep(Duration::from_millis(200));
    }
    for (pid, _) in running_in(dir) {
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) {
                let _ = TerminateProcess(h, 1);
                let _ = WaitForSingleObject(h, 5_000);
                let _ = CloseHandle(h);
            }
        }
    }
    running_in(dir).is_empty()
}

/// Makes (or replaces) a shortcut at `lnk` to `target`, carrying the app's id so Windows
/// groups its taskbar button and shows its notifications under it.
pub fn shortcut(lnk: &Path, target: &Path, app_id: &str) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let t = HSTRING::from(target.as_os_str());
        link.SetPath(&t)?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        }
        link.SetIconLocation(&t, 0)?;
        let store: IPropertyStore = link.cast()?;
        let id = HSTRING::from(app_id);
        let mut v = PROPVARIANT::default();
        // VT_LPWSTR (what the shell expects here); PropVariantClear frees the copy.
        let copy = SHStrDupW(&id)?;
        (*v.Anonymous.Anonymous).vt = VT_LPWSTR;
        (*v.Anonymous.Anonymous).Anonymous.pwszVal = copy;
        let set = store.SetValue(&PKEY_AppUserModel_ID, &v).and_then(|()| store.Commit());
        let _ = PropVariantClear(&mut v);
        set?;
        if let Some(parent) = lnk.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file: IPersistFile = link.cast()?;
        file.Save(&HSTRING::from(lnk.as_os_str()), true)
    }
}

/// The program a shortcut opens.
pub fn shortcut_target(lnk: &Path) -> Option<PathBuf> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let file: IPersistFile = link.cast().ok()?;
        file.Load(&HSTRING::from(lnk.as_os_str()), STGM_READ).ok()?;
        let mut buf = vec![0u16; 32_768];
        link.GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32).ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        (len > 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len])))
    }
}

/// Windows' folder picker, opening at `start` (or its nearest existing parent).
pub fn pick_folder(owner: isize, start: &Path) -> Option<PathBuf> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dialog.GetOptions().ok()?;
        dialog.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST).ok()?;
        let _ = dialog.SetTitle(&HSTRING::from("Install Ferrocast in"));
        let mut at = start;
        while !at.is_dir() {
            match at.parent() {
                Some(p) => at = p,
                None => break,
            }
        }
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(at.as_os_str()), None) {
            let _ = dialog.SetFolder(&item);
        }
        dialog.Show(Some(HWND(owner as *mut _))).ok()?;
        let item = dialog.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = PathBuf::from(OsString::from_wide(p.as_wide()));
        CoTaskMemFree(Some(p.0 as *const _));
        Some(path)
    }
}

/// Whether Windows apps are set to light mode (for the window's colour before the page draws).
pub fn light_theme() -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .is_ok_and(|v| v == 1)
}

/// Removes the app's saved sign-ins from Credential Manager (`<name>.<service>`).
pub fn forget_signins(service: &str) -> usize {
    let filter = HSTRING::from(format!("*.{service}"));
    let mut removed = 0;
    unsafe {
        let mut count = 0u32;
        let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
        if CredEnumerateW(&filter, None, &mut count, &mut list).is_err() || list.is_null() {
            return 0;
        }
        let creds = std::slice::from_raw_parts(list, count as usize);
        let names: Vec<(PWSTR, u32)> = creds.iter().filter(|c| !c.is_null()).map(|c| ((**c).TargetName, (**c).Type.0)).collect();
        for (name, kind) in names {
            if kind == CRED_TYPE_GENERIC.0 && CredDeleteW(PCWSTR(name.0), CRED_TYPE_GENERIC, Some(CRED_FLAGS(0).0)).is_ok() {
                removed += 1;
            }
        }
        CredFree(list as *const _);
    }
    removed
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    unsafe {
        ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from(url), None, None, SW_SHOWNORMAL);
    }
}

/// A plain Windows message, for when setup can't show its own window.
pub fn message(title: &str, text: &str) {
    unsafe {
        MessageBoxW(Some(HWND::default()), &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONERROR);
    }
}

/// Deletes `file` a moment after this program quits (a running program can't delete itself).
pub fn delete_after_exit(file: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let quoted = format!("\"{}\"", file.display());
    let _ = std::process::Command::new("cmd")
        .args(["/d", "/c", "ping", "-n", "3", "127.0.0.1", ">nul", "&", "del", "/f", "/q"])
        .raw_arg(&quoted)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// Minimizes setup's window.
pub fn minimize(hwnd: isize) {
    unsafe {
        let _ = ShowWindow(HWND(hwnd as *mut _), SW_MINIMIZE);
    }
}

/// Starts moving the window with the mouse, as a press on a normal title bar does.
pub fn start_drag(hwnd: isize) {
    unsafe {
        let _ = ReleaseCapture();
        SendMessageW(HWND(hwnd as *mut _), WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(0)));
    }
}
