//! `.ferropack`: everything a streamer made, in one open file (R-02).
//!
//! A zip with plain JSON per table, the media files, and `tips.csv` (tip history for
//! taxes). It never contains tokens or secrets: those live in the secret store, not the
//! database. Overlay URLs are kept, so moving to a new PC keeps OBS working.
//!
//! Import takes a snapshot first, checks every path inside the zip (no `../`), and writes
//! everything in one transaction, so a bad pack changes nothing.

use std::io::{Read, Seek, Write};
use std::path::Path;

use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use zip::write::SimpleFileOptions;

use crate::error::{PackError, Result, StoreError};
use crate::{Store, now_ms, repo};

pub const FORMAT: &str = "ferropack";
pub const VERSION: u32 = 1;

/// Tables copied as-is. Order matters on import (foreign keys).
const TABLES: &[&str] = &[
    "settings",
    "overlays",
    "alert_rules",
    "media",
    "designs",
    "goals",
    "commands",
    "timers",
    "counters",
    "quotes",
    "viewers",
    "viewer_identities",
    "loyalty_balances",
    "pending_points",
    "imported_tips",
];

/// Largest single entry, all entries together once unpacked, and the number of entries:
/// a zip bomb stops here.
pub const MAX_ENTRY: u64 = 64 * 1024 * 1024;
pub const MAX_UNPACKED: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
/// Largest `.ferropack` file the dashboard may upload.
pub const MAX_PACK: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub app_version: String,
    pub created_at: i64,
    pub counts: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct ImportReport {
    pub imported: Map<String, Value>,
    pub media_files: usize,
    pub backup: String,
}

/// Writing a pack failed: a disk problem on this computer.
fn zerr(e: impl std::fmt::Display) -> StoreError {
    StoreError::Io(std::io::Error::other(format!("the pack couldn't be written: {e}")))
}

