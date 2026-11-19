//! A VST3 module: the bundle's library loaded, its entry called, its factory, and the sound
//! effects it holds.

use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};

use vst3::Steinberg::*;
use vst3::{ComPtr, Interface};

use crate::Found;

/// The library inside a `.vst3` bundle for this system (Windows also takes an old single-file
/// `.vst3`, which is the library itself).
pub fn binary_of(bundle: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "windows") && bundle.is_file() {
        return Some(bundle.to_path_buf());
    }
    let stem = bundle.file_stem()?.to_string_lossy().to_string();
    let at = if cfg!(target_os = "macos") {
        // The executable: the one file in Contents/MacOS (named as the bundle, usually).
        let dir = bundle.join("Contents/MacOS");
        let named = dir.join(&stem);
        if named.is_file() {
            named
        } else {
            std::fs::read_dir(&dir).ok()?.filter_map(Result::ok).map(|e| e.path()).find(|p| p.is_file())?
        }
    } else if cfg!(target_os = "windows") {
        let arch = if cfg!(target_arch = "aarch64") { "arm64-win" } else { "x86_64-win" };
        bundle.join("Contents").join(arch).join(format!("{stem}.vst3"))
    } else {
        let arch = if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" };
        bundle.join("Contents").join(arch).join(format!("{stem}.so"))
    };
    at.is_file().then_some(at)
}

/// Where this system keeps VST3 plugins (`FERROCAST_VST_DIRS`, folders split as PATH is, looked
/// in instead: for tests and other places).
pub fn plugin_dirs() -> Vec<PathBuf> {
    if let Some(dirs) = std::env::var_os("FERROCAST_VST_DIRS") {
        return std::env::split_paths(&dirs).collect();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
        if let Some(h) = &home {
            dirs.push(h.join("Library/Audio/Plug-Ins/VST3"));
        }
    } else if cfg!(target_os = "windows") {
        let common = std::env::var_os("CommonProgramFiles")
            .map_or_else(|| PathBuf::from(r"C:\Program Files\Common Files"), PathBuf::from);
        dirs.push(common.join("VST3"));
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Programs/Common/VST3"));
        }
    } else {
        dirs.push(PathBuf::from("/usr/lib/vst3"));
        dirs.push(PathBuf::from("/usr/local/lib/vst3"));
        if let Some(h) = &home {
            dirs.push(h.join(".vst3"));
        }
    }
    dirs
}

/// Every `.vst3` bundle under `dirs` (a vendor folder or two deep).
pub fn bundles_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.filter_map(Result::ok) {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vst3")) {
                out.push(p);
            } else if depth < 2 && p.is_dir() {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        walk(d, 0, &mut out);
    }
    out.sort();
    out
}

type Entry = unsafe extern "system" fn(*mut c_void) -> bool;
type Exit = unsafe extern "system" fn() -> bool;
type GetFactory = unsafe extern "system" fn() -> *mut IPluginFactory;

/// A loaded module. Dropped: its factory is let go, its exit called, the library unloaded.
pub struct Module {
    /// Released by hand in `drop`, before the module's exit (a plugin may free what its
    /// factory needs there).
    pub factory: std::mem::ManuallyDrop<ComPtr<IPluginFactory>>,
    exit: Option<Exit>,
    #[cfg(target_os = "macos")]
    _bundle: objc2_core_foundation::CFRetained<objc2_core_foundation::CFBundle>,
    // Last: unloaded after everything above.
    _lib: libloading::Library,
}

impl std::fmt::Debug for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Module")
    }
}

