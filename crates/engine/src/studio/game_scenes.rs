//! A scene per game (plan item 9, 29 Oct): when an app on the scene switcher's list comes to
//! the front, its scene goes on air in the Studio. Only when it comes to the front: a scene
//! picked by hand while the game stays in front stays, and leaving the game changes nothing.

use std::time::Duration;

use ferrocast_core::show::GameScene;

use crate::Engine;
use crate::studio::rig::Rig;

/// The app last seen in front.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    front: Option<String>,
}

impl Watch {
    /// The scene to put on air now `front` is in front: only when it just came to the front
    /// and is on the list (names match in any case).
    pub(crate) fn step(&mut self, front: Option<&str>, list: &[GameScene]) -> Option<String> {
        let came = front.is_some() && front != self.front.as_deref();
        self.front = front.map(str::to_string);
        if !came {
            return None;
        }
        let front = front?.trim();
        list.iter().find(|g| g.app.trim().eq_ignore_ascii_case(front)).map(|g| g.scene.clone())
    }
}

pub(crate) async fn run(engine: Engine) {
    let mut watch = Watch::default();
    loop {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let cfg = engine.scenes_config();
        if cfg.game_scenes.is_empty() {
            watch = Watch::default();
            continue;
        }
        let Ok(Rig::Studio) = engine.rig().await else { continue };
        let Ok(apps) = engine.studio_apps(false).await else { continue };
        let Some(scene) = watch.step(apps["front"].as_str(), &cfg.game_scenes) else { continue };
        // Never over a break: BRB stays until you come back.
        let on_air = engine.rig_scenes(&Rig::Studio).await.ok().and_then(|(c, _)| c);
        if on_air.as_deref() == Some(scene.as_str()) || (on_air.is_some() && on_air == cfg.brb_scene) {
            continue;
        }
        match engine.rig_switch(&Rig::Studio, &scene).await {
            Ok(()) => tracing::info!(%scene, "game in front: its scene is on"),
            Err(err) => tracing::warn!(%scene, %err, "game in front: its scene didn't go on"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Vec<GameScene> {
        vec![GameScene { app: "Minecraft".into(), scene: "In game".into() }]
    }

    #[test]
    fn a_game_coming_to_the_front_picks_its_scene_once() {
        let mut w = Watch::default();
        assert_eq!(w.step(Some("Safari"), &list()), None);
        assert_eq!(w.step(Some("minecraft"), &list()).as_deref(), Some("In game"));
        // Still in front: no switch again (a scene picked by hand stays).
        assert_eq!(w.step(Some("minecraft"), &list()), None);
        // Away and back: picks it again.
        assert_eq!(w.step(Some("Ferrocast"), &list()), None);
        assert_eq!(w.step(Some("Minecraft"), &list()).as_deref(), Some("In game"));
    }

    #[test]
    fn nothing_in_front_or_not_listed_changes_nothing() {
        let mut w = Watch::default();
        assert_eq!(w.step(None, &list()), None);
        assert_eq!(w.step(Some("Mine"), &list()), None);
        assert_eq!(w.step(Some("Minecraft"), &[]), None);
    }
}
