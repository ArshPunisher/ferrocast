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
    async fn load_creator_links_or_empty(&self) -> Result<CreatorLinks, EngineError> {
        match self.db(|s| repo::settings::get_or_default::<CreatorLinks>(s, KEY)).await {
            Ok(links) => Ok(links),
            Err(e) => {
                tracing::warn!(error = %e, key = KEY, "corrupt or unreadable creator_links setting; falling back to empty links");
                Ok(CreatorLinks::default())
            }
        }
    }

    /// Returns saved "My links" overrides along with the currently resolved overlay values.
    pub async fn links(&self) -> Result<LinksView, EngineError> {
        let links = self.load_creator_links_or_empty().await?;
        let resolved = self.resolve_creator_links(&links).await?;
        Ok(LinksView { links, resolved })
    }

    /// Resolves effective overlay links from saved overrides, connected accounts, and schedule.
    pub async fn resolved_links(&self) -> Result<ResolvedLinks, EngineError> {
        let links = self.load_creator_links_or_empty().await?;
        self.resolve_creator_links(&links).await
    }

    async fn resolve_creator_links(&self, links: &CreatorLinks) -> Result<ResolvedLinks, EngineError> {
        let accounts = self.db(repo::settings::accounts).await?;
        let scheduled = self.schedule_list().await?;
        let now_ms = now().0;
        let next_stream_at = scheduled.into_iter().find(|s| s.start_ms >= now_ms).map(|s| s.start_ms);
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

    /// Validates, saves, and broadcasts updated links to connected overlays and dashboards.
    pub async fn save_links(&self, incoming: CreatorLinks) -> Result<LinksView, EngineError> {
        let links = incoming.validated().map_err(EngineError::Invalid)?;
        let _w = self.config_lock().await;
        let to_save = links.clone();
        self.db(move |s| repo::settings::set(s, KEY, &to_save)).await?;
        let resolved = self.resolve_creator_links(&links).await?;
        let view = LinksView { links, resolved };
        let seq = self.next_seq();
        self.send_overlay(OverlayFrame::new(Audience::All, seq, "state", &json!({ "links": view.resolved })));
        self.send_live(LiveFrame::new("links", &view));
        Ok(view)
    }

    /// Applies a partial JSON update to saved "My links", preserving omitted and unknown future keys.
    pub async fn patch_links(&self, patch: &Map<String, Value>) -> Result<LinksView, EngineError> {
        let _w = self.config_lock().await;
        let existing = self.load_creator_links_or_empty().await?;
        let links = patch_creator_links(&existing, patch).map_err(EngineError::Invalid)?;
        let to_save = links.clone();
        self.db(move |s| repo::settings::set(s, KEY, &to_save)).await?;
        let resolved = self.resolve_creator_links(&links).await?;
        let view = LinksView { links, resolved };
        let seq = self.next_seq();
        self.send_overlay(OverlayFrame::new(Audience::All, seq, "state", &json!({ "links": view.resolved })));
        self.send_live(LiveFrame::new("links", &view));
        Ok(view)
    }

    /// Broadcasts current resolved links when `config_lock` is already held by the caller.
    pub(crate) async fn push_overlay_links_locked(&self) {
        match self.links().await {
            Ok(view) => {
                let seq = self.next_seq();
                self.send_overlay(OverlayFrame::new(Audience::All, seq, "state", &json!({ "links": view.resolved })));
                self.send_live(LiveFrame::new("links", &view));
            }
            Err(e) => tracing::warn!(%e, "couldn't push updated overlay links"),
        }
    }

    /// Acquires `config_lock` and broadcasts updated links (serializes against concurrent `save_links`).
    pub(crate) async fn push_overlay_links(&self) {
        let _w = self.config_lock().await;
        self.push_overlay_links_locked().await;
    }
}
