//! Where Pro keeps files (cloud backups, tip page images): a [`BlobStore`] by key. Today
//! that's [`FsBlobStore`], a folder next to the database (`RELAY_BLOB_DIR`, default
//! `<db dir>/blobs`); an object store (R2) can take its place later behind the same trait.
//!
//! Keys are the caller's, never a user's: `backups/<sub>/<id>`. Each segment is letters,
//! digits, `-`, `_` or `.`, never `.`/`..` or starting with a dot, so a key can't leave the
//! folder or collide with a half-written file. Writes go to a temporary file first and are
//! renamed into place, so a reader never sees half a blob.

use std::io;
use std::path::{Path, PathBuf};

use axum::body::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The longest key, and the longest segment of one.
pub const MAX_KEY: usize = 256;
const MAX_SEGMENT: usize = 128;
/// Bytes per chunk when a blob is read back.
const CHUNK: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("not a valid blob key")]
    BadKey,
    #[error("no blob with that key")]
    NotFound,
    /// More than the caller's `max_bytes`; nothing was kept.
    #[error("the blob is larger than {0} bytes")]
    TooLarge(u64),
    #[error("blob storage failed: {0}")]
    Io(#[from] io::Error),
}

impl BlobError {
    /// The API error for a request that hit this: 413 with the limit, 404, or 503.
    pub fn into_api(self) -> crate::ApiError {
        use axum::http::StatusCode;
        match self {
            BlobError::TooLarge(max) => crate::ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!("That file is over the {} limit.", human_bytes(max)),
            )
            .hint("Make it smaller and try again."),
            BlobError::NotFound => {
                crate::ApiError::new(StatusCode::NOT_FOUND, "not_found", "That file isn't there any more.")
            }
            e => {
                tracing::error!(error = %e, "blob storage failed");
                unavailable()
            }
        }
    }
}

/// What a feature that needs file storage answers when the server has none.
pub fn unavailable() -> crate::ApiError {
    crate::ApiError::new(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "storage_unavailable",
        "Ferrocast's servers couldn't store that just now.",
    )
    .hint("Try again in a few minutes. Nothing on your PC changed.")
}

/// `250 MB`, `3 MB`, `900 KB`, `12 bytes` (decimal units, like the limits are written).
pub fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1_000_000 && n % 100_000 == 0 && n % 1_000_000 != 0 => format!("{:.1} MB", n as f64 / 1e6),
        n if n >= 1_000_000 => format!("{} MB", n / 1_000_000),
        n if n >= 1_000 => format!("{} KB", n / 1_000),
        n => format!("{n} bytes"),
    }
}

/// A body to store, read as it arrives.
pub type Body<'a> = BoxStream<'a, io::Result<Bytes>>;

pub trait BlobStore: Send + Sync + std::fmt::Debug {
    /// Stores `body` under `key` (replacing what was there), refusing more than `max_bytes`.
    /// Returns the size stored.
    fn put_stream<'a>(&'a self, key: &'a str, body: Body<'a>, max_bytes: u64) -> BoxFuture<'a, Result<u64, BlobError>>;

    /// Reads a blob back as a stream of chunks.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Body<'static>, BlobError>>;

    /// Removes a blob; `false` if there was none.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool, BlobError>>;

    /// A blob's size in bytes, or `None` if there's none.
    fn size<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<u64>, BlobError>>;

    /// Stores bytes already in memory.
    fn put<'a>(&'a self, key: &'a str, bytes: Bytes) -> BoxFuture<'a, Result<u64, BlobError>> {
        let len = bytes.len() as u64;
        self.put_stream(key, futures_util::stream::once(async move { Ok(bytes) }).boxed(), len)
    }

    /// Reads a small blob into memory, refusing one over `max_bytes`.
    fn get_bytes<'a>(&'a self, key: &'a str, max_bytes: u64) -> BoxFuture<'a, Result<Vec<u8>, BlobError>> {
        Box::pin(async move {
            let mut body = self.get(key).await?;
            let mut out = Vec::new();
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                if out.len() as u64 + chunk.len() as u64 > max_bytes {
                    return Err(BlobError::TooLarge(max_bytes));
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        })
    }
}

