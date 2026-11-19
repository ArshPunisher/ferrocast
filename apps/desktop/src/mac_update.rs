//! macOS: puts a downloaded, already verified update in place inside the same
//! `Ferrocast.app` folder.
//!
//! The updater plugin swaps the whole bundle for a new folder. That new folder is a new file
//! on disk, and the Dock keeps a pinned app by its file, so after every update the pin
//! pointed at the old, deleted bundle and Ferrocast ran as a second Dock icon (owner, 9 Nov).
//! Here only `Contents` is swapped: `Ferrocast.app` itself stays the same folder, so the pin,
//! Login Items and Finder aliases keep finding it. If this can't be done (another volume, a
//! folder that needs an admin password), the caller falls back to the plugin's own install.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// The `.app` folder this executable runs from (`…/Ferrocast.app/Contents/MacOS/ferrocast`).
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

/// Unpacks `tar_gz` (`Ferrocast.app/…`, as the release's `Ferrocast.app.tar.gz`) and puts its
/// `Contents` in place of `bundle`'s, keeping `bundle` itself. On any error `bundle` is left
/// as it was.
pub fn install(tar_gz: &[u8], bundle: &Path) -> io::Result<()> {
    let bundle_dev = std::fs::symlink_metadata(bundle)?.dev();
    // Renames only work within one volume: unpack next to the app when the temp folder is on
    // another one.
    let root = [Some(std::env::temp_dir()), bundle.parent().map(Path::to_path_buf)]
        .into_iter()
        .flatten()
        .find(|d| d.metadata().is_ok_and(|m| m.dev() == bundle_dev))
        .ok_or_else(|| io::Error::other("no folder on the app's volume to unpack the update in"))?;
    let staged = tempfile::Builder::new().prefix("ferrocast_update").tempdir_in(&root)?;
    unpack(tar_gz, staged.path())?;
    let new_contents = staged.path().join("Contents");
    // The update must hold the app's own program, or it isn't this app.
    let exe_name = main_exe(bundle).ok_or_else(|| io::Error::other("can't tell this app's program"))?;
    if !new_contents.join("MacOS").join(&exe_name).is_file() {
        return Err(io::Error::other("the update has no app inside"));
    }
    let contents = bundle.join("Contents");
    match swap(&contents, &new_contents) {
        Ok(()) => {}
        // A volume that can't swap (HFS+): move the old one aside, then the new one in.
        Err(e) if e.kind() != io::ErrorKind::PermissionDenied => {
            let aside = staged.path().join("previous");
            std::fs::rename(&contents, &aside)?;
            if let Err(e) = std::fs::rename(&new_contents, &contents) {
                let _ = std::fs::rename(&aside, &contents);
                return Err(e);
            }
        }
        Err(e) => return Err(e),
    }
    // The old Contents now sits in the staging folder, removed when it drops. A new
    // modification date tells Launch Services to read the bundle again.
    let _ = std::process::Command::new("touch").arg(bundle).status();
    Ok(())
}

/// The bundle's program: this executable's name when it runs from `bundle`, else the first
/// file in its `Contents/MacOS`.
fn main_exe(bundle: &Path) -> Option<std::ffi::OsString> {
    let exe = std::env::current_exe().ok();
    if let Some(exe) = exe.filter(|e| bundle_of(e).as_deref() == Some(bundle)) {
        return exe.file_name().map(|n| n.to_os_string());
    }
    let mut names: Vec<_> = std::fs::read_dir(bundle.join("Contents/MacOS"))
        .ok()?
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name())
        .collect();
    names.sort();
    names.into_iter().next()
}

/// A real bundle and package, when given (`FERROCAST_UPDATE_APP`, `FERROCAST_UPDATE_PKG`):
/// `cargo test -p ferrocast-desktop real_update -- --ignored` against a copy of the app.
#[cfg(test)]
pub fn install_from_env() -> Option<io::Result<()>> {
    let app = std::env::var_os("FERROCAST_UPDATE_APP")?;
    let pkg = std::fs::read(std::env::var_os("FERROCAST_UPDATE_PKG")?).ok()?;
    Some(install(&pkg, Path::new(&app)))
}

