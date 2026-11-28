//! Creator profile links ("My links") and resolved overlay state.

use ferrocast_core::links::{ConnectedAccountInfo, CreatorLinks, ResolvedLinks, patch_creator_links, resolve_links};
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::EngineError;
use crate::frames::{Audience, LiveFrame, OverlayFrame};
use crate::{Engine, now};

const KEY: &str = "creator_links";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LinksView {
    #[serde(flatten)]
    pub links: CreatorLinks,
    pub resolved: ResolvedLinks,
}

impl Engine {
    async fn load_creator_links(&self) -> Result<CreatorLinks, EngineError> {
        self.db(|s| repo::settings::get_or_default::<CreatorLinks>(s, KEY)).await
    }

    async fn load_creator_links_or_empty(&self) -> CreatorLinks {
        match self.load_creator_links().await {
            Ok(links) => links,
            Err(e) => {
                tracing::warn!(error = %e, key = KEY, "corrupt or unreadable creator_links setting; falling back to empty links");
                CreatorLinks::default()
            }
        }
    }

    /// Returns saved "My links" overrides along with the currently resolved overlay values.
    pub async fn links(&self) -> Result<LinksView, EngineError> {
        self.links_at(now().0).await
    }

    pub(crate) async fn links_at(&self, at_ms: i64) -> Result<LinksView, EngineError> {
        let links = self.load_creator_links_or_empty().await;
        let resolved = self.resolve_creator_links_at(&links, at_ms).await?;
        Ok(LinksView { links, resolved })
    }

    /// Resolves effective overlay links from saved overrides, connected accounts, and schedule.
    pub async fn resolved_links(&self) -> Result<ResolvedLinks, EngineError> {
        let links = self.load_creator_links_or_empty().await;
        self.resolve_creator_links_at(&links, now().0).await
    }

    async fn resolve_creator_links_at(&self, links: &CreatorLinks, at_ms: i64) -> Result<ResolvedLinks, EngineError> {
        let accounts = self.db(repo::settings::accounts).await?;
        let scheduled = self.schedule_list().await?;
        let next_stream_at = scheduled.into_iter().find(|s| s.start_ms >= at_ms).map(|s| s.start_ms);
        let infos: Vec<ConnectedAccountInfo<'_>> = accounts
            .iter()
            .map(|a| ConnectedAccountInfo {
                platform: &a.platform,
                display_name: &a.display_name,
                login: &a.login,
                user_id: &a.user_id,
            })
            .collect();
        Ok(resolve_links(links, &infos, next_stream_at))
    }

    fn broadcast_links(&self, view: &LinksView) {
        *self.inner.last_pushed_next_stream_at.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(view.resolved.next_stream_at);
        let seq = self.next_seq();
        self.send_overlay(OverlayFrame::new(Audience::All, seq, "state", &json!({ "links": view.resolved })));
        self.send_live(LiveFrame::new("links", view));
    }

    /// Validates, saves, and broadcasts updated links to connected overlays and dashboards.
    pub async fn save_links(&self, incoming: CreatorLinks) -> Result<LinksView, EngineError> {
        let links = incoming.validated().map_err(EngineError::Invalid)?;
        let _w = self.config_lock().await;
        let to_save = links.clone();
        self.db(move |s| repo::settings::set(s, KEY, &to_save)).await?;
        let resolved = self.resolve_creator_links_at(&links, now().0).await?;
        let view = LinksView { links, resolved };
        self.broadcast_links(&view);
        Ok(view)
    }

    /// Applies a partial JSON update to saved "My links", preserving omitted and unknown future keys.
    pub async fn patch_links(&self, patch: &Map<String, Value>) -> Result<LinksView, EngineError> {
        let _w = self.config_lock().await;
        let existing = match self.load_creator_links().await {
            Ok(links) => links,
            Err(err) => {
                let patch_keys: Vec<&str> = patch.keys().map(String::as_str).collect();
                tracing::error!(
                    error = %err,
                    key = KEY,
                    ?patch_keys,
                    "couldn't read stored creator_links before applying patch; refusing to overwrite"
                );
                let (message, hint) = match &err {
                    EngineError::Store(ferrocast_store::StoreError::Json(_)) => (
                        "Saved \"My links\" data couldn't be read, so this change wasn't saved over it.".to_string(),
                        "Restore a backup in Settings or fix the stored links before saving partial updates."
                            .to_string(),
                    ),
                    _ => (
                        "Ferrocast couldn't read your saved \"My links\", so this change wasn't saved over them."
                            .to_string(),
                        "Check there's free disk space and the database isn't busy, then try again.".to_string(),
                    ),
                };
                return Err(EngineError::Refused { status: 500, code: "storage", message, hint, retry_after: None });
            }
        };
        let links = patch_creator_links(&existing, patch).map_err(EngineError::Invalid)?;
        let to_save = links.clone();
        self.db(move |s| repo::settings::set(s, KEY, &to_save)).await?;
        let resolved = self.resolve_creator_links_at(&links, now().0).await?;
        let view = LinksView { links, resolved };
        self.broadcast_links(&view);
        Ok(view)
    }

    /// Broadcasts current resolved links when `config_lock` is already held by the caller.
    pub(crate) async fn push_overlay_links_locked(&self) {
        self.push_overlay_links_at_locked(now().0).await;
    }

    pub(crate) async fn push_overlay_links_at_locked(&self, at_ms: i64) {
        match self.links_at(at_ms).await {
            Ok(view) => self.broadcast_links(&view),
            Err(e) => tracing::warn!(%e, "couldn't push updated overlay links"),
        }
    }

    /// Acquires `config_lock` and broadcasts updated links (serializes against concurrent `save_links`).
    pub(crate) async fn push_overlay_links(&self) {
        let _w = self.config_lock().await;
        self.push_overlay_links_locked().await;
    }
}
