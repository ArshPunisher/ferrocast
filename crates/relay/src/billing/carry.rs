//! Carrying Pro's data over to a new subscription (decided 2026-10-19).
//!
//! A streamer whose Pro lapsed and who buys again with a new checkout gets a new Razorpay
//! subscription id. Within the 30 days the old one's data is kept, everything keyed by the old
//! id (stream history, finished cloud backups with their files, the cloud bot with its sealed sign-ins, the Discord
//! server, the weekly email's sealed address, paired phones and their audit) moves to the new
//! id when the new one grants Pro, in one transaction. Values sealed with the subscription id
//! as their AAD (`vault`) are opened with the old id and sealed again for the new one.
//!
//! Only the same owner's: the old subscription must have been linked to an install that the
//! new one is linked to now (`pro_install_subs`), and both must carry the same checkout email
//! hash. The old one must no longer grant Pro and still be inside its keep window
//! (`SubRow::stopped_at` + 30 days). Test grants (`comp_…`) are never a source or a target:
//! they're the owner's short trials, and moving real data into one would tie it to a grant
//! that ends within 90 days without any renewal. Each feature moves only when the new
//! subscription has nothing of its own there yet (a fresh subscription has nothing), so
//! nothing on the new one is overwritten. Running again moves nothing (the old id has nothing
//! left), so a replayed webhook is a no-op.

use rusqlite::{OptionalExtension, params};

use super::access;
use super::comp::is_comp;
use super::store::SubRow;
use crate::db::Db;
use crate::vault::Sealed;
use crate::{AppState, now_ms};

/// The keep window every Pro feature uses after a lapse.
pub const KEEP_AFTER_LAPSE_MS: i64 = crate::history::KEEP_AFTER_LAPSE_MS;

/// Opens a value sealed for `(old, purpose)` and seals it for `(new, purpose)`; `None` if it
/// doesn't open (or there's no vault).
pub(crate) type Reseal<'a> = dyn Fn(&str, &str, &str, &Sealed) -> Option<Sealed> + 'a;

/// What one carry-over moved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carried {
    /// The old subscriptions whose data moved (empty: nothing to do).
    pub from: Vec<String>,
    /// Features left on an old subscription (the new one had its own, or a sealed value
    /// didn't open); they expire with it.
    pub left: Vec<&'static str>,
}

fn sealed_at(r: &rusqlite::Row<'_>, i: usize) -> rusqlite::Result<Option<Sealed>> {
    let key_v: Option<i64> = r.get(i)?;
    let nonce: Option<Vec<u8>> = r.get(i + 1)?;
    let ct: Option<Vec<u8>> = r.get(i + 2)?;
    Ok(match (key_v, nonce, ct) {
        (Some(key_v), Some(nonce), Some(ct)) => Some(Sealed { key_v, nonce, ct }),
        _ => None,
    })
}

fn has(tx: &rusqlite::Transaction<'_>, table: &str, sub: &str) -> rusqlite::Result<bool> {
    Ok(tx
        .query_row(&format!("SELECT 1 FROM {table} WHERE sub_id = ?1 LIMIT 1"), [sub], |_| Ok(()))
        .optional()?
        .is_some())
}

fn retag(tx: &rusqlite::Transaction<'_>, tables: &[&str], old: &str, new: &str) -> rusqlite::Result<()> {
    for t in tables {
        tx.execute(&format!("UPDATE {t} SET sub_id = ?2 WHERE sub_id = ?1"), params![old, new])?;
    }
    Ok(())
}

