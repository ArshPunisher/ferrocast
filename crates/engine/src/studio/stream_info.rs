//! The stream-info editor: one form pushed to Twitch, YouTube and Kick at once (API.md §4
//! "Studio: stream info"). Each platform answers for itself, so one refusing never stops
//! the others; the dashboard shows each answer with what to do.

use std::time::Duration;

use ferrocast_connector::{CategoryHit, ConnectorError};
use ferrocast_core::event::Platform;
use ferrocast_core::stream_info::{
    self, HistoryEntry, MAX_PRESETS, PlatformError, PlatformResult, Preset, Saved, StreamInfo,
};
use ferrocast_store::repo;
use serde::Serialize;

use super::{STREAM_INFO_KEY, save_setting};
use crate::{Engine, EngineError, now, platform_key};

/// A platform gets this long to answer before it's reported as unreachable.
const PLATFORM_DEADLINE: Duration = Duration::from_secs(15);

/// One platform's line in the editor: can Ferrocast change its stream info right now?
#[derive(Debug, Clone, Serialize)]
pub struct PlatformAccess {
    pub platform: Platform,
    pub connected: bool,
    /// Signed in with the permission the editor needs.
    pub can_edit: bool,
    /// What to do when it can't ("Reconnect Twitch to allow changing your stream info.").
    pub needs: Option<String>,
    pub supports: stream_info::Supports,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamInfoView {
    pub last: Option<HistoryEntry>,
    pub presets: Vec<Preset>,
    pub history: Vec<HistoryEntry>,
    pub platforms: Vec<PlatformAccess>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyResult {
    pub results: Vec<PlatformResult>,
    pub ok: usize,
    pub failed: usize,
}

const PRESET_NOT_FOUND: EngineError = EngineError::Missing {
    code: "preset_not_found",
    message: "That preset doesn't exist.",
    hint: "Reload the page; it may have been deleted.",
};

impl Engine {
    pub(crate) async fn stream_info_saved(&self) -> Result<Saved, EngineError> {
        self.db(|s| repo::settings::get_or_default::<Saved>(s, STREAM_INFO_KEY)).await
    }

    /// Who can be changed, and what each needs.
    pub async fn stream_info_access(&self) -> Result<Vec<PlatformAccess>, EngineError> {
        let accounts = self.db(repo::settings::accounts).await?;
        Ok(Platform::CHAT
            .iter()
            .map(|&p| {
                let account = accounts.iter().find(|a| a.platform == platform_key(p));
                let connected = self.connector(p).is_some();
                let scope = stream_info::required_scope(p).unwrap_or("");
                // A sign-in from before the editor existed lacks the scope (Twitch records the
                // real list; Kick records what was asked for at the time).
                let has_scope = account.is_none_or(|a| a.scopes.is_empty() || a.scopes.iter().any(|s| s == scope));
                let needs = if !connected {
                    Some(format!("Connect {} in Settings, Accounts.", p.label()))
                } else if !has_scope {
                    Some(format!(
                        "Reconnect {} in Settings, Accounts to let Ferrocast change your stream info (it needs the \"{scope}\" permission).",
                        p.label()
                    ))
                } else {
                    None
                };
                PlatformAccess {
                    platform: p,
                    connected,
                    can_edit: connected && has_scope,
                    needs,
                    supports: stream_info::supports(p),
                }
            })
            .collect())
    }

    pub async fn stream_info(&self) -> Result<StreamInfoView, EngineError> {
        let saved = self.stream_info_saved().await?;
        Ok(StreamInfoView {
            last: saved.history.first().cloned(),
            presets: saved.presets,
            history: saved.history,
            platforms: self.stream_info_access().await?,
        })
    }

    /// Pushes `info` to `platforms` (all connected ones when `None`), at the same time.
    /// 400 for a bad field; 422 when none of them is connected. A platform refusing is in
    /// its own result, never a failure of the whole call.
    pub async fn apply_stream_info(
        &self,
        info: StreamInfo,
        platforms: Option<Vec<Platform>>,
        by: Option<&str>,
    ) -> Result<ApplyResult, EngineError> {
        let info = info.normalised().map_err(EngineError::Invalid)?;
        let targets: Vec<Platform> = match platforms {
            Some(list) if !list.is_empty() => {
                let mut l: Vec<Platform> = Vec::new();
                for p in list {
                    if !l.contains(&p) {
                        l.push(p);
                    }
                }
                if let Some(bad) = l.iter().find(|p| !Platform::CHAT.contains(p)) {
                    return Err(EngineError::Invalid(format!("platforms: {bad} has no stream info.")));
                }
                l
            }
            _ => Platform::CHAT.iter().copied().filter(|p| self.connector(*p).is_some()).collect(),
        };
        if targets.is_empty() {
            return Err(EngineError::Refused {
                status: 422,
                code: "no_platforms",
                message: "No platform is connected, so there's nowhere to send this.".into(),
                hint: "Connect Twitch, YouTube or Kick in Settings, Accounts.".into(),
                retry_after: None,
            });
        }
        // Each platform gets the everywhere fields with its own on top. The thumbnail is read
        // from the media library only for platforms that take one.
        let mut jobs = Vec::new();
        for &p in &targets {
            let info = info.for_platform(p);
            let thumb = match &info.thumbnail {
                Some(id) if stream_info::supports(p).thumbnail => Some(self.thumbnail_image(id).await),
                _ => None,
            };
            let c = self.connector(p);
            jobs.push(async move { (p, push_one(p, c, &info, thumb).await) });
        }
        let results: Vec<PlatformResult> =
            futures_util::future::join_all(jobs).await.into_iter().map(|(_, r)| r).collect();
        let ok = results.iter().filter(|r| r.ok).count();
        let failed = results.len() - ok;
        let entry = HistoryEntry { info: info.clone(), at: now().0, results: results.clone() };
        {
            let _one_at_a_time = self.config_lock().await;
            let mut saved = self.stream_info_saved().await?;
            saved.remember(entry);
            save_setting(self, STREAM_INFO_KEY, &saved).await?;
        }
        if let Some(by) = by {
            tracing::info!(by, ok, failed, "stream info changed from chat");
        }
        Ok(ApplyResult { results, ok, failed })
    }

    /// Categories for the form: the platform's own search (YouTube: its video categories).
    pub async fn search_categories(&self, platform: Platform, query: &str) -> Result<Vec<CategoryHit>, EngineError> {
        let q = query.trim();
        if q.is_empty() || q.chars().count() > 100 {
            return Err(EngineError::Invalid("q: type 1 to 100 characters to search.".into()));
        }
        let c = self.connector(platform).ok_or(ConnectorError::NotConnected(platform))?;
        match tokio::time::timeout(PLATFORM_DEADLINE, c.search_categories(q)).await {
            Ok(r) => Ok(r?),
            Err(_) => Err(ConnectorError::Network { platform, message: "it took too long to answer".into() }.into()),
        }
    }

    pub async fn save_preset(&self, mut p: Preset, create: bool) -> Result<Preset, EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut saved = self.stream_info_saved().await?;
        if create {
            if saved.presets.len() >= MAX_PRESETS {
                return Err(EngineError::Conflict(format!("You can save up to {MAX_PRESETS} presets.")));
            }
            p.id = ferrocast_store::new_id();
        } else if !saved.presets.iter().any(|x| x.id == p.id) {
            return Err(PRESET_NOT_FOUND);
        }
        p.updated_at = now().0;
        let p = p.validate(&saved.presets).map_err(|m| {
            if m.contains("already exists") { EngineError::Conflict(m) } else { EngineError::Invalid(m) }
        })?;
        match saved.presets.iter_mut().find(|x| x.id == p.id) {
            Some(slot) => *slot = p.clone(),
            None => saved.presets.push(p.clone()),
        }
        save_setting(self, STREAM_INFO_KEY, &saved).await?;
        Ok(p)
    }

    pub async fn delete_preset(&self, id: &str) -> Result<(), EngineError> {
        let _one_at_a_time = self.config_lock().await;
        let mut saved = self.stream_info_saved().await?;
        let before = saved.presets.len();
        saved.presets.retain(|p| p.id != id);
        if saved.presets.len() == before {
            return Err(PRESET_NOT_FOUND);
        }
        save_setting(self, STREAM_INFO_KEY, &saved).await
    }

    /// A media-library picture as `(bytes, mime)` for a thumbnail, or why it can't be one.
    async fn thumbnail_image(&self, id: &str) -> Result<(Vec<u8>, String), String> {
        let lookup = id.to_string();
        let found = self.db(move |s| repo::media::get(s, &lookup)).await.map_err(|e| e.to_string())?;
        let Some((media, path)) = found else {
            return Err("that picture isn't in your media library any more".into());
        };
        if !matches!(media.mime.as_str(), "image/jpeg" | "image/png") {
            return Err("thumbnails must be a JPEG or PNG picture".into());
        }
        let bytes = tokio::fs::read(&path).await.map_err(|_| "the picture's file couldn't be read".to_string())?;
        Ok((bytes, media.mime))
    }

    pub async fn preset(&self, id: &str) -> Result<Preset, EngineError> {
        self.stream_info_saved().await?.presets.into_iter().find(|p| p.id == id).ok_or(PRESET_NOT_FOUND)
    }
}

/// One platform's push, turned into a result the dashboard can show.
async fn push_one(
    p: Platform,
    c: Option<std::sync::Arc<dyn ferrocast_connector::Connector>>,
    info: &StreamInfo,
    thumb: Option<Result<(Vec<u8>, String), String>>,
) -> PlatformResult {
    let sup = stream_info::supports(p);
    let mut skipped = Vec::new();
    let mut info = info.clone();
    if info.language.is_some() && !sup.language {
        skipped.push(format!("language: {} has no stream language setting", p.label()));
        info.language = None;
    }
    if info.description.is_some() && !sup.description {
        skipped.push(format!("description: {} has no stream description", p.label()));
        info.description = None;
    }
    if info.thumbnail.is_some() && !sup.thumbnail {
        skipped.push(format!("thumbnail: {} makes its own from your stream", p.label()));
    }
    let thumb = if sup.thumbnail { thumb } else { None };
    info.thumbnail = None;
    if p == Platform::Youtube && info.category.is_some() {
        skipped.push("category: YouTube only takes a video category (a game becomes Gaming)".into());
    }
    let fail = |code: &str, message: String, hint: Option<&str>, skipped: Vec<String>| PlatformResult {
        platform: p,
        ok: false,
        applied: vec![],
        skipped,
        error: Some(PlatformError { code: code.into(), message, hint: hint.map(String::from) }),
    };
    let Some(c) = c else {
        return fail(
            "not_connected",
            format!("{} isn't connected.", p.label()),
            Some("Connect it in Settings, Accounts."),
            skipped,
        );
    };
    // Nothing this platform takes (only YouTube's description or thumbnail were set).
    let nothing = info.title.is_none()
        && info.category.is_none()
        && info.tags.is_none()
        && info.language.is_none()
        && info.description.is_none();
    let pushed = if nothing {
        Ok(Ok(Vec::new()))
    } else {
        tokio::time::timeout(PLATFORM_DEADLINE, c.update_stream_info(&info)).await
    };
    match pushed {
        Err(_) => fail(
            "unreachable",
            format!("{} took too long to answer.", p.label()),
            Some("Check your connection and try again."),
            skipped,
        ),
        Ok(Ok(mut applied)) => {
            if let Some(t) = thumb {
                let r = match t {
                    Err(why) => Err(why),
                    Ok((bytes, mime)) => {
                        match tokio::time::timeout(PLATFORM_DEADLINE, c.set_thumbnail(&bytes, &mime)).await {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(e)) => Err(match e {
                                ConnectorError::Refused { message, .. } => message,
                                other => other.to_string(),
                            }),
                            Err(_) => Err(format!("{} took too long to take the picture", p.label())),
                        }
                    }
                };
                match r {
                    Ok(()) => applied.push("thumbnail".into()),
                    Err(why) if applied.is_empty() => {
                        return fail(
                            "refused",
                            sentence(&format!("Thumbnail: {why}")),
                            Some("Pick a JPEG or PNG of up to 2 MB, then try again."),
                            skipped,
                        );
                    }
                    Err(why) => skipped.push(format!("thumbnail: {why}")),
                }
            }
            // With nothing this platform takes, `applied` is empty and `skipped` says why.
            PlatformResult { platform: p, ok: true, applied, skipped, error: None }
        }
        Ok(Err(e)) => {
            let (code, hint) = match &e {
                ConnectorError::MissingScope { .. } | ConnectorError::SignInNeeded(_) => {
                    ("reconnect_needed", "Reconnect it in Settings, Accounts, then try again.")
                }
                ConnectorError::RateLimited { .. } => ("rate_limited", "Wait a minute, then try again."),
                ConnectorError::Network { .. } => ("unreachable", "Check your connection and try again."),
                ConnectorError::NotConnected(_) => ("not_connected", "Connect it in Settings, Accounts."),
                ConnectorError::NotSupported { .. } => ("not_supported", "Change it on the platform's own site."),
                ConnectorError::Refused { message, .. } if message.contains("no category called") => {
                    ("unknown_category", "Pick the category from the search list.")
                }
                ConnectorError::Refused { message, .. } if message.contains("live or scheduled") => {
                    ("no_broadcast", "Start or schedule the stream in YouTube Studio, then try again.")
                }
                ConnectorError::Refused { .. } => ("refused", "Check the text, then try again."),
            };
            let message = match &e {
                ConnectorError::MissingScope { scope, .. } => {
                    format!("{} needs a permission it didn't get when you connected (\"{scope}\").", p.label())
                }
                other => sentence(&other.to_string()),
            };
            fail(code, message, Some(hint), skipped)
        }
    }
}

/// "twitch refused: x" → "Twitch refused: x."
fn sentence(s: &str) -> String {
    let mut out: String = s.chars().take(300).collect();
    if let Some(first) = out.get(0..1) {
        out = format!("{}{}", first.to_uppercase(), &out[1..]);
    }
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}
