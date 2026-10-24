//! A whole theme into OBS in one click (API.md §4 "Theme in OBS"; the 24 Oct board): each of
//! the theme's screens becomes an overlay and an OBS scene with it on top, the streamer's own
//! scene nested under it, Start show's scenes set, and old StreamElements / Streamlabs widgets
//! hidden. Everything Ferrocast adds is written down, so Undo takes away exactly that and
//! nothing the streamer made.

use ferrocast_core::design::{DesignContent, Scene};
use ferrocast_obs::control::Scenes;
use ferrocast_obs::theme::{CAMERA_KINDS, CAPTURE_KINDS, MIC_KINDS, OldWidget};
use ferrocast_store::repo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::studio::obs_error;
use crate::studio::show::Step;
use crate::{Engine, EngineError, now};

const KEY: &str = "obs_theme";
const MAX_PREFIX: usize = 40;

/// Only one apply or undo at a time (they read OBS, then change it).
static BUSY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What Start show (and the BRB button) uses a screen for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Starting,
    Live,
    Brb,
    Ending,
}

/// Where a screen goes in OBS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Place {
    /// A new scene, named the prefix and the screen's name.
    New,
    /// On top of a scene the streamer has; nothing else in it changes.
    OnTop { scene: String },
    /// On top of a scene the streamer has, hiding its old browser and image sources.
    Replace { scene: String },
    /// Left out.
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenPlan {
    /// The theme's scene key (`in_game`, `brb`…).
    pub key: String,
    pub place: Place,
    /// A scene or source the streamer has, put at the bottom (their game and camera).
    #[serde(default)]
    pub under: Option<String>,
    #[serde(default)]
    pub role: Option<Role>,
}

fn yes() -> bool {
    true
}

/// What to do; `GET` gives Ferrocast's guesses, which the Advanced table edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemePlan {
    /// New scenes are named this plus the screen's name ("Forge · In game").
    pub prefix: String,
    pub screens: Vec<ScreenPlan>,
    /// Switch OBS to the Starting screen when done.
    #[serde(default = "yes")]
    pub switch_to_starting: bool,
    /// Old widgets (source names) to hide, not delete.
    #[serde(default)]
    pub hide_old: Vec<String>,
    /// Also play alerts with the theme's alert design.
    #[serde(default = "yes")]
    pub alert_style: bool,
    /// Your mic on the Starting, BRB and Ending screens (off: most want them quiet).
    #[serde(default)]
    pub mic_on_breaks: bool,
}

/// A theme screen, for the dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScreenInfo {
    pub key: String,
    pub name: String,
}

/// An old widget, for the dialog: its name, service and scenes (never its address).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OldWidgetView {
    pub source: String,
    pub vendor: String,
    pub scenes: Vec<String>,
}

/// `GET /designs/{id}/obs`: Ferrocast's guesses and what OBS has.
#[derive(Debug, Clone, Serialize)]
pub struct ThemeObsPlan {
    pub plan: ThemePlan,
    pub screens: Vec<ScreenInfo>,
    /// OBS's scenes, top first.
    pub obs_scenes: Vec<String>,
    /// Cameras OBS has (a screen can hold one instead of a scene).
    pub obs_cameras: Vec<String>,
    pub old_widgets: Vec<OldWidgetView>,
    /// The theme in OBS now, if any.
    pub applied: Option<ObsThemeView>,
}

/// One screen as it went into OBS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedScreen {
    pub key: String,
    pub name: String,
    pub scene: String,
    pub overlay_id: String,
    pub role: Option<Role>,
}

/// Start show's and the BRB button's scenes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleScenes {
    pub starting: Option<String>,
    pub live: Option<String>,
    pub brb: Option<String>,
    pub ending: Option<String>,
}

/// What Ferrocast added to OBS, so Undo takes exactly that away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObsTheme {
    pub design_id: String,
    pub design_name: String,
    pub at: i64,
    pub screens: Vec<AppliedScreen>,
    pub made_scenes: Vec<String>,
    /// Browser sources made (removed from OBS on Undo).
    pub made_inputs: Vec<String>,
    /// (scene, source) put into a scene that was already there.
    pub added_items: Vec<(String, String)>,
    /// (scene, item id) hidden.
    pub hidden: Vec<(String, i64)>,
    pub roles_before: RoleScenes,
    pub roles_set: RoleScenes,
    /// The theme's alert look was put in; `alert_before` was in use before it.
    #[serde(default)]
    pub alert_changed: bool,
    #[serde(default)]
    pub alert_before: Option<String>,
    /// The scene on air before, for Undo to go back to.
    #[serde(default)]
    pub scene_before: Option<String>,
    /// (scene, mic) Ferrocast added for "Mic on break screens", so turning it off takes them out.
    #[serde(default)]
    pub mic_items: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObsThemeScreen {
    pub key: String,
    pub name: String,
    pub scene: String,
    pub role: Option<Role>,
    /// Still in OBS (`None` when OBS can't be asked).
    pub found: Option<bool>,
}

/// The "Your theme in OBS" card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObsThemeView {
    pub design_id: String,
    pub design_name: String,
    pub at: i64,
    pub screens: Vec<ObsThemeScreen>,
}

