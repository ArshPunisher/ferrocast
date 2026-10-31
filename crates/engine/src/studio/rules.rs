//! "When X, do Y" (Part 1 of the engine plan, 31 Oct): a live stream event puts a scene on air,
//! or shows a source in the scene on air, for a while, then puts it back. In the Studio while it
//! runs, else in OBS (the rig). A rule already running lets new events pass until it's done, so
//! a burst of follows doesn't flicker the scenes.

use std::time::Duration;

use ferrocast_core::event::{Event, Origin};
use ferrocast_core::show::EventRule;

use crate::Engine;

impl Engine {
    /// Every event, after the pipeline handled it: the rules it sets off start.
    pub(crate) async fn rules_saw(&self, e: &Event) {
        if e.origin != Origin::Live {
            return;
        }
        let rules = self.scenes_config().event_rules;
        for (i, rule) in rules.into_iter().enumerate() {
            if !rule.fires(&e.kind) {
                continue;
            }
            if !self.studio().rules_busy.lock().unwrap_or_else(|p| p.into_inner()).insert(i) {
                continue;
            }
            let engine = self.clone();
            tokio::spawn(async move {
                if let Err(err) = engine.run_rule(&rule).await {
                    tracing::warn!(%err, "a rule didn't run");
                }
                engine.studio().rules_busy.lock().unwrap_or_else(|p| p.into_inner()).remove(&i);
            });
        }
    }

    async fn run_rule(&self, rule: &EventRule) -> Result<(), crate::EngineError> {
        let rig = self.rig().await?;
        let wait = Duration::from_secs(u64::from(rule.seconds));
        let (on_air, _) = self.rig_scenes(&rig).await?;
        if let Some(scene) = &rule.scene {
            if on_air.as_deref() == Some(scene.as_str()) {
                return Ok(());
            }
            self.rig_switch(&rig, scene).await?;
            tracing::info!(%scene, "a rule put its scene on");
            tokio::time::sleep(wait).await;
            // Back, unless something else changed the scene meanwhile.
            let (now, _) = self.rig_scenes(&rig).await?;
            if let (Some(back), true) = (on_air, now.as_deref() == Some(scene.as_str())) {
                self.rig_switch(&rig, &back).await?;
            }
        } else if let (Some(source), Some(scene)) = (&rule.source, on_air) {
            if self.rig_visible(&rig, &scene, source).await? != Some(false) {
                // Not in this scene, or already showing: nothing to do.
                return Ok(());
            }
            self.rig_set_visible(&rig, &scene, source, true).await?;
            tracing::info!(%source, %scene, "a rule showed its source");
            tokio::time::sleep(wait).await;
            self.rig_set_visible(&rig, &scene, source, false).await?;
        }
        Ok(())
    }
}