/// Whether `key` is a safe relative key (see the module docs).
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY
        && key.split('/').all(|s| {
            !s.is_empty()
                && s.len() <= MAX_SEGMENT
                && !s.starts_with('.')
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
}

/// Blobs as files under one folder.
#[derive(Debug, Clone)]
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    /// A store rooted at `root`, made (0700 on Unix) if it isn't there.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<FsBlobStore> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(FsBlobStore { root })
    }

    /// `RELAY_BLOB_DIR`, else `blobs` next to the database file.
    pub fn default_root(db_path: &Path) -> PathBuf {
        match std::env::var("RELAY_BLOB_DIR").ok().filter(|v| !v.trim().is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => db_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).join("blobs"),
        }
    }

    fn path(&self, key: &str) -> Result<PathBuf, BlobError> {
        if !valid_key(key) {
            return Err(BlobError::BadKey);
        }
        Ok(key.split('/').fold(self.root.clone(), |p, s| p.join(s)))
    }

    async fn write(&self, key: &str, mut body: Body<'_>, max_bytes: u64) -> Result<u64, BlobError> {
        let path = self.path(key)?;
        let dir = path.parent().unwrap_or(&self.root).to_path_buf();
        tokio::fs::create_dir_all(&dir).await?;
        // A dot name: never a valid key, so it can't clash with a blob.
        let tmp = TempFile(Some(dir.join(format!(".tmp-{}", crate::random(12)))));
        let at = tmp.path().to_path_buf();
        let mut f = tokio::fs::File::create(&at).await?;
        let mut n = 0u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            n += chunk.len() as u64;
            if n > max_bytes {
                return Err(BlobError::TooLarge(max_bytes));
            }
            f.write_all(&chunk).await?;
        }
        f.sync_all().await?;
        drop(f);
        tokio::fs::rename(&at, &path).await?;
        tmp.commit();
        Ok(n)
    }
}

/// A half-written blob, removed when dropped unless committed: on an error, and also when
/// the write is cancelled (the request went away and its future was dropped mid-stream).
struct TempFile(Option<PathBuf>);

impl TempFile {
    fn path(&self) -> &Path {
        self.0.as_deref().unwrap_or(Path::new(""))
    }
    /// It was renamed into place: nothing to clean up.
    fn commit(mut self) {
        self.0 = None;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            // One small unlink; Drop can't await.
            let _ = std::fs::remove_file(p);
        }
    }
}

fn not_found(e: io::Error) -> BlobError {
    if e.kind() == io::ErrorKind::NotFound { BlobError::NotFound } else { BlobError::Io(e) }
}