/// What apply or undo did, step by step.
#[derive(Debug, Clone, Serialize)]
pub struct ThemeObsResult {
    pub steps: Vec<Step>,
    pub applied: Option<ObsThemeView>,
}

fn role_of(key: &str) -> Option<Role> {
    match key {
        "starting_soon" => Some(Role::Starting),
        "in_game" => Some(Role::Live),
        "brb" => Some(Role::Brb),
        "ending" => Some(Role::Ending),
        _ => None,
    }
}

fn has(text: &str, words: &[&str]) -> bool {
    let t = text.to_lowercase();
    words.iter().any(|w| t.contains(w))
}

/// A scene that's a break screen, not the one with the game.
fn breaky(name: &str) -> bool {
    has(name, &["start", "brb", "be right", "ending", "end screen", "offline", "intermission", "pause", "outro"])
}

/// Ferrocast's guesses: every screen a new scene; the scene with the game under In game; a
/// chat (or camera) scene under Just chatting; roles from the screen; old widgets hidden.
pub fn guess(theme_name: &str, screens: &[Scene], obs: &Scenes, old: &[OldWidget], skip: &[String]) -> ThemePlan {
    // "Forge (mine)" and "Forge copy" name their scenes like Forge: switching to your copy
    // keeps the scene names.
    let base = theme_name.trim();
    let base = [" (mine)", " copy"].iter().find_map(|x| base.strip_suffix(x)).unwrap_or(base);
    let prefix = format!("{} · ", base.chars().take(MAX_PREFIX - 3).collect::<String>().trim_end());
    let own: Vec<&ferrocast_obs::control::Scene> =
        obs.scenes.iter().filter(|s| !s.name.starts_with(&prefix) && !skip.contains(&s.name)).collect();
    let captures = |s: &ferrocast_obs::control::Scene, on: bool| {
        s.items.iter().any(|i| (i.enabled || !on) && i.kind.as_deref().is_some_and(|k| CAPTURE_KINDS.contains(&k)))
    };
    let camera = |s: &ferrocast_obs::control::Scene| {
        s.items.iter().any(|i| i.kind.as_deref().is_some_and(|k| CAMERA_KINDS.contains(&k)))
    };
    // The game scene: its name says so, it captures the screen (even switched off for now) and
    // has a camera; never a break or recording-only scene.
    let score = |s: &ferrocast_obs::control::Scene| {
        if breaky(&s.name) || has(&s.name, &["record"]) {
            return 0;
        }
        4 * i32::from(has(&s.name, &["game", "main", "live", "play", "stream"]))
            + 3 * i32::from(captures(s, false))
            + 2 * i32::from(camera(s))
            + i32::from(captures(s, true))
            + i32::from(obs.current.as_deref() == Some(s.name.as_str()))
    };
    let game = own
        .iter()
        .filter(|s| score(s) > 0)
        .max_by_key(|s| (score(s), std::cmp::Reverse(own.iter().position(|x| x.name == s.name))))
        .or_else(|| own.iter().find(|s| !breaky(&s.name) && !has(&s.name, &["record"])))
        .map(|s| s.name.clone());
    let chat = own
        .iter()
        .find(|s| has(&s.name, &["chat", "talk", "just"]) && Some(&s.name) != game.as_ref())
        .or_else(|| {
            own.iter().find(|s| camera(s) && !captures(s, false) && !breaky(&s.name) && Some(&s.name) != game.as_ref())
        })
        .map(|s| s.name.clone())
        .or_else(|| {
            own.iter()
                .flat_map(|s| &s.items)
                .find(|i| i.kind.as_deref().is_some_and(|k| CAMERA_KINDS.contains(&k)))
                .map(|i| i.source.clone())
        });
    ThemePlan {
        prefix,
        screens: screens
            .iter()
            .map(|s| ScreenPlan {
                key: s.key.clone(),
                place: Place::New,
                under: match s.key.as_str() {
                    "in_game" => game.clone(),
                    "just_chatting" => chat.clone(),
                    _ => None,
                },
                role: role_of(&s.key),
            })
            .collect(),
        switch_to_starting: true,
        hide_old: old.iter().map(|w| w.source.clone()).collect(),
        alert_style: true,
        mic_on_breaks: false,
    }
}

/// The scene a screen goes in.
fn target(plan: &ThemePlan, screen: &Scene, p: &ScreenPlan) -> Option<String> {
    match &p.place {
        Place::New => Some(format!("{}{}", plan.prefix, screen.name.trim())),
        Place::OnTop { scene } | Place::Replace { scene } => Some(scene.clone()),
        Place::Skip => None,
    }
}