impl Db {
    /// The lapsed subscriptions whose data `new` may take over at `now` (see the module docs),
    /// the most recently stopped first.
    pub(crate) fn carry_sources(&self, new: &SubRow, now: i64) -> rusqlite::Result<Vec<SubRow>> {
        if is_comp(&new.id) || !access::is_pro(&new.access(), now) {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = {
            let c = self.c();
            let mut q = c.prepare(
                "SELECT DISTINCT h.sub_id FROM pro_install_subs h
                 JOIN pro_installs p ON p.install_id = h.install_id AND p.sub_id = ?1
                 WHERE h.sub_id != ?1",
            )?;
            q.query_map([&new.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for id in ids {
            let Some(old) = self.pro_sub(&id)? else { continue };
            let lapsed = !access::is_pro(&old.access(), now);
            let kept = now < old.stopped_at().saturating_add(KEEP_AFTER_LAPSE_MS);
            if !is_comp(&old.id) && old.email_hash == new.email_hash && lapsed && kept {
                out.push(old);
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.stopped_at()));
        Ok(out)
    }

    /// Moves `old`'s data to `new` in one transaction. Each feature moves whole, and only when
    /// `new` has none of it yet; sealed values are sealed again for `new` (a feature with a
    /// value that doesn't open stays behind whole). Backup files are keyed by the subscription:
    /// only the finished backups in `copied` (their files already copied to `new`'s keys)
    /// move. Returns whether anything moved, and the features left behind.
    pub(crate) fn carry_pro_data(
        &self,
        old: &str,
        new: &str,
        copied: &[String],
        reseal: &Reseal<'_>,
    ) -> rusqlite::Result<(bool, Vec<&'static str>)> {
        let mut c = self.c();
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut left = Vec::new();
        let changes = |tx: &rusqlite::Transaction<'_>| -> rusqlite::Result<i64> {
            tx.query_row("SELECT total_changes()", [], |r| r.get(0))
        };
        let start = changes(&tx)?;

        // Stream history (samples hang off the stream rows) and finished cloud backups (an
        // unfinished upload stays behind and is swept with the old subscription).
        if has(&tx, "streams", old)? {
            if has(&tx, "streams", new)? {
                left.push("history");
            } else {
                retag(&tx, &["streams"], old, new)?;
            }
        }
        if has(&tx, "cloud_backups", old)? {
            if has(&tx, "cloud_backups", new)? || copied.is_empty() {
                left.push("backups");
            } else {
                for id in copied {
                    tx.execute(
                        "UPDATE cloud_backups SET sub_id = ?2 WHERE sub_id = ?1 AND id = ?3",
                        params![old, new, id],
                    )?;
                }
            }
        }

        // The cloud bot: channels, sealed sign-ins, config, counter changes.
        if has(&tx, "bot_channels", old)? || has(&tx, "bot_config", old)? || has(&tx, "bot_tokens", old)? {
            let theirs = has(&tx, "bot_channels", new)? || has(&tx, "bot_config", new)? || has(&tx, "bot_tokens", new)?;
            let tokens: Vec<(String, String, Sealed)> = {
                let mut q =
                    tx.prepare("SELECT platform, purpose, key_v, nonce, ct FROM bot_tokens WHERE sub_id = ?1")?;
                q.query_map([old], |r| {
                    let s = sealed_at(r, 2)?.unwrap_or(Sealed { key_v: 0, nonce: Vec::new(), ct: Vec::new() });
                    Ok((r.get(0)?, r.get(1)?, s))
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            let resealed: Option<Vec<_>> = tokens
                .iter()
                .map(|(platform, purpose, s)| {
                    reseal(old, new, &crate::bot::token_purpose(platform, purpose), s)
                        .map(|n| (platform.clone(), purpose.clone(), n))
                })
                .collect();
            match (theirs, resealed) {
                (false, Some(resealed)) => {
                    tx.execute("DELETE FROM bot_tokens WHERE sub_id = ?1", [old])?;
                    let now = now_ms();
                    for (platform, purpose, s) in resealed {
                        tx.execute(
                            "INSERT INTO bot_tokens(sub_id, platform, purpose, key_v, nonce, ct, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            params![new, platform, purpose, s.key_v, s.nonce, s.ct, now],
                        )?;
                    }
                    retag(&tx, &["bot_channels", "bot_config", "bot_counter_changes"], old, new)?;
                }
                _ => left.push("cloud bot"),
            }
        }

        // The Discord server with its rules and go-live posts (links and grants are keyed by
        // the server, so they follow it).
        if has(&tx, "discord_guilds", old)? {
            if has(&tx, "discord_guilds", new)? {
                left.push("discord");
            } else {
                retag(&tx, &["discord_guilds", "discord_rules", "discord_posts"], old, new)?;
            }
        }

        // The weekly email: the sealed address (and one waiting for its confirm link), sends.
        let mail: Option<(Option<Sealed>, Option<Sealed>)> = tx
            .query_row(
                "SELECT key_v, nonce, ct, p_key_v, p_nonce, p_ct FROM mail_addresses WHERE sub_id = ?1",
                [old],
                |r| Ok((sealed_at(r, 0)?, sealed_at(r, 3)?)),
            )
            .optional()?;
        if let Some((active, pending)) = mail {
            let p = crate::mail::PURPOSE;
            let a = active.as_ref().map(|s| reseal(old, new, p, s));
            let b = pending.as_ref().map(|s| reseal(old, new, p, s));
            let opened = !matches!(a, Some(None)) && !matches!(b, Some(None));
            if opened && !has(&tx, "mail_addresses", new)? {
                let (a, b) = (a.flatten(), b.flatten());
                tx.execute(
                    "UPDATE mail_addresses SET sub_id = ?2, key_v = ?3, nonce = ?4, ct = ?5, p_key_v = ?6, p_nonce = ?7,
                       p_ct = ?8 WHERE sub_id = ?1",
                    params![
                        old,
                        new,
                        a.as_ref().map(|s| s.key_v),
                        a.as_ref().map(|s| s.nonce.clone()),
                        a.as_ref().map(|s| s.ct.clone()),
                        b.as_ref().map(|s| s.key_v),
                        b.as_ref().map(|s| s.nonce.clone()),
                        b.as_ref().map(|s| s.ct.clone()),
                    ],
                )?;
                tx.execute("UPDATE OR IGNORE mail_sent SET sub_id = ?2 WHERE sub_id = ?1", params![old, new])?;
                tx.execute("DELETE FROM mail_sent WHERE sub_id = ?1", [old])?;
            } else {
                left.push("weekly email");
            }
        }

        // Paired phones, pairing codes and their audit (phones are tied to their PC's install;
        // only the subscription changes, so they stop getting 401 device_moved).
        retag(&tx, &["remote_devices", "remote_codes", "remote_audit"], old, new)?;

        let moved = changes(&tx)? > start;
        tx.commit()?;
        Ok((moved, left))
    }
}

/// Copies the files of `old`'s finished backups to `new`'s keys (before the rows move); the
/// ids copied. Any failure copies nothing (what was copied is removed), so backups stay
/// behind whole.
async fn copy_backups(st: &AppState, old: &str, new: &str) -> Vec<String> {
    let Ok(blobs) = st.blobs() else { return Vec::new() };
    let Ok(rows) = st.db.backups(old, crate::backups::store::COMPLETE) else { return Vec::new() };
    if rows.is_empty() || st.db.backups(new, crate::backups::store::COMPLETE).map_or(true, |r| !r.is_empty()) {
        return Vec::new();
    }
    let mut done: Vec<String> = Vec::new();
    for b in &rows {
        let (from, to) = (crate::backups::blob_key(old, &b.id), crate::backups::blob_key(new, &b.id));
        let ok = match blobs.get(&from).await {
            Ok(body) => blobs.put_stream(&to, body, b.bytes.max(0) as u64).await.is_ok(),
            Err(_) => false,
        };
        if !ok {
            tracing::warn!(backup = %b.id, "couldn't copy a backup to the new subscription; backups stay behind");
            for id in &done {
                let _ = blobs.delete(&crate::backups::blob_key(new, id)).await;
            }
            let _ = blobs.delete(&to).await;
            return Vec::new();
        }
        done.push(b.id.clone());
    }
    done
}

/// Moves every lapsed subscription's data that `new` may take over. Logged, never fatal: the
/// next refresh or webhook (or the daily re-check) tries again.
pub(crate) async fn carry_over(st: &AppState, new: &str) -> Carried {
    let mut out = Carried::default();
    let Ok(Some(row)) = st.db.pro_sub(new) else { return out };
    let sources = match st.db.carry_sources(&row, now_ms()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't look for Pro data to carry over");
            return out;
        }
    };
    let vault = st.vault().ok();
    let reseal = |old: &str, new: &str, purpose: &str, s: &Sealed| -> Option<Sealed> {
        let v = vault?;
        let plain = v.open(old, purpose, s).ok()?;
        v.seal(new, purpose, &plain).ok()
    };
    for old in sources {
        let copied = copy_backups(st, &old.id, new).await;
        let moved = st.db.carry_pro_data(&old.id, new, &copied, &reseal);
        // Whichever copy of the backup files is no longer referenced goes.
        let stale = if matches!(&moved, Ok((_, left)) if !left.contains(&"backups")) { &old.id } else { new };
        if let Ok(blobs) = st.blobs() {
            for id in &copied {
                let _ = blobs.delete(&crate::backups::blob_key(stale, id)).await;
            }
        }
        match moved {
            Ok((false, _)) => {}
            Ok((true, left)) => {
                tracing::info!(from = %old.id, to = %new, ?left, "Pro data carried over to a new subscription");
                out.left.extend(left);
                out.from.push(old.id);
            }
            Err(e) => tracing::warn!(error = %e, from = %old.id, "couldn't carry Pro data over"),
        }
    }
    out
}

/// After a subscription's state was copied from Razorpay: if it grants Pro, take over a lapsed
/// subscription's data and give back its server's paused Discord roles.
pub(crate) async fn after_sync(st: &AppState, sub: &str) {
    let Ok(Some(row)) = st.db.pro_sub(sub) else { return };
    if !access::is_pro(&row.access(), now_ms()) {
        return;
    }
    carry_over(st, sub).await;
    crate::discord::resume_for_sub(st, sub).await;
}
