//! Installing and removing the app's folder, without ever leaving half an app behind.
//!
//! A new version is unpacked beside the install folder (`<folder>.incoming`), checked, then
//! swapped in with two renames: the old folder becomes `<folder>.previous`, the new one takes
//! its name, and the old one is deleted. If anything fails before the swap, the old app is
//! untouched; if the swap itself fails, the old folder is put back. A crash in the middle is
//! repaired the next time setup runs.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::payload::{self, Manifest, Progress, UnpackError};

/// What setup leaves in the install folder: the manifest of what it put there.
pub const MARK: &str = ".ferrocast-setup.json";

#[derive(Debug)]
pub enum Problem {
    /// The chosen folder has someone else's files in it.
    NotOurs(PathBuf),
    Cancelled,
    Damaged(String),
    /// Not enough room on the drive.
    NoSpace { need: u64, free: u64, drive: String },
    /// A file couldn't be written, moved or removed.
    Io { what: String, err: io::Error },
}

impl Problem {
    pub fn io(what: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        let what = what.into();
        move |err| Self::Io { what, err }
    }

    /// One plain sentence for the error screen, and what to do about it.
    pub fn words(&self) -> (String, String) {
        match self {
            Self::NotOurs(dir) => (
                format!("{} already has other files in it.", dir.display()),
                "Pick an empty folder, or one that only holds Ferrocast.".into(),
            ),
            Self::Cancelled => ("Setup was stopped.".into(), "Nothing was changed.".into()),
            Self::Damaged(why) => (
                format!("This setup file is damaged ({why})."),
                "Download it again from ferrocast.clearcraftlabs.com. Nothing was changed.".into(),
            ),
            Self::NoSpace { need, free, drive } => (
                format!("Not enough space on {drive}"),
                format!(
                    "Ferrocast needs {} and {drive} has {} free. Free some space, or pick another drive in Options. Nothing was changed.",
                    mb(*need),
                    mb(*free)
                ),
            ),
            Self::Io { what, err } if is_full(err) => {
                (format!("The drive filled up while {what}."), "Free some space and try again. Nothing was changed.".into())
            }
            Self::Io { what, err } if err.kind() == io::ErrorKind::PermissionDenied => (
                format!("Windows didn't allow {what} ({err})."),
                "Close Ferrocast and any window showing its folder, then try again.".into(),
            ),
            Self::Io { what, err } => (format!("Something went wrong while {what}: {err}."), "Try again. If it keeps happening, send us the details.".into()),
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (what, next) = self.words();
        write!(f, "{what} {next}")
    }
}

fn is_full(err: &io::Error) -> bool {
    // ERROR_DISK_FULL (112) and ERROR_HANDLE_DISK_FULL (39) on Windows.
    err.kind() == io::ErrorKind::StorageFull || matches!(err.raw_os_error(), Some(112 | 39)) && cfg!(windows)
}

/// `bytes` as megabytes for people ("118.4 MB").
pub fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

fn sibling(dir: &Path, suffix: &str) -> PathBuf {
    let mut name = dir.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    dir.with_file_name(name)
}

pub fn incoming(dir: &Path) -> PathBuf {
    sibling(dir, ".incoming")
}

pub fn previous(dir: &Path) -> PathBuf {
    sibling(dir, ".previous")
}

/// Whether setup may replace `dir`: it doesn't exist, is empty, or holds Ferrocast (put there
/// by this setup, or by the older installer, which left `uninstall.exe` beside the program).
pub fn ours_or_empty(dir: &Path, main: &str) -> bool {
    match std::fs::read_dir(dir) {
        Err(e) => e.kind() == io::ErrorKind::NotFound,
        Ok(mut entries) => {
            entries.next().is_none() || dir.join(MARK).is_file() || (dir.join(main).is_file() && dir.join("uninstall.exe").is_file())
        }
    }
}

/// Renames, trying again for a moment: antivirus scanners often hold a fresh file briefly.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if tries < 20 && e.kind() != io::ErrorKind::NotFound => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(150));
                let _ = e;
            }
            Err(e) => return Err(e),
        }
    }
}