/// Checks a plan against the theme and what OBS has. Messages name the field.
pub fn check(plan: &ThemePlan, screens: &[Scene], obs: &Scenes, sources: &[String]) -> Result<(), String> {
    if plan.prefix.chars().count() > MAX_PREFIX || plan.prefix.chars().any(char::is_control) {
        return Err(format!("prefix: up to {MAX_PREFIX} letters, on one line."));
    }
    let mut seen = Vec::new();
    let mut roles = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    for (i, p) in plan.screens.iter().enumerate() {
        let at = format!("screens.{i}");
        let Some(screen) = screens.iter().find(|s| s.key == p.key) else {
            return Err(format!("{at}.key: this theme has no screen \"{}\".", p.key));
        };
        if seen.contains(&p.key) {
            return Err(format!("{at}.key: \"{}\" is listed twice.", p.key));
        }
        seen.push(p.key.clone());
        let Some(scene) = target(plan, screen, p) else {
            if p.role.is_some() || p.under.is_some() {
                return Err(format!("{at}: a screen left out can't have a job or anything under it."));
            }
            continue;
        };
        if scene.trim().is_empty() || scene.chars().count() > 100 {
            return Err(format!("{at}: the scene name must be 1 to 100 letters."));
        }
        let exists = obs.scenes.iter().any(|s| s.name == scene);
        if matches!(p.place, Place::OnTop { .. } | Place::Replace { .. }) && !exists {
            return Err(format!("{at}.place: OBS has no scene called \"{scene}\"."));
        }
        if targets.contains(&scene) {
            return Err(format!("{at}.place: two screens can't go in \"{scene}\"."));
        }
        targets.push(scene.clone());
        if let Some(u) = &p.under {
            if u == &scene {
                return Err(format!("{at}.under: a scene can't go under itself."));
            }
            if !obs.scenes.iter().any(|s| &s.name == u) && !sources.contains(u) {
                return Err(format!("{at}.under: OBS has no scene or source called \"{u}\"."));
            }
        }
        if let Some(r) = p.role {
            if roles.contains(&r) {
                return Err(format!("{at}.role: only one screen can be used for each job."));
            }
            roles.push(r);
        }
    }
    Ok(())
}

impl ObsTheme {
    fn view(&self, obs: Option<&Scenes>) -> ObsThemeView {
        ObsThemeView {
            design_id: self.design_id.clone(),
            design_name: self.design_name.clone(),
            at: self.at,
            screens: self
                .screens
                .iter()
                .map(|s| ObsThemeScreen {
                    key: s.key.clone(),
                    name: s.name.clone(),
                    scene: s.scene.clone(),
                    role: s.role,
                    found: obs.map(|o| o.scenes.iter().any(|x| x.name == s.scene)),
                })
                .collect(),
        }
    }
}

fn not_theme() -> EngineError {
    EngineError::Invalid("design: only a theme can be put into OBS; use Add to OBS on an overlay instead.".into())
}

fn role_patch(roles: &RoleScenes) -> (Map<String, Value>, Map<String, Value>) {
    let mut show = Map::new();
    show.insert("starting_scene".into(), json!(roles.starting));
    show.insert("live_scene".into(), json!(roles.live));
    show.insert("ending_scene".into(), json!(roles.ending));
    let mut scenes = Map::new();
    scenes.insert("brb_scene".into(), json!(roles.brb));
    (show, scenes)
}

impl Engine {
    fn current_roles(&self) -> RoleScenes {
        let show = self.show_config();
        RoleScenes {
            starting: show.starting_scene,
            live: show.live_scene,
            brb: self.scenes_config().brb_scene,
            ending: show.ending_scene,
        }
    }

    async fn obs_theme_record(&self) -> Result<Option<ObsTheme>, EngineError> {
        // A record that no longer reads is treated as none (it can't be undone anyway).
        Ok(self.db(|s| repo::settings::get::<ObsTheme>(s, KEY)).await.unwrap_or(None))
    }

    /// The theme in OBS now ("Your theme in OBS"); `found` says whether each scene is still there.
    pub async fn obs_theme(&self) -> Result<Option<ObsThemeView>, EngineError> {
        let Some(rec) = self.obs_theme_record().await? else { return Ok(None) };
        let obs = match self.obs_client().await {
            Ok(c) => ferrocast_obs::control::scenes(&c).await.ok(),
            Err(_) => None,
        };
        Ok(Some(rec.view(obs.as_ref())))
    }

    fn theme_screens(&self, design: &ferrocast_core::design::Design) -> Result<Vec<Scene>, EngineError> {
        match &design.content {
            DesignContent::Theme(t) => Ok(t.scenes.clone()),
            _ => Err(not_theme()),
        }
    }

    /// Ferrocast's guesses for putting a theme into OBS, with what OBS has.
    pub async fn theme_obs_plan(&self, design_id: &str) -> Result<ThemeObsPlan, EngineError> {
        let design = self.design(design_id).await?;
        let screens = self.theme_screens(&design)?;
        let client = self.obs_client().await?;
        let obs = ferrocast_obs::control::scenes(&client).await.map_err(obs_error)?;
        let old = ferrocast_obs::theme::old_widgets(&client).await.map_err(obs_error)?;
        let rec = self.obs_theme_record().await?;
        let made: Vec<String> = rec.as_ref().map(|r| r.made_scenes.clone()).unwrap_or_default();
        let inputs = ferrocast_obs::control::inputs(&client).await.map_err(obs_error)?;
        Ok(ThemeObsPlan {
            plan: guess(&design.name, &screens, &obs, &old, &made),
            screens: screens.iter().map(|s| ScreenInfo { key: s.key.clone(), name: s.name.clone() }).collect(),
            obs_scenes: obs.scenes.iter().map(|s| s.name.clone()).collect(),
            obs_cameras: inputs
                .into_iter()
                .filter(|(_, k)| CAMERA_KINDS.contains(&k.as_str()))
                .map(|(n, _)| n)
                .collect(),
            old_widgets: old
                .into_iter()
                .map(|w| OldWidgetView {
                    source: w.source,
                    vendor: w.vendor,
                    scenes: w.scenes.into_iter().map(|(s, _, _)| s).collect(),
                })
                .collect(),
            applied: rec.map(|r| r.view(Some(&obs))),
        })
    }

