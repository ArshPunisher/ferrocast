//! Cloud sync and "Use cloud overlays" (docs/CLOUD.md §1.1, M6). Off by default, and only
//! with a signed-in Ferrocast account: the app works fully without either (R-01).
//!
//! **Sync** pushes and pulls the overlays, alert rules, own designs and the alert design in
//! use through the relay's `/v1/cloud/docs`, with per-document versions. The app remembers the
//! version and a hash of each document as it last agreed with the cloud; a document whose hash
//! changed here since then is pushed with that version as its base. When the cloud moved on
//! meanwhile, **the cloud copy wins**: an overlay or design of this PC is kept as a copy named
//! "… (this PC)", an alert rule's change is named in a notice.
//!
//! **Use cloud overlays** shows the cloud overlays' URLs (`/o/<token>/` on Ferrocast's servers)
//! for the synced overlays, so OBS keeps working with this PC off.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use ferrocast_core::alert::AlertRule;
use ferrocast_core::design::{Design, Origin, OriginKind};
use ferrocast_core::overlay::Overlay;
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::frames::{LiveFrame, Notice, NoticeLevel};
use crate::{Engine, EngineError, now};

/// Where the sync's state is kept (settings table; no secrets in it).
const KEY: &str = "cloud_sync";
/// How often a sync runs while it's on (and after a change, a little later).
const EVERY: Duration = Duration::from_secs(300);
/// Documents per push (the relay takes up to 100).
const PUSH_CHUNK: usize = 100;
/// Conflict notes kept for the Settings page.
const NOTES_KEPT: usize = 10;

const OVERLAY: &str = "overlay";
const RULE: &str = "alert_rule";
const DESIGN: &str = "design";
const SETTINGS: &str = "settings";
const MAIN: &str = "main";

/// What the cloud and this PC last agreed on for one document.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Seen {
    pub version: i64,
    /// SHA-256 of the body as agreed; empty for a document deleted on both sides.
    pub hash: String,
}

/// The sync's state, kept between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncState {
    pub enabled: bool,
    pub use_cloud_overlays: bool,
    /// The highest version pulled.
    pub since: i64,
    /// `kind:id` → what both sides last agreed on.
    pub docs: BTreeMap<String, Seen>,
    pub last_sync_at: Option<i64>,
    /// `{code, message, hint}` of the last run that failed.
    pub last_error: Option<Value>,
    /// What the cloud replaced here, newest first.
    pub notes: Vec<String>,
    /// Overlay id → its cloud URL (read on each sync).
    pub cloud_urls: HashMap<String, String>,
}

/// In memory, for overlay views (no database read per view).
#[derive(Debug, Default)]
pub(crate) struct Mem {
    pub use_cloud_overlays: bool,
    pub cloud_urls: HashMap<String, String>,
    pub running: bool,
    /// Sync is on: a change here syncs soon instead of at the next round.
    pub enabled: bool,
    pub wake: std::sync::Arc<tokio::sync::Notify>,
}

/// A change here waits this long before syncing (several quick edits go up together).
const AFTER_CHANGE: Duration = Duration::from_secs(10);

fn refused(status: u16, code: &'static str, message: &str, hint: &str) -> EngineError {
    EngineError::Refused { status, code, message: message.into(), hint: hint.into(), retry_after: None }
}

/// The last failure for the Settings page, in the API's error shape.
fn error_json(e: &EngineError) -> Value {
    match e {
        EngineError::Refused { code, message, hint, .. } => json!({ "code": code, "message": message, "hint": hint }),
        other => json!({ "code": "sync_failed", "message": format!("Cloud sync didn't finish: {other}"),
                         "hint": "Ferrocast tries again in a few minutes." }),
    }
}