impl BlobStore for FsBlobStore {
    fn put_stream<'a>(&'a self, key: &'a str, body: Body<'a>, max_bytes: u64) -> BoxFuture<'a, Result<u64, BlobError>> {
        Box::pin(self.write(key, body, max_bytes))
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Body<'static>, BlobError>> {
        Box::pin(async move {
            let file = tokio::fs::File::open(self.path(key)?).await.map_err(not_found)?;
            let stream = futures_util::stream::try_unfold(file, |mut f| async move {
                let mut buf = vec![0u8; CHUNK];
                let n = f.read(&mut buf).await?;
                if n == 0 {
                    return Ok(None);
                }
                buf.truncate(n);
                Ok(Some((Bytes::from(buf), f)))
            });
            Ok(stream.boxed())
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool, BlobError>> {
        Box::pin(async move {
            match tokio::fs::remove_file(self.path(key)?).await {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn size<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<u64>, BlobError>> {
        Box::pin(async move {
            match tokio::fs::metadata(self.path(key)?).await {
                Ok(m) if m.is_file() => Ok(Some(m.len())),
                Ok(_) => Ok(None),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, FsBlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FsBlobStore::new(dir.path().join("blobs")).unwrap();
        (dir, s)
    }

    #[test]
    fn keys_cant_leave_the_folder() {
        for ok in ["a", "backups/sub_1/b-2_x.age", "t/a/3f9c.webp"] {
            assert!(valid_key(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "a//b",
            "a/",
            ".tmp-x",
            "a/.hidden",
            "a\\b",
            "a b",
            "ä",
            "a\0b",
            &"x".repeat(MAX_SEGMENT + 1),
            &"a/".repeat(MAX_KEY),
        ] {
            assert!(!valid_key(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn put_get_size_and_delete() {
        let (_d, s) = store();
        assert_eq!(s.put("backups/sub_1/one", Bytes::from_static(b"hello")).await.unwrap(), 5);
        assert_eq!(s.size("backups/sub_1/one").await.unwrap(), Some(5));
        assert_eq!(s.get_bytes("backups/sub_1/one", 100).await.unwrap(), b"hello");
        // Replacing keeps one blob with the new contents.
        s.put("backups/sub_1/one", Bytes::from_static(b"bye")).await.unwrap();
        assert_eq!(s.get_bytes("backups/sub_1/one", 100).await.unwrap(), b"bye");
        assert!(matches!(s.get_bytes("backups/sub_1/one", 2).await, Err(BlobError::TooLarge(2))));
        assert!(s.delete("backups/sub_1/one").await.unwrap());
        assert!(!s.delete("backups/sub_1/one").await.unwrap(), "already gone");
        assert_eq!(s.size("backups/sub_1/one").await.unwrap(), None);
        assert!(matches!(s.get("backups/sub_1/one").await, Err(BlobError::NotFound)));
        // A folder isn't a blob.
        assert_eq!(s.size("backups").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_stream_over_the_limit_keeps_nothing() {
        let (dir, s) = store();
        let chunks = (0..4).map(|_| Ok(Bytes::from(vec![1u8; 1000])));
        let r = s.put_stream("big", futures_util::stream::iter(chunks).boxed(), 2500).await;
        assert!(matches!(r, Err(BlobError::TooLarge(2500))));
        assert_eq!(s.size("big").await.unwrap(), None);
        let left: Vec<_> = std::fs::read_dir(dir.path().join("blobs")).unwrap().collect();
        assert!(left.is_empty(), "no temporary file left behind: {left:?}");
        // A broken body keeps nothing either.
        let broken = futures_util::stream::iter([Ok(Bytes::from_static(b"part")), Err(io::Error::other("cut off"))]);
        assert!(matches!(s.put_stream("cut", broken.boxed(), 100).await, Err(BlobError::Io(_))));
        assert_eq!(s.size("cut").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_cancelled_write_leaves_no_temporary_file() {
        let (dir, s) = store();
        // One chunk, then a body that never ends: the request is cut off mid-upload.
        let body = futures_util::stream::once(async { Ok(Bytes::from_static(b"part")) })
            .chain(futures_util::stream::pending())
            .boxed();
        let r = tokio::time::timeout(std::time::Duration::from_millis(100), s.put_stream("b/c", body, 100)).await;
        assert!(r.is_err(), "still waiting for the body when it was dropped");
        assert_eq!(s.size("b/c").await.unwrap(), None);
        let left: Vec<_> = std::fs::read_dir(dir.path().join("blobs/b")).unwrap().collect();
        assert!(left.is_empty(), "no temporary file left behind: {left:?}");
    }

    #[tokio::test]
    async fn big_blobs_stream_back_in_chunks() {
        let (_d, s) = store();
        let data: Vec<u8> = (0..(CHUNK * 3 + 17)).map(|i| (i % 251) as u8).collect();
        s.put("b", Bytes::from(data.clone())).await.unwrap();
        let mut body = s.get("b").await.unwrap();
        let (mut n, mut back) = (0, Vec::new());
        while let Some(c) = body.next().await {
            n += 1;
            back.extend_from_slice(&c.unwrap());
        }
        assert!(n >= 4, "{n} chunks");
        assert_eq!(back, data);
    }

    #[tokio::test]
    async fn bad_keys_are_refused_without_touching_the_disk() {
        let (dir, s) = store();
        std::fs::write(dir.path().join("outside"), b"keep").unwrap();
        assert!(matches!(s.put("../outside", Bytes::from_static(b"x")).await, Err(BlobError::BadKey)));
        assert!(matches!(s.delete("../outside").await, Err(BlobError::BadKey)));
        assert!(matches!(s.size("../outside").await, Err(BlobError::BadKey)));
        assert!(matches!(s.get("../outside").await, Err(BlobError::BadKey)));
        assert_eq!(std::fs::read(dir.path().join("outside")).unwrap(), b"keep");
    }

    #[test]
    fn errors_have_their_status() {
        use axum::response::IntoResponse;
        assert_eq!(human_bytes(250_000_000), "250 MB");
        assert_eq!(human_bytes(1_500_000), "1.5 MB");
        assert_eq!(human_bytes(900_000), "900 KB");
        assert_eq!(BlobError::TooLarge(3_000_000).into_api().into_response().status(), 413);
        assert_eq!(BlobError::NotFound.into_api().into_response().status(), 404);
        assert_eq!(BlobError::Io(io::Error::other("disk")).into_api().into_response().status(), 503);
    }

    #[test]
    fn the_default_folder_is_next_to_the_database() {
        // RELAY_BLOB_DIR isn't set in tests.
        if std::env::var_os("RELAY_BLOB_DIR").is_none() {
            assert_eq!(FsBlobStore::default_root(Path::new("/data/relay.db")), PathBuf::from("/data/blobs"));
            assert_eq!(FsBlobStore::default_root(Path::new("relay.db")), PathBuf::from("./blobs"));
        }
    }
}
