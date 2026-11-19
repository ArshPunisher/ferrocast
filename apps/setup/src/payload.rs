//! The app's files, packed into the setup program.
//!
//! A setup `.exe` is the setup program (the "stub") with the packed app appended and a short
//! trailer at the very end, so one program can install itself anywhere:
//!
//! ```text
//! [stub .exe][FCPK1\0][u32 manifest length][manifest JSON][xz stream of every file][u64 stub length][FCSETUP1]
//! ```
//!
//! The manifest lists every file (path and size, in stream order) and the SHA-256 of all of
//! them together, so a damaged download is found before anything is swapped in. Paths are
//! checked before any file is written: none may leave the install folder.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Ends every setup program that carries the app.
pub const TRAILER: &[u8; 8] = b"FCSETUP1";
/// Starts the packed app.
const HEAD: &[u8; 6] = b"FCPK1\0";
/// A manifest bigger than this is not ours (a few hundred files take a few kB).
const MAX_MANIFEST: u32 = 8 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The app version these files are.
    pub version: String,
    /// The app's program, at the top of the install folder (`Ferrocast.exe`).
    pub main: String,
    pub files: Vec<Entry>,
    /// Every file's size added up: what the progress bar counts to.
    pub total: u64,
    /// SHA-256 (hex) of every file's bytes, in order.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Relative, with `/` between folders.
    pub path: String,
    pub size: u64,
}

/// How far unpacking has got.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'a> {
    pub done: u64,
    pub total: u64,
    /// The file being written.
    pub file: &'a str,
}

#[derive(Debug)]
pub enum UnpackError {
    /// The caller asked to stop.
    Cancelled,
    /// The packed files don't match what the manifest says (a damaged download).
    Damaged(String),
    /// Writing a file failed (disk full, no permission, the file in use).
    Write { path: PathBuf, err: io::Error },
}

impl std::fmt::Display for UnpackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("stopped"),
            Self::Damaged(why) => write!(f, "the setup file is damaged ({why})"),
            Self::Write { path, err } => write!(f, "{}: {err}", path.display()),
        }
    }
}

impl std::error::Error for UnpackError {}

/// Every file under `dir`, relative and sorted (so two packs of the same folder match).
fn list(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_symlink() {
                return Err(io::Error::other(format!("{} is a link; links aren't packed", path.display())));
            }
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else {
                let rel = path.strip_prefix(root).map_err(io::Error::other)?;
                let parts: Vec<String> =
                    rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                out.push((parts.join("/"), path));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Packs every file under `dir` into `out`. `main` must be one of them, at the top.
pub fn pack(dir: &Path, version: &str, main: &str, out: &mut impl Write) -> io::Result<Manifest> {
    let files = list(dir)?;
    if !files.iter().any(|(rel, _)| rel == main) {
        return Err(io::Error::other(format!("{main} isn't in {}", dir.display())));
    }
    let mut hash = Sha256::new();
    let mut entries = Vec::with_capacity(files.len());
    for (rel, path) in &files {
        safe_path(rel).ok_or_else(|| io::Error::other(format!("{rel} can't be packed")))?;
        let mut f = File::open(path)?;
        let size = io::copy(&mut f, &mut HashWriter(&mut hash))?;
        entries.push(Entry { path: rel.clone(), size });
    }
    let manifest = Manifest {
        version: version.to_string(),
        main: main.to_string(),
        total: entries.iter().map(|e| e.size).sum(),
        files: entries,
        sha256: hex(&hash.finalize()),
    };
    let json = serde_json::to_vec(&manifest).map_err(io::Error::other)?;
    out.write_all(HEAD)?;
    out.write_all(&u32::try_from(json.len()).map_err(io::Error::other)?.to_le_bytes())?;
    out.write_all(&json)?;

    // Strongest LZMA2, in 32 MB blocks so every core helps. No x86 (BCJ) pre-filter:
    // lzma-rust2 0.25's damaged real programs with it (the first runner test caught it).
    let mut opts = lzma_rust2::XzOptions::with_preset(9);
    opts.set_block_size(NonZeroU64::new(32 << 20));
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).min(16) as u32;
    let mut xz = lzma_rust2::XzWriterMt::new(out, opts, workers)?;
    for (_, path) in &files {
        io::copy(&mut File::open(path)?, &mut xz)?;
    }
    xz.finish()?;
    Ok(manifest)
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The manifest and the compressed files after it.
pub fn read_head(payload: &[u8]) -> Result<(Manifest, &[u8]), UnpackError> {
    let bad = |why: &str| UnpackError::Damaged(why.to_string());
    let rest = payload.strip_prefix(HEAD).ok_or_else(|| bad("no packed app"))?;
    let (len, rest) = rest.split_at_checked(4).ok_or_else(|| bad("cut short"))?;
    let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]);
    if len > MAX_MANIFEST {
        return Err(bad("file list too big"));
    }
    let (json, packed) = rest.split_at_checked(len as usize).ok_or_else(|| bad("cut short"))?;
    let manifest: Manifest = serde_json::from_slice(json).map_err(|e| bad(&format!("file list: {e}")))?;
    if manifest.files.iter().map(|e| e.size).sum::<u64>() != manifest.total {
        return Err(bad("sizes don't add up"));
    }
    for e in &manifest.files {
        if safe_path(&e.path).is_none() {
            return Err(bad(&format!("unsafe path {}", e.path)));
        }
    }
    if safe_path(&manifest.main).is_none() || manifest.main.contains('/') {
        return Err(bad("unsafe program name"));
    }
    Ok((manifest, packed))
}

