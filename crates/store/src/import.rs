//! Bringing another service's setup (StreamElements, Streamlabs) into Ferrocast.
//!
//! The engine fetches or parses the other service's data and maps it to Ferrocast's own
//! types ([`Incoming`]). This module compares that with what is already here ([`preview`]:
//! nothing is written) and writes it ([`apply`]: a backup first, then one transaction, so a
//! failure changes nothing). It never deletes anything: when something clashes, the
//! streamer chooses to keep Ferrocast's version or replace it with the imported one.
//!
//! People are matched by platform user id when the source has one, else by their unique
//! login on Twitch and Kick, never by display name (anyone can set one). People Ferrocast
//! already knows get their balance now; the rest wait in `pending_points` and are paid the
//! first time they show up (see [`claim_pending`]). YouTube names aren't unique, so a YouTube
//! entry with only a name "needs a match": it's listed, kept, and never paid automatically.

use std::collections::{HashMap, HashSet};

use ferrocast_core::command::{Command, Timer};
use ferrocast_core::event::Platform;
use ferrocast_core::loyalty::{LoyaltyConfig, MAX_BALANCE, is_known_bot};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Result, StoreError};
use crate::{Store, new_id, now_ms};

/// Where the data comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    #[serde(rename = "streamelements")]
    StreamElements,
    #[serde(rename = "streamlabs")]
    Streamlabs,
}

impl Source {
    pub fn key(self) -> &'static str {
        match self {
            Source::StreamElements => "streamelements",
            Source::Streamlabs => "streamlabs",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::StreamElements => "StreamElements",
            Source::Streamlabs => "Streamlabs",
        }
    }
}

/// One kind of thing an import brings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Commands,
    Timers,
    Points,
    LoyaltySettings,
    Tips,
}

/// Something named, with why it was skipped or how it clashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub name: String,
    pub reason: String,
}

/// Something the other service has that Ferrocast doesn't bring over, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotImported {
    pub what: String,
    pub count: usize,
    pub reason: String,
}

/// A balance from the other service: the person's name there and, when the source has it,
/// their platform user id (Twitch/Kick user id, YouTube channel id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointsRow {
    pub name: String,
    pub user_id: Option<String>,
    pub points: i64,
}

/// A past tip, in minor units of its currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipRow {
    /// The other service's id for it (or a stable hash when it has none).
    pub source_id: String,
    pub occurred_at: i64,
    pub name: String,
    pub amount_minor: i64,
    pub currency: String,
    pub message: Option<String>,
    /// Who handled the payment there ("paypal", "stripe"…).
    pub provider: String,
}

/// Everything an import would bring, already in Ferrocast's shapes.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub source: Source,
    /// The platform the points belong to; `None` when the other service's channel is on a
    /// platform Ferrocast doesn't support.
    pub platform: Option<Platform>,
    /// The kinds this import covers, in the order the report shows them.
    pub kinds: Vec<Kind>,
    pub commands: Vec<Command>,
    pub timers: Vec<Timer>,
    pub points: Vec<PointsRow>,
    /// Fields of [`LoyaltyConfig`] to set.
    pub loyalty: Option<Map<String, Value>>,
    pub tips: Vec<TipRow>,
    /// Entries left out while reading the other service's data, with why.
    pub skipped: Vec<(Kind, Item)>,
    /// Things worth knowing ("Regulars became VIPs").
    pub notes: Vec<(Kind, String)>,
    pub not_imported: Vec<NotImported>,
}

impl Incoming {
    pub fn new(source: Source, platform: Option<Platform>) -> Self {
        Incoming {
            source,
            platform,
            kinds: Vec::new(),
            commands: Vec::new(),
            timers: Vec::new(),
            points: Vec::new(),
            loyalty: None,
            tips: Vec::new(),
            skipped: Vec::new(),
            notes: Vec::new(),
            not_imported: Vec::new(),
        }
    }

    pub fn skip(&mut self, kind: Kind, name: impl Into<String>, reason: impl Into<String>) {
        self.skipped.push((kind, Item { name: name.into(), reason: reason.into() }));
    }

    /// Adds a note once (the same note for many entries is said one time).
    pub fn note(&mut self, kind: Kind, text: impl Into<String>) {
        let text = text.into();
        if !self.notes.iter().any(|(k, t)| *k == kind && *t == text) {
            self.notes.push((kind, text));
        }
    }
}