impl Module {
    #[allow(unsafe_code)]
    pub fn load(bundle: &Path) -> Result<Module, String> {
        let bin = binary_of(bundle).ok_or_else(|| format!("{} has no library for this computer", bundle.display()))?;
        // SAFETY: loading the plugin's library runs its initialisers: that's why this is the
        // helper, in its sandbox, and never Ferrocast itself.
        let lib = unsafe { libloading::Library::new(&bin) }.map_err(|e| format!("couldn't load it: {e}"))?;
        #[cfg(target_os = "macos")]
        let bundle_ref = {
            use objc2_core_foundation::{CFBundle, CFURL};
            let url = CFURL::from_directory_path(bundle).ok_or("not a folder path")?;
            CFBundle::new(None, Some(&url)).ok_or("not a bundle")?
        };
        // SAFETY: each symbol, when there, has the VST3 SDK's signature for this system.
        let (entry, exit, get) = unsafe {
            let sym = |name: &[u8]| lib.get::<Entry>(name).ok().map(|s| *s);
            let entry = if cfg!(target_os = "macos") {
                sym(b"bundleEntry").or_else(|| sym(b"BundleEntry"))
            } else if cfg!(target_os = "windows") {
                // Windows' InitDll takes nothing: called below.
                None
            } else {
                sym(b"ModuleEntry")
            };
            let exit_name: &[u8] = if cfg!(target_os = "macos") {
                b"bundleExit"
            } else if cfg!(target_os = "windows") {
                b"ExitDll"
            } else {
                b"ModuleExit"
            };
            let exit = lib.get::<Exit>(exit_name).ok().map(|s| *s);
            let get = lib.get::<GetFactory>(b"GetPluginFactory").map_err(|_| "it isn't a VST3 plugin".to_string())?;
            (entry, exit, *get)
        };
        #[cfg(target_os = "windows")]
        // SAFETY: InitDll takes nothing (the SDK's dllmain.cpp).
        unsafe {
            if let Ok(init) = lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll")
                && !init()
            {
                return Err("its InitDll said no".into());
            }
        }
        if let Some(entry) = entry {
            #[cfg(target_os = "macos")]
            let arg = objc2_core_foundation::CFRetained::as_ptr(&bundle_ref).as_ptr().cast::<c_void>();
            #[cfg(not(target_os = "macos"))]
            let arg = std::ptr::null_mut::<c_void>();
            // SAFETY: the bundle (Mac) or library handle the entry wants; it's kept alive.
            if !unsafe { entry(arg) } {
                return Err("its entry point said no".into());
            }
        }
        // SAFETY: the SDK's factory function; it hands over one reference.
        let raw = unsafe { get() };
        // SAFETY: an owned reference, or null.
        let factory = unsafe { ComPtr::from_raw(raw) }.ok_or("it gave no factory")?;
        Ok(Module {
            factory: std::mem::ManuallyDrop::new(factory),
            exit,
            #[cfg(target_os = "macos")]
            _bundle: bundle_ref,
            _lib: lib,
        })
    }

    /// Its sound effects (instruments and the rest left out), each as a `Found`.
    #[allow(unsafe_code)]
    pub fn effects(&self, bundle: &Path) -> Vec<Found> {
        let f = &self.factory;
        let mut vendor = String::new();
        let mut fi: PFactoryInfo = unsafe_zeroed();
        // SAFETY: a struct for the factory to fill.
        if unsafe { f.getFactoryInfo(&mut fi) } == kResultOk {
            vendor = text(&fi.vendor);
        }
        let f2 = f.cast::<IPluginFactory2>();
        let mut out = Vec::new();
        // SAFETY: the factory's own count and classes, each into a struct of ours.
        for i in 0..unsafe { f.countClasses() } {
            let (cid, category, name, subs, class_vendor) = match &f2 {
                Some(f2) => {
                    let mut c: PClassInfo2 = unsafe_zeroed();
                    // SAFETY: as above.
                    if unsafe { f2.getClassInfo2(i, &mut c) } != kResultOk {
                        continue;
                    }
                    (c.cid, text(&c.category), text(&c.name), text(&c.subCategories), text(&c.vendor))
                }
                None => {
                    let mut c: PClassInfo = unsafe_zeroed();
                    // SAFETY: as above.
                    if unsafe { f.getClassInfo(i, &mut c) } != kResultOk {
                        continue;
                    }
                    (c.cid, text(&c.category), text(&c.name), String::new(), String::new())
                }
            };
            if category != "Audio Module Class" {
                continue;
            }
            // An instrument makes sound from notes: no use on a source's sound.
            if subs.split('|').any(|s| s.eq_ignore_ascii_case("Instrument")) {
                continue;
            }
            out.push(Found {
                path: bundle.display().to_string(),
                cid: hex(&cid),
                name,
                vendor: if class_vendor.is_empty() { vendor.clone() } else { class_vendor },
                kinds: subs,
            });
        }
        out
    }
}

impl Drop for Module {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: dropped once, here, and never used again; the library is still loaded.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.factory) };
        if let Some(exit) = self.exit {
            // SAFETY: the SDK's exit, called once, before the library goes.
            unsafe { exit() };
        }
    }
}

/// A plain-data SDK struct, zeroed (the SDK's own hosts do the same before a get…Info call).
#[allow(unsafe_code)]
pub fn unsafe_zeroed<T: Copy>() -> T {
    // SAFETY: only used for the SDK's info structs: integers and char arrays, for which all
    // zeros is a valid value.
    unsafe { std::mem::zeroed() }
}

/// A NUL-ended char array as text.
pub fn text(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A class id as 32 hex digits, as written in settings.
pub fn hex(cid: &TUID) -> String {
    cid.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

/// 32 hex digits back into a class id.
pub fn cid_of(hex: &str) -> Option<TUID> {
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let mut t: TUID = [0; 16];
    for (i, b) in t.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()? as c_char;
    }
    Some(t)
}

/// The interface id as the SDK's TUID (what `createInstance` takes).
pub fn iid_of<I: Interface>() -> TUID {
    I::IID.map(|b| b as c_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_ids_go_to_hex_and_back() {
        let t: TUID = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, -1];
        let h = hex(&t);
        assert_eq!(h, "0102030405060708090A0B0C0D0E0FFF");
        assert_eq!(cid_of(&h), Some(t));
        assert_eq!(cid_of("xyz"), None);
        assert_eq!(cid_of("0102030405060708090A0B0C0D0E0FZZ"), None);
    }
}