fn hash(v: &Value) -> String {
    Sha256::digest(v.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash both sides are compared by: times and this PC's token left out, so a save that
/// changed nothing else never counts as a change.
fn norm_hash(kind: &str, body: &Value) -> String {
    let mut v = body.clone();
    if kind == OVERLAY {
        v["token"] = json!("");
    }
    if kind == OVERLAY || kind == DESIGN {
        v["updated_at"] = json!(0);
    }
    hash(&v)
}

fn key(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

/// An overlay as it's synced: never this PC's token.
fn overlay_body(o: &Overlay) -> Value {
    let mut v = serde_json::to_value(o).unwrap_or(Value::Null);
    v["token"] = json!("");
    v["updated_at"] = json!(0);
    v
}

fn design_body(d: &Design) -> Value {
    let mut v = serde_json::to_value(d).unwrap_or(Value::Null);
    v["updated_at"] = json!(0);
    v
}

/// Background: a sync every few minutes while it's on and the account signed in.
pub(crate) async fn run(engine: Engine) {
    if engine.relay_url().is_err() {
        return;
    }
    engine.cloud_sync_load().await;
    let wake = engine.cloud_mem(|m| m.wake.clone());
    loop {
        tokio::select! {
            _ = tokio::time::sleep(EVERY) => {}
            _ = wake.notified() => tokio::time::sleep(AFTER_CHANGE).await,
        }
        let on = engine.cloud_mem(|m| m.use_cloud_overlays) || engine.sync_state().await.enabled;
        if on && engine.account_signed_in() {
            let _ = engine.cloud_sync_now().await;
        }
    }
}

impl Engine {
    pub(crate) fn cloud_mem<T>(&self, f: impl FnOnce(&mut Mem) -> T) -> T {
        f(&mut self.inner.cloud_sync.lock().unwrap_or_else(|p| p.into_inner()))
    }

    async fn sync_state(&self) -> SyncState {
        self.db(|s| repo::settings::get::<SyncState>(s, KEY)).await.ok().flatten().unwrap_or_default()
    }

    async fn sync_save(&self, st: &SyncState) {
        let v = st.clone();
        if let Err(e) = self.db(move |s| repo::settings::set(s, KEY, &v)).await {
            tracing::warn!(error = %e, "couldn't save the cloud sync state");
        }
        self.cloud_mem(|m| {
            m.use_cloud_overlays = st.use_cloud_overlays && st.enabled;
            m.enabled = st.enabled;
            m.cloud_urls = st.cloud_urls.clone();
        });
    }

    /// At start: the switches into memory (overlay views read them).
    pub(crate) async fn cloud_sync_load(&self) {
        let st = self.sync_state().await;
        self.cloud_mem(|m| {
            m.use_cloud_overlays = st.use_cloud_overlays && st.enabled;
            m.enabled = st.enabled;
            m.cloud_urls = st.cloud_urls.clone();
        });
    }

    /// The cloud URL to show for an overlay, when "Use cloud overlays" is on and it's synced.
    /// Something sync carries changed here: sync soon (when it's on).
    pub(crate) fn cloud_changed(&self) {
        let (on, wake) = self.cloud_mem(|m| (m.enabled, m.wake.clone()));
        if on {
            wake.notify_one();
        }
    }

    pub(crate) fn cloud_url_for(&self, overlay_id: &str) -> Option<String> {
        self.cloud_mem(|m| if m.use_cloud_overlays { m.cloud_urls.get(overlay_id).cloned() } else { None })
    }

    /// `GET /api/v1/cloud`: the switches, the last sync and what it replaced.
    pub async fn cloud_status(&self) -> Value {
        let st = self.sync_state().await;
        json!({
            "signed_in": self.account_signed_in(),
            "sync": st.enabled,
            "use_cloud_overlays": st.use_cloud_overlays,
            "last_sync_at": st.last_sync_at,
            "error": st.last_error,
            "notes": st.notes,
            "synced": st.docs.values().filter(|d| !d.hash.is_empty()).count(),
            "cloud_overlays": st.cloud_urls.len(),
            "running": self.cloud_mem(|m| m.running),
        })
    }

    /// `PUT /api/v1/cloud {sync?, use_cloud_overlays?}`. Turning sync on needs the account,
    /// turns cloud mode on for it (free) and syncs at once.
    pub async fn cloud_set(&self, sync: Option<bool>, use_cloud_overlays: Option<bool>) -> Result<Value, EngineError> {
        let mut st = self.sync_state().await;
        let turning_on = sync == Some(true) && !st.enabled;
        if (turning_on || use_cloud_overlays == Some(true)) && !self.account_signed_in() {
            return Err(refused(
                409,
                "signed_out",
                "Sign in to your Ferrocast account first.",
                "Cloud sync keeps your overlays, alerts and designs on your account. Sign in at the bottom of the sidebar.",
            ));
        }
        if turning_on {
            self.cloud_call(reqwest::Method::POST, "/v1/cloud/enable", None).await?;
        }
        if let Some(s) = sync {
            st.enabled = s;
            if !s {
                // Off: the cloud copies stay; turning it on later starts from what both have.
                st.use_cloud_overlays = false;
            }
        }
        if let Some(u) = use_cloud_overlays {
            st.use_cloud_overlays = u && st.enabled;
        }
        self.sync_save(&st).await;
        if turning_on {
            let _ = self.cloud_sync_now().await;
        }
        Ok(self.cloud_status().await)
    }

    /// The relay's `/v1/cloud…` with the account's app session.
    async fn cloud_call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, EngineError> {
        let token = self.account_token()?.ok_or_else(|| {
            refused(409, "signed_out", "You're signed out of your Ferrocast account.", "Sign in again to keep syncing.")
        })?;
        Ok(self.account_call(&token, method, path, body).await?.1)
    }

    /// One sync now (`POST /api/v1/cloud/sync`); its status.
    pub async fn cloud_sync_now(&self) -> Result<Value, EngineError> {
        if self.cloud_mem(|m| std::mem::replace(&mut m.running, true)) {
            return Ok(self.cloud_status().await);
        }
        let result = self.cloud_sync_inner().await;
        self.cloud_mem(|m| m.running = false);
        let mut st = self.sync_state().await;
        match &result {
            Ok(()) => {
                st.last_sync_at = Some(now().0);
                st.last_error = None;
            }
            Err(e) => st.last_error = Some(error_json(e)),
        }
        self.sync_save(&st).await;
        result?;
        Ok(self.cloud_status().await)
    }

    /// This PC's documents as they're synced: `kind:id` → body.
    async fn local_docs(&self) -> Result<BTreeMap<String, (String, String, Value)>, EngineError> {
        let (overlays, rules, designs, active) = self
            .db(|s| {
                Ok((
                    repo::overlays::list(s)?,
                    repo::rules::list(s)?,
                    repo::designs::list(s)?,
                    repo::designs::active_alert(s)?,
                ))
            })
            .await?;
        let mut out = BTreeMap::new();
        for o in &overlays {
            out.insert(key(OVERLAY, &o.id), (OVERLAY.to_string(), o.id.clone(), overlay_body(o)));
        }
        for r in &rules {
            out.insert(key(RULE, &r.id), (RULE.to_string(), r.id.clone(), json!(r)));
        }
        for d in designs.iter().filter(|d| !d.built_in) {
            out.insert(key(DESIGN, &d.id), (DESIGN.to_string(), d.id.clone(), design_body(d)));
        }
        let test_alerts = self.state().settings.test_alerts;
        out.insert(
            key(SETTINGS, MAIN),
            (
                SETTINGS.to_string(),
                MAIN.to_string(),
                json!({ "active_alert_design": active, "test_alerts": test_alerts }),
            ),
        );
        Ok(out)
    }

    async fn cloud_sync_inner(&self) -> Result<(), EngineError> {
        let mut st = self.sync_state().await;
        if !st.enabled {
            return Ok(());
        }
        // Pull everything that changed since the last time.
        let mut local = self.local_docs().await?;
        loop {
            let page =
                self.cloud_call(reqwest::Method::GET, &format!("/v1/cloud/docs?since={}", st.since), None).await?;
            for d in page["docs"].as_array().cloned().unwrap_or_default() {
                let (Some(kind), Some(id)) = (d["kind"].as_str(), d["id"].as_str()) else { continue };
                let version = d["version"].as_i64().unwrap_or(0);
                let body = if d["deleted"].as_bool() == Some(true) { None } else { Some(d["body"].clone()) };
                let k = key(kind, id);
                let mine = local.get(&k).map(|(_, _, b)| b.clone());
                let changed_here = match (st.docs.get(&k), &mine) {
                    (Some(seen), Some(b)) => norm_hash(kind, b) != seen.hash,
                    (Some(seen), None) => !seen.hash.is_empty(),
                    (None, Some(b)) => body.as_ref().is_none_or(|sb| norm_hash(kind, sb) != norm_hash(kind, b)),
                    (None, None) => false,
                };
                if changed_here {
                    self.keep_mine(&mut st, kind, mine.as_ref()).await;
                }
                self.apply(kind, id, body.as_ref()).await;
                st.docs
                    .insert(k, Seen { version, hash: body.as_ref().map(|b| norm_hash(kind, b)).unwrap_or_default() });
            }
            st.since = page["version"].as_i64().unwrap_or(st.since).max(st.since);
            if page["more"].as_bool() != Some(true) {
                break;
            }
        }
        // Push what changed here.
        local = self.local_docs().await?;
        let mut push: Vec<Value> = Vec::new();
        for (k, (kind, id, body)) in &local {
            let seen = st.docs.get(k);
            if seen.is_none_or(|s| s.hash != norm_hash(kind, body)) {
                push.push(
                    json!({ "kind": kind, "id": id, "base_version": seen.map_or(0, |s| s.version), "body": body }),
                );
            }
        }
        for (k, seen) in &st.docs {
            if !seen.hash.is_empty() && !local.contains_key(k) {
                let Some((kind, id)) = k.split_once(':') else { continue };
                // A built-in design is never synced (and the settings document is never deleted).
                if kind == SETTINGS {
                    continue;
                }
                push.push(json!({ "kind": kind, "id": id, "base_version": seen.version, "body": null }));
            }
        }
        for chunk in push.chunks(PUSH_CHUNK) {
            let r = self.cloud_call(reqwest::Method::PUT, "/v1/cloud/docs", Some(json!({ "docs": chunk }))).await?;
            for a in r["applied"].as_array().cloned().unwrap_or_default() {
                let (Some(kind), Some(id)) = (a["kind"].as_str(), a["id"].as_str()) else { continue };
                let k = key(kind, id);
                let h = local.get(&k).map(|(_, _, b)| norm_hash(kind, b)).unwrap_or_default();
                st.docs.insert(k, Seen { version: a["version"].as_i64().unwrap_or(0), hash: h });
            }
            for c in r["conflicts"].as_array().cloned().unwrap_or_default() {
                let (Some(kind), Some(id)) = (c["kind"].as_str(), c["id"].as_str()) else { continue };
                let server = &c["server"];
                let body = if server["deleted"].as_bool() == Some(true) { None } else { Some(server["body"].clone()) };
                let k = key(kind, id);
                let mine = local.get(&k).map(|(_, _, b)| b.clone());
                self.keep_mine(&mut st, kind, mine.as_ref()).await;
                self.apply(kind, id, body.as_ref()).await;
                st.docs.insert(
                    k,
                    Seen {
                        version: server["version"].as_i64().unwrap_or(0),
                        hash: body.as_ref().map(|b| norm_hash(kind, b)).unwrap_or_default(),
                    },
                );
            }
            st.since = st.since.max(r["version"].as_i64().unwrap_or(0));
        }
        // The cloud overlays' URLs, for "Use cloud overlays".
        let list = self.cloud_call(reqwest::Method::GET, "/v1/cloud/overlays", None).await?;
        st.cloud_urls = list
            .as_array()
            .map(|a| {
                a.iter().filter_map(|o| Some((o["id"].as_str()?.to_string(), o["url"].as_str()?.to_string()))).collect()
            })
            .unwrap_or_default();
        self.sync_save(&st).await;
        Ok(())
    }

    /// The cloud's copy wins; this PC's is kept: an overlay or design as "… (this PC)", an
    /// alert rule named in a notice.
    async fn keep_mine(&self, st: &mut SyncState, kind: &str, mine: Option<&Value>) {
        let Some(mine) = mine else { return };
        let note = match kind {
            OVERLAY => {
                let Ok(mut o) = serde_json::from_value::<Overlay>(mine.clone()) else { return };
                o.name = format!("{} (this PC)", o.name.trim()).chars().take(60).collect();
                let name = o.name.clone();
                match self.db(move |s| repo::overlays::create_from(s, o)).await {
                    Ok(_) => format!("The web’s change to an overlay replaced this PC’s. Yours is kept as “{name}”."),
                    Err(_) => return,
                }
            }
            DESIGN => {
                let Ok(d) = serde_json::from_value::<Design>(mine.clone()) else { return };
                let mut copy = d.clone().into_user(
                    ferrocast_store::new_id(),
                    Origin { kind: OriginKind::Duplicate, from: Some(d.name.clone()) },
                    now().0,
                );
                copy.name =
                    format!("{} (this PC)", d.name.trim()).chars().take(ferrocast_core::design::MAX_NAME).collect();
                let name = copy.name.clone();
                match self.db(move |s| repo::designs::insert(s, &copy)).await {
                    Ok(_) => format!("The web’s change to a design replaced this PC’s. Yours is kept as “{name}”."),
                    Err(_) => return,
                }
            }
            RULE => {
                let name = mine["name"].as_str().unwrap_or("an alert").to_string();
                format!("Your change to the {name} rule on this PC was replaced by the one from the web.")
            }
            _ => return,
        };
        self.send_live(LiveFrame::new(
            "notice",
            &Notice { key: Some("cloud-sync".into()), level: NoticeLevel::Info, message: note.clone(), action: None },
        ));
        st.notes.insert(0, note);
        st.notes.truncate(NOTES_KEPT);
    }

    /// Makes this PC's copy of a document what the cloud has (`None`: deleted there).
    async fn apply(&self, kind: &str, id: &str, body: Option<&Value>) {
        let id = id.to_string();
        let r: Result<(), EngineError> = match (kind, body) {
            (OVERLAY, Some(b)) => match serde_json::from_value::<Overlay>(b.clone()) {
                Ok(o) => match self.db(move |s| repo::overlays::put_synced(s, o)).await {
                    // Pages open on this PC's URL show the new layout too.
                    Ok(saved) => {
                        self.push_config(&saved);
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                Err(_) => Ok(()),
            },
            (OVERLAY, None) => self.db(move |s| repo::overlays::delete(s, &id).or(Ok(()))).await,
            (RULE, Some(b)) => match serde_json::from_value::<AlertRule>(b.clone()) {
                Ok(r) => self.db(move |s| repo::rules::save(s, &r)).await,
                Err(_) => Ok(()),
            },
            (RULE, None) => self.db(move |s| repo::rules::delete(s, &id).or(Ok(()))).await,
            (DESIGN, Some(b)) => match serde_json::from_value::<Design>(b.clone()) {
                Ok(d) if !d.built_in && !d.branded => {
                    self.db(move |s| match repo::designs::get(s, &d.id) {
                        Ok(_) => repo::designs::update(s, &d).map(|_| ()),
                        Err(_) => repo::designs::insert(s, &d).map(|_| ()),
                    })
                    .await
                }
                _ => Ok(()),
            },
            (DESIGN, None) => self.db(move |s| repo::designs::delete(s, &id).or(Ok(()))).await,
            (SETTINGS, Some(b)) => {
                let active = b["active_alert_design"].as_str().map(String::from);
                self.db(move |s| {
                    // Only a design this PC has (built-in or synced) can be the one in use.
                    let ok =
                        active.as_deref().is_none_or(|a| a.starts_with("builtin.") || repo::designs::get(s, a).is_ok());
                    if ok { repo::designs::set_active_alert(s, active.as_deref()) } else { Ok(()) }
                })
                .await
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            tracing::warn!(error = %e, kind, "couldn't apply a synced document");
        }
        if kind == RULE
            && let Ok(rules) = self.db(repo::rules::list).await
        {
            self.state_mut().rules = rules;
        }
        if kind == SETTINGS || kind == DESIGN {
            self.refresh_active_design().await;
        }
    }
}