/// `rel` as a path that stays inside the folder it's joined to, or `None`: no `..`, no
/// absolute or drive paths, no empty parts, no characters Windows won't allow in a name.
pub fn safe_path(rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.len() > 400 {
        return None;
    }
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        let bad_char = |c: char| matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control();
        if part.is_empty() || part == "." || part == ".." || part.ends_with(['.', ' ']) || part.chars().any(bad_char) {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

/// Writes every file into `dest` (which must exist), reporting as it goes. `on` returns
/// false to stop. The files are checked against the manifest's SHA-256 at the end: a damaged
/// payload is an error even if every file was written.
pub fn unpack(
    packed: &[u8],
    manifest: &Manifest,
    dest: &Path,
    on: &mut dyn FnMut(Progress<'_>) -> bool,
) -> Result<(), UnpackError> {
    let mut xz = lzma_rust2::XzReader::new(packed, false);
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    let damaged = |e: io::Error| UnpackError::Damaged(e.to_string());
    for e in &manifest.files {
        let rel = safe_path(&e.path).ok_or_else(|| UnpackError::Damaged(format!("unsafe path {}", e.path)))?;
        let path = dest.join(rel);
        let write_err = |err| UnpackError::Write { path: path.clone(), err };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(write_err)?;
        }
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&path).map_err(write_err)?);
        let mut left = e.size;
        if !on(Progress { done, total: manifest.total, file: &e.path }) {
            return Err(UnpackError::Cancelled);
        }
        while left > 0 {
            let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
            let n = xz.read(&mut buf[..want]).map_err(damaged)?;
            if n == 0 {
                return Err(UnpackError::Damaged(format!("{} is cut short", e.path)));
            }
            hash.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(write_err)?;
            left -= n as u64;
            done += n as u64;
            if !on(Progress { done, total: manifest.total, file: &e.path }) {
                return Err(UnpackError::Cancelled);
            }
        }
        out.into_inner().map_err(|e| write_err(e.into_error()))?.sync_all().map_err(write_err)?;
    }
    if xz.read(&mut buf[..1]).map_err(damaged)? != 0 {
        return Err(UnpackError::Damaged("extra data after the last file".into()));
    }
    if hex(&hash.finalize()) != manifest.sha256 {
        return Err(UnpackError::Damaged("checksum doesn't match".into()));
    }
    Ok(())
}