    /// Puts a theme into OBS: `plan` from the Advanced table, or Ferrocast's guesses.
    pub async fn apply_theme_to_obs(
        &self,
        design_id: &str,
        plan: Option<ThemePlan>,
    ) -> Result<ThemeObsResult, EngineError> {
        let _one = BUSY.lock().await;
        let design = self.design(design_id).await?;
        let screens = self.theme_screens(&design)?;
        let previous = self.obs_theme_record().await?;
        // Another theme in OBS: this one takes its place in the same scenes and sources.
        let switching = previous.as_ref().is_some_and(|p| p.design_id != design.id);
        let client = self.obs_client().await?;
        let obs = ferrocast_obs::control::scenes(&client).await.map_err(obs_error)?;
        let sources: Vec<String> =
            ferrocast_obs::control::inputs(&client).await.map_err(obs_error)?.into_iter().map(|(n, _)| n).collect();
        let plan = match plan {
            Some(p) => p,
            None => {
                let old = ferrocast_obs::theme::old_widgets(&client).await.map_err(obs_error)?;
                let made = previous.as_ref().map(|r| r.made_scenes.clone()).unwrap_or_default();
                let mut g = guess(&design.name, &screens, &obs, &old, &made);
                // No plan with a theme in OBS (Make it yours, again): the same screens as now.
                if let Some(prev) = &previous {
                    for p in &mut g.screens {
                        if !prev.screens.iter().any(|s| s.key == p.key) {
                            *p = ScreenPlan { key: p.key.clone(), place: Place::Skip, under: None, role: None };
                        }
                    }
                }
                g
            }
        };
        check(&plan, &screens, &obs, &sources).map_err(EngineError::Invalid)?;

        let mut rec = previous.unwrap_or_else(|| ObsTheme {
            design_id: design.id.clone(),
            design_name: design.name.clone(),
            at: now().0,
            screens: Vec::new(),
            made_scenes: Vec::new(),
            made_inputs: Vec::new(),
            added_items: Vec::new(),
            hidden: Vec::new(),
            roles_before: self.current_roles(),
            roles_set: RoleScenes::default(),
            alert_changed: false,
            alert_before: None,
            scene_before: obs.current.clone(),
            mic_items: Vec::new(),
        });
        rec.at = now().0;
        rec.design_id = design.id.clone();
        rec.design_name = design.name.clone();
        let mut steps = Vec::new();
        if switching {
            steps.push(Step::new(
                "switch_theme",
                "Theme",
                "done",
                format!("{} takes the old theme's place.", design.name),
            ));
        }
        let overlays = self.overlays().await?;
        // Turned off: the mics Ferrocast added come out again (ones that were there stay).
        if !plan.mic_on_breaks && !rec.mic_items.is_empty() {
            for (scene, m) in std::mem::take(&mut rec.mic_items) {
                if ferrocast_obs::theme::remove_item(&client, &scene, &m).await.is_ok() {
                    rec.added_items.retain(|(s, x)| !(s == &scene && x == &m));
                } else {
                    rec.mic_items.push((scene, m));
                }
            }
        }
        // The mic for break screens: Start show's mic check list, else every mic OBS has.
        let mics: Vec<String> = if plan.mic_on_breaks {
            let kinds = ferrocast_obs::control::inputs(&client).await.map_err(obs_error)?;
            let picked: Vec<String> =
                self.show_config().mic_sources.into_iter().filter(|m| kinds.iter().any(|(n, _)| n == m)).collect();
            if picked.is_empty() {
                kinds.into_iter().filter(|(_, k)| MIC_KINDS.contains(&k.as_str())).map(|(n, _)| n).collect()
            } else {
                picked
            }
        } else {
            Vec::new()
        };
        let mut roles = self.current_roles();
        let mut starting_scene = None;

        for p in &plan.screens {
            let Some(screen) = screens.iter().find(|s| s.key == p.key) else { continue };
            let Some(scene) = target(&plan, screen, p) else {
                steps.push(Step::new(&format!("screen.{}", p.key), &screen.name, "skipped", "Left out."));
                continue;
            };
            match self.put_screen(&client, &design, screen, p, &scene, &overlays, &mics, &obs, &mut rec).await {
                Ok(detail) => {
                    steps.push(Step::new(&format!("screen.{}", p.key), &screen.name, "done", detail));
                    match p.role {
                        Some(Role::Starting) => {
                            roles.starting = Some(scene.clone());
                            starting_scene = Some(scene.clone());
                        }
                        Some(Role::Live) => roles.live = Some(scene.clone()),
                        Some(Role::Brb) => roles.brb = Some(scene.clone()),
                        Some(Role::Ending) => roles.ending = Some(scene.clone()),
                        None => {}
                    }
                }
                Err(e) => steps.push(Step::new(&format!("screen.{}", p.key), &screen.name, "failed", e.to_string())),
            }
        }

        // Start show's and the BRB button's scenes.
        let (show, scenes) = role_patch(&roles);
        let set = async {
            self.patch_show_config(&show).await?;
            self.patch_scenes_config(&scenes).await
        };
        match set.await {
            Ok(_) => {
                rec.roles_set = roles.clone();
                let n =
                    [&roles.starting, &roles.live, &roles.brb, &roles.ending].iter().filter(|r| r.is_some()).count();
                steps.push(Step::new("roles", "Start show's scenes", "done", format!("Start show uses {n} of them.")));
            }
            Err(e) => steps.push(Step::new("roles", "Start show's scenes", "failed", e.to_string())),
        }

        // Old widgets: hidden, not deleted.
        if !plan.hide_old.is_empty() {
            match ferrocast_obs::theme::old_widgets(&client).await {
                Ok(found) => {
                    let mut n = 0;
                    for w in found.iter().filter(|w| plan.hide_old.contains(&w.source)) {
                        for (scene, id, enabled) in &w.scenes {
                            if *enabled && ferrocast_obs::control::set_visible(&client, scene, *id, false).await.is_ok()
                            {
                                rec.hidden.push((scene.clone(), *id));
                                n += 1;
                            }
                        }
                    }
                    steps.push(Step::new(
                        "old",
                        "Old widgets",
                        "done",
                        if n == 0 { "Already hidden.".to_string() } else { format!("Hid {n} (not deleted).") },
                    ));
                }
                Err(e) => steps.push(Step::new("old", "Old widgets", "failed", e.to_string())),
            }
        }

        // The theme's alert look.
        if plan.alert_style
            && let DesignContent::Theme(t) = &design.content
            && let Some(a) = &t.alert_design
        {
            let before = self.active_alert_design().await?;
            match self.set_active_alert_design(Some(a)).await {
                Ok(_) => {
                    if !rec.alert_changed {
                        rec.alert_changed = true;
                        rec.alert_before = before;
                    }
                    steps.push(Step::new(
                        "alerts",
                        "Alert style",
                        "done",
                        format!("Alerts play with {}'s look.", design.name),
                    ));
                }
                Err(e) => steps.push(Step::new("alerts", "Alert style", "failed", e.to_string())),
            }
        }

        if plan.switch_to_starting
            && let Some(scene) = &starting_scene
        {
            steps.push(match ferrocast_obs::control::switch_scene(&client, scene).await {
                Ok(()) => Step::new("switch", "OBS", "done", format!("OBS is on \"{scene}\".")),
                Err(e) => Step::new("switch", "OBS", "failed", e.to_string()),
            });
        }

        let saved = rec.clone();
        self.db(move |s| repo::settings::set(s, KEY, &saved)).await?;
        let after = ferrocast_obs::control::scenes(&client).await.ok();
        Ok(ThemeObsResult { steps, applied: Some(rec.view(after.as_ref())) })
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_screen(
        &self,
        client: &ferrocast_obs::Client,
        design: &ferrocast_core::design::Design,
        screen: &Scene,
        p: &ScreenPlan,
        scene: &str,
        overlays: &[crate::snapshot::OverlayView],
        mics: &[String],
        obs: &Scenes,
        rec: &mut ObsTheme,
    ) -> Result<String, EngineError> {
        // The overlay: the one made from this screen before (its address stays), or a new one.
        let existing = rec
            .screens
            .iter()
            .find(|s| s.key == p.key)
            .map(|s| s.overlay_id.clone())
            .filter(|id| overlays.iter().any(|o| &o.id == id))
            .or_else(|| {
                overlays
                    .iter()
                    .find(|o| o.design.as_ref().is_some_and(|d| d.id == design.id && d.scene == p.key))
                    .map(|o| o.id.clone())
            });
        let name: String = format!("{} ({})", screen.name, design.name).chars().take(60).collect();
        let name = name.trim().to_string();
        // As Use does: no overlay edit in between (released before OBS is asked anything).
        let mut overlay = {
            let _one_at_a_time = self.config_lock().await;
            self.place_scene(design, screen, &name, existing.as_deref()).await?
        };
        // Switching theme: the overlay and its OBS source take the new name (the URL stays).
        if overlay.name != name {
            let old_source = ferrocast_obs::source_name(&overlay.name);
            let new_source = ferrocast_obs::source_name(&name);
            // OBS first: if it refuses, the overlay keeps its old name and the next apply retries.
            if ferrocast_obs::theme::rename_input(client, &old_source, &new_source).await.map_err(obs_error)? {
                for n in rec.made_inputs.iter_mut().chain(rec.added_items.iter_mut().map(|(_, s)| s)) {
                    if *n == old_source {
                        n.clone_from(&new_source);
                    }
                }
            }
            overlay = self.rename_overlay(&overlay.id, &name).await?;
        }
        // And a scene Ferrocast made for this screen takes the new name.
        if matches!(p.place, Place::New)
            && let Some(old) = rec.screens.iter().find(|s| s.key == p.key).map(|s| s.scene.clone())
            && old != scene
            && rec.made_scenes.contains(&old)
            && ferrocast_obs::theme::rename_scene(client, &old, scene).await.map_err(obs_error)?
        {
            for n in &mut rec.made_scenes {
                if *n == old {
                    *n = scene.to_string();
                }
            }
        }

        if matches!(p.place, Place::New)
            && ferrocast_obs::theme::ensure_scene(client, scene).await.map_err(obs_error)?
        {
            rec.made_scenes.push(scene.to_string());
        }
        let mine = rec.made_scenes.iter().any(|s| s == scene);
        let mut hid = 0;
        if matches!(p.place, Place::Replace { .. }) {
            let all = ferrocast_obs::control::scenes(client).await.map_err(obs_error)?;
            if let Some(sc) = all.scenes.iter().find(|s| s.name == scene) {
                for it in &sc.items {
                    let looks = it.kind.as_deref().is_some_and(|k| k == "browser_source" || k == "image_source");
                    if it.enabled && looks && !it.source.starts_with("Ferrocast:") {
                        ferrocast_obs::control::set_visible(client, scene, it.id, false).await.map_err(obs_error)?;
                        rec.hidden.push((scene.to_string(), it.id));
                        hid += 1;
                    }
                }
            }
        }
        let source = ferrocast_obs::source_name(&overlay.name);
        let placed = ferrocast_obs::theme::put_overlay(
            client,
            scene,
            &source,
            &overlay.url,
            u32::try_from(overlay.width).unwrap_or(1920),
            u32::try_from(overlay.height).unwrap_or(1080),
        )
        .await
        .map_err(obs_error)?;
        if placed.input_created {
            rec.made_inputs.push(source.clone());
        } else if placed.item_added && !mine {
            rec.added_items.push((scene.to_string(), source.clone()));
        }
        let mut under_added = false;
        if let Some(u) = &p.under {
            under_added = ferrocast_obs::theme::put_under(client, scene, u).await.map_err(obs_error)?;
            if under_added && !mine {
                rec.added_items.push((scene.to_string(), u.clone()));
            }
        }
        let break_screen = matches!(p.role, Some(Role::Starting | Role::Brb | Role::Ending))
            || matches!(p.key.as_str(), "starting_soon" | "brb" | "ending");
        let mut mic_added = false;
        if break_screen {
            // A scene under it that has the mic already brings it: no second copy.
            let under_has = |m: &String| {
                p.under.as_ref().is_some_and(|u| {
                    u == m || obs.scenes.iter().any(|x| &x.name == u && x.items.iter().any(|i| &i.source == m))
                })
            };
            for m in mics.iter().filter(|m| !under_has(m)) {
                if ferrocast_obs::theme::put_under(client, scene, m).await.map_err(obs_error)? {
                    mic_added = true;
                    rec.mic_items.push((scene.to_string(), m.clone()));
                    if !mine {
                        rec.added_items.push((scene.to_string(), m.clone()));
                    }
                }
            }
        }
        rec.screens.retain(|s| s.key != p.key);
        rec.screens.push(AppliedScreen {
            key: p.key.clone(),
            name: screen.name.clone(),
            scene: scene.to_string(),
            overlay_id: overlay.id.clone(),
            role: p.role,
        });
        let mut detail = match p.place {
            Place::New if mine => format!("In the scene \"{scene}\""),
            _ => format!("On top of \"{scene}\""),
        };
        if let Some(u) = &p.under {
            detail.push_str(&format!(", with \"{u}\" under it"));
            if !under_added {
                detail.push_str(" (already there)");
            }
        }
        if mic_added {
            detail.push_str(", with your mic");
        }
        if hid > 0 {
            detail.push_str(&format!(", {hid} old source{} hidden", if hid == 1 { "" } else { "s" }));
        }
        detail.push('.');
        Ok(detail)
    }

    /// Takes the theme out of OBS: only what Ferrocast added, and Start show's scenes back.
    pub async fn undo_theme_in_obs(&self) -> Result<ThemeObsResult, EngineError> {
        let _one = BUSY.lock().await;
        let Some(rec) = self.obs_theme_record().await? else {
            return Err(EngineError::Missing {
                code: "no_theme_in_obs",
                message: "Ferrocast hasn't put a theme into OBS.",
                hint: "Put a theme into OBS from Designs first.",
            });
        };
        let client = self.obs_client().await?;
        let mut steps = Vec::new();
        // What couldn't be undone stays in the record, so Undo can be tried again.
        let mut left = ObsTheme {
            screens: rec.screens.clone(),
            made_scenes: Vec::new(),
            made_inputs: Vec::new(),
            added_items: Vec::new(),
            hidden: Vec::new(),
            ..rec.clone()
        };
        // Shown again first, while the scenes they're in still exist.
        for (scene, id) in &rec.hidden {
            if ferrocast_obs::theme::show_item(&client, scene, *id).await.is_err() {
                left.hidden.push((scene.clone(), *id));
            }
        }
        for (scene, source) in &rec.added_items {
            if ferrocast_obs::theme::remove_item(&client, scene, source).await.is_err() {
                left.added_items.push((scene.clone(), source.clone()));
            }
        }
        for input in &rec.made_inputs {
            if ferrocast_obs::theme::remove_input(&client, input).await.is_err() {
                left.made_inputs.push(input.clone());
            }
        }
        // OBS needs a scene on air: the made ones stay if they're all OBS has.
        let current = ferrocast_obs::control::scene_names(&client).await.map_err(obs_error)?;
        let fallback = current.1.iter().find(|n| !rec.made_scenes.contains(n)).cloned();
        match &fallback {
            Some(f) => {
                if current.0.as_ref().is_some_and(|c| rec.made_scenes.contains(c)) {
                    let back = [&rec.scene_before, &rec.roles_before.live]
                        .into_iter()
                        .flatten()
                        .find(|s| current.1.contains(s) && !rec.made_scenes.contains(s))
                        .cloned()
                        .unwrap_or_else(|| f.clone());
                    let _ = ferrocast_obs::control::switch_scene(&client, &back).await;
                }
                for scene in &rec.made_scenes {
                    if ferrocast_obs::theme::remove_scene(&client, scene).await.is_err() {
                        left.made_scenes.push(scene.clone());
                    }
                }
            }
            None => left.made_scenes = rec.made_scenes.clone(),
        }
        let pending = left.hidden.len() + left.added_items.len() + left.made_inputs.len() + left.made_scenes.len();
        let n = rec.made_scenes.len();
        steps.push(if pending == 0 {
            Step::new("obs", "OBS", "done", format!("Took out {n} scene{} and Ferrocast's sources.", if n == 1 { "" } else { "s" }))
        } else if fallback.is_none() {
            Step::new("obs", "OBS", "failed", "The theme's scenes are the only ones in OBS, and OBS needs one. Add a scene of your own, then take it out again.")
        } else {
            Step::new(
                "obs",
                "OBS",
                "failed",
                format!("OBS refused {pending} change{}. Check OBS is open, then take it out again.", if pending == 1 { "" } else { "s" }),
            )
        });

        // Start show's scenes go back only where they're still what Ferrocast set.
        let now_roles = self.current_roles();
        let pick = |cur: &Option<String>, set: &Option<String>, before: &Option<String>| {
            if cur == set { before.clone() } else { cur.clone() }
        };
        let back = RoleScenes {
            starting: pick(&now_roles.starting, &rec.roles_set.starting, &rec.roles_before.starting),
            live: pick(&now_roles.live, &rec.roles_set.live, &rec.roles_before.live),
            brb: pick(&now_roles.brb, &rec.roles_set.brb, &rec.roles_before.brb),
            ending: pick(&now_roles.ending, &rec.roles_set.ending, &rec.roles_before.ending),
        };
        let (show, scenes) = role_patch(&back);
        steps.push(
            match async {
                self.patch_show_config(&show).await?;
                self.patch_scenes_config(&scenes).await
            }
            .await
            {
                Ok(_) => Step::new("roles", "Start show's scenes", "done", "Back as they were."),
                Err(e) => Step::new("roles", "Start show's scenes", "failed", e.to_string()),
            },
        );
        if rec.alert_changed {
            let before = &rec.alert_before;
            let theme_alert = match &self.design(&rec.design_id).await.map(|d| d.content) {
                Ok(DesignContent::Theme(t)) => t.alert_design.clone(),
                _ => None,
            };
            if self.active_alert_design().await? == theme_alert {
                steps.push(match self.set_active_alert_design(before.as_deref()).await {
                    Ok(_) => Step::new("alerts", "Alert style", "done", "Back as it was."),
                    Err(e) => Step::new("alerts", "Alert style", "failed", e.to_string()),
                });
            }
        }
        if pending == 0 {
            self.db(|s| repo::settings::delete(s, KEY)).await?;
            return Ok(ThemeObsResult { steps, applied: None });
        }
        // Start show and the alert look are back already: only OBS's leftovers are kept.
        left.roles_set = left.roles_before.clone();
        left.alert_changed = false;
        let saved = left.clone();
        self.db(move |s| repo::settings::set(s, KEY, &saved)).await?;
        let after = ferrocast_obs::control::scenes(&client).await.ok();
        Ok(ThemeObsResult { steps, applied: Some(left.view(after.as_ref())) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrocast_obs::control::{Scene as ObsScene, SceneItem};

    fn item(source: &str, kind: &str) -> SceneItem {
        SceneItem { id: 1, source: source.into(), enabled: true, kind: Some(kind.into()) }
    }

    fn obs() -> Scenes {
        Scenes {
            current: Some("Starting".into()),
            scenes: vec![
                ObsScene { name: "Starting".into(), items: vec![item("Card", "image_source")] },
                ObsScene {
                    name: "Gameplay".into(),
                    items: vec![item("Game", "game_capture"), item("Cam", "dshow_input")],
                },
                ObsScene { name: "Talk".into(), items: vec![item("Cam", "dshow_input")] },
                ObsScene { name: "BRB".into(), items: vec![] },
            ],
        }
    }

    fn screens() -> Vec<Scene> {
        ["in_game", "just_chatting", "starting_soon", "brb", "ending"]
            .iter()
            .map(|k| Scene {
                key: (*k).into(),
                name: k.replace('_', " "),
                width: 1920,
                height: 1080,
                background: None,
                layers: vec![],
            })
            .collect()
    }

    #[test]
    fn guesses_put_the_game_and_chat_scenes_under_their_screens() {
        let p = guess("Forge", &screens(), &obs(), &[], &[]);
        assert_eq!(p.prefix, "Forge · ");
        assert_eq!(guess("Forge (mine)", &screens(), &obs(), &[], &[]).prefix, "Forge · ");
        assert_eq!(guess("Forge copy", &screens(), &obs(), &[], &[]).prefix, "Forge · ");
        let by = |k: &str| p.screens.iter().find(|s| s.key == k).unwrap().clone();
        assert_eq!(by("in_game").under.as_deref(), Some("Gameplay"));
        assert_eq!(by("in_game").role, Some(Role::Live));
        assert_eq!(by("just_chatting").under.as_deref(), Some("Talk"));
        assert_eq!(by("just_chatting").role, None);
        assert_eq!(by("starting_soon").role, Some(Role::Starting));
        assert!(by("brb").under.is_none() && by("brb").place == Place::New);
        check(&p, &screens(), &obs(), &[]).unwrap();
    }

    #[test]
    fn without_a_chat_scene_the_camera_goes_under_just_chatting() {
        let mut o = obs();
        o.scenes.retain(|s| s.name != "Talk");
        let p = guess("Forge", &screens(), &o, &[], &[]);
        assert_eq!(p.screens[1].under.as_deref(), Some("Cam"));
    }

    #[test]
    fn the_game_scene_wins_over_a_recording_one_even_with_its_capture_off() {
        let off = |source: &str, kind: &str| SceneItem {
            id: 1,
            source: source.into(),
            enabled: false,
            kind: Some(kind.into()),
        };
        let o = Scenes {
            current: Some("Be Right Back".into()),
            scenes: vec![
                ObsScene { name: "Starting Scene".into(), items: vec![item("Intro", "ffmpeg_source")] },
                ObsScene {
                    name: "Gameplay".into(),
                    items: vec![
                        off("macOS Screen Capture", "screen_capture"),
                        item("Video Capture Device", "macos-avcapture"),
                    ],
                },
                ObsScene { name: "Be Right Back".into(), items: vec![item("BRB", "image_source")] },
                ObsScene { name: "Chatting".into(), items: vec![item("Webcam", "macos-avcapture")] },
                ObsScene { name: "Recording".into(), items: vec![item("macOS Screen Capture", "screen_capture")] },
            ],
        };
        let p = guess("Forge", &screens(), &o, &[], &[]);
        assert_eq!(p.screens[0].under.as_deref(), Some("Gameplay"));
        assert_eq!(p.screens[1].under.as_deref(), Some("Chatting"));
    }

    #[test]
    fn a_break_scene_is_never_the_game_even_when_its_on_air() {
        let o = Scenes {
            current: Some("Starting".into()),
            scenes: vec![
                ObsScene { name: "Starting".into(), items: vec![] },
                ObsScene { name: "Main".into(), items: vec![] },
            ],
        };
        assert_eq!(guess("Forge", &screens(), &o, &[], &[]).screens[0].under.as_deref(), Some("Main"));
    }

    #[test]
    fn plans_that_cant_work_are_refused_with_the_field() {
        let base = guess("Forge", &screens(), &obs(), &[], &[]);
        let bad = |f: &dyn Fn(&mut ThemePlan)| {
            let mut p = base.clone();
            f(&mut p);
            check(&p, &screens(), &obs(), &["Cam".into()]).unwrap_err()
        };
        assert!(bad(&|p| p.screens[0].key = "nope".into()).starts_with("screens.0.key"));
        assert!(bad(&|p| p.screens[1].key = "in_game".into()).contains("twice"));
        assert!(bad(&|p| p.screens[2].place = Place::OnTop { scene: "Missing".into() }).contains("no scene"));
        assert!(bad(&|p| p.screens[3].role = Some(Role::Live)).contains("only one screen"));
        assert!(bad(&|p| p.screens[0].under = Some("Nothing here".into())).contains("under"));
        assert!(
            bad(&|p| {
                p.screens[2].place = Place::OnTop { scene: "BRB".into() };
                p.screens[3].place = Place::OnTop { scene: "BRB".into() };
            })
            .contains("two screens")
        );
        assert!(
            bad(&|p| p.screens[4] =
                ScreenPlan { key: "ending".into(), place: Place::Skip, under: None, role: Some(Role::Ending) })
            .contains("left out")
        );
        assert!(bad(&|p| p.prefix = "x".repeat(41)).starts_with("prefix"));
        assert!(bad(&|p| p.screens[1].under = Some("Forge · in game".into())).starts_with("screens.1.under"));
        // Under a camera input is fine.
        let mut ok = base.clone();
        ok.screens[1].under = Some("Cam".into());
        check(&ok, &screens(), &obs(), &["Cam".into()]).unwrap();
    }
}

#[cfg(test)]
#[path = "obs_theme_tests.rs"]
mod engine_tests;