/// Unpacks the archive's `Ferrocast.app/…` into `into` (its first folder dropped). A path that
/// climbs out (`..`) or is absolute is refused, though the archive is signed.
fn unpack(tar_gz: &[u8], into: &Path) -> io::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tar_gz));
    archive.set_preserve_permissions(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let inside: PathBuf = path.components().skip(1).collect();
        if inside.as_os_str().is_empty() {
            continue;
        }
        if inside.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(io::Error::other(format!("the update has an unsafe path: {}", path.display())));
        }
        let to = into.join(&inside);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&to)?;
    }
    // Read to the end so a cut-off archive fails here, not later.
    let mut rest = Vec::new();
    archive.into_inner().read_to_end(&mut rest)?;
    Ok(())
}

/// Exchanges two folders in one step (APFS), so `a` is never missing.
fn swap(a: &Path, b: &Path) -> io::Result<()> {
    let a = CString::new(a.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let b = CString::new(b.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: both are valid NUL-terminated paths that outlive the call, which only reads them.
    let r = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            let mut t = tar::Builder::new(gz);
            for (name, body) in files {
                let mut h = tar::Header::new_old();
                // Written by hand: the tar crate itself refuses a name that climbs out.
                h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
                h.set_size(body.len() as u64);
                h.set_mode(0o755);
                h.set_entry_type(tar::EntryType::Regular);
                h.set_cksum();
                t.append(&h, *body).unwrap_or_else(|e| panic!("{e}"));
            }
            t.into_inner().and_then(|gz| gz.finish()).unwrap_or_else(|e| panic!("{e}"));
        }
        out
    }

    fn bundle(dir: &Path, body: &[u8]) -> PathBuf {
        let app = dir.join("Ferrocast.app");
        let exe = std::env::current_exe().unwrap_or_else(|e| panic!("{e}"));
        let name = exe.file_name().unwrap_or_default();
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(app.join("Contents/MacOS").join(name), body).unwrap_or_else(|e| panic!("{e}"));
        app
    }

    fn exe_entry() -> String {
        let exe = std::env::current_exe().unwrap_or_else(|e| panic!("{e}"));
        format!("Ferrocast.app/Contents/MacOS/{}", exe.file_name().unwrap_or_default().to_string_lossy())
    }

    #[test]
    fn the_update_goes_inside_the_same_app_folder() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let app = bundle(dir.path(), b"old");
        let before = std::fs::metadata(&app).unwrap_or_else(|e| panic!("{e}")).ino();
        let exe = exe_entry();
        let pkg = tar_gz(&[(exe.as_str(), b"new"), ("Ferrocast.app/Contents/Info.plist", b"plist")]);
        install(&pkg, &app).unwrap_or_else(|e| panic!("{e}"));
        let after = std::fs::metadata(&app).unwrap_or_else(|e| panic!("{e}")).ino();
        assert_eq!(before, after, "the same Ferrocast.app folder: the Dock's pin still finds it");
        let name = PathBuf::from(&exe).file_name().map(|n| n.to_os_string()).unwrap_or_default();
        assert_eq!(std::fs::read(app.join("Contents/MacOS").join(name)).unwrap_or_default(), b"new");
        assert!(app.join("Contents/Info.plist").is_file());
    }

    #[test]
    fn a_bad_update_leaves_the_app_as_it_was() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let app = bundle(dir.path(), b"old");
        let exe = exe_entry();
        // Climbing out of the bundle, and an archive with no app inside.
        for pkg in
            [tar_gz(&[("Ferrocast.app/../../evil", b"x")]), tar_gz(&[("Ferrocast.app/Contents/Info.plist", b"plist")])]
        {
            assert!(install(&pkg, &app).is_err());
        }
        assert!(install(b"not an archive", &app).is_err());
        let name = PathBuf::from(&exe).file_name().map(|n| n.to_os_string()).unwrap_or_default();
        assert_eq!(std::fs::read(app.join("Contents/MacOS").join(name)).unwrap_or_default(), b"old");
        assert!(!dir.path().join("evil").exists());
    }

    #[test]
    #[ignore = "needs a copy of the real app and its update package"]
    fn real_update() {
        match install_from_env() {
            Some(r) => r.unwrap_or_else(|e| panic!("{e}")),
            None => panic!("set FERROCAST_UPDATE_APP and FERROCAST_UPDATE_PKG"),
        }
    }

    #[test]
    fn the_bundle_is_found_from_the_executable() {
        let exe = Path::new("/Applications/Ferrocast.app/Contents/MacOS/ferrocast");
        assert_eq!(bundle_of(exe), Some(PathBuf::from("/Applications/Ferrocast.app")));
        assert_eq!(bundle_of(Path::new("/usr/local/bin/ferrocast")), None);
    }
}
