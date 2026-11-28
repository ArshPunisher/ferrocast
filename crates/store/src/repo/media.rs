//! The media library: sounds, images and videos the streamer uploads for alerts.
//!
//! Files live in `media/` next to the database under their id (never the uploaded name),
//! and are checked by content, not by the name's extension. Refusals carry the exact limit
//! or reason so the API can answer 413 or 415 with it.

use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Result, StoreError};
use crate::{Store, new_id, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Sound,
    Video,
}

impl MediaKind {
    /// Upload limit in bytes.
    pub fn max_bytes(self) -> u64 {
        match self {
            MediaKind::Image => 10 * 1024 * 1024,
            MediaKind::Sound => 5 * 1024 * 1024,
            MediaKind::Video => 50 * 1024 * 1024,
        }
    }

    pub fn limit_label(self) -> &'static str {
        match self {
            MediaKind::Image => "Images can be up to 10 MB.",
            MediaKind::Sound => "Sounds can be up to 5 MB.",
            MediaKind::Video => "Videos can be up to 50 MB.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Media {
    pub id: String,
    pub kind: MediaKind,
    pub name: String,
    pub mime: String,
    pub bytes: u64,
    pub created_at: i64,
}

/// Why an upload was refused. The server maps these to 413 and 415.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    #[error("{0}")]
    TooBig(&'static str),
    #[error(
        "That file type isn't supported. Use PNG, JPEG, GIF or WebP for images, MP3, OGG or WAV for sounds, and WebM or MP4 for videos."
    )]
    UnsupportedType,
    #[error("The file is empty.")]
    Empty,
}

/// Detects the type from the first bytes. SVG isn't accepted: it can carry scripts.
pub fn sniff(head: &[u8]) -> Option<(MediaKind, &'static str)> {
    let starts = |sig: &[u8]| head.starts_with(sig);
    let at = |off: usize, sig: &[u8]| head.get(off..off + sig.len()) == Some(sig);
    Some(if starts(b"\x89PNG\r\n\x1a\n") {
        (MediaKind::Image, "image/png")
    } else if starts(b"\xff\xd8\xff") {
        (MediaKind::Image, "image/jpeg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        (MediaKind::Image, "image/gif")
    } else if starts(b"RIFF") && at(8, b"WEBP") {
        (MediaKind::Image, "image/webp")
    } else if starts(b"RIFF") && at(8, b"WAVE") {
        (MediaKind::Sound, "audio/wav")
    } else if starts(b"OggS") {
        (MediaKind::Sound, "audio/ogg")
    } else if starts(b"ID3") || (head.len() > 1 && head[0] == 0xff && head[1] & 0xe0 == 0xe0) {
        (MediaKind::Sound, "audio/mpeg")
    } else if starts(b"\x1a\x45\xdf\xa3") {
        (MediaKind::Video, "video/webm")
    } else if at(4, b"ftyp") {
        (MediaKind::Video, "video/mp4")
    } else {
        return None;
    })
}

/// The true type, size and digest of a file already on disk, or `None` when it isn't a
/// type we serve or is over its kind's limit. Imports use it so a pack can't label a file
/// (say, an HTML page) with a type of its choosing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub kind: MediaKind,
    pub mime: &'static str,
    pub bytes: u64,
    pub sha256: String,
}

pub fn check_file(path: &std::path::Path) -> std::io::Result<Option<Checked>> {
    let data = std::fs::read(path)?;
    let Some((kind, mime)) = sniff(&data) else {
        return Ok(None);
    };
    if data.is_empty() || data.len() as u64 > kind.max_bytes() {
        return Ok(None);
    }
    let sha256 = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
    Ok(Some(Checked { kind, mime, bytes: data.len() as u64, sha256 }))
}

/// Every type `sniff` can return: what the server is willing to send as media.
pub fn servable(mime: &str) -> bool {
    matches!(
        mime,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "audio/wav"
            | "audio/ogg"
            | "audio/mpeg"
            | "video/webm"
            | "video/mp4"
    )
}

fn media_dir(store: &Store) -> PathBuf {
    store.dir().join("media")
}

/// The names the engine gives its synthesized alert speech; `prune_speech` removes only these.
const SPEECH_NAMES: [&str; 2] = ["speech.wav", "speech.ogg"];

/// Stores an uploaded file after checking its type and size.
/// A file the streamer calls `speech.wav` or `speech.ogg` is kept under another name, so the
/// cleanup of the engine's own speech (`prune_speech`) can never take their upload.
pub fn save(store: &Store, name: &str, data: &[u8]) -> std::result::Result<Media, SaveError> {
    save_as(store, name, data, false)
}

/// Stores the engine's synthesized alert speech (named `speech.wav` or `speech.ogg`).
pub fn save_speech(store: &Store, name: &str, data: &[u8]) -> std::result::Result<Media, SaveError> {
    save_as(store, name, data, true)
}