/// The setup program `stub` carrying `payload`.
pub fn attach(stub: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(stub.len() + payload.len() + 16);
    out.extend_from_slice(stub);
    out.extend_from_slice(payload);
    out.extend_from_slice(&(stub.len() as u64).to_le_bytes());
    out.extend_from_slice(TRAILER);
    out
}

/// Where a Windows code signature (Authenticode) starts, when one ends the file: signing
/// appends it after everything, so the packed app's trailer sits just before it.
fn signature_start(exe: &[u8]) -> Option<usize> {
    let u16_at = |at: usize| exe.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |at: usize| exe.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if exe.get(..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(0x3c)?;
    if exe.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let opt = pe + 24;
    // The certificate table is data directory 4; its place depends on PE32 or PE32+.
    let dirs = match u16_at(opt)? {
        0x10b => opt + 96,
        0x20b => opt + 112,
        _ => return None,
    };
    let (start, len) = (u32_at(dirs + 32)?, u32_at(dirs + 36)?);
    (len > 0 && start.checked_add(len)? == exe.len()).then_some(start)
}

/// The setup program alone, and the packed app if it carries one.
pub fn split(exe: &[u8]) -> (&[u8], Option<&[u8]>) {
    let end = signature_start(exe).unwrap_or(exe.len());
    let Some(body) = exe[..end].strip_suffix(TRAILER) else { return (exe, None) };
    let Some((rest, len)) = body.split_last_chunk::<8>() else { return (exe, None) };
    match usize::try_from(u64::from_le_bytes(*len)) {
        Ok(stub) if stub <= rest.len() => (&rest[..stub], Some(&rest[stub..])),
        _ => (exe, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(dir: &Path) {
        std::fs::create_dir_all(dir.join("resources/dashboard")).unwrap();
        std::fs::write(dir.join("Ferrocast.exe"), b"MZ program bytes".repeat(5000)).unwrap();
        std::fs::write(dir.join("resources/dashboard/app.js"), b"console.log(1)").unwrap();
        std::fs::write(dir.join("resources/empty.txt"), b"").unwrap();
    }

    fn packed(dir: &Path) -> (Manifest, Vec<u8>) {
        let mut out = Vec::new();
        let m = pack(dir, "2.4.0", "Ferrocast.exe", &mut out).unwrap();
        (m, out)
    }

    #[test]
    fn what_is_packed_comes_back_the_same() {
        let src = tempfile::tempdir().unwrap();
        tree(src.path());
        let (m, payload) = packed(src.path());
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.files[0].path, "Ferrocast.exe");
        assert_eq!(m.total, 80_000 + 14);

        let exe = attach(b"STUB", &payload);
        let (stub, carried) = split(&exe);
        assert_eq!(stub, b"STUB");
        let (read, body) = read_head(carried.unwrap()).unwrap();
        assert_eq!(read, m);

        let dest = tempfile::tempdir().unwrap();
        let mut last = 0;
        unpack(body, &read, dest.path(), &mut |p| {
            assert!(p.done >= last && p.done <= p.total);
            last = p.done;
            true
        })
        .unwrap();
        assert_eq!(last, m.total);
        for f in ["Ferrocast.exe", "resources/dashboard/app.js", "resources/empty.txt"] {
            assert_eq!(std::fs::read(dest.path().join(f)).unwrap(), std::fs::read(src.path().join(f)).unwrap());
        }
        // Repeated program bytes pack small.
        assert!(payload.len() < 4_000, "{}", payload.len());
    }

    #[test]
    fn a_program_without_an_app_is_only_a_program() {
        assert_eq!(split(b"just a program"), (&b"just a program"[..], None));
        // A trailer that points past the start isn't believed.
        let mut fake = b"xx".to_vec();
        fake.extend_from_slice(&999u64.to_le_bytes());
        fake.extend_from_slice(TRAILER);
        assert!(split(&fake).1.is_none());
    }

    #[test]
    fn a_signed_setup_still_finds_its_app() {
        // A tiny PE32+ header whose certificate table (directory 4) covers the file's end.
        let mut stub = vec![0u8; 0x200];
        stub[..2].copy_from_slice(b"MZ");
        stub[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        stub[0x80..0x84].copy_from_slice(b"PE\0\0");
        stub[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
        let mut exe = attach(&stub, b"PAYLOAD");
        let cert_at = exe.len() as u32;
        let dir = 0x98 + 112 + 32;
        exe[dir..dir + 4].copy_from_slice(&cert_at.to_le_bytes());
        exe[dir + 4..dir + 8].copy_from_slice(&64u32.to_le_bytes());
        exe.extend_from_slice(&[7u8; 64]);
        let (s, p) = split(&exe);
        assert_eq!(p, Some(&b"PAYLOAD"[..]));
        assert_eq!(s.len(), stub.len());
    }

    #[test]
    fn a_damaged_download_is_caught() {
        let src = tempfile::tempdir().unwrap();
        tree(src.path());
        let (_, mut payload) = packed(src.path());
        let (m, _) = read_head(&payload).unwrap();
        let body_at = payload.len() - 40;
        payload[body_at] ^= 0x55;
        let dest = tempfile::tempdir().unwrap();
        let (_, body) = read_head(&payload).unwrap();
        let err = unpack(body, &m, dest.path(), &mut |_| true).unwrap_err();
        assert!(matches!(err, UnpackError::Damaged(_)), "{err}");

        // A file list that claims different bytes fails the checksum.
        let (mut m2, body) = read_head(&packed(src.path()).1).map(|(m, b)| (m, b.to_vec())).unwrap();
        m2.sha256 = "0".repeat(64);
        let err = unpack(&body, &m2, dest.path(), &mut |_| true).unwrap_err();
        assert!(matches!(err, UnpackError::Damaged(_)), "{err}");
    }

    #[test]
    fn stopping_is_honoured() {
        let src = tempfile::tempdir().unwrap();
        tree(src.path());
        let (m, payload) = packed(src.path());
        let (_, body) = read_head(&payload).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let err = unpack(body, &m, dest.path(), &mut |p| p.done < 10).unwrap_err();
        assert!(matches!(err, UnpackError::Cancelled));
    }

    #[test]
    fn no_file_can_leave_the_install_folder() {
        for bad in
            ["../evil.exe", "a/../../b", "/etc/passwd", "C:/Windows/x", "a\\..\\b", "a//b", "", "con.", "a/ ", "x?y"]
        {
            assert!(safe_path(bad).is_none(), "{bad}");
        }
        assert_eq!(safe_path("resources/app.js"), Some(PathBuf::from("resources").join("app.js")));

        // A hand-made file list with a way out is refused before anything is written.
        let m = Manifest {
            version: "1".into(),
            main: "Ferrocast.exe".into(),
            files: vec![Entry { path: "../evil.exe".into(), size: 0 }],
            total: 0,
            sha256: String::new(),
        };
        let json = serde_json::to_vec(&m).unwrap();
        let mut payload = HEAD.to_vec();
        payload.extend_from_slice(&(json.len() as u32).to_le_bytes());
        payload.extend_from_slice(&json);
        assert!(matches!(read_head(&payload), Err(UnpackError::Damaged(_))));
    }
}

#[cfg(test)]
mod real {
    /// A real build of the app, packed and unpacked (slow, so by hand):
    /// `FC_PACK_DIR=<folder with ferrocast.exe> cargo test --release -p ferrocast-setup real -- --ignored`
    #[test]
    #[ignore]
    fn a_real_app_round_trips() {
        let dir = std::path::PathBuf::from(std::env::var("FC_PACK_DIR").unwrap());
        let mut out = Vec::new();
        let m = super::pack(&dir, "1", "ferrocast.exe", &mut out).unwrap();
        let (m2, body) = super::read_head(&out).unwrap();
        let dest = tempfile::tempdir().unwrap();
        super::unpack(body, &m2, dest.path(), &mut |_| true).unwrap();
        assert_eq!(m, m2);
    }
}