fn table_json(conn: &rusqlite::Connection, table: &str) -> Result<Vec<Value>> {
    let mut st = conn.prepare(&format!("SELECT * FROM {table}"))?;
    let cols: Vec<String> = st.column_names().iter().map(|c| c.to_string()).collect();
    let rows = st.query_map([], |r| {
        let mut obj = Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v: SqlValue = r.get(i)?;
            obj.insert(
                c.clone(),
                match v {
                    SqlValue::Null => Value::Null,
                    SqlValue::Integer(n) => json!(n),
                    SqlValue::Real(f) => json!(f),
                    SqlValue::Text(t) => json!(t),
                    SqlValue::Blob(_) => Value::Null,
                },
            );
        }
        Ok(Value::Object(obj))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn csv_field(s: &str) -> String {
    // Spreadsheet formulas from a viewer's name ("=HYPERLINK(...)") are neutralised.
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

/// `minor` as a decimal in its currency's own digits: 50000 INR is "500.00".
fn decimal(minor: i64, currency: &str) -> String {
    let digits = ferrocast_core::money::exponent(currency) as usize;
    if digits == 0 {
        return minor.to_string();
    }
    let scale = 10i64.pow(digits as u32);
    format!("{}.{:0width$}", minor / scale, (minor % scale).abs(), width = digits)
}

/// The tip history as CSV: tips imported from another service first (oldest first), then
/// Ferrocast's own, newest last.
fn tips_csv(conn: &rusqlite::Connection) -> Result<String> {
    let mut out = String::from("date_utc_ms,provider,payment_id,name,amount,currency,status,message\n");
    let mut st = conn.prepare(
        "SELECT occurred_at, source, provider, source_id, name, amount_minor, currency, message
         FROM imported_tips ORDER BY occurred_at, source_id",
    )?;
    let rows = st.query_map([], |r| {
        Ok([
            r.get::<_, i64>(0)?.to_string(),
            format!("{}:{}", r.get::<_, String>(1)?, r.get::<_, String>(2)?),
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            decimal(r.get::<_, i64>(5)?, &r.get::<_, String>(6)?),
            r.get::<_, String>(6)?,
            "imported".to_string(),
            r.get::<_, Option<String>>(7)?.unwrap_or_default(),
        ])
    })?;
    for line in rows {
        out.push_str(&line?.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    let mut st = conn.prepare("SELECT occurred_at, data FROM events WHERE kind IN ('tip', 'reversal') ORDER BY seq")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (at, data) = row?;
        let v: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let line = if v["type"] == "tip" {
            let minor = v.pointer("/money/minor").and_then(Value::as_i64).unwrap_or(0);
            let cur = v.pointer("/money/currency").and_then(Value::as_str).unwrap_or("");
            let money = ferrocast_core::money::Money::new(minor, cur);
            let amount = decimal(minor, &money.currency);
            [
                at.to_string(),
                v["provider"].as_str().unwrap_or("").into(),
                v["payment_ref"].as_str().unwrap_or("").into(),
                v.pointer("/actor/display_name").and_then(Value::as_str).unwrap_or("").into(),
                amount,
                cur.into(),
                v["status"].as_str().unwrap_or("").into(),
                v["message"].as_str().unwrap_or("").into(),
            ]
        } else {
            [
                at.to_string(),
                String::new(),
                v["original_source_id"].as_str().unwrap_or("").into(),
                String::new(),
                String::new(),
                String::new(),
                v["reason"].as_str().unwrap_or("reversal").into(),
                String::new(),
            ]
        };
        out.push_str(&line.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    Ok(out)
}

/// Writes the pack to `out` (via a temp file, renamed when complete).
pub fn export(store: &Store, out: &Path, app_version: &str) -> Result<Manifest> {
    let tmp = out.with_extension("ferropack.partial");
    let file = std::fs::File::create(&tmp)?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut counts = Map::new();
    let (tables, csv, media_ids) = store.read(|c| {
        let mut tables = Vec::new();
        for t in TABLES {
            let mut rows = table_json(c, t)?;
            if *t == "settings" {
                rows.retain(|r| !is_device_local(r));
            }
            tables.push((*t, rows));
        }
        let ids: Vec<String> =
            c.prepare("SELECT id FROM media")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((tables, tips_csv(c)?, ids))
    })?;
    for (name, rows) in &tables {
        counts.insert((*name).into(), json!(rows.len()));
        zip.start_file(format!("{name}.json"), opts).map_err(zerr)?;
        zip.write_all(serde_json::to_string_pretty(rows)?.as_bytes())?;
    }
    zip.start_file("tips.csv", opts).map_err(zerr)?;
    zip.write_all(csv.as_bytes())?;
    let mut files = 0;
    for id in media_ids {
        let path = store.dir().join("media").join(&id);
        if let Ok(bytes) = std::fs::read(&path) {
            zip.start_file(
                format!("media/{id}"),
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .map_err(zerr)?;
            zip.write_all(&bytes)?;
            files += 1;
        }
    }
    counts.insert("media_files".into(), json!(files));
    let manifest = Manifest {
        format: FORMAT.into(),
        version: VERSION,
        app_version: app_version.into(),
        created_at: now_ms(),
        counts,
    };
    zip.start_file("manifest.json", opts).map_err(zerr)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    zip.finish().map_err(zerr)?.sync_all()?;
    crate::backup::rename_patiently(&tmp, out)?;
    Ok(manifest)
}

/// Settings rows that belong to this install, not to the setup: the remembered Pro status
/// (it's this install's, re-checked every 10 minutes) and stream history waiting to upload.
/// They're never exported, an import never replaces them, and [`content_hash`] leaves them
/// out. (Cloud backup state is a file of its own, never in the database.)
pub const DEVICE_LOCAL_SETTINGS: &[&str] = &["pro_status", "history_upload"];

fn is_device_local(row: &Value) -> bool {
    row["key"].as_str().is_some_and(|k| DEVICE_LOCAL_SETTINGS.contains(&k))
}

/// A SHA-256 (hex) of what a pack exported now would hold: the tables, the tip history and
/// every media file, without the export time. Two exports of an unchanged setup hash the
/// same, so the automatic cloud backup can skip a day when nothing changed.
pub fn content_hash(store: &Store) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let (tables, csv, media_ids) = store.read(|c| {
        let mut tables = Vec::new();
        for t in TABLES {
            let mut rows = table_json(c, t)?;
            if *t == "settings" {
                rows.retain(|r| !is_device_local(r));
            }
            tables.push((*t, rows));
        }
        let ids: Vec<String> = c
            .prepare("SELECT id FROM media ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((tables, tips_csv(c)?, ids))
    })?;
    // Each part is framed by its name and length, so moving bytes between parts changes it.
    let mut part = |name: &str, bytes: &[u8]| {
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    };
    for (name, rows) in &tables {
        part(name, serde_json::to_string(rows)?.as_bytes());
    }
    part("tips.csv", csv.as_bytes());
    for id in media_ids {
        if let Ok(bytes) = std::fs::read(store.dir().join("media").join(&id)) {
            part(&format!("media/{id}"), &bytes);
        }
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn corrupt(e: impl std::fmt::Display) -> StoreError {
    PackError::Corrupt(e.to_string()).into()
}

fn too_large(name: &str) -> StoreError {
    PackError::TooLarge(format!(
        "\"{name}\" in the pack is over the {} MB limit for one file.",
        MAX_ENTRY / 1024 / 1024
    ))
    .into()
}

fn read_entry<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Option<Vec<u8>>> {
    let mut f = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(corrupt(e)),
    };
    if f.size() > MAX_ENTRY {
        return Err(too_large(name));
    }
    let mut buf = Vec::new();
    // A failed inflate or CRC check is a damaged pack, not a disk problem.
    f.by_ref().take(MAX_ENTRY + 1).read_to_end(&mut buf).map_err(corrupt)?;
    if buf.len() as u64 > MAX_ENTRY {
        return Err(too_large(name));
    }
    Ok(Some(buf))
}

fn safe_media_name(name: &str) -> Option<&str> {
    let id = name.strip_prefix("media/")?;
    (!id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
}

/// Unpacks every `media/<id>` entry into `staging`; returns the ids.
fn stage_media<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, staging: &Path) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(corrupt)?;
        let name = f.name().map_err(corrupt)?.to_string();
        if !name.starts_with("media/") {
            continue;
        }
        let Some(id) = safe_media_name(&name) else {
            return Err(PackError::Unsafe(name).into());
        };
        if f.size() > MAX_ENTRY {
            return Err(too_large(&name));
        }
        let mut out = std::fs::File::create(staging.join(id))?;
        let n = std::io::copy(&mut f.by_ref().take(MAX_ENTRY + 1), &mut out).map_err(corrupt)?;
        if n > MAX_ENTRY {
            return Err(too_large(&name));
        }
        ids.push(id.to_string());
    }
    Ok(ids)
}

/// What import learned about the pack's media by reading the files themselves.
struct CheckedMedia {
    /// Pack media rows that describe a real, servable file, with that file's true facts.
    /// Rows missing here are skipped, so they never replace a library record.
    verified: std::collections::HashMap<String, repo::media::Checked>,
    /// Staged files to move into the library: servable, and backed by a verified row.
    copy: std::collections::HashSet<String>,
}

fn check_media(
    store: &Store,
    staging: &Path,
    staged: &[String],
    tables: &[(&str, Vec<Map<String, Value>>)],
) -> Result<CheckedMedia> {
    let staged: std::collections::HashSet<&str> = staged.iter().map(String::as_str).collect();
    let mut verified = std::collections::HashMap::new();
    let mut copy = std::collections::HashSet::new();
    let pack_rows = tables.iter().filter(|(t, _)| *t == "media").flat_map(|(_, r)| r);
    for row in pack_rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        if verified.contains_key(id) || safe_media_name(&format!("media/{id}")).is_none() {
            continue;
        }
        // The pack's own file if it brought one, else the one already in the library.
        let (path, from_pack) =
            if staged.contains(id) { (staging.join(id), true) } else { (store.dir().join("media").join(id), false) };
        if !path.is_file() {
            continue;
        }
        if let Some(found) = repo::media::check_file(&path)? {
            if from_pack {
                copy.insert(id.to_string());
            }
            verified.insert(id.to_string(), found);
        }
    }
    Ok(CheckedMedia { verified, copy })
}

/// Imports a pack, replacing rows with the same keys and keeping everything else.
pub fn import(store: &Store, pack: &Path) -> Result<ImportReport> {
    let file = std::fs::File::open(pack)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|_| PackError::NotAPack)?;
    if zip.len() > MAX_ENTRIES {
        return Err(PackError::TooLarge(format!("The pack has more than {MAX_ENTRIES} files.")).into());
    }
    let manifest: Manifest =
        serde_json::from_slice(&read_entry(&mut zip, "manifest.json")?.ok_or(PackError::NotAPack)?)
            .map_err(|_| PackError::Corrupt("its manifest can't be read".into()))?;
    if manifest.format != FORMAT {
        return Err(PackError::NotAPack.into());
    }
    if manifest.version > VERSION {
        return Err(PackError::TooNew(manifest.version).into());
    }
    let mut unpacked: u64 = 0;
    for i in 0..zip.len() {
        let f = zip.by_index(i).map_err(corrupt)?;
        unpacked = unpacked.saturating_add(f.size());
    }
    if unpacked > MAX_UNPACKED {
        return Err(PackError::TooLarge(format!(
            "The pack unpacks to more than {} GB, the most Ferrocast imports.",
            MAX_UNPACKED / 1024 / 1024 / 1024
        ))
        .into());
    }
    let mut tables = Vec::new();
    for t in TABLES {
        if let Some(bytes) = read_entry(&mut zip, &format!("{t}.json"))? {
            let rows: Vec<Map<String, Value>> =
                serde_json::from_slice(&bytes).map_err(|_| PackError::Corrupt(format!("{t}.json can't be read")))?;
            tables.push((*t, rows));
        }
    }
    keep_device_settings(store, &mut tables);
    // Media is unpacked to a staging folder one file at a time, never all in memory.
    let staging = store.dir().join("tmp").join(format!("import-{}", crate::new_id()));
    std::fs::create_dir_all(&staging)?;
    let result = import_staged(store, &mut zip, &staging, &tables);
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// The design in a pack's `designs` row, made safe: its id matches the row, it isn't
/// built-in or branded, and it passes every check. `None` skips the row.
fn pack_design(row: &Map<String, Value>) -> Option<ferrocast_core::design::Design> {
    let id = row.get("id")?.as_str()?;
    let mut d: ferrocast_core::design::Design = serde_json::from_str(row.get("data")?.as_str()?).ok()?;
    if d.id != id {
        return None;
    }
    d.built_in = false;
    d.branded = false;
    d.pro = false;
    d.validate().ok()?;
    Some(d)
}

/// Settings that belong to this computer, not to the streamer's setup: a pack (perhaps one
/// someone else shared) must not open the server to the network, start the app at login,
/// or point the OBS connection, and with it the OBS password, at another machine.
const DEVICE_ENGINE_FIELDS: &[&str] = &["lan_access", "launch_at_login", "close_to_tray"];

fn keep_device_settings(store: &Store, tables: &mut [(&str, Vec<Map<String, Value>>)]) {
    let Some((_, rows)) = tables.iter_mut().find(|(t, _)| *t == "settings") else {
        return;
    };
    // Unreadable local settings count as none: the device fields then fall back to off.
    let local_engine: Option<Map<String, Value>> =
        repo::settings::get::<Map<String, Value>>(store, "engine").ok().flatten();
    let parse = |row: &Map<String, Value>| -> Option<Map<String, Value>> {
        serde_json::from_str(row.get("value")?.as_str()?).ok()
    };
    rows.retain_mut(|row| match row.get("key").and_then(Value::as_str) {
        Some("engine") => {
            let Some(mut engine) = parse(row) else {
                return false;
            };
            for field in DEVICE_ENGINE_FIELDS {
                match local_engine.as_ref().and_then(|l| l.get(*field)) {
                    Some(v) => engine.insert((*field).to_string(), v.clone()),
                    None => engine.remove(*field),
                };
            }
            row.insert("value".into(), Value::String(Value::Object(engine).to_string()));
            true
        }
        // This install's own rows stay as they are (unsent history, its Pro status).
        Some(k) if DEVICE_LOCAL_SETTINGS.contains(&k) => false,
        Some("obs") => parse(row)
            .and_then(|o| o.get("host").and_then(Value::as_str).map(str::to_string))
            .is_some_and(|h| matches!(h.as_str(), "127.0.0.1" | "localhost" | "::1")),
        _ => true,
    });
}

fn import_staged<R: Read + Seek>(
    store: &Store,
    zip: &mut zip::ZipArchive<R>,
    staging: &Path,
    tables: &[(&str, Vec<Map<String, Value>>)],
) -> Result<ImportReport> {
    let media = stage_media(zip, staging)?;
    // A pack's media rows say what type each file is; never believe them. Check every file
    // the pack brings (and any local file a row points at) by its content instead.
    let checked = check_media(store, staging, &media, tables)?;
    let backup = store.snapshot("before-import")?;
    let mut report = ImportReport { backup: backup.display().to_string(), ..Default::default() };
    store.write(|tx| {
        for (table, rows) in tables {
            let allowed: Vec<String> = {
                let mut st = tx.prepare(&format!("PRAGMA table_info({table})"))?;
                st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for row in rows {
                if *table == "media"
                    && !row.get("id").and_then(Value::as_str).is_some_and(|id| checked.verified.contains_key(id))
                {
                    continue;
                }
                // A balance must be a whole number from 0 to the cap; a hand-edited pack
                // with anything else is skipped rather than failing the whole import.
                if *table == "loyalty_balances"
                    && !row
                        .get("points")
                        .and_then(Value::as_i64)
                        .is_some_and(|p| (0..=ferrocast_core::loyalty::MAX_BALANCE).contains(&p))
                {
                    continue;
                }
                if *table == "pending_points"
                    && !row
                        .get("points")
                        .and_then(Value::as_i64)
                        .is_some_and(|p| (1..=ferrocast_core::loyalty::MAX_BALANCE).contains(&p))
                {
                    continue;
                }
                if *table == "imported_tips" && !row.get("amount_minor").and_then(Value::as_i64).is_some_and(|a| a >= 0)
                {
                    continue;
                }
                // A design must be one this build can read, and never a forged built-in: a
                // pack someone shared can't plant a "branded" design or a broken one.
                let mut row = row.clone();
                if *table == "designs" {
                    let Some(design) = pack_design(&row) else { continue };
                    row.insert("data".into(), Value::String(serde_json::to_string(&design)?));
                    row.insert("kind".into(), Value::String(design.kind().key().into()));
                    row.insert("name".into(), Value::String(design.name.clone()));
                }
                let row = &row;
                if *table == "loyalty_balances" {
                    let owner = row.get("viewer_id").and_then(Value::as_i64);
                    let known: bool = tx.query_row(
                        "SELECT EXISTS (SELECT 1 FROM viewers WHERE id = ?1)",
                        [owner.unwrap_or(-1)],
                        |r| r.get(0),
                    )?;
                    if !known {
                        continue;
                    }
                }
                // Only real columns, so a crafted pack can't inject SQL through key names.
                let cols: Vec<&String> = row.keys().filter(|k| allowed.contains(k)).collect();
                if cols.is_empty() {
                    continue;
                }
                let sql = format!(
                    "INSERT OR REPLACE INTO {table} ({}) VALUES ({})",
                    cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", "),
                    (1..=cols.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ")
                );
                let vals: Vec<SqlValue> = cols
                    .iter()
                    .map(|c| match &row[*c] {
                        Value::Null => SqlValue::Null,
                        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
                        Value::Number(n) => n
                            .as_i64()
                            .map(SqlValue::Integer)
                            .unwrap_or_else(|| SqlValue::Real(n.as_f64().unwrap_or(0.0))),
                        Value::String(s) => SqlValue::Text(s.clone()),
                        other => SqlValue::Text(other.to_string()),
                    })
                    .collect();
                tx.execute(&sql, rusqlite::params_from_iter(vals))?;
            }
            report.imported.insert((*table).into(), json!(rows.len()));
        }
        for (id, c) in &checked.verified {
            let kind = serde_json::to_value(c.kind)?;
            tx.execute(
                "UPDATE media SET kind = ?1, mime = ?2, bytes = ?3, sha256 = ?4 WHERE id = ?5",
                rusqlite::params![kind.as_str().unwrap_or("image"), c.mime, c.bytes as i64, c.sha256, id],
            )?;
        }
        Ok(())
    })?;
    let dir = store.dir().join("media");
    std::fs::create_dir_all(&dir)?;
    for id in media {
        // Only a servable file with a verified row ever replaces anything on disk.
        if !checked.copy.contains(&id) {
            continue;
        }
        crate::backup::rename_patiently(&staging.join(&id), &dir.join(&id))
            .or_else(|_| std::fs::copy(staging.join(&id), dir.join(&id)).map(|_| ()))?;
        report.media_files += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_then_import_into_a_fresh_install_keeps_everything() {
        let a = tempfile::tempdir().unwrap();
        let s = Store::open(a.path()).unwrap();
        let o = repo::overlays::create(&s, "Main", ferrocast_core::overlay::Preset::Starter).unwrap();
        repo::bot::bump_counter(&s, "deaths", 7).unwrap();
        repo::bot::add_quote(&s, "=HYPERLINK(\"x\") gg", "mod").unwrap();
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        let m = repo::media::save(&s, "logo.png", png).unwrap();
        let mut tip = ferrocast_core::event::Event::live(
            ferrocast_core::event::Platform::Tips,
            Some("razorpay:pay_1".into()),
            ferrocast_core::event::Millis(1),
            ferrocast_core::event::EventKind::Tip {
                actor: ferrocast_core::event::Actor::named(ferrocast_core::event::Platform::Tips, "=cmd|' /C calc'!A0"),
                money: ferrocast_core::money::Money::new(50_000, "INR"),
                message: Some("thanks, \"legend\"".into()),
                provider: ferrocast_core::event::TipProvider::Razorpay,
                payment_ref: "pay_1".into(),
                status: ferrocast_core::event::PaymentStatus::Captured,
            },
        );
        tip.id = "t1".into();
        repo::events::insert(&s, &tip).unwrap();
        let mut fan = ferrocast_core::event::Actor::named(ferrocast_core::event::Platform::Twitch, "Fan");
        fan.id = Some("fan-1".into());
        repo::viewers::touch(&s, &fan, true).unwrap();
        repo::loyalty::change(&s, ferrocast_core::event::Platform::Twitch, "fan-1", 420, repo::loyalty::Floor::Strict)
            .unwrap();
        let pack = a.path().join("out.ferropack");
        let manifest = export(&s, &pack, "0.1.0").unwrap();
        assert_eq!(manifest.counts["loyalty_balances"], 1);
        assert_eq!(manifest.counts["overlays"], 1);

        let mut zip = zip::ZipArchive::new(std::fs::File::open(&pack).unwrap()).unwrap();
        let csv = String::from_utf8(read_entry(&mut zip, "tips.csv").unwrap().unwrap()).unwrap();
        assert!(csv.contains("500.00,INR"));
        assert!(csv.contains("'=cmd"), "spreadsheet formulas are neutralised");
        assert!(csv.contains("\"thanks, \"\"legend\"\"\""));

        let b = tempfile::tempdir().unwrap();
        let s2 = Store::open(b.path()).unwrap();
        let report = import(&s2, &pack).unwrap();
        assert_eq!(report.media_files, 1);
        let back = repo::overlays::get(&s2, &o.id).unwrap();
        assert_eq!(back.token, o.token, "OBS URLs keep working on the new PC");
        assert_eq!(repo::bot::counter(&s2, "deaths").unwrap(), 7);
        assert_eq!(
            repo::loyalty::balance(&s2, ferrocast_core::event::Platform::Twitch, "fan-1").unwrap(),
            Some(420),
            "points come across with their viewer"
        );
        let (media, path) = repo::media::get(&s2, &m.id).unwrap().unwrap();
        assert_eq!(media.mime, "image/png");
        assert_eq!(std::fs::read(path).unwrap(), png);
    }

    #[test]
    fn the_content_hash_changes_with_the_setup_only() {
        let a = tempfile::tempdir().unwrap();
        let s = Store::open(a.path()).unwrap();
        repo::bot::bump_counter(&s, "deaths", 1).unwrap();
        let first = content_hash(&s).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(content_hash(&s).unwrap(), first, "nothing changed, the same hash");
        // The Pro status being re-checked, or history waiting to upload, isn't a change.
        repo::settings::set(&s, "pro_status", &json!({ "checked_at": 5 })).unwrap();
        repo::settings::set(&s, "history_upload", &json!({ "queue": [1] })).unwrap();
        assert_eq!(content_hash(&s).unwrap(), first);
        repo::bot::bump_counter(&s, "deaths", 1).unwrap();
        let second = content_hash(&s).unwrap();
        assert_ne!(second, first, "a counter moved");
        repo::media::save(&s, "logo.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR").unwrap();
        assert_ne!(content_hash(&s).unwrap(), second, "a new media file");
    }

    fn write_pack(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let mut zip = zip::ZipWriter::new(f.reopen().unwrap());
        for (name, data) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        f
    }

    fn manifest_bytes(version: u32) -> Vec<u8> {
        serde_json::to_vec(
            &json!({ "format": FORMAT, "version": version, "app_version": "x", "created_at": 0, "counts": {} }),
        )
        .unwrap()
    }

    #[test]
    fn hostile_packs_change_nothing() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let m = manifest_bytes(1);
        let pack_err = |path: &Path| match import(&s, path) {
            Err(StoreError::Pack(p)) => p,
            other => panic!("expected a pack error, got {other:?}"),
        };
        let slip = write_pack(&[("manifest.json", &m), ("media/../../evil", b"x")]);
        assert!(matches!(pack_err(slip.path()), PackError::Unsafe(_)));
        assert!(!d.path().parent().unwrap().join("evil").exists());
        let newer = write_pack(&[("manifest.json", &manifest_bytes(99))]);
        assert_eq!(pack_err(newer.path()), PackError::TooNew(99));
        let not_a_pack = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(not_a_pack.path(), b"hello").unwrap();
        assert_eq!(pack_err(not_a_pack.path()), PackError::NotAPack);
        let no_manifest = write_pack(&[("overlays.json", b"[]")]);
        assert_eq!(pack_err(no_manifest.path()), PackError::NotAPack);
        // A pack cut short (a download that didn't finish) is damaged, not a disk error.
        let good = write_pack(&[("manifest.json", &m), ("media/abc", &[7u8; 50_000])]);
        let bytes = std::fs::read(good.path()).unwrap();
        let cut = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(cut.path(), &bytes[..bytes.len() - 200]).unwrap();
        assert!(matches!(pack_err(cut.path()), PackError::NotAPack | PackError::Corrupt(_)));
        let mut flipped = bytes.clone();
        let at = flipped.windows(4).position(|w| w == [7, 7, 7, 7]).unwrap_or(100) + 10;
        flipped[at] ^= 0xff;
        let bad_crc = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(bad_crc.path(), &flipped).unwrap();
        assert!(matches!(pack_err(bad_crc.path()), PackError::Corrupt(_)), "a CRC mismatch is a damaged pack");
        assert!(!d.path().join("media").join("abc").exists(), "nothing from a damaged pack is kept");
        let injection = br#"[{"key": "x", "value": "1", "updated_at": 0, "x\") ; DROP TABLE overlays; --": 1}]"#;
        let sql = write_pack(&[("manifest.json", &m), ("settings.json", injection)]);
        import(&s, sql.path()).unwrap();
        assert!(repo::overlays::list(&s).is_ok(), "unknown column names are ignored, tables intact");
        let broken = write_pack(&[("manifest.json", &m), ("overlays.json", b"{not json")]);
        assert!(matches!(pack_err(broken.path()), PackError::Corrupt(_)));
    }

    #[test]
    fn a_pack_cannot_choose_what_type_its_media_is_served_as() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let png: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        let mine = repo::media::save(&s, "mine.png", png).unwrap();
        let m = manifest_bytes(1);
        let row = |id: &str, mime: &str| json!({ "id": id, "kind": "image", "name": "x", "mime": mime, "bytes": 1, "sha256": "", "created_at": 0 });
        let rows = serde_json::to_vec(&json!([
            row("page", "text/html"),   // an HTML page dressed as media
            row("real", "text/html"),   // a real PNG with a lying type
            row(&mine.id, "text/html"), // relabels a file already in the library
            row("ghost", "image/png"),  // a row with no file anywhere
        ]))
        .unwrap();
        let page = b"<html><script>fetch('/api/v1/settings')</script>";
        let pack = write_pack(&[
            ("manifest.json", &m),
            ("media.json", &rows),
            ("media/page", page),
            ("media/real", png),
            ("media/stray", page), // no row, not a media type
        ]);
        let report = import(&s, pack.path()).unwrap();
        assert_eq!(report.media_files, 1, "only the real image is unpacked");

        assert!(repo::media::get(&s, "page").unwrap().is_none(), "the page's row is dropped");
        let dir = d.path().join("media");
        assert!(!dir.join("page").exists() && !dir.join("stray").exists(), "and its file is never written");
        let (real, _) = repo::media::get(&s, "real").unwrap().unwrap();
        assert_eq!(
            (real.mime.as_str(), real.bytes),
            ("image/png", png.len() as u64),
            "type and size come from the file"
        );
        let (kept, path) = repo::media::get(&s, &mine.id).unwrap().unwrap();
        assert_eq!(kept.mime, "image/png", "an existing file keeps its true type");
        assert_eq!(std::fs::read(path).unwrap(), png);
        assert!(repo::media::get(&s, "ghost").unwrap().is_none());

        // A pack file that isn't media never overwrites a good file with the same id.
        assert!(!d.path().join("tmp").read_dir().unwrap().any(|_| true), "staging is cleaned up");
        let swap = write_pack(&[("manifest.json", &m), (&format!("media/{}", mine.id), page)]);
        import(&s, swap.path()).unwrap();
        assert_eq!(std::fs::read(dir.join(&mine.id)).unwrap(), png);
        // ...nor does a row whose packed file fails the check remove the library's record.
        let mine_row = serde_json::to_vec(&json!([row(&mine.id, "image/png")])).unwrap();
        let bad_file =
            write_pack(&[("manifest.json", &m), ("media.json", &mine_row), (&format!("media/{}", mine.id), page)]);
        import(&s, bad_file.path()).unwrap();
        let (still, _) = repo::media::get(&s, &mine.id).unwrap().unwrap();
        assert_eq!(still.mime, "image/png", "the record is kept");
        // ...nor does a real image with no row behind it replace a library file silently.
        let other_png = [png, b"different"].concat();
        let rowless = write_pack(&[("manifest.json", &m), (&format!("media/{}", mine.id), &other_png)]);
        import(&s, rowless.path()).unwrap();
        assert_eq!(std::fs::read(dir.join(&mine.id)).unwrap(), png);
    }

    #[test]
    fn this_installs_own_rows_never_leave_or_get_replaced() {
        let a = tempfile::tempdir().unwrap();
        let s = Store::open(a.path()).unwrap();
        repo::settings::set(&s, "pro_status", &json!({ "pro": true, "checked_at": 1 })).unwrap();
        repo::settings::set(&s, "history_upload", &json!({ "queue": ["unsent"] })).unwrap();
        repo::settings::set(&s, "welcome", &json!({ "on": true })).unwrap();
        let pack = a.path().join("out.ferropack");
        export(&s, &pack, "0.1.0").unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&pack).unwrap()).unwrap();
        let settings = String::from_utf8(read_entry(&mut zip, "settings.json").unwrap().unwrap()).unwrap();
        assert!(settings.contains("welcome"), "the setup is exported");
        assert!(!settings.contains("pro_status") && !settings.contains("history_upload"), "{settings}");
        // A pack that has them anyway (an older export) can't replace this install's rows.
        let b = tempfile::tempdir().unwrap();
        let s2 = Store::open(b.path()).unwrap();
        repo::settings::set(&s2, "history_upload", &json!({ "queue": ["mine"] })).unwrap();
        let setting = |key: &str, value: Value| json!({ "key": key, "value": value.to_string(), "updated_at": 0 });
        let rows = serde_json::to_vec(&json!([
            setting("history_upload", json!({ "queue": ["old pc"] })),
            setting("pro_status", json!({ "pro": true })),
            setting("welcome", json!({ "on": false })),
        ]))
        .unwrap();
        let old = write_pack(&[("manifest.json", &manifest_bytes(1)), ("settings.json", &rows)]);
        import(&s2, old.path()).unwrap();
        let q: Value = repo::settings::get(&s2, "history_upload").unwrap().unwrap();
        assert_eq!(q["queue"], json!(["mine"]));
        assert!(repo::settings::get::<Value>(&s2, "pro_status").unwrap().is_none());
        let w: Value = repo::settings::get(&s2, "welcome").unwrap().unwrap();
        assert_eq!(w["on"], false, "the rest of the setup comes across");
    }

    #[test]
    fn a_pack_cannot_change_this_computers_settings() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        repo::settings::set(&s, "engine", &json!({ "theme": "dark", "launch_at_login": true })).unwrap();
        let setting = |key: &str, value: Value| json!({ "key": key, "value": value.to_string(), "updated_at": 0 });
        let rows = serde_json::to_vec(&json!([
            setting("engine", json!({ "theme": "light", "lan_access": true, "launch_at_login": false })),
            setting("obs", json!({ "host": "203.0.113.9", "port": 4455 })),
        ]))
        .unwrap();
        let pack = write_pack(&[("manifest.json", &manifest_bytes(1)), ("settings.json", &rows)]);
        import(&s, pack.path()).unwrap();
        let engine: Map<String, Value> = repo::settings::get(&s, "engine").unwrap().unwrap();
        assert_eq!(engine["theme"], "light", "the streamer's own choices come across");
        assert_eq!(engine.get("lan_access"), None, "but never network access");
        assert_eq!(engine["launch_at_login"], true, "or this computer's start-up choice");
        assert!(repo::settings::get::<Value>(&s, "obs").unwrap().is_none(), "OBS on another machine is dropped");

        let local = write_pack(&[
            ("manifest.json", &manifest_bytes(1)),
            (
                "settings.json",
                &serde_json::to_vec(&json!([setting("obs", json!({ "host": "127.0.0.1", "port": 4460 }))])).unwrap(),
            ),
        ]);
        import(&s, local.path()).unwrap();
        let obs: Value = repo::settings::get(&s, "obs").unwrap().unwrap();
        assert_eq!(obs["port"], 4460, "OBS on this computer is kept");
    }

    #[test]
    fn designs_travel_in_a_pack_but_never_as_forged_built_ins() {
        let a = tempfile::tempdir().unwrap();
        let s = Store::open(a.path()).unwrap();
        let mut d: ferrocast_core::design::Design =
            serde_json::from_str(include_str!("../../core/fixtures/designs/alert-design.json")).unwrap();
        d.id = "mine".into();
        repo::designs::insert(&s, &d).unwrap();
        let mut o = repo::overlays::create(&s, "BRB", ferrocast_core::overlay::Preset::Blank).unwrap();
        o.design = Some(ferrocast_core::overlay::DesignRef { id: "mine".into(), scene: "brb".into(), branded: false });
        repo::overlays::update(&s, &o).unwrap();
        let pack = a.path().join("p.ferropack");
        let manifest = export(&s, &pack, "0.1.0").unwrap();
        assert_eq!(manifest.counts["designs"], 1);
        let b = tempfile::tempdir().unwrap();
        let s2 = Store::open(b.path()).unwrap();
        import(&s2, &pack).unwrap();
        assert_eq!(repo::designs::get(&s2, "mine").unwrap(), d);
        assert_eq!(repo::overlays::get(&s2, &o.id).unwrap().design, o.design, "the overlay keeps its design link");

        // A hand-edited pack: a forged built-in, a broken design and a mismatched id.
        let mut forged = d.clone();
        forged.id = "forged".into();
        forged.built_in = true;
        forged.branded = true;
        let mut broken = d.clone();
        broken.id = "broken".into();
        broken.name = String::new();
        let row = |id: &str, design: &ferrocast_core::design::Design| {
            json!({ "id": id, "kind": "alert", "name": "x", "data": serde_json::to_string(design).unwrap(),
                    "created_at": 0, "updated_at": 0 })
        };
        let rows = serde_json::to_vec(&json!([
            row("forged", &forged),
            row("broken", &broken),
            row("other-id", &d),
            json!({ "id": "junk", "kind": "alert", "name": "x", "data": "{not json", "created_at": 0, "updated_at": 0 }),
        ]))
        .unwrap();
        let hostile = write_pack(&[("manifest.json", &manifest_bytes(1)), ("designs.json", &rows)]);
        import(&s2, hostile.path()).unwrap();
        let got = repo::designs::get(&s2, "forged").unwrap();
        assert!(!got.built_in && !got.branded, "a pack can't make a branded built-in");
        assert!(repo::designs::get(&s2, "broken").is_err());
        assert!(repo::designs::get(&s2, "other-id").is_err());
        assert!(repo::designs::get(&s2, "junk").is_err());
    }

    #[test]
    fn a_pack_cannot_plant_impossible_balances() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        let viewers = serde_json::to_vec(&json!([{ "id": 7, "note": "" }])).unwrap();
        let balances = serde_json::to_vec(&json!([
            { "viewer_id": 7, "points": -50, "updated_at": 0 },
            { "viewer_id": 99, "points": 10, "updated_at": 0 },
            { "viewer_id": 7, "points": "lots", "updated_at": 0 },
        ]))
        .unwrap();
        let pack = write_pack(&[
            ("manifest.json", &manifest_bytes(1)),
            ("viewers.json", &viewers),
            ("loyalty_balances.json", &balances),
        ]);
        import(&s, pack.path()).unwrap();
        let n: i64 = s.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM loyalty_balances", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 0, "negative, orphaned and non-numeric balances are skipped, the import still works");
        let ok = serde_json::to_vec(&json!([{ "viewer_id": 7, "points": 25, "updated_at": 0 }])).unwrap();
        let pack = write_pack(&[
            ("manifest.json", &manifest_bytes(1)),
            ("viewers.json", &viewers),
            ("loyalty_balances.json", &ok),
        ]);
        import(&s, pack.path()).unwrap();
        let p: i64 = s.read(|c| Ok(c.query_row("SELECT points FROM loyalty_balances", [], |r| r.get(0))?)).unwrap();
        assert_eq!(p, 25);
    }
}