fn save_as(store: &Store, name: &str, data: &[u8], speech: bool) -> std::result::Result<Media, SaveError> {
    if data.is_empty() {
        return Err(SaveError::Upload(UploadError::Empty));
    }
    let (kind, mime) = sniff(data).ok_or(SaveError::Upload(UploadError::UnsupportedType))?;
    if data.len() as u64 > kind.max_bytes() {
        return Err(SaveError::Upload(UploadError::TooBig(kind.limit_label())));
    }
    let id = new_id();
    let dir = media_dir(store);
    std::fs::create_dir_all(&dir).map_err(StoreError::from)?;
    let tmp = dir.join(format!("{id}.partial"));
    std::fs::write(&tmp, data).map_err(StoreError::from)?;
    crate::backup::rename_patiently(&tmp, &dir.join(&id)).map_err(StoreError::from)?;
    let mut clean_name: String = name.chars().filter(|c| !c.is_control()).take(120).collect();
    if !speech && SPEECH_NAMES.contains(&clean_name.as_str()) {
        clean_name = format!("upload {clean_name}");
    }
    let m = Media {
        id,
        kind,
        name: if clean_name.trim().is_empty() { "file".into() } else { clean_name },
        mime: mime.into(),
        bytes: data.len() as u64,
        created_at: now_ms(),
    };
    let kind_s = serde_json::to_value(m.kind).map_err(StoreError::from)?;
    let digest: String = Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect();
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO media(id, kind, name, mime, bytes, sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![m.id, kind_s.as_str().unwrap_or("image"), m.name, m.mime, m.bytes as i64, digest, m.created_at],
            )?;
            Ok(())
        })
        .map_err(|e| {
            // Don't leave an orphan file behind when the record couldn't be saved.
            let _ = std::fs::remove_file(dir.join(&m.id));
            SaveError::Store(e)
        })?;
    Ok(m)
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error(transparent)]
    Upload(UploadError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Media> {
    let kind: String = r.get(1)?;
    let bytes: i64 = r.get(4)?;
    Ok(Media {
        id: r.get(0)?,
        kind: super::from_json(1, format!("\"{kind}\""))?,
        name: r.get(2)?,
        mime: r.get(3)?,
        bytes: u64::try_from(bytes).unwrap_or(0),
        created_at: r.get(5)?,
    })
}

pub fn list(store: &Store) -> Result<Vec<Media>> {
    store.read(|c| {
        let mut st = c.prepare("SELECT id, kind, name, mime, bytes, created_at FROM media ORDER BY created_at DESC")?;
        Ok(st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

pub fn get(store: &Store, id: &str) -> Result<Option<(Media, PathBuf)>> {
    let m = store.read(|c| {
        Ok(c.query_row("SELECT id, kind, name, mime, bytes, created_at FROM media WHERE id = ?1", [id], row)
            .optional()?)
    })?;
    Ok(m.map(|m| {
        let path = media_dir(store).join(&m.id);
        (m, path)
    }))
}

/// The library file with this content, if there is one (imports reuse it).
pub fn find_by_sha256(store: &Store, sha256: &str) -> Result<Option<String>> {
    let id: Option<String> = store.read(|c| {
        Ok(c.query_row("SELECT id FROM media WHERE sha256 = ?1 ORDER BY created_at LIMIT 1", [sha256], |r| r.get(0))
            .optional()?)
    })?;
    // Only if the file is really still there.
    Ok(id.filter(|id| media_dir(store).join(id).is_file()))
}

pub fn delete(store: &Store, id: &str) -> Result<()> {
    let n = store.write(|tx| Ok(tx.execute("DELETE FROM media WHERE id = ?1", [id])?))?;
    if n == 0 {
        return Err(StoreError::NotFound("media"));
    }
    let _ = std::fs::remove_file(media_dir(store).join(id));
    Ok(())
}

/// Removes synthesized alert TTS files (`speech.wav`, `speech.ogg`) older than `older_than_ms`
/// (Unix ms) that are not in `keep_ids`, deleting both their database rows and disk files.
pub fn prune_speech(store: &Store, older_than_ms: i64, keep_ids: &[String]) -> Result<usize> {
    let candidates: Vec<String> = store.read(|c| {
        let mut st = c.prepare(
            "SELECT id FROM media WHERE kind = 'sound' AND name IN ('speech.wav', 'speech.ogg') AND created_at < ?1",
        )?;
        Ok(st.query_map([older_than_ms], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    let victims: Vec<String> = candidates.into_iter().filter(|id| !keep_ids.iter().any(|k| k == id)).collect();
    if victims.is_empty() {
        return Ok(0);
    }
    let removed = store.write(|tx| {
        let mut n = 0usize;
        for id in &victims {
            n += tx.execute("DELETE FROM media WHERE id = ?1", [id])?;
        }
        Ok(n)
    })?;
    let dir = media_dir(store);
    for id in &victims {
        let _ = std::fs::remove_file(dir.join(id));
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn sniffs_by_content_not_name() {
        assert_eq!(sniff(PNG).map(|s| s.0), Some(MediaKind::Image));
        assert_eq!(sniff(b"ID3\x04\0").map(|s| s.1), Some("audio/mpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt ").map(|s| s.1), Some("audio/wav"));
        assert_eq!(sniff(b"\0\0\0\x20ftypisom").map(|s| s.1), Some("video/mp4"));
        assert_eq!(sniff(b"<svg onload=alert(1)>"), None);
        assert_eq!(sniff(b"MZ\x90\0"), None, "executables");
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"RIFF"), None, "truncated header");
    }

    #[test]
    fn save_list_get_delete() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let m = save(&s, "../../etc/passwd.png", PNG).unwrap();
        let (_, path) = get(&s, &m.id).unwrap().unwrap();
        assert_eq!(path.parent().unwrap(), dir.path().join("media"), "stored by id, not by name");
        assert_eq!(std::fs::read(&path).unwrap(), PNG);
        assert_eq!(list(&s).unwrap().len(), 1);
        delete(&s, &m.id).unwrap();
        assert!(!path.exists());
        assert!(get(&s, &m.id).unwrap().is_none());
    }

    #[test]
    fn refuses_empty_wrong_type_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert!(matches!(save(&s, "a", b""), Err(SaveError::Upload(UploadError::Empty))));
        assert!(matches!(save(&s, "a.png", b"<svg/>"), Err(SaveError::Upload(UploadError::UnsupportedType))));
        let mut big = b"ID3".to_vec();
        big.resize(6 * 1024 * 1024, 0);
        assert!(matches!(save(&s, "a.mp3", &big), Err(SaveError::Upload(UploadError::TooBig(_)))));
    }

    #[test]
    fn prune_speech_spares_a_users_upload_named_speech_wav() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let wav = b"RIFF\0\0\0\0WAVEfmt ";
        // A streamer's own alert sound that happens to be called speech.wav, and the engine's.
        let mine = save(&s, "speech.wav", wav).unwrap();
        let mine_ogg = save(&s, "speech.ogg", b"OggS\0\0\0\0").unwrap();
        let tts = save_speech(&s, "speech.wav", wav).unwrap();
        s.write(|tx| {
            tx.execute("UPDATE media SET created_at = ?1", [now_ms() - 7_200_000])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(prune_speech(&s, now_ms() - 3_600_000, &[]).unwrap(), 1, "only the synthesized one");
        assert!(get(&s, &tts.id).unwrap().is_none());
        assert!(get(&s, &mine.id).unwrap().is_some(), "an upload named speech.wav is theirs");
        assert!(get(&s, &mine_ogg.id).unwrap().is_some());
        assert!(dir.path().join("media").join(&mine.id).exists());
        assert!(SPEECH_NAMES.contains(&tts.name.as_str()));
    }

    #[test]
    fn prune_speech_deletes_old_unkept_tts_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let wav = b"RIFF\0\0\0\0WAVEfmt ";
        let ogg = b"OggS\0\0\0\0";
        let old_wav = save_speech(&s, "speech.wav", wav).unwrap();
        let old_ogg = save_speech(&s, "speech.ogg", ogg).unwrap();
        let kept_ogg = save_speech(&s, "speech.ogg", ogg).unwrap();
        let user_wav = save(&s, "alert.wav", wav).unwrap();
        // Backdate every row so `older_than_ms` selects only eligible speech files.
        let past = now_ms() - 7_200_000;
        s.write(|tx| {
            tx.execute("UPDATE media SET created_at = ?1", [past])?;
            Ok(())
        })
        .unwrap();
        let fresh_wav = save_speech(&s, "speech.wav", wav).unwrap();

        let removed = prune_speech(&s, now_ms() - 3_600_000, std::slice::from_ref(&kept_ogg.id)).unwrap();
        assert_eq!(removed, 2);
        assert!(get(&s, &old_wav.id).unwrap().is_none());
        assert!(!dir.path().join("media").join(&old_wav.id).exists());
        assert!(get(&s, &old_ogg.id).unwrap().is_none());
        assert!(!dir.path().join("media").join(&old_ogg.id).exists());
        assert!(get(&s, &kept_ogg.id).unwrap().is_some(), "kept preview/active alert preserved");
        assert!(get(&s, &user_wav.id).unwrap().is_some(), "user upload preserved");
        assert!(get(&s, &fresh_wav.id).unwrap().is_some(), "fresh tts preserved");
    }
}