/// What to do when an imported thing clashes with one Ferrocast already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Ferrocast's version stays; the imported one is left out.
    Keep,
    /// The imported version takes its place (same id, so nothing that points at it breaks).
    Replace,
}

/// At most this many names per list in a preview or report; the counts are always exact.
pub const LIST_MAX: usize = 100;
const NOTES_MAX: usize = 20;

/// One kind in a preview: what would happen, nothing written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewSection {
    pub kind: Kind,
    /// Entries found at the source.
    pub found: usize,
    /// Would be added.
    pub new: usize,
    /// Clash with something here; the conflict choice decides.
    pub conflicts: usize,
    /// Points for people Ferrocast hasn't seen yet: paid when they first show up.
    pub waiting: usize,
    /// YouTube points with only a name: never paid automatically.
    pub needs_match: usize,
    pub skipped: usize,
    pub conflict_items: Vec<Item>,
    pub needs_match_items: Vec<Item>,
    pub skipped_items: Vec<Item>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preview {
    pub sections: Vec<PreviewSection>,
    pub not_imported: Vec<NotImported>,
}

/// One kind in a finished import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportSection {
    pub kind: Kind,
    pub found: usize,
    pub added: usize,
    pub replaced: usize,
    /// Clashes where Ferrocast's version was kept.
    pub kept: usize,
    pub waiting: usize,
    pub needs_match: usize,
    pub skipped: usize,
    pub skipped_items: Vec<Item>,
    pub needs_match_items: Vec<Item>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub source: Source,
    /// The backup taken just before writing (restore it to undo the import).
    pub backup: String,
    pub on_conflict: OnConflict,
    pub sections: Vec<ReportSection>,
    pub not_imported: Vec<NotImported>,
}

#[derive(Debug, Default)]
struct Tally {
    found: usize,
    new: usize,
    conflicts: usize,
    replaced: usize,
    kept: usize,
    waiting: usize,
    needs_match: usize,
    skipped: usize,
    conflict_items: Vec<Item>,
    skipped_items: Vec<Item>,
    needs_match_items: Vec<Item>,
    notes: Vec<String>,
}

impl Tally {
    fn skip(&mut self, name: &str, reason: impl Into<String>) {
        self.skipped += 1;
        if self.skipped_items.len() < LIST_MAX {
            self.skipped_items.push(Item { name: name.to_string(), reason: reason.into() });
        }
    }

    /// Counts a clash; returns whether the imported version should be written.
    fn conflict(&mut self, name: &str, detail: impl Into<String>, choice: OnConflict) -> bool {
        self.conflicts += 1;
        if self.conflict_items.len() < LIST_MAX {
            self.conflict_items.push(Item { name: name.to_string(), reason: detail.into() });
        }
        match choice {
            OnConflict::Keep => {
                self.kept += 1;
                false
            }
            OnConflict::Replace => {
                self.replaced += 1;
                true
            }
        }
    }

    fn note(&mut self, text: String) {
        if self.notes.len() < NOTES_MAX && !self.notes.contains(&text) {
            self.notes.push(text);
        }
    }
}