fn remove_all(dir: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Puts things right after a setup that was stopped half way: a swapped-out app comes back,
/// a half-unpacked one goes.
pub fn repair(dir: &Path) -> Result<(), Problem> {
    let prev = previous(dir);
    if prev.is_dir() {
        if dir.exists() {
            remove_all(&prev).map_err(Problem::io("clearing the last update's leftovers"))?;
        } else {
            rename(&prev, dir).map_err(Problem::io("putting the last version back"))?;
        }
    }
    remove_all(&incoming(dir)).map_err(Problem::io("clearing a stopped install"))
}

/// Installs the packed app into `dir`, replacing what's there. `on` gets progress and returns
/// false to stop (before the swap, so a stop changes nothing).
pub fn install(
    packed: &[u8],
    manifest: &Manifest,
    dir: &Path,
    extra: &[(&str, &[u8])],
    on: &mut dyn FnMut(Progress<'_>) -> bool,
) -> Result<(), Problem> {
    if !ours_or_empty(dir, &manifest.main) {
        return Err(Problem::NotOurs(dir.to_path_buf()));
    }
    repair(dir)?;
    let new = incoming(dir);
    std::fs::create_dir_all(&new).map_err(Problem::io("making the install folder"))?;
    let unpacked = payload::unpack(packed, manifest, &new, on).map_err(|e| match e {
        UnpackError::Cancelled => Problem::Cancelled,
        UnpackError::Damaged(why) => Problem::Damaged(why),
        UnpackError::Write { path, err } => Problem::Io { what: format!("writing {}", path.display()), err },
    });
    let marked = unpacked.and_then(|()| {
        // Files setup adds itself (its own uninstaller), before the swap like the rest.
        for (name, bytes) in extra {
            let rel = payload::safe_path(name).ok_or_else(|| Problem::Damaged(format!("unsafe name {name}")))?;
            std::fs::write(new.join(rel), bytes).map_err(Problem::io(format!("writing {name}")))?;
        }
        let json = serde_json::to_vec_pretty(manifest).map_err(|e| Problem::Io { what: "noting what was installed".into(), err: io::Error::other(e) })?;
        std::fs::write(new.join(MARK), json).map_err(Problem::io("noting what was installed"))
    });
    if let Err(e) = marked {
        let _ = remove_all(&new);
        return Err(e);
    }
    swap(dir)
}

/// The two renames that put `<dir>.incoming` in place of `dir`.
fn swap(dir: &Path) -> Result<(), Problem> {
    let (new, prev) = (incoming(dir), previous(dir));
    let had_old = dir.exists();
    if had_old
        && let Err(err) = rename(dir, &prev) {
            let _ = remove_all(&new);
            return Err(Problem::Io { what: "moving the old version aside".into(), err });
        }
    if let Err(err) = rename(&new, dir) {
        if had_old {
            let _ = rename(&prev, dir);
        }
        let _ = remove_all(&new);
        return Err(Problem::Io { what: "putting the new version in place".into(), err });
    }
    // The old version is no longer needed; a file still held is cleared next time.
    let _ = remove_all(&prev);
    Ok(())
}

/// Every file under `dir`, for counting removal progress.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => files_under(&path, out),
            Ok(_) => out.push(path),
            Err(_) => {}
        }
    }
}

/// Removes the install folder, file by file so progress is real. `skip` is left in place (the
/// running uninstaller, which deletes itself after). Only a folder that holds Ferrocast.
pub fn remove_app(dir: &Path, main: &str, skip: Option<&Path>, on: &mut dyn FnMut(usize, usize, &Path)) -> Result<(), Problem> {
    if !dir.exists() {
        return Ok(());
    }
    if !ours_or_empty(dir, main) {
        return Err(Problem::NotOurs(dir.to_path_buf()));
    }
    let mut files = Vec::new();
    files_under(dir, &mut files);
    let total = files.len();
    for (i, f) in files.iter().enumerate() {
        if skip.is_some_and(|s| same(s, f)) {
            continue;
        }
        on(i, total, f);
        match std::fs::remove_file(f) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(Problem::Io { what: format!("removing {}", f.display()), err }),
        }
    }
    on(total, total, dir);
    // Folders, deepest first; a folder still holding the skipped file stays.
    remove_empty_dirs(dir);
    Ok(())
}

