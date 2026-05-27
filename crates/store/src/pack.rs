//! `.ferropack`: everything a streamer made, in one open file (R-02).
//!
//! A zip with plain JSON per table, the media files, and `tips.csv` (tip history for
//! taxes). It never contains tokens or secrets: those live in the keychain, not the
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

use crate::error::{Result, StoreError};
use crate::{Store, now_ms};

pub const FORMAT: &str = "ferropack";
pub const VERSION: u32 = 1;

/// Tables copied as-is. Order matters on import (foreign keys).
const TABLES: &[&str] = &[
    "settings",
    "overlays",
    "alert_rules",
    "media",
    "goals",
    "commands",
    "timers",
    "counters",
    "quotes",
    "viewers",
    "viewer_identities",
];

/// Largest pack we'll read, and largest single entry: a zip bomb stops here.
const MAX_ENTRY: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;

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

fn zerr(e: impl std::fmt::Display) -> StoreError {
    StoreError::Conflict(format!("the pack couldn't be written or read: {e}"))
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

/// The tip history as CSV, newest last.
fn tips_csv(conn: &rusqlite::Connection) -> Result<String> {
    let mut out = String::from("date_utc_ms,provider,payment_id,name,amount,currency,status,message\n");
    let mut st = conn.prepare("SELECT received_at, data FROM events WHERE kind IN ('tip', 'reversal') ORDER BY seq")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (at, data) = row?;
        let v: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let line = if v["type"] == "tip" {
            let minor = v.pointer("/money/minor").and_then(Value::as_i64).unwrap_or(0);
            let cur = v.pointer("/money/currency").and_then(Value::as_str).unwrap_or("");
            let money = ferrocast_core::money::Money::new(minor, cur);
            let digits = ferrocast_core::money::exponent(&money.currency) as usize;
            let amount = if digits == 0 {
                minor.to_string()
            } else {
                format!(
                    "{}.{:0width$}",
                    minor / 10i64.pow(digits as u32),
                    (minor % 10i64.pow(digits as u32)).abs(),
                    width = digits
                )
            };
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
            tables.push((*t, table_json(c, t)?));
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
    std::fs::rename(&tmp, out)?;
    Ok(manifest)
}

fn read_entry<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Option<Vec<u8>>> {
    let mut f = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(zerr(e)),
    };
    if f.size() > MAX_ENTRY {
        return Err(StoreError::Conflict(format!("\"{name}\" in the pack is too large")));
    }
    let mut buf = Vec::new();
    f.by_ref().take(MAX_ENTRY + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_ENTRY {
        return Err(StoreError::Conflict(format!("\"{name}\" in the pack is too large")));
    }
    Ok(Some(buf))
}

fn safe_media_name(name: &str) -> Option<&str> {
    let id = name.strip_prefix("media/")?;
    (!id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
}

/// Imports a pack, replacing rows with the same keys and keeping everything else.
pub fn import(store: &Store, pack: &Path) -> Result<ImportReport> {
    let file = std::fs::File::open(pack)?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|_| StoreError::Conflict("That file isn't a .ferropack.".into()))?;
    if zip.len() > MAX_ENTRIES {
        return Err(StoreError::Conflict("The pack has too many files.".into()));
    }
    let manifest: Manifest = serde_json::from_slice(
        &read_entry(&mut zip, "manifest.json")?
            .ok_or_else(|| StoreError::Conflict("That file isn't a .ferropack.".into()))?,
    )
    .map_err(|_| StoreError::Conflict("The pack's manifest is damaged.".into()))?;
    if manifest.format != FORMAT || manifest.version > VERSION {
        return Err(StoreError::Conflict(format!(
            "This pack was made by a newer Ferrocast (pack version {}). Update Ferrocast, then import it.",
            manifest.version
        )));
    }
    let mut tables = Vec::new();
    for t in TABLES {
        if let Some(bytes) = read_entry(&mut zip, &format!("{t}.json"))? {
            let rows: Vec<Map<String, Value>> = serde_json::from_slice(&bytes)
                .map_err(|_| StoreError::Conflict(format!("{t}.json in the pack is damaged.")))?;
            tables.push((*t, rows));
        }
    }
    let mut media = Vec::new();
    for i in 0..zip.len() {
        let name = zip.by_index(i).map_err(zerr)?.name().map_err(zerr)?.to_string();
        if name.starts_with("media/") {
            let Some(id) = safe_media_name(&name) else {
                return Err(StoreError::Conflict(format!("The pack has an unsafe file name: {name}")));
            };
            if let Some(bytes) = read_entry(&mut zip, &name)? {
                media.push((id.to_string(), bytes));
            }
        }
    }
    let backup = store.snapshot("before-import")?;
    let mut report = ImportReport { backup: backup.display().to_string(), ..Default::default() };
    store.write(|tx| {
        for (table, rows) in &tables {
            let allowed: Vec<String> = {
                let mut st = tx.prepare(&format!("PRAGMA table_info({table})"))?;
                st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for row in rows {
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
        Ok(())
    })?;
    let dir = store.dir().join("media");
    std::fs::create_dir_all(&dir)?;
    for (id, bytes) in media {
        std::fs::write(dir.join(&id), bytes)?;
        report.media_files += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo;

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
        let pack = a.path().join("out.ferropack");
        let manifest = export(&s, &pack, "0.1.0").unwrap();
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
        let (media, path) = repo::media::get(&s2, &m.id).unwrap().unwrap();
        assert_eq!(media.mime, "image/png");
        assert_eq!(std::fs::read(path).unwrap(), png);
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
        let slip = write_pack(&[("manifest.json", &m), ("media/../../evil", b"x")]);
        assert!(import(&s, slip.path()).is_err());
        assert!(!d.path().parent().unwrap().join("evil").exists());
        let newer = write_pack(&[("manifest.json", &manifest_bytes(99))]);
        assert!(import(&s, newer.path()).unwrap_err().to_string().contains("newer"));
        let not_a_pack = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(not_a_pack.path(), b"hello").unwrap();
        assert!(import(&s, not_a_pack.path()).is_err());
        let injection = br#"[{"key": "x", "value": "1", "updated_at": 0, "x\") ; DROP TABLE overlays; --": 1}]"#;
        let sql = write_pack(&[("manifest.json", &m), ("settings.json", injection)]);
        import(&s, sql.path()).unwrap();
        assert!(repo::overlays::list(&s).is_ok(), "unknown column names are ignored, tables intact");
        let broken = write_pack(&[("manifest.json", &m), ("overlays.json", b"{not json")]);
        assert!(import(&s, broken.path()).is_err());
    }
}