fn pkey(p: Platform) -> String {
    serde_json::to_value(p).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

fn damaged(what: &'static str) -> impl Fn(serde_json::Error) -> StoreError {
    move |e| StoreError::Corrupt { what, detail: e.to_string() }
}

fn load_commands(c: &Connection) -> Result<Vec<Command>> {
    let mut st = c.prepare("SELECT command FROM commands ORDER BY rowid")?;
    let raw = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    raw.iter().map(|s| serde_json::from_str(s).map_err(damaged("a saved command"))).collect()
}

fn load_timers(c: &Connection) -> Result<Vec<Timer>> {
    let mut st = c.prepare("SELECT timer FROM timers ORDER BY rowid")?;
    let raw = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    raw.iter().map(|s| serde_json::from_str(s).map_err(damaged("a saved timer"))).collect()
}

/// Does the work for both [`preview`] (`write` false) and [`apply`], so what the preview
/// promises is exactly what the import does.
fn run(c: &Connection, inc: &Incoming, choice: OnConflict, write: bool) -> Result<Vec<(Kind, Tally)>> {
    let mut out = Vec::new();
    for kind in &inc.kinds {
        let mut t = Tally::default();
        for (k, item) in &inc.skipped {
            if k == kind {
                t.found += 1;
                t.skip(&item.name, item.reason.clone());
            }
        }
        for (k, note) in &inc.notes {
            if k == kind {
                t.note(note.clone());
            }
        }
        match kind {
            Kind::Commands => commands(c, inc, choice, write, &mut t)?,
            Kind::Timers => timers(c, inc, choice, write, &mut t)?,
            Kind::Points => points(c, inc, choice, write, &mut t)?,
            Kind::LoyaltySettings => loyalty(c, inc, choice, write, &mut t)?,
            Kind::Tips => tips(c, inc, write, &mut t)?,
        }
        out.push((*kind, t));
    }
    Ok(out)
}

fn commands(c: &Connection, inc: &Incoming, choice: OnConflict, write: bool, t: &mut Tally) -> Result<()> {
    let mut existing = load_commands(c)?;
    let mut seen = HashSet::new();
    let label = inc.source.label();
    for incoming in &inc.commands {
        t.found += 1;
        if !seen.insert(incoming.name.to_lowercase()) {
            t.skip(&incoming.name, format!("{label} has {} twice; the first one was used.", incoming.name));
            continue;
        }
        let clash = existing.iter().position(|e| e.answers_to(&incoming.name));
        let mut cmd = incoming.clone();
        if let Some(i) = clash {
            cmd.id = existing[i].id.clone();
            cmd.platforms = existing[i].platforms.clone();
        } else {
            cmd.id = new_id();
        }
        // Aliases another command already answers to are left off, never taken from it.
        let mut kept_aliases: Vec<String> = Vec::new();
        let before = cmd.aliases.len();
        for a in std::mem::take(&mut cmd.aliases) {
            let taken = a.eq_ignore_ascii_case(&cmd.name)
                || kept_aliases.iter().any(|k| k.eq_ignore_ascii_case(&a))
                || existing.iter().any(|o| o.id != cmd.id && o.answers_to(&a));
            if !taken {
                kept_aliases.push(a);
            }
        }
        cmd.aliases = kept_aliases;
        if cmd.aliases.len() < before {
            t.note(format!("{}: aliases another command already uses were left off.", cmd.name));
        }
        if let Err(e) = cmd.validate(&existing) {
            t.skip(&incoming.name, format!("Ferrocast can't use it: {e}."));
            continue;
        }
        match clash {
            Some(i) => {
                let detail = format!("You already have {} in Ferrocast.", existing[i].name);
                if !t.conflict(&incoming.name, detail, choice) {
                    continue;
                }
                existing[i] = cmd.clone();
            }
            None => {
                t.new += 1;
                existing.push(cmd.clone());
            }
        }
        if write {
            c.execute(
                "INSERT INTO commands(id, command) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET command = excluded.command",
                params![cmd.id, serde_json::to_string(&cmd)?],
            )?;
        }
    }
    Ok(())
}

fn timers(c: &Connection, inc: &Incoming, choice: OnConflict, write: bool, t: &mut Tally) -> Result<()> {
    let mut existing = load_timers(c)?;
    let mut seen = HashSet::new();
    let label = inc.source.label();
    for incoming in &inc.timers {
        t.found += 1;
        let key = incoming.name.trim().to_lowercase();
        if !seen.insert(key.clone()) {
            t.skip(
                &incoming.name,
                format!("{label} has a timer called {} twice; the first one was used.", incoming.name),
            );
            continue;
        }
        if let Err(e) = incoming.validate() {
            t.skip(&incoming.name, format!("Ferrocast can't use it: {e}."));
            continue;
        }
        let mut timer = incoming.clone();
        match existing.iter().position(|e| e.name.trim().to_lowercase() == key) {
            Some(i) => {
                timer.id = existing[i].id.clone();
                timer.platforms = existing[i].platforms.clone();
                let detail = format!("You already have a timer called {}.", existing[i].name);
                if !t.conflict(&incoming.name, detail, choice) {
                    continue;
                }
                existing[i] = timer.clone();
            }
            None => {
                timer.id = new_id();
                t.new += 1;
                existing.push(timer.clone());
            }
        }
        if write {
            c.execute(
                "INSERT INTO timers(id, timer) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET timer = excluded.timer",
                params![timer.id, serde_json::to_string(&timer)?],
            )?;
        }
    }
    Ok(())
}

fn set_balance(c: &Connection, viewer_id: i64, points: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO loyalty_balances(viewer_id, points, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(viewer_id) DO UPDATE SET points = excluded.points, updated_at = excluded.updated_at",
        params![viewer_id, points.clamp(0, MAX_BALANCE), now_ms()],
    )?;
    Ok(())
}

/// Why a YouTube entry with only a name isn't paid automatically.
pub const NEEDS_MATCH_REASON: &str = "YouTube names aren't unique and this list has no channel id, so Ferrocast won't pay it \
     automatically. Add their points by hand from their viewer card once they chat.";

fn points(c: &Connection, inc: &Incoming, choice: OnConflict, write: bool, t: &mut Tally) -> Result<()> {
    let label = inc.source.label();
    let Some(platform) = inc.platform else {
        for row in &inc.points {
            t.found += 1;
            t.skip(&row.name, "Ferrocast doesn't know which platform these points belong to.");
        }
        return Ok(());
    };
    let p = pkey(platform);
    // YouTube names aren't unique: only a channel id identifies someone there.
    let ids_only = platform == Platform::Youtube;
    // Everyone Ferrocast knows on this platform, by user id and by unique login (never by
    // display name, which anyone can set). The account seen most recently wins a tie.
    let mut by_id: HashMap<String, i64> = HashMap::new();
    let mut by_login: HashMap<String, i64> = HashMap::new();
    {
        let mut st = c.prepare(
            "SELECT user_id, login, viewer_id FROM viewer_identities WHERE platform = ?1 ORDER BY last_seen",
        )?;
        let rows = st
            .query_map([&p], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, login, v) in rows {
            by_id.insert(id, v);
            if !ids_only {
                by_login.insert(login.trim().to_lowercase(), v);
            }
        }
    }
    let mut pending: HashMap<String, i64> = HashMap::new();
    {
        let mut st = c.prepare("SELECT login, points FROM pending_points WHERE platform = ?1")?;
        for row in st.query_map([&p], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (login, pts) = row?;
            pending.insert(login.to_lowercase(), pts);
        }
    }
    let mut seen_names = HashSet::new();
    let mut seen_viewers: HashMap<i64, String> = HashMap::new();
    let group = ferrocast_core::loyalty::group;
    for row in &inc.points {
        t.found += 1;
        let name = row.name.trim().trim_start_matches('@').trim();
        let key = name.to_lowercase();
        let user_id = row.user_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if key.is_empty() && user_id.is_none() {
            t.skip(&row.name, "It has no name.");
            continue;
        }
        let shown = if name.is_empty() { user_id.unwrap_or_default() } else { name };
        if !seen_names.insert(user_id.map_or_else(|| key.clone(), |id| format!("id:{id}"))) {
            t.skip(shown, format!("{label} lists {shown} twice; the first balance was used."));
            continue;
        }
        if is_known_bot(&key) {
            t.skip(shown, "A chat bot; bots don't earn points in Ferrocast.");
            continue;
        }
        if row.points <= 0 {
            t.skip(shown, "They have 0 points, so there's nothing to bring over.");
            continue;
        }
        let pts = row.points.min(MAX_BALANCE);
        // An id decides on its own: logins are reused after renames, so a row with an id
        // that matches nobody waits for that id instead of going to whoever has the name.
        let known = match user_id {
            Some(id) => by_id.get(id).copied(),
            None => by_login.get(&key).copied(),
        };
        match known {
            Some(viewer) => {
                if let Some(first) = seen_viewers.get(&viewer) {
                    t.skip(
                        shown,
                        format!("Ferrocast knows {shown} and {first} as the same person; {first}'s balance was used."),
                    );
                    continue;
                }
                seen_viewers.insert(viewer, shown.to_string());
                let have: i64 = c
                    .query_row("SELECT points FROM loyalty_balances WHERE viewer_id = ?1", [viewer], |r| r.get(0))
                    .optional()?
                    .unwrap_or(0);
                if have == pts {
                    t.skip(shown, "Ferrocast already has the same balance.");
                    continue;
                }
                if have > 0 {
                    let detail = format!("Ferrocast has {}, {label} has {}.", group(have), group(pts));
                    if !t.conflict(shown, detail, choice) {
                        continue;
                    }
                } else {
                    t.new += 1;
                }
                if write {
                    set_balance(c, viewer, pts)?;
                }
            }
            None => {
                let needs_match = ids_only && user_id.is_none();
                // Pending rows are keyed by the id when there is one, else the name.
                let pkey_login = user_id.map_or_else(|| name.to_string(), |id| format!("id:{id}"));
                if needs_match {
                    t.needs_match += 1;
                    if t.needs_match_items.len() < LIST_MAX {
                        t.needs_match_items.push(Item {
                            name: format!("{shown} ({})", group(pts)),
                            reason: NEEDS_MATCH_REASON.into(),
                        });
                    }
                } else {
                    match pending.get(&pkey_login.to_lowercase()) {
                        Some(&before) if before == pts => {
                            t.skip(shown, "Already waiting for them from an earlier import.");
                            continue;
                        }
                        Some(&before) => {
                            let detail = format!(
                                "An earlier import is holding {} for them; {label} has {}.",
                                group(before),
                                group(pts)
                            );
                            if !t.conflict(shown, detail, choice) {
                                continue;
                            }
                        }
                        None => t.waiting += 1,
                    }
                }
                if write {
                    c.execute(
                        "INSERT INTO pending_points(platform, login, user_id, needs_match, points, source, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(platform, login) DO UPDATE SET points = excluded.points, user_id = excluded.user_id,
                           needs_match = excluded.needs_match, source = excluded.source, updated_at = excluded.updated_at",
                        params![p, pkey_login, user_id, needs_match, pts, inc.source.key(), now_ms()],
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn loyalty(c: &Connection, inc: &Incoming, choice: OnConflict, write: bool, t: &mut Tally) -> Result<()> {
    let Some(patch) = &inc.loyalty else { return Ok(()) };
    t.found += 1;
    let saved: Option<String> =
        c.query_row("SELECT value FROM settings WHERE key = 'loyalty'", [], |r| r.get(0)).optional()?;
    let current: LoyaltyConfig = match &saved {
        Some(s) => serde_json::from_str(s).map_err(damaged("the points settings"))?,
        None => LoyaltyConfig::default(),
    };
    let mut merged = serde_json::to_value(&current)?;
    if let Some(obj) = merged.as_object_mut() {
        for (k, v) in patch {
            if obj.contains_key(k) {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    let name = "Points settings";
    let merged: LoyaltyConfig = match serde_json::from_value(merged) {
        Ok(m) => m,
        Err(_) => {
            t.skip(name, "The settings didn't fit Ferrocast's points settings.");
            return Ok(());
        }
    };
    if let Err(e) = merged.validate() {
        t.skip(name, format!("Ferrocast can't use them: {e}"));
        return Ok(());
    }
    if merged == current && saved.is_some() {
        t.skip(name, "Ferrocast's points settings are already the same.");
        return Ok(());
    }
    if saved.is_some() {
        let detail = format!(
            "You've already set up points in Ferrocast (\"{}\", {} every {} minutes); {} uses \"{}\", {} every {} minutes.",
            current.name,
            current.per_interval,
            current.interval_min,
            inc.source.label(),
            merged.name,
            merged.per_interval,
            merged.interval_min
        );
        if !t.conflict(name, detail, choice) {
            return Ok(());
        }
    } else {
        t.new += 1;
    }
    if write {
        c.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES ('loyalty', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![serde_json::to_string(&merged)?, now_ms()],
        )?;
    }
    Ok(())
}

fn tips(c: &Connection, inc: &Incoming, write: bool, t: &mut Tally) -> Result<()> {
    let mut seen = HashSet::new();
    let source = inc.source.key();
    for tip in &inc.tips {
        t.found += 1;
        let label =
            format!("{} ({})", tip.name, ferrocast_core::money::Money::new(tip.amount_minor, &tip.currency).display());
        if !seen.insert(tip.source_id.clone()) {
            t.skip(&label, "Listed twice; it was kept once.");
            continue;
        }
        let known: bool = c.query_row(
            "SELECT EXISTS (SELECT 1 FROM imported_tips WHERE source = ?1 AND source_id = ?2)",
            params![source, tip.source_id],
            |r| r.get(0),
        )?;
        if known {
            t.skip(&label, "Already imported.");
            continue;
        }
        t.new += 1;
        if write {
            c.execute(
                "INSERT OR IGNORE INTO imported_tips
                   (source, source_id, occurred_at, name, amount_minor, currency, message, provider, imported_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    source,
                    tip.source_id,
                    tip.occurred_at,
                    tip.name,
                    tip.amount_minor,
                    tip.currency,
                    tip.message,
                    tip.provider,
                    now_ms()
                ],
            )?;
        }
    }
    Ok(())
}

/// What an import would do. Nothing is written.
pub fn preview(store: &Store, inc: &Incoming) -> Result<Preview> {
    let tallies = store.read(|c| run(c, inc, OnConflict::Keep, false))?;
    Ok(Preview {
        sections: tallies
            .into_iter()
            .map(|(kind, t)| PreviewSection {
                kind,
                found: t.found,
                new: t.new,
                conflicts: t.conflicts,
                waiting: t.waiting,
                needs_match: t.needs_match,
                skipped: t.skipped,
                conflict_items: t.conflict_items,
                skipped_items: t.skipped_items,
                needs_match_items: t.needs_match_items,
                notes: t.notes,
            })
            .collect(),
        not_imported: inc.not_imported.clone(),
    })
}

/// Imports: a backup first, then everything in one transaction.
pub fn apply(store: &Store, inc: &Incoming, choice: OnConflict) -> Result<Report> {
    let backup = store.snapshot(&format!("before-{}-import", inc.source.key()))?;
    let tallies = store.write(|tx| run(tx, inc, choice, true))?;
    Ok(Report {
        source: inc.source,
        backup: backup.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        on_conflict: choice,
        sections: tallies
            .into_iter()
            .map(|(kind, t)| ReportSection {
                kind,
                found: t.found,
                added: t.new,
                replaced: t.replaced,
                kept: t.kept,
                waiting: t.waiting,
                needs_match: t.needs_match,
                skipped: t.skipped,
                skipped_items: t.skipped_items,
                needs_match_items: t.needs_match_items,
                notes: t.notes,
            })
            .collect(),
        not_imported: inc.not_imported.clone(),
    })
}

/// Pays points an import is holding for someone who just showed up for the first time on
/// `platform`: an entry with their user id, or (Twitch and Kick only, entries without an id)
/// their login. Never by display name, and never a "needs a match" entry. Called inside the
/// transaction that creates their identity. Returns the points paid.
pub(crate) fn claim_pending(
    tx: &Connection,
    platform: &str,
    user_id: &str,
    login: Option<&str>,
    viewer_id: i64,
) -> rusqlite::Result<i64> {
    // No login: an id entry is the only way to match. An empty string never equals a login.
    let login = login.map(|l| l.trim().trim_start_matches('@')).unwrap_or("");
    let mut st = tx.prepare(
        "SELECT rowid, points FROM pending_points
         WHERE platform = ?1 AND needs_match = 0
           AND (user_id = ?2 OR (user_id IS NULL AND ?1 <> 'youtube' AND ?3 <> '' AND login = ?3 COLLATE NOCASE))",
    )?;
    let rows = st
        .query_map(params![platform, user_id, login], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // An id entry and a login entry can both be theirs; the larger balance is used.
    let Some(best) = rows.iter().map(|(_, p)| *p).max() else { return Ok(0) };
    for (rowid, _) in &rows {
        tx.execute("DELETE FROM pending_points WHERE rowid = ?1", [rowid])?;
    }
    tx.execute(
        "INSERT INTO loyalty_balances(viewer_id, points, updated_at) VALUES (?1, MIN(?2, ?3), ?4)
         ON CONFLICT(viewer_id) DO UPDATE SET points = MIN(points + excluded.points, ?3),
           updated_at = excluded.updated_at",
        params![viewer_id, best, MAX_BALANCE, now_ms()],
    )?;
    Ok(best)
}

/// How many balances are waiting for people Ferrocast hasn't seen yet (not counting
/// "needs a match" entries, which are never paid automatically).
pub fn pending_count(store: &Store) -> Result<i64> {
    store.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM pending_points WHERE needs_match = 0", [], |r| r.get(0))?))
}

#[cfg(test)]
mod tests;
