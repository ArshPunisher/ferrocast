//! The app's entry in Windows' installed-apps list (Settings > Apps), for this user.
//!
//! The same keys the old installer wrote, so installing over it takes its entry over and
//! Windows keeps one Ferrocast in the list.

use std::io;
use std::path::{Path, PathBuf};

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

use super::{APP, PUBLISHER};

const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Ferrocast";
/// Where the old installer noted the install folder (its default value).
const MANUFACTURER: &str = r"Software\ClearCraft Labs\Ferrocast";

/// What the installed-apps entry says.
#[derive(Debug, Clone)]
pub struct Entry {
    pub dir: PathBuf,
    pub version: String,
    pub main: String,
    /// Kilobytes, for the size Windows shows.
    pub size_kb: u32,
}

pub fn write(e: &Entry) -> io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(UNINSTALL)?;
    let dir = e.dir.display().to_string();
    let uninstaller = format!("\"{}\"", e.dir.join("uninstall.exe").display());
    key.set_value("DisplayName", &APP)?;
    key.set_value("DisplayVersion", &e.version)?;
    key.set_value("Publisher", &PUBLISHER)?;
    key.set_value("DisplayIcon", &format!("\"{}\"", e.dir.join(&e.main).display()))?;
    key.set_value("InstallLocation", &format!("\"{dir}\""))?;
    key.set_value("UninstallString", &format!("{uninstaller} --uninstall"))?;
    key.set_value("QuietUninstallString", &format!("{uninstaller} --uninstall /S"))?;
    key.set_value("MainBinaryName", &e.main)?;
    key.set_value("URLInfoAbout", &"https://ferrocast.clearcraftlabs.com")?;
    key.set_value("HelpLink", &"https://ferrocast.clearcraftlabs.com")?;
    key.set_value("NoModify", &1u32)?;
    key.set_value("NoRepair", &1u32)?;
    key.set_value("EstimatedSize", &e.size_kb)?;
    let (manu, _) = hkcu.create_subkey(MANUFACTURER)?;
    manu.set_value("", &dir)?;
    Ok(())
}

/// Where Ferrocast is installed, if it is (by this setup or the old installer).
pub fn installed() -> Option<(PathBuf, Option<String>)> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey_with_flags(UNINSTALL, KEY_READ).ok()?;
    let loc: String = key.get_value("InstallLocation").ok()?;
    let dir = PathBuf::from(loc.trim().trim_matches('"'));
    let version = key.get_value("DisplayVersion").ok();
    (!dir.as_os_str().is_empty()).then_some((dir, version))
}

/// The program name the installed entry names (`Ferrocast.exe`), for an older install.
pub fn main_binary() -> Option<String> {
    RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(UNINSTALL, KEY_READ).ok()?.get_value("MainBinaryName").ok()
}

pub fn remove(dir: &Path) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    // Only our own entry: another install folder's entry is someone else's to remove.
    if installed().is_none_or(|(d, _)| same_dir(&d, dir)) {
        let _ = hkcu.delete_subkey_all(UNINSTALL);
        let _ = hkcu.delete_subkey_all(MANUFACTURER);
        let _ = hkcu.delete_subkey(r"Software\ClearCraft Labs");
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase();
    n(a) == n(b)
}