fn same(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

fn remove_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_dirs(&entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// Removes a folder of the app's own (data, downloads), if it's there.
pub fn remove_folder(dir: &Path, what: &str) -> Result<(), Problem> {
    remove_all(dir).map_err(Problem::io(format!("removing {what}")))
}

/// What the app keeps on this computer, by what the uninstall choices call it.
#[derive(Debug, Clone)]
pub struct DataPlaces {
    /// Scenes, settings, history (`%APPDATA%\Ferrocast`).
    pub data: PathBuf,
    /// The web view's own storage (`%APPDATA%` and `%LOCALAPPDATA%\com.clearcraftlabs.ferrocast`).
    pub webview: Vec<PathBuf>,
}

impl DataPlaces {
    /// Downloaded extras inside the data folder: FFmpeg and speech/background models.
    pub fn extras(&self) -> Vec<PathBuf> {
        vec![self.data.join("tools"), self.data.join("models")]
    }

    /// How much the downloaded extras take up.
    pub fn extras_size(&self) -> u64 {
        self.extras().iter().map(|d| size_of(d)).sum()
    }
}

/// Bytes under `dir`.
pub fn size_of(dir: &Path) -> u64 {
    let mut files = Vec::new();
    files_under(dir, &mut files);
    files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::pack;

    fn app(version: &str, extra: &str) -> (Manifest, Vec<u8>) {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("resources")).unwrap();
        std::fs::write(src.path().join("Ferrocast.exe"), format!("program {version}")).unwrap();
        std::fs::write(src.path().join("resources").join(extra), "x").unwrap();
        let mut out = Vec::new();
        let m = pack(src.path(), version, "Ferrocast.exe", &mut out).unwrap();
        let (m2, body) = payload::read_head(&out).unwrap();
        assert_eq!(m, m2);
        let body = body.to_vec();
        (m, body)
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn a_new_version_replaces_the_old_one_whole() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Ferrocast");
        let (m1, b1) = app("1.0", "old.js");
        install(&b1, &m1, &dir, &[], &mut |_| true).unwrap();
        assert_eq!(read(&dir.join("Ferrocast.exe")), "program 1.0");
        assert!(dir.join(MARK).is_file());

        let (m2, b2) = app("2.0", "new.js");
        install(&b2, &m2, &dir, &[], &mut |_| true).unwrap();
        assert_eq!(read(&dir.join("Ferrocast.exe")), "program 2.0");
        // No file of the old version lingers, and nothing is left beside the folder.
        assert!(!dir.join("resources/old.js").exists());
        assert!(dir.join("resources/new.js").exists());
        let names: Vec<_> = std::fs::read_dir(root.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("Ferrocast")]);
    }

    #[test]
    fn a_stopped_or_failed_install_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Ferrocast");
        let (m1, b1) = app("1.0", "old.js");
        install(&b1, &m1, &dir, &[], &mut |_| true).unwrap();

        let (m2, b2) = app("2.0", "new.js");
        assert!(matches!(install(&b2, &m2, &dir, &[], &mut |p| p.done == 0), Err(Problem::Cancelled)));
        assert_eq!(read(&dir.join("Ferrocast.exe")), "program 1.0");
        assert!(!incoming(&dir).exists());

        let mut bad = m2.clone();
        bad.sha256 = "0".repeat(64);
        assert!(matches!(install(&b2, &bad, &dir, &[], &mut |_| true), Err(Problem::Damaged(_))));
        assert_eq!(read(&dir.join("Ferrocast.exe")), "program 1.0");
        assert!(!incoming(&dir).exists());
    }

    #[test]
    fn a_crash_in_the_middle_is_repaired() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Ferrocast");
        let (m1, b1) = app("1.0", "old.js");
        install(&b1, &m1, &dir, &[], &mut |_| true).unwrap();
        // As if setup died between the two renames.
        std::fs::rename(&dir, previous(&dir)).unwrap();
        std::fs::create_dir_all(incoming(&dir)).unwrap();
        repair(&dir).unwrap();
        assert_eq!(read(&dir.join("Ferrocast.exe")), "program 1.0");
        assert!(!previous(&dir).exists() && !incoming(&dir).exists());
    }

    #[test]
    fn other_peoples_folders_are_never_replaced_or_removed() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Games");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("save.dat"), "precious").unwrap();
        let (m, b) = app("1.0", "a.js");
        assert!(matches!(install(&b, &m, &dir, &[], &mut |_| true), Err(Problem::NotOurs(_))));
        assert!(matches!(remove_app(&dir, "Ferrocast.exe", None, &mut |_, _, _| {}), Err(Problem::NotOurs(_))));
        assert_eq!(read(&dir.join("save.dat")), "precious");

        // The older installer's folder (program + its uninstall.exe) is taken over.
        let old = root.path().join("Ferrocast");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("Ferrocast.exe"), "nsis").unwrap();
        std::fs::write(old.join("uninstall.exe"), "nsis").unwrap();
        install(&b, &m, &old, &[], &mut |_| true).unwrap();
        assert!(!old.join("uninstall.exe").exists());
    }

    #[test]
    fn removing_counts_every_file_and_keeps_the_running_uninstaller() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Ferrocast");
        let (m, b) = app("1.0", "a.js");
        install(&b, &m, &dir, &[], &mut |_| true).unwrap();
        std::fs::write(dir.join("uninstall.exe"), "me").unwrap();
        let mut seen = Vec::new();
        remove_app(&dir, "Ferrocast.exe", Some(&dir.join("uninstall.exe")), &mut |i, n, _| seen.push((i, n))).unwrap();
        assert_eq!(seen.last(), Some(&(4, 4)));
        assert!(dir.join("uninstall.exe").exists());
        assert!(!dir.join("Ferrocast.exe").exists() && !dir.join("resources").exists());
    }

    #[test]
    fn problems_read_as_sentences() {
        let p = Problem::NoSpace { need: 124_000_000, free: 67_000_000, drive: "C:".into() };
        let (what, next) = p.words();
        assert_eq!(what, "Not enough space on C:");
        assert!(next.starts_with("Ferrocast needs 118.3 MB and C: has 63.9 MB free."), "{next}");
        assert!(next.ends_with("Nothing was changed."));
    }
}
